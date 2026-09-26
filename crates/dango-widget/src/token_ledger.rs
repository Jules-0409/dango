//! The process-wide token ledger: one scanner shared by `GET /tokens` and the
//! detail card's "today" line. Scans are blocking file IO, so they run on the
//! blocking pool; the ledger is persisted after every scan. Cursor's usage
//! comes from its server instead of a log file and is synced every 5 min.

use std::collections::BTreeMap;
use std::sync::Mutex;

use dango_lib::tokens::{self, Ledger, TokenReport};

static LEDGER: Mutex<Option<Ledger>> = Mutex::new(None);
/// Cursor's usage events come from its server: at most one sync per
/// [`CURSOR_EVERY`]. (last attempt, last error, events booked last time)
static CURSOR_SYNC: Mutex<(Option<std::time::Instant>, Option<String>, usize)> =
    Mutex::new((None, None, 0));
const CURSOR_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
/// First sync reaches this far back.
const CURSOR_BACKFILL_MS: i64 = 30 * 86_400_000;
/// source id → today's total, refreshed by every scan; read by the card.
static TODAY: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

/// Which ledger source a plan's ball stands for (plans without a local log
/// have none).
pub fn source_for_plan(plan_id: &str) -> Option<&'static str> {
    match plan_id {
        "claude" => Some(tokens::SOURCE_CLAUDE_CODE),
        "factory" => Some(tokens::SOURCE_FACTORY),
        "cursor" => Some(tokens::SOURCE_CURSOR),
        "devin" => Some(tokens::SOURCE_DEVIN),
        _ => None,
    }
}

/// Today's tokens for a plan's source, as of the last scan.
pub fn today_for_plan(plan_id: &str) -> Option<u64> {
    let source = source_for_plan(plan_id)?;
    TODAY.lock().ok()?.get(source).copied()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn with_ledger<T>(f: impl FnOnce(&mut Ledger) -> T) -> Result<T, String> {
    let path = tokens::ledger_path()?;
    let mut guard = LEDGER.lock().map_err(|_| "token ledger lock poisoned")?;
    if guard.is_none() {
        *guard = Some(Ledger::load(&path)?);
    }
    let ledger = guard.as_mut().ok_or("token ledger missing")?;
    Ok(f(ledger))
}

/// Pull new Cursor usage events when due. Errors are kept for the source
/// status line, never turned into "no usage".
async fn sync_cursor() {
    {
        let Ok(state) = CURSOR_SYNC.lock() else {
            return;
        };
        if state.0.is_some_and(|at| at.elapsed() < CURSOR_EVERY) {
            return;
        }
    }
    let now = now_ms();
    let since = match tokio::task::spawn_blocking(move || {
        with_ledger(|ledger| ledger.cursor_sync_since(now, CURSOR_BACKFILL_MS))
    })
    .await
    {
        Ok(Ok(since)) => since,
        _ => return,
    };
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::Client::new);
    let result = dango_lib::probes::fetch_cursor_usage_events(client, since, now).await;
    let outcome = match result {
        Ok(events) => {
            let count = events.len();
            let booked = tokio::task::spawn_blocking(move || {
                with_ledger(|ledger| ledger.book_cursor(&events, now))
            })
            .await;
            match booked {
                Ok(Ok(())) => (None, count),
                Ok(Err(error)) => (Some(error), 0),
                Err(error) => (Some(error.to_string()), 0),
            }
        }
        Err(error) => (Some(error), 0),
    };
    if let Ok(mut state) = CURSOR_SYNC.lock() {
        *state = (Some(std::time::Instant::now()), outcome.0, outcome.1);
    }
}

fn cursor_status() -> tokens::SourceStatus {
    let (at, error, count) = CURSOR_SYNC.lock().map(|state| state.clone()).unwrap_or((
        None,
        Some("状态锁失败".into()),
        0,
    ));
    tokens::SourceStatus {
        id: tokens::SOURCE_CURSOR.into(),
        label: "Cursor".into(),
        found: at.is_some() && error.is_none(),
        files: count,
        error,
    }
}

/// Scan all sources, persist, and report the last `days` days.
pub fn scan_blocking(days: u32) -> Result<TokenReport, String> {
    let path = tokens::ledger_path()?;
    let home = std::env::var_os("HOME").ok_or("HOME is unavailable")?;
    let mut guard = LEDGER.lock().map_err(|_| "token ledger lock poisoned")?;
    if guard.is_none() {
        *guard = Some(Ledger::load(&path)?);
    }
    let ledger = guard.as_mut().ok_or("token ledger missing")?;
    let mut sources = ledger.scan(std::path::Path::new(&home));
    sources.insert(2, cursor_status());
    ledger.save(&path)?;
    let report = ledger.report(now_ms(), days, sources);
    if let (Some(today), Ok(mut cache)) = (report.days.last(), TODAY.lock()) {
        cache.clear();
        for (source, counts) in &today.by_source {
            cache.insert(source.clone(), counts.total());
        }
    }
    Ok(report)
}

pub async fn scan(days: u32) -> Result<TokenReport, String> {
    sync_cursor().await;
    tokio::task::spawn_blocking(move || scan_blocking(days))
        .await
        .map_err(|error| format!("token scan task failed: {error}"))?
}

/// Fed from every snapshot: Devin's daily-quota reading (its only usage
/// signal). Kept in memory; persisted with the next scan.
pub fn record_devin(plans: &[dango_lib::models::PlanQuota]) {
    let Some(remaining) = plans
        .iter()
        .find(|plan| plan.id == "devin" && plan.ok)
        .and_then(|plan| {
            plan.buckets
                .iter()
                .find(|b| b.window.as_deref() == Some("day"))
        })
        .and_then(|bucket| bucket.remaining_percent)
    else {
        return;
    };
    let now = now_ms();
    let _ = with_ledger(|ledger| ledger.record_devin_daily(remaining, now));
}
