//! Local token ledger: what the AI tools on this machine actually burned, read
//! from their own on-disk logs (never from a vendor API).
//!
//! Sources:
//! - **Claude Code** — `~/.claude/projects/**/*.jsonl` (and `~/.config/claude`):
//!   one entry per streamed content block, each carrying the message's full
//!   `usage`. Blocks of one message repeat identical usage, so entries are
//!   de-duplicated on `message.id` + `requestId`.
//! - **Factory** — `~/.factory/sessions/*/<id>.settings.json` holds only a
//!   running `tokenUsage` total per session. The ledger remembers the last
//!   total it saw and books the growth on the day the file was last written.
//! - **Devin** — `~/.local/share/devin/cli/sessions.db` (the CLI's own store,
//!   shared by desktop-spawned `devin acp` sessions too): every assistant
//!   chain node carries `metadata.metrics` with real per-inference
//!   input/output/cache token counts and the model id. Rows are append-only
//!   (AUTOINCREMENT `row_id`), so the scan cursor is just the last row seen.
//! - **DimAgent** — `~/.dimcode/v2/dimcode.sqlite`, table `usage_ledger`: one
//!   append-only row per agent run with `promptTokens` (cache reads included),
//!   `completionTokens`, `cacheReadTokens`. The paying end comes from the
//!   run's provider: DimAgent's own OAuth plan, or a custom provider's base URL.
//! - **Grok Build** — `~/.grok/sessions/<cwd>/<session>/usage.json`, rewritten
//!   after every turn: each turn carries `endedAt` and per-model counts
//!   (`inputTokens` includes cache reads). The ledger remembers the last turn
//!   booked per session.
//!
//! The ledger persists to `tokens.json` next to `settings.json`, so history
//! survives the tools pruning their logs. Files are read incrementally from
//! the byte offset reached last time; only complete lines are consumed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Days are cut in UTC+8. The system clock is deliberately set to New York;
/// the person reading these numbers lives in China (no DST there).
pub const DAY_OFFSET_SECS: i64 = 8 * 3600;
const LEDGER_VERSION: u32 = 1;

pub const SOURCE_CLAUDE_CODE: &str = "claude-code";
pub const SOURCE_FACTORY: &str = "factory";
pub const SOURCE_CURSOR: &str = "cursor";
pub const SOURCE_DEVIN: &str = "devin";
pub const SOURCE_DIM: &str = "dim";
pub const SOURCE_GROK: &str = "grok";
/// DimAgent's built-in provider id for its own subscription.
const DIM_OWN_PROVIDER: &str = "dimcode-api-oauth";
/// Route id prefix of local ports whose tokens are not kept: retired
/// proxies outside [`crate::ports`], and the Cursor Agent bridge.
const RETIRED_PREFIX: &str = "local:";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Counts {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, other: &Counts) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    /// Growth from `before` to `self`; `None` when any field went down (the
    /// session was reset or replaced — count it fresh).
    fn growth_since(&self, before: &Counts) -> Option<Counts> {
        Some(Counts {
            input: self.input.checked_sub(before.input)?,
            output: self.output.checked_sub(before.output)?,
            cache_read: self.cache_read.checked_sub(before.cache_read)?,
            cache_write: self.cache_write.checked_sub(before.cache_write)?,
        })
    }

    fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

/// date (`YYYY-MM-DD`) → model → counts.
type DayModelCounts = BTreeMap<String, BTreeMap<String, Counts>>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogFile {
    offset: u64,
    seen: BTreeSet<String>,
    days: DayModelCounts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FactorySession {
    model: String,
    last: Counts,
    days: BTreeMap<String, Counts>,
    /// Where this session's tokens were actually billed, resolved the first
    /// time the session is seen (configs get edited later; history must not
    /// move with them).
    #[serde(default)]
    route: Option<Route>,
}

impl FactorySession {
    fn is_retired(&self) -> bool {
        self.route
            .as_ref()
            .is_some_and(|route| route.id.starts_with(RETIRED_PREFIX))
    }
}

/// The paying end of a token: which subscription / API account it hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    /// Stable key: `anthropic`, `factory`, `gemini`, `qoder`,
    /// `cursor`, `local:<port>`, `host:<host>`, `unknown`.
    pub id: String,
    pub label: String,
    /// The base URL the agent was configured with, when there was one.
    pub endpoint: Option<String>,
    /// True when the mapping came from a config backup or the model id
    /// pointed at several endpoints over time — a best guess, not a fact.
    pub inferred: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    version: u32,
    claude_files: BTreeMap<String, LogFile>,
    /// Proxy-side request logs; inner key is `route\u{1f}account\u{1f}model`.
    #[serde(default)]
    proxy_files: BTreeMap<String, LogFile>,
    /// Dedupe keys per proxy source, shared by all of its files: a rotated
    /// log (`requests.jsonl` → `.1`) repeats lines already counted.
    #[serde(default)]
    proxy_seen: BTreeMap<String, BTreeSet<String>>,
    factory_sessions: BTreeMap<String, FactorySession>,
    /// Cursor's own server-side usage events (App, CLI Agent, Grok Bot).
    #[serde(default)]
    cursor: CursorLedger,
    /// Devin gives neither tokens nor (on Pro) ACU — only the daily quota's
    /// remaining %. date → lowest remaining % seen that day.
    #[serde(default)]
    devin_daily: BTreeMap<String, f64>,
    /// Devin's real per-inference token counts, read out of the CLI's own
    /// `sessions.db` (each assistant chain node carries `metadata.metrics`).
    #[serde(default)]
    devin: DevinLedger,
    /// DimAgent's `usage_ledger` rows, booked per day under `route\u{1f}model`.
    #[serde(default)]
    dim: DimLedger,
    /// Grok Build sessions' `usage.json`, booked per finished turn.
    #[serde(default)]
    grok: GrokLedger,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrokLedger {
    /// session id → highest turn number booked.
    turns: BTreeMap<String, u64>,
    days: DayModelCounts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DimLedger {
    /// usage_ledger is append-only, so the highest rowid seen is the cursor.
    last_rowid: i64,
    /// date → `route id\u{1f}model` → counts.
    days: DayModelCounts,
    /// Route id → route, as resolved when the row was booked.
    routes: BTreeMap<String, Route>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevinLedger {
    /// message_nodes rows are append-only (AUTOINCREMENT row_id), so the
    /// highest row seen is a complete scan cursor.
    last_rowid: i64,
    days: DayModelCounts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorLedger {
    /// Events up to here are booked; the next sync starts a little before.
    synced_until: i64,
    seen: BTreeSet<String>,
    days: DayModelCounts,
}

/// One source's scan outcome, shown on the page so a silent source is
/// distinguishable from an idle one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    pub id: String,
    pub label: String,
    pub found: bool,
    pub files: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayReport {
    pub date: String,
    /// Measured at this machine's own proxies (overlaps the agent view).
    pub by_proxy: BTreeMap<String, Counts>,
    /// Agent that spent the tokens (`claude-code`, `factory`).
    pub by_source: BTreeMap<String, Counts>,
    /// Route id that paid for them.
    pub by_route: BTreeMap<String, Counts>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelReport {
    pub source: String,
    pub model: String,
    pub route: String,
    pub counts: Counts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteReport {
    pub id: String,
    pub label: String,
    pub endpoints: Vec<String>,
    pub inferred: bool,
    pub counts: Counts,
    /// Which agents spent through this route.
    pub by_source: BTreeMap<String, Counts>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyReport {
    /// Same ids as [`Route::id`]: `gemini`, `qoder`, `cursor`.
    pub id: String,
    pub label: String,
    pub counts: Counts,
    /// Masked account → counts (the bridge knows which pool account paid).
    pub accounts: Vec<(String, Counts)>,
    pub models: Vec<(String, Counts)>,
    /// The part of the agent view that was routed here (Factory custom
    /// models). What the proxy saw beyond this came from other clients.
    pub from_agents: Counts,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenReport {
    pub today: String,
    /// Oldest first, one entry per day in range (empty days included).
    pub days: Vec<DayReport>,
    /// Range totals per (source, model), largest first.
    pub models: Vec<ModelReport>,
    /// Range totals per paying route, largest first.
    pub routes: Vec<RouteReport>,
    /// Devin's daily quota used, per day seen in range (date, used %).
    pub devin_daily: Vec<(String, f64)>,
    /// What the local proxies measured, per proxy (a second view of the
    /// same tokens — never added to the agent totals).
    pub proxies: Vec<ProxyReport>,
    pub sources: Vec<SourceStatus>,
}

/// `YYYY-MM-DD` of a unix-ms instant in the ledger's day zone.
pub fn day_key(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000) + DAY_OFFSET_SECS;
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|at| at.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".into())
}

fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.timestamp_millis())
}

fn shift_day(date: &str, delta_days: i64) -> Option<String> {
    let day = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let shifted = day.checked_add_signed(chrono::Duration::days(delta_days))?;
    Some(shifted.format("%Y-%m-%d").to_string())
}

pub fn ledger_path() -> Result<PathBuf, String> {
    crate::settings::settings_dir().map(|dir| dir.join("tokens.json"))
}

impl Ledger {
    /// Load from disk; a missing file is an empty ledger, an unreadable one
    /// is an error (never silently start over and lose history).
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let ledger: Ledger = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("tokens.json 解析失败: {error}"))?;
                Ok(ledger)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Ledger {
                version: LEDGER_VERSION,
                ..Ledger::default()
            }),
            Err(error) => Err(format!("tokens.json 读取失败: {error}")),
        }
    }

    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).map_err(|error| error.to_string())?;
        std::fs::rename(&tmp, path).map_err(|error| error.to_string())
    }

    /// Scan every source under `home`; returns per-source status.
    pub fn scan(&mut self, home: &Path) -> Vec<SourceStatus> {
        let claude_roots = [
            home.join(".claude").join("projects"),
            home.join(".config").join("claude").join("projects"),
        ];
        let bridge_logs = home.join(".antigravity-bridge/logs");
        vec![
            self.scan_claude_code(&claude_roots),
            self.scan_factory(&home.join(".factory").join("sessions")),
            self.scan_devin(&home.join(".local/share/devin/cli/sessions.db")),
            self.scan_dim(&home.join(".dimcode/v2/dimcode.sqlite")),
            self.scan_grok(&home.join(".grok/sessions")),
            self.scan_proxy_logs(
                "proxy:gemini",
                "Gemini 桥",
                std::iter::once(bridge_logs.join("requests.jsonl"))
                    .chain((1..=5).map(|n| bridge_logs.join(format!("requests.jsonl.{n}"))))
                    .collect(),
                parse_bridge_line,
            ),
        ]
    }

    fn scan_claude_code(&mut self, roots: &[PathBuf]) -> SourceStatus {
        let mut status = SourceStatus {
            id: SOURCE_CLAUDE_CODE.into(),
            label: "Claude Code".into(),
            found: false,
            files: 0,
            error: None,
        };
        let mut errors = Vec::new();
        for root in roots.iter().filter(|root| root.is_dir()) {
            status.found = true;
            let mut files = Vec::new();
            collect_jsonl(root, &mut files, 0);
            for file in files {
                status.files += 1;
                let key = file.to_string_lossy().into_owned();
                let entry = self.claude_files.entry(key).or_default();
                if let Err(error) = read_jsonl_log(&file, entry, None, &parse_claude_line) {
                    errors.push(format!("{}: {error}", file.display()));
                }
            }
        }
        if !errors.is_empty() {
            status.error = Some(format!("{} 个文件读取失败：{}", errors.len(), errors[0]));
        }
        status
    }

    fn scan_proxy_logs(
        &mut self,
        id: &str,
        label: &str,
        files: Vec<PathBuf>,
        parse: impl Fn(&[u8]) -> Option<LineUsage>,
    ) -> SourceStatus {
        let mut status = SourceStatus {
            id: id.into(),
            label: label.into(),
            found: false,
            files: 0,
            error: None,
        };
        let mut errors = Vec::new();
        for file in files.into_iter().filter(|file| file.is_file()) {
            status.found = true;
            status.files += 1;
            let key = file.to_string_lossy().into_owned();
            let entry = self.proxy_files.entry(key).or_default();
            let seen = self.proxy_seen.entry(id.to_string()).or_default();
            if let Err(error) = read_jsonl_log(&file, entry, Some(seen), &parse) {
                errors.push(format!("{}: {error}", file.display()));
            }
        }
        if !errors.is_empty() {
            status.error = Some(format!("{} 个文件读取失败：{}", errors.len(), errors[0]));
        }
        status
    }

    fn scan_factory(&mut self, root: &Path) -> SourceStatus {
        let routes = FactoryRoutes::load(root.parent().unwrap_or(root));
        let mut status = SourceStatus {
            id: SOURCE_FACTORY.into(),
            label: "Factory".into(),
            found: root.is_dir(),
            files: 0,
            error: None,
        };
        let Ok(projects) = std::fs::read_dir(root) else {
            return status;
        };
        let mut errors = Vec::new();
        for project in projects.flatten() {
            let Ok(entries) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let Some(session_id) = name.strip_suffix(".settings.json") else {
                    continue;
                };
                status.files += 1;
                let read = std::fs::read(&path)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| parse_factory_settings(&bytes));
                let (model_id, counts) = match read {
                    Ok(Some(parsed)) => parsed,
                    Ok(None) => continue,
                    Err(error) => {
                        errors.push(format!("{name}: {error}"));
                        continue;
                    }
                };
                let modified_ms = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|elapsed| elapsed.as_millis() as i64)
                    .unwrap_or(0);
                let route = routes.route_for(&model_id, modified_ms);
                self.book_factory(session_id, &model_id, counts, &day_key(modified_ms));
                if let Some(session) = self.factory_sessions.get_mut(session_id) {
                    // A known route sticks; a guess may improve as backups
                    // are read (they never change, so it converges).
                    // …and an endpoint we have since stopped keeping (retired, or the
                    // Cursor Agent bridge) drops the session whatever it was before.
                    let now_dropped = route.id.starts_with(RETIRED_PREFIX)
                        && session
                            .route
                            .as_ref()
                            .is_some_and(|old| old.endpoint == route.endpoint);
                    if now_dropped || session.route.as_ref().is_none_or(|route| route.inferred) {
                        session.route = Some(route);
                    }
                    if session.is_retired() {
                        session.days.clear();
                    }
                }
            }
        }
        if !errors.is_empty() {
            status.error = Some(format!("{} 个会话读取失败：{}", errors.len(), errors[0]));
        }
        status
    }

    /// Devin's own session store keeps per-inference metrics on every
    /// assistant chain node: input/output/cache tokens, model, timestamps.
    /// Rows are append-only (AUTOINCREMENT row_id), so a row-id cursor is a
    /// complete incremental scan — no dedupe set needed.
    fn scan_devin(&mut self, db_path: &Path) -> SourceStatus {
        let mut status = SourceStatus {
            id: SOURCE_DEVIN.into(),
            label: "Devin".into(),
            found: db_path.is_file(),
            files: 0,
            error: None,
        };
        if !status.found {
            return status;
        }
        match self.scan_devin_db(db_path) {
            Ok(rows) => status.files = rows,
            Err(error) => status.error = Some(error),
        }
        status
    }

    fn scan_devin_db(&mut self, db_path: &Path) -> Result<usize, String> {
        let conn = crate::probes::credentials::open_vscdb_readonly(db_path)?;
        let mut stmt = conn
            .prepare(
                "SELECT row_id,
                        created_at,
                        json_extract(chat_message, '$.metadata.generation_model'),
                        json_extract(chat_message, '$.metadata.metrics.input_tokens'),
                        json_extract(chat_message, '$.metadata.metrics.output_tokens'),
                        json_extract(chat_message, '$.metadata.metrics.cache_read_tokens'),
                        json_extract(chat_message, '$.metadata.metrics.cache_creation_tokens')
                 FROM message_nodes
                 WHERE row_id > ?1
                 ORDER BY row_id",
            )
            .map_err(|error| format!("devin.prepare: {error}"))?;
        let rows = stmt
            .query_map([self.devin.last_rowid], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            })
            .map_err(|error| format!("devin.query: {error}"))?;
        let mut scanned = 0usize;
        for row in rows {
            let (row_id, created_at, model, input, output, cache_read, cache_write) =
                row.map_err(|error| format!("devin.row: {error}"))?;
            self.devin.last_rowid = self.devin.last_rowid.max(row_id);
            let counts = Counts {
                input: input.unwrap_or(0).max(0) as u64,
                output: output.unwrap_or(0).max(0) as u64,
                cache_read: cache_read.unwrap_or(0).max(0) as u64,
                cache_write: cache_write.unwrap_or(0).max(0) as u64,
            };
            if counts.is_zero() {
                continue;
            }
            scanned += 1;
            self.devin
                .days
                .entry(day_key(created_at * 1000))
                .or_default()
                .entry(model.unwrap_or_else(|| "devin".to_string()))
                .or_default()
                .add(&counts);
        }
        Ok(scanned)
    }

    /// Grok Build keeps one directory per working dir, one per session inside,
    /// each with a `usage.json` that is rewritten after every turn.
    fn scan_grok(&mut self, root: &Path) -> SourceStatus {
        let mut status = SourceStatus {
            id: SOURCE_GROK.into(),
            label: "Grok Build".into(),
            found: root.is_dir(),
            files: 0,
            error: None,
        };
        let Ok(projects) = std::fs::read_dir(root) else {
            return status;
        };
        let mut errors = Vec::new();
        for project in projects.flatten() {
            let Ok(sessions) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for session in sessions.flatten() {
                let path = session.path().join("usage.json");
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                status.files += 1;
                if let Err(error) = self.book_grok(&bytes) {
                    let name = session.file_name().to_string_lossy().into_owned();
                    errors.push(format!("{name}: {error}"));
                }
            }
        }
        if !errors.is_empty() {
            status.error = Some(format!("{} 个会话读取失败：{}", errors.len(), errors[0]));
        }
        status
    }

    fn book_grok(&mut self, bytes: &[u8]) -> Result<(), String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Usage {
            session_id: String,
            #[serde(default)]
            turns: Vec<Turn>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Turn {
            turn_number: u64,
            ended_at: String,
            #[serde(default)]
            primary_model_id: Option<String>,
            #[serde(flatten)]
            counts: GrokCounts,
            #[serde(default)]
            model_usage: BTreeMap<String, GrokCounts>,
        }
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase")]
        struct GrokCounts {
            #[serde(default)]
            input_tokens: u64,
            #[serde(default)]
            output_tokens: u64,
            #[serde(default)]
            cached_read_tokens: u64,
            #[serde(default)]
            cache_creation_tokens: u64,
        }
        impl GrokCounts {
            fn counts(&self) -> Counts {
                Counts {
                    input: self.input_tokens.saturating_sub(self.cached_read_tokens),
                    output: self.output_tokens,
                    cache_read: self.cached_read_tokens,
                    cache_write: self.cache_creation_tokens,
                }
            }
        }
        let usage: Usage = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        let booked = self.grok.turns.get(&usage.session_id).copied().unwrap_or(0);
        let mut last = booked;
        for turn in usage.turns.iter().filter(|turn| turn.turn_number > booked) {
            let at = parse_rfc3339_ms(&turn.ended_at)
                .ok_or_else(|| format!("endedAt 读不懂: {}", turn.ended_at))?;
            let day = self.grok.days.entry(day_key(at)).or_default();
            if turn.model_usage.is_empty() {
                let model = turn
                    .primary_model_id
                    .clone()
                    .unwrap_or_else(|| "grok".into());
                day.entry(model).or_default().add(&turn.counts.counts());
            } else {
                for (model, counts) in &turn.model_usage {
                    day.entry(model.clone()).or_default().add(&counts.counts());
                }
            }
            last = last.max(turn.turn_number);
        }
        self.grok.turns.insert(usage.session_id, last);
        Ok(())
    }

    fn scan_dim(&mut self, db_path: &Path) -> SourceStatus {
        let mut status = SourceStatus {
            id: SOURCE_DIM.into(),
            label: "DimAgent".into(),
            found: db_path.is_file(),
            files: 0,
            error: None,
        };
        if !status.found {
            return status;
        }
        match self.scan_dim_db(db_path) {
            Ok(rows) => status.files = rows,
            Err(error) => status.error = Some(error),
        }
        status
    }

    fn scan_dim_db(&mut self, db_path: &Path) -> Result<usize, String> {
        let conn = crate::probes::credentials::open_vscdb_readonly(db_path)?;
        // Base URL per provider (never the credential column).
        let mut urls: BTreeMap<String, String> = BTreeMap::new();
        if let Ok(mut stmt) =
            conn.prepare("SELECT providerId, coalesce(baseUrl, defaultBaseUrl) FROM providers")
        {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            }) {
                for (id, url) in rows.flatten() {
                    if let Some(url) = url {
                        urls.insert(id, url);
                    }
                }
            }
        }
        let mut stmt = conn
            .prepare(
                "SELECT rowid, providerId, modelId, createdAt,
                        json_extract(usage, '$.promptTokens'),
                        json_extract(usage, '$.completionTokens'),
                        json_extract(usage, '$.cacheReadTokens'),
                        json_extract(usage, '$.cacheWriteTokens')
                 FROM usage_ledger
                 WHERE rowid > ?1
                 ORDER BY rowid",
            )
            .map_err(|error| format!("dim.prepare: {error}"))?;
        let rows = stmt
            .query_map([self.dim.last_rowid], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                ))
            })
            .map_err(|error| format!("dim.query: {error}"))?;
        let mut scanned = 0usize;
        for row in rows {
            let (rowid, provider, model, created_at, prompt, completion, cache_read, cache_write) =
                row.map_err(|error| format!("dim.row: {error}"))?;
            self.dim.last_rowid = self.dim.last_rowid.max(rowid);
            let cache_read = cache_read.unwrap_or(0).max(0) as u64;
            let cache_write = cache_write.unwrap_or(0).max(0) as u64;
            // promptTokens already includes the cached part.
            let counts = Counts {
                input: (prompt.unwrap_or(0).max(0) as u64).saturating_sub(cache_read + cache_write),
                output: completion.unwrap_or(0).max(0) as u64,
                cache_read,
                cache_write,
            };
            let Some(at) =
                crate::models::timestamp_to_millis(&serde_json::Value::String(created_at))
            else {
                continue;
            };
            if counts.is_zero() {
                continue;
            }
            let route = if provider == DIM_OWN_PROVIDER {
                Route {
                    id: SOURCE_DIM.into(),
                    label: "DimAgent 订阅".into(),
                    endpoint: urls.get(&provider).cloned(),
                    inferred: false,
                }
            } else if let Some(url) = urls.get(&provider) {
                classify_endpoint(url, &model)
            } else {
                Route {
                    id: "unknown".into(),
                    label: "未知接入（配置里已没有这个模型）".into(),
                    endpoint: None,
                    inferred: true,
                }
            };
            // Local ports that are no longer running proxies aren't kept.
            if route.id.starts_with(RETIRED_PREFIX) {
                continue;
            }
            scanned += 1;
            self.dim
                .days
                .entry(day_key(at as i64))
                .or_default()
                .entry(format!("{}\u{1f}{model}", route.id))
                .or_default()
                .add(&counts);
            self.dim.routes.insert(route.id.clone(), route);
        }
        Ok(scanned)
    }

    /// Where the next Cursor sync should start: an hour before the last one
    /// (late-arriving events; `seen` absorbs the overlap), or `backfill_ms`
    /// before `now_ms` on first run.
    pub fn cursor_sync_since(&self, now_ms: i64, backfill_ms: i64) -> i64 {
        if self.cursor.synced_until == 0 {
            now_ms - backfill_ms
        } else {
            self.cursor.synced_until - 3_600_000
        }
    }

    /// Remember Devin's daily remaining % at `at_ms`; the day's lowest
    /// reading is how much of that day's quota went.
    pub fn record_devin_daily(&mut self, remaining_percent: f64, at_ms: i64) {
        if !remaining_percent.is_finite() {
            return;
        }
        let entry = self
            .devin_daily
            .entry(day_key(at_ms))
            .or_insert(remaining_percent);
        *entry = entry.min(remaining_percent.clamp(0.0, 100.0));
    }

    /// Book Cursor usage events fetched up to `until_ms`.
    pub fn book_cursor(&mut self, events: &[crate::probes::CursorUsageEvent], until_ms: i64) {
        for event in events {
            if !self.cursor.seen.insert(event.key.clone()) {
                continue;
            }
            self.cursor
                .days
                .entry(day_key(event.at_ms))
                .or_default()
                .entry(event.model.clone())
                .or_default()
                .add(&event.counts);
        }
        self.cursor.synced_until = self.cursor.synced_until.max(until_ms);
    }

    fn book_factory(&mut self, session_id: &str, model_id: &str, counts: Counts, day: &str) {
        let model = display_model(model_id);
        let session = self
            .factory_sessions
            .entry(session_id.to_string())
            .or_default();
        let growth = counts.growth_since(&session.last).unwrap_or(counts);
        if !model.is_empty() {
            session.model = model;
        }
        session.last = counts;
        if !growth.is_zero() {
            session
                .days
                .entry(day.to_string())
                .or_default()
                .add(&growth);
        }
    }

    /// Report for the `days` days ending today (`now_ms`).
    pub fn report(&self, now_ms: i64, days: u32, sources: Vec<SourceStatus>) -> TokenReport {
        let today = day_key(now_ms);
        let days = days.clamp(1, 366) as i64;
        let first = shift_day(&today, -(days - 1)).unwrap_or_else(|| today.clone());
        let mut by_day: BTreeMap<String, (BTreeMap<String, Counts>, BTreeMap<String, Counts>)> =
            BTreeMap::new();
        let mut by_model: BTreeMap<(String, String, String), Counts> = BTreeMap::new();
        let mut by_route: BTreeMap<String, RouteReport> = BTreeMap::new();
        let mut book = |date: &str, source: &str, model: &str, route: &Route, counts: &Counts| {
            if date < first.as_str() || date > today.as_str() {
                return;
            }
            let day = by_day.entry(date.to_string()).or_default();
            day.0.entry(source.to_string()).or_default().add(counts);
            day.1.entry(route.id.clone()).or_default().add(counts);
            by_model
                .entry((source.to_string(), model.to_string(), route.id.clone()))
                .or_default()
                .add(counts);
            let entry = by_route
                .entry(route.id.clone())
                .or_insert_with(|| RouteReport {
                    id: route.id.clone(),
                    // Labels are presentation: take today's wording, not the
                    // one stored when the session was first seen.
                    label: current_route_label(&route.id).unwrap_or_else(|| route.label.clone()),
                    endpoints: Vec::new(),
                    inferred: false,
                    counts: Counts::default(),
                    by_source: BTreeMap::new(),
                });
            entry.counts.add(counts);
            entry.inferred |= route.inferred;
            entry
                .by_source
                .entry(source.to_string())
                .or_default()
                .add(counts);
            if let Some(endpoint) = &route.endpoint {
                if !entry.endpoints.contains(endpoint) {
                    entry.endpoints.push(endpoint.clone());
                }
            }
        };
        let anthropic = claude_code_route();
        for file in self.claude_files.values() {
            for (date, models) in &file.days {
                for (model, counts) in models {
                    book(date, SOURCE_CLAUDE_CODE, model, &anthropic, counts);
                }
            }
        }
        let cursor_route = Route {
            id: "cursor".into(),
            label: "Cursor 订阅".into(),
            endpoint: None,
            inferred: false,
        };
        for (date, models) in &self.cursor.days {
            for (model, counts) in models {
                book(date, SOURCE_CURSOR, model, &cursor_route, counts);
            }
        }
        let devin_route = Route {
            id: "devin".into(),
            label: "Devin 订阅".into(),
            endpoint: None,
            inferred: false,
        };
        for (date, models) in &self.devin.days {
            for (model, counts) in models {
                book(date, SOURCE_DEVIN, model, &devin_route, counts);
            }
        }
        for (date, keyed) in &self.dim.days {
            for (key, counts) in keyed {
                let (route_id, model) = key.split_once('\u{1f}').unwrap_or(("unknown", key));
                if let Some(route) = self.dim.routes.get(route_id) {
                    book(date, SOURCE_DIM, model, route, counts);
                }
            }
        }
        let grok_route = Route {
            id: "grok".into(),
            label: "SuperGrok 订阅".into(),
            endpoint: None,
            inferred: false,
        };
        for (date, models) in &self.grok.days {
            for (model, counts) in models {
                book(date, SOURCE_GROK, model, &grok_route, counts);
            }
        }
        let unknown = Route {
            id: "unknown".into(),
            label: "未知接入".into(),
            endpoint: None,
            inferred: true,
        };
        for session in self.factory_sessions.values().filter(|s| !s.is_retired()) {
            let route = session.route.as_ref().unwrap_or(&unknown);
            for (date, counts) in &session.days {
                book(date, SOURCE_FACTORY, &session.model, route, counts);
            }
        }
        let mut proxy_days: BTreeMap<String, BTreeMap<String, Counts>> = BTreeMap::new();
        let mut proxies: BTreeMap<String, ProxyReport> = BTreeMap::new();
        for file in self.proxy_files.values() {
            for (date, keyed) in &file.days {
                if date.as_str() < first.as_str() || date.as_str() > today.as_str() {
                    continue;
                }
                for (key, counts) in keyed {
                    let mut parts = key.split('\u{1f}');
                    let route = parts.next().unwrap_or("unknown").to_string();
                    let account = parts.next().unwrap_or("").to_string();
                    let model = parts.next().unwrap_or("").to_string();
                    proxy_days
                        .entry(date.clone())
                        .or_default()
                        .entry(route.clone())
                        .or_default()
                        .add(counts);
                    let proxy = proxies.entry(route.clone()).or_insert_with(|| ProxyReport {
                        label: proxy_label(&route),
                        id: route,
                        counts: Counts::default(),
                        accounts: Vec::new(),
                        models: Vec::new(),
                        from_agents: Counts::default(),
                    });
                    proxy.counts.add(counts);
                    add_keyed(&mut proxy.accounts, &account, counts);
                    add_keyed(&mut proxy.models, &model, counts);
                }
            }
        }
        let mut proxies: Vec<ProxyReport> = proxies
            .into_values()
            .map(|mut proxy| {
                proxy.accounts.retain(|(account, _)| !account.is_empty());
                proxy.accounts.sort_by(|a, b| b.1.total().cmp(&a.1.total()));
                proxy.models.sort_by(|a, b| b.1.total().cmp(&a.1.total()));
                if let Some(route) = by_route.get(&proxy.id) {
                    proxy.from_agents = route.counts;
                }
                proxy
            })
            .collect();
        proxies.sort_by(|a, b| b.counts.total().cmp(&a.counts.total()));
        let days = (0..days)
            .filter_map(|offset| shift_day(&first, offset))
            .map(|date| {
                let (by_source, by_route) = by_day.remove(&date).unwrap_or_default();
                DayReport {
                    by_proxy: proxy_days.remove(&date).unwrap_or_default(),
                    date,
                    by_source,
                    by_route,
                }
            })
            .collect();
        let mut models: Vec<ModelReport> = by_model
            .into_iter()
            .map(|((source, model, route), counts)| ModelReport {
                source,
                model,
                route,
                counts,
            })
            .collect();
        models.sort_by(|a, b| b.counts.total().cmp(&a.counts.total()));
        let mut routes: Vec<RouteReport> = by_route.into_values().collect();
        routes.sort_by(|a, b| b.counts.total().cmp(&a.counts.total()));
        let devin_daily = self
            .devin_daily
            .iter()
            .filter(|(date, _)| date.as_str() >= first.as_str() && date.as_str() <= today.as_str())
            .map(|(date, remaining)| (date.clone(), 100.0 - remaining))
            .collect();
        TokenReport {
            today,
            days,
            models,
            routes,
            devin_daily,
            proxies,
            sources,
        }
    }

    /// Today's total for one source (for the detail card).
    pub fn today_total(&self, now_ms: i64, source: &str) -> u64 {
        let report = self.report(now_ms, 1, Vec::new());
        report
            .days
            .last()
            .and_then(|day| day.by_source.get(source))
            .map(Counts::total)
            .unwrap_or(0)
    }
}

/// Claude Code bills the signed-in Claude subscription. (An
/// `ANTHROPIC_BASE_URL` override would change that; this machine has none.)
fn claude_code_route() -> Route {
    Route {
        id: "anthropic".into(),
        label: "Anthropic · Claude 订阅".into(),
        endpoint: None,
        inferred: false,
    }
}

/// `custom:kimi-k3-pro` → `kimi-k3-pro`.
fn display_model(model_id: &str) -> String {
    model_id
        .strip_prefix("custom:")
        .unwrap_or(model_id)
        .to_string()
}

/// Factory's `customModels` (id → baseUrl): the live `settings.json` plus
/// every `settings.json.bak*`. A backup is copied just before the config is
/// edited, so the first backup written after a session is the config that
/// session ran under.
#[derive(Debug, Default)]
struct FactoryRoutes {
    live: BTreeMap<String, String>,
    /// (backup time ms, id → base URL), oldest first.
    backups: Vec<(i64, BTreeMap<String, String>)>,
    /// ids that pointed at more than one endpoint across the files.
    ambiguous: BTreeSet<String>,
}

impl FactoryRoutes {
    fn load(factory_dir: &Path) -> Self {
        let mut routes = FactoryRoutes {
            live: read_custom_models(&factory_dir.join("settings.json"))
                .into_iter()
                .collect(),
            ..FactoryRoutes::default()
        };
        for entry in std::fs::read_dir(factory_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let is_backup = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("settings.json.bak"));
            if !is_backup {
                continue;
            }
            let at = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_millis() as i64)
                .unwrap_or(0);
            let models: BTreeMap<String, String> =
                read_custom_models(&entry.path()).into_iter().collect();
            if !models.is_empty() {
                routes.backups.push((at, models));
            }
        }
        routes.backups.sort_by_key(|(at, _)| *at);
        let mut seen: BTreeMap<&String, BTreeSet<&String>> = BTreeMap::new();
        for (id, url) in routes
            .backups
            .iter()
            .flat_map(|(_, models)| models)
            .chain(&routes.live)
        {
            seen.entry(id).or_default().insert(url);
        }
        routes.ambiguous = seen
            .into_iter()
            .filter(|(_, urls)| urls.len() > 1)
            .map(|(id, _)| id.clone())
            .collect();
        routes
    }

    /// Route of `model_id` for a session last active at `session_ms`.
    fn route_for(&self, model_id: &str, session_ms: i64) -> Route {
        if !model_id.starts_with("custom:") {
            return Route {
                id: "factory".into(),
                label: "Factory 官方额度".into(),
                endpoint: None,
                inferred: false,
            };
        }
        // The config in force then: the first backup taken after the
        // session, else the live file, else the newest backup that has it.
        let after = self
            .backups
            .iter()
            .filter(|(at, _)| *at >= session_ms)
            .find_map(|(_, models)| models.get(model_id));
        let found = match (after, self.live.get(model_id)) {
            (Some(url), _) => Some((url, true)),
            (None, Some(url)) => Some((url, false)),
            (None, None) => self
                .backups
                .iter()
                .rev()
                .find_map(|(_, models)| models.get(model_id))
                .map(|url| (url, true)),
        };
        let Some((url, from_backup)) = found else {
            return Route {
                id: "unknown".into(),
                label: "未知接入（配置里已没有这个模型）".into(),
                endpoint: None,
                inferred: true,
            };
        };
        let mut route = classify_endpoint(url, model_id);
        // One consistent endpoint across every file is a fact even when read
        // from a backup; only a model that moved is a guess.
        route.inferred = from_backup && self.ambiguous.contains(model_id);
        route
    }
}

/// `(id, baseUrl)` pairs of a Factory settings file. Only those two fields
/// are read — the entries also hold API keys, which never leave this fn.
fn read_custom_models(path: &Path) -> Vec<(String, String)> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    value
        .get("customModels")
        .and_then(|models| models.as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?;
            let url = model
                .get("baseUrl")
                .or_else(|| model.get("base_url"))?
                .as_str()?;
            Some((id.to_string(), url.to_string()))
        })
        .collect()
}

/// Base URL → who pays. Local ports are this project's own proxies.
fn classify_endpoint(url: &str, model_id: &str) -> Route {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest).to_ascii_lowercase();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => (host, Some(port)),
        _ => (authority.as_str(), None),
    };
    let local = matches!(host, "127.0.0.1" | "localhost" | "[::1]");
    let port = port.and_then(|port| port.parse::<u16>().ok());
    use crate::ports;
    let (id, label) = match (local, port) {
        (true, Some(ports::GEMINI_BRIDGE)) if model_id.contains("qoder/") => {
            ("qoder".to_string(), "Qoder".to_string())
        }
        (true, Some(ports::GEMINI_BRIDGE)) => ("gemini".to_string(), "Gemini 账号池".to_string()),
        // The Cursor Agent bridge is not a pass-through proxy (each request
        // is a whole Cursor Agent turn), so its tokens can't be traced to a
        // source and are not kept — same as any retired local port.
        (true, Some(ports::CURSOR_BRIDGE)) => (
            format!("{RETIRED_PREFIX}{}", ports::CURSOR_BRIDGE),
            "Cursor Agent（不记账）".to_string(),
        ),
        // Anything else on this machine is a proxy that no longer runs: its
        // sessions are dropped from the ledger (see `RETIRED_PREFIX`).
        (true, port) => {
            let port = port
                .map(|port| port.to_string())
                .unwrap_or_else(|| "?".into());
            (
                format!("{RETIRED_PREFIX}{port}"),
                format!("已停用的本机 :{port}"),
            )
        }
        (false, _) => {
            let label = match host {
                "api.stepfun.com" => "阶跃 StepFun API".to_string(),
                "api.commandcode.ai" => "CommandCode API".to_string(),
                "openrouter.ai" => "OpenRouter".to_string(),
                "api.deepseek.com" => "DeepSeek API".to_string(),
                "api.moonshot.cn" | "api.moonshot.ai" => "Moonshot API".to_string(),
                other => other.to_string(),
            };
            (format!("host:{host}"), label)
        }
    };
    Route {
        id,
        label,
        endpoint: Some(url.to_string()),
        inferred: false,
    }
}

/// Current label of a known route id (hosts keep their stored label).
fn current_route_label(id: &str) -> Option<String> {
    let label = match id {
        "anthropic" => claude_code_route().label,
        "factory" => "Factory 官方额度".into(),
        "gemini" => "Gemini 账号池".into(),
        "qoder" => "Qoder".into(),
        "cursor" => "Cursor 订阅".into(),
        "devin" => "Devin 订阅".into(),
        "dim" => "DimAgent 订阅".into(),
        "grok" => "SuperGrok 订阅".into(),
        _ => return None,
    };
    Some(label)
}

fn add_keyed(list: &mut Vec<(String, Counts)>, key: &str, counts: &Counts) {
    match list.iter_mut().find(|(existing, _)| existing == key) {
        Some((_, total)) => total.add(counts),
        None => list.push((key.to_string(), *counts)),
    }
}

fn proxy_label(route: &str) -> String {
    match route {
        "gemini" => "Gemini 桥 · 8050",
        "qoder" => "Qoder · 经桥 8050",
        other => other,
    }
    .to_string()
}

/// `someone@gmail.com` → `s***@g***`; other ids keep their first 4 chars.
pub fn mask_account(account: &str) -> String {
    match account.split_once('@') {
        Some((user, domain)) => {
            let first = |part: &str| part.chars().next().map(String::from).unwrap_or_default();
            format!("{}***@{}***", first(user), first(domain))
        }
        None if account.chars().count() > 6 => {
            format!("{}…", account.chars().take(4).collect::<String>())
        }
        None => account.to_string(),
    }
}

fn proxy_key(route: &str, account: Option<&str>, model: Option<&str>) -> String {
    format!(
        "{route}\u{1f}{}\u{1f}{}",
        account.map(mask_account).unwrap_or_default(),
        model.unwrap_or("unknown")
    )
}

/// FNV-1a — a stable dedupe key for log lines that carry no id.
fn line_hash(line: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in line {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A Gemini bridge `requests.jsonl` entry. `inputTokens` includes the
/// cached part (`cachedTokens`); `outputTokens` already includes thinking.
/// Entries written before the bridge logged input only carry output.
fn parse_bridge_line(line: &[u8]) -> Option<LineUsage> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let num = |key: &str| value.get(key).and_then(|v| v.as_u64());
    let input = num("inputTokens").unwrap_or(0);
    let cached = num("cachedTokens").unwrap_or(0);
    let counts = Counts {
        input: input.saturating_sub(cached),
        output: num("outputTokens").unwrap_or(0),
        cache_read: cached,
        cache_write: 0,
    };
    if counts.is_zero() {
        return None;
    }
    let at = value
        .get("at")
        .and_then(|at| at.as_str())
        .and_then(parse_rfc3339_ms)?;
    let model = value.get("model").and_then(|model| model.as_str());
    let route = if model.is_some_and(|model| model.starts_with("qoder")) {
        "qoder"
    } else {
        "gemini"
    };
    let key = value
        .get("id")
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| line_hash(line));
    Some((
        Some(key),
        day_key(at),
        proxy_key(route, value.get("account").and_then(|a| a.as_str()), model),
        counts,
    ))
}

/// Recursively collect `*.jsonl` (subagent transcripts live one level down).
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out, depth + 1);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
}

/// Read the complete lines appended since `state.offset` and book them.
/// (dedupe key, day, inner key, counts) of one log line.
type LineUsage = (Option<String>, String, String, Counts);

/// `shared_seen` replaces the file's own dedupe set when several files can
/// carry the same lines.
fn read_jsonl_log(
    path: &Path,
    state: &mut LogFile,
    mut shared_seen: Option<&mut BTreeSet<String>>,
    parse: &impl Fn(&[u8]) -> Option<LineUsage>,
) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let len = file.metadata().map_err(|error| error.to_string())?.len();
    if len < state.offset {
        // Rewritten or truncated: re-read; `seen` keeps it from double counting.
        state.offset = 0;
    }
    if len == state.offset {
        return Ok(());
    }
    file.seek(SeekFrom::Start(state.offset))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity((len - state.offset) as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    // Stop at the last newline: a line still being written is read next time.
    let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(());
    };
    for line in bytes[..end].split(|byte| *byte == b'\n') {
        if let Some((key, date, model, counts)) = parse(line) {
            if let Some(key) = key {
                let seen = match shared_seen.as_deref_mut() {
                    Some(shared) => shared,
                    None => &mut state.seen,
                };
                if !seen.insert(key) {
                    continue;
                }
            }
            state
                .days
                .entry(date)
                .or_default()
                .entry(model)
                .or_default()
                .add(&counts);
        }
    }
    state.offset += end as u64 + 1;
    Ok(())
}

/// One Claude Code log line → (dedupe key, day, model, counts). Lines that
/// carry no usage (user turns, tool results, summaries) are `None`.
fn parse_claude_line(line: &[u8]) -> Option<LineUsage> {
    if line.is_empty() || !line.windows(7).any(|window| window == b"\"usage\"") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let message = value.get("message")?;
    let usage = message.get("usage")?;
    let model = message
        .get("model")
        .and_then(|model| model.as_str())
        .unwrap_or("unknown");
    if model == "<synthetic>" {
        return None;
    }
    let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
    let counts = Counts {
        input: field("input_tokens"),
        output: field("output_tokens"),
        cache_read: field("cache_read_input_tokens"),
        cache_write: field("cache_creation_input_tokens"),
    };
    if counts.is_zero() {
        return None;
    }
    let at = value
        .get("timestamp")
        .and_then(|at| at.as_str())
        .and_then(parse_rfc3339_ms)?;
    let key = message.get("id").and_then(|id| id.as_str()).map(|id| {
        let request = value
            .get("requestId")
            .and_then(|id| id.as_str())
            .unwrap_or("");
        format!("{id}:{request}")
    });
    Some((key, day_key(at), model.to_string(), counts))
}

/// Factory session settings → (raw model id, running totals). Thinking tokens are
/// output the model generated, so they count as output.
fn parse_factory_settings(bytes: &[u8]) -> Result<Option<(String, Counts)>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let Some(usage) = value.get("tokenUsage") else {
        return Ok(None);
    };
    let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
    let model = value
        .get("model")
        .and_then(|model| model.as_str())
        .unwrap_or("unknown");
    Ok(Some((
        model.to_string(),
        Counts {
            input: field("inputTokens"),
            output: field("outputTokens") + field("thinkingTokens"),
            cache_read: field("cacheReadTokens"),
            cache_write: field("cacheCreationTokens"),
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_line(id: &str, at: &str, model: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{at}","requestId":"req_{id}","message":{{"id":"msg_{id}","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":100,"cache_creation_input_tokens":10}}}}}}"#
        )
    }

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dango-tokens-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn days_are_cut_in_utc_plus_8() {
        // 2026-09-25 17:00Z is already the 26th in Shanghai.
        let ms = parse_rfc3339_ms("2026-09-25T17:00:00Z").unwrap();
        assert_eq!(day_key(ms), "2026-09-26");
        let ms = parse_rfc3339_ms("2026-09-25T15:59:59Z").unwrap();
        assert_eq!(day_key(ms), "2026-09-25");
    }

    #[test]
    fn claude_blocks_of_one_message_count_once_and_lines_without_usage_are_skipped() {
        let home = temp_home("claude");
        let dir = home.join(".claude/projects/p");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("s.jsonl");
        let lines = [
            r#"{"type":"user","timestamp":"2026-09-26T01:00:00Z","message":{"role":"user","content":"hi"}}"#.to_string(),
            claude_line("a", "2026-09-26T01:00:01Z", "claude-opus-5-5", 5, 50),
            claude_line("a", "2026-09-26T01:00:02Z", "claude-opus-5-5", 5, 50),
            claude_line("b", "2026-09-26T01:00:03Z", "<synthetic>", 5, 50),
        ];
        std::fs::write(&log, lines.join("\n") + "\n").unwrap();
        let mut ledger = Ledger::default();
        let status = ledger.scan(&home);
        assert_eq!(status[0].files, 1);
        let now = parse_rfc3339_ms("2026-09-26T02:00:00Z").unwrap();
        let report = ledger.report(now, 1, Vec::new());
        let today = &report.days[0].by_source[SOURCE_CLAUDE_CODE];
        assert_eq!(
            *today,
            Counts {
                input: 5,
                output: 50,
                cache_read: 100,
                cache_write: 10
            }
        );
        assert_eq!(report.models[0].model, "claude-opus-5-5");
    }

    #[test]
    fn claude_log_is_read_incrementally_and_a_half_written_line_waits() {
        let home = temp_home("incremental");
        let dir = home.join(".claude/projects/p");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("s.jsonl");
        let first = claude_line("a", "2026-09-26T01:00:01Z", "m", 1, 1);
        let second = claude_line("b", "2026-09-26T01:00:02Z", "m", 2, 2);
        // Second line not yet terminated.
        std::fs::write(&log, format!("{first}\n{}", &second[..20])).unwrap();
        let mut ledger = Ledger::default();
        ledger.scan(&home);
        let now = parse_rfc3339_ms("2026-09-26T02:00:00Z").unwrap();
        assert_eq!(ledger.today_total(now, SOURCE_CLAUDE_CODE), 1 + 1 + 110);
        std::fs::write(&log, format!("{first}\n{second}\n")).unwrap();
        ledger.scan(&home);
        assert_eq!(
            ledger.today_total(now, SOURCE_CLAUDE_CODE),
            112 + 2 + 2 + 110
        );
        // A rescan with nothing new changes nothing.
        ledger.scan(&home);
        assert_eq!(ledger.today_total(now, SOURCE_CLAUDE_CODE), 226);
    }

    #[test]
    fn factory_books_only_growth_and_survives_a_reset() {
        let mut ledger = Ledger::default();
        let counts = |input, output| Counts {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
        };
        ledger.book_factory("s1", "kimi", counts(100, 10), "2026-09-25");
        ledger.book_factory("s1", "kimi", counts(150, 20), "2026-09-26");
        ledger.book_factory("s1", "kimi", counts(150, 20), "2026-09-26");
        let session = &ledger.factory_sessions["s1"];
        assert_eq!(session.days["2026-09-25"], counts(100, 10));
        assert_eq!(session.days["2026-09-26"], counts(50, 10));
        // Totals went down: a fresh session under the same id.
        ledger.book_factory("s1", "kimi", counts(30, 3), "2026-09-26");
        assert_eq!(
            ledger.factory_sessions["s1"].days["2026-09-26"],
            counts(80, 13)
        );
    }

    #[test]
    fn factory_settings_parse_keeps_the_raw_id_and_counts_thinking_as_output() {
        let bytes = br#"{"model":"custom:kimi-k3-pro","tokenUsage":{"inputTokens":10,"outputTokens":3,"cacheCreationTokens":1,"cacheReadTokens":7,"thinkingTokens":2,"factoryCredits":9}}"#;
        let (model, counts) = parse_factory_settings(bytes).unwrap().unwrap();
        assert_eq!(model, "custom:kimi-k3-pro");
        assert_eq!(
            counts,
            Counts {
                input: 10,
                output: 5,
                cache_read: 7,
                cache_write: 1
            }
        );
        assert_eq!(parse_factory_settings(br#"{"model":"x"}"#).unwrap(), None);
    }

    #[test]
    fn report_fills_empty_days_and_drops_out_of_range() {
        let mut ledger = Ledger::default();
        let one = Counts {
            input: 1,
            ..Counts::default()
        };
        ledger.book_factory("s", "m", one, "2026-09-20");
        let now = parse_rfc3339_ms("2026-09-26T02:00:00Z").unwrap();
        let report = ledger.report(now, 3, Vec::new());
        assert_eq!(
            report
                .days
                .iter()
                .map(|day| day.date.as_str())
                .collect::<Vec<_>>(),
            ["2026-09-24", "2026-09-25", "2026-09-26"]
        );
        assert!(report.days.iter().all(|day| day.by_source.is_empty()));
        assert!(report.models.is_empty());
    }

    #[test]
    fn ledger_round_trips_and_a_corrupt_file_is_an_error() {
        let home = temp_home("persist");
        let path = home.join("tokens.json");
        let mut ledger = Ledger::load(&path).unwrap();
        ledger.book_factory(
            "s",
            "m",
            Counts {
                input: 3,
                ..Counts::default()
            },
            "2026-09-26",
        );
        ledger.save(&path).unwrap();
        let loaded = Ledger::load(&path).unwrap();
        assert_eq!(loaded.factory_sessions["s"].last.input, 3);
        std::fs::write(&path, b"{nope").unwrap();
        assert!(Ledger::load(&path).is_err());
    }

    #[test]
    fn endpoints_resolve_to_who_pays() {
        let route = |url: &str, id: &str| classify_endpoint(url, id).id;
        assert_eq!(
            route("http://127.0.0.1:8099/v1", "custom:kimi"),
            "local:8099"
        );
        assert_eq!(route("http://127.0.0.1:8050", "custom:gemini-3"), "gemini");
        assert_eq!(
            route("http://127.0.0.1:8050", "custom:qoder/qfmodel-2"),
            "qoder"
        );
        assert_eq!(
            route("http://127.0.0.1:8052", "custom:cursor/auto-3"),
            "local:8052"
        );
        assert_eq!(route("http://127.0.0.1:8099", "custom:x"), "local:8099");
        assert_eq!(
            route("https://api.stepfun.com/step_plan/v1", "custom:x"),
            "host:api.stepfun.com"
        );
        assert_eq!(
            classify_endpoint("https://api.stepfun.com/v1", "x").label,
            "阶跃 StepFun API"
        );
    }

    #[test]
    fn factory_routes_use_the_config_in_force_when_the_session_ran() {
        let home = temp_home("routes");
        let dir = home.join(".factory");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"customModels":[{"id":"custom:kimi","baseUrl":"http://127.0.0.1:8099/v1","apiKey":"never-read"},
                {"id":"custom:ds","baseUrl":"https://api.commandcode.ai/provider/v1"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("settings.json.bak-1"),
            r#"{"customModels":[{"id":"custom:ds","baseUrl":"http://127.0.0.1:8099"},
                {"id":"custom:old","baseUrl":"https://openrouter.ai/api/v1"}]}"#,
        )
        .unwrap();
        let routes = FactoryRoutes::load(&dir);
        let backup_at = routes.backups[0].0;
        let kimi = routes.route_for("custom:kimi", 0);
        assert_eq!((kimi.id.as_str(), kimi.inferred), ("local:8099", false));
        // Before the backup was taken, ds still went to the local port…
        let ds = routes.route_for("custom:ds", backup_at - 1);
        assert_eq!((ds.id.as_str(), ds.inferred), ("local:8099", true));
        // …after it, the live config applies.
        let ds = routes.route_for("custom:ds", backup_at + 1);
        assert_eq!(
            (ds.id.as_str(), ds.inferred),
            ("host:api.commandcode.ai", false)
        );
        // Only ever one endpoint: a fact, even from a backup.
        let old = routes.route_for("custom:old", backup_at + 1);
        assert_eq!(
            (old.id.as_str(), old.inferred),
            ("host:openrouter.ai", false)
        );
        assert_eq!(routes.route_for("gpt-6-luna", 0).id, "factory");
        assert_eq!(routes.route_for("custom:gone", 0).id, "unknown");
    }

    #[test]
    fn a_session_keeps_the_route_it_was_first_seen_with() {
        let home = temp_home("sticky");
        let factory = home.join(".factory");
        let sessions = factory.join("sessions/p");
        std::fs::create_dir_all(&sessions).unwrap();
        let config =
            |url: &str| format!(r#"{{"customModels":[{{"id":"custom:m","baseUrl":"{url}"}}]}}"#);
        std::fs::write(
            factory.join("settings.json"),
            config("http://127.0.0.1:8099/v1"),
        )
        .unwrap();
        std::fs::write(
            sessions.join("s.settings.json"),
            r#"{"model":"custom:m","tokenUsage":{"inputTokens":5,"outputTokens":1}}"#,
        )
        .unwrap();
        let mut ledger = Ledger::default();
        ledger.scan(&home);
        std::fs::write(
            factory.join("settings.json"),
            config("https://openrouter.ai/api/v1"),
        )
        .unwrap();
        ledger.scan(&home);
        assert_eq!(
            ledger.factory_sessions["s"].route.as_ref().unwrap().id,
            "local:8099"
        );
        assert_eq!(ledger.factory_sessions["s"].model, "m");
    }

    #[test]
    fn accounts_are_masked() {
        assert_eq!(mask_account("someone@gmail.com"), "s***@g***");
        assert_eq!(mask_account("fp-1234abcd"), "fp-1…");
        assert_eq!(mask_account("key1"), "key1");
    }

    #[test]
    fn proxy_logs_land_per_proxy_account_and_model_and_never_in_agent_totals() {
        let home = temp_home("proxies");
        let logs = home.join(".antigravity-bridge/logs");
        std::fs::create_dir_all(&logs).unwrap();
        let bridge = [
            r#"{"id":"r1","at":"2026-09-26T01:00:00Z","model":"gemini-3.8-flash","account":"alice@gmail.com","inputTokens":1000,"cachedTokens":800,"outputTokens":50}"#,
            // Rotation copies lines into .1 — same id, counted once.
            r#"{"id":"r2","at":"2026-09-26T01:05:00Z","model":"qoder/qfmodel-2","account":"bob@x.com","outputTokens":9}"#,
            r#"{"id":"r3","at":"2026-09-26T01:06:00Z","model":"gemini-3.8-flash","account":"alice@gmail.com","ok":false}"#,
        ];
        std::fs::write(logs.join("requests.jsonl"), bridge.join("\n") + "\n").unwrap();
        std::fs::write(logs.join("requests.jsonl.1"), format!("{}\n", bridge[0])).unwrap();
        let mut ledger = Ledger::default();
        let status = ledger.scan(&home);
        assert!(status
            .iter()
            .any(|s| s.id == "proxy:gemini" && s.files == 2));
        let now = parse_rfc3339_ms("2026-09-26T02:00:00Z").unwrap();
        let report = ledger.report(now, 1, Vec::new());
        let gemini = report.proxies.iter().find(|p| p.id == "gemini").unwrap();
        assert_eq!(
            gemini.counts,
            Counts {
                input: 200,
                output: 50,
                cache_read: 800,
                cache_write: 0
            }
        );
        assert_eq!(gemini.accounts[0].0, "a***@g***");
        let qoder = report.proxies.iter().find(|p| p.id == "qoder").unwrap();
        assert_eq!(qoder.counts.output, 9);
        // Agent view untouched.
        assert!(report.days[0].by_source.is_empty());
        assert_eq!(report.days[0].by_proxy["gemini"].output, 50);
    }

    #[test]
    fn sessions_through_a_retired_local_port_are_not_kept() {
        let home = temp_home("retired");
        let factory = home.join(".factory");
        let sessions = factory.join("sessions/p");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            factory.join("settings.json"),
            r#"{"customModels":[{"id":"custom:old","baseUrl":"http://127.0.0.1:8099"},
                {"id":"custom:live","baseUrl":"http://127.0.0.1:8099/v1"}]}"#,
        )
        .unwrap();
        for (id, model) in [("a", "custom:old"), ("b", "custom:live")] {
            std::fs::write(
                sessions.join(format!("{id}.settings.json")),
                format!(r#"{{"model":"{model}","tokenUsage":{{"inputTokens":7}}}}"#),
            )
            .unwrap();
        }
        let mut ledger = Ledger::default();
        ledger.scan(&home);
        assert!(ledger.factory_sessions["a"].days.is_empty());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let report = ledger.report(now, 2, Vec::new());
        assert!(report
            .routes
            .iter()
            .all(|route| !route.id.starts_with("local:")));
        assert!(report.routes.is_empty());
    }

    #[test]
    fn cursor_events_are_booked_once_and_the_sync_window_overlaps() {
        let mut ledger = Ledger::default();
        let at = parse_rfc3339_ms("2026-09-26T01:00:00Z").unwrap();
        let event = crate::probes::CursorUsageEvent {
            at_ms: at,
            model: "auto".into(),
            counts: Counts {
                input: 10,
                output: 2,
                cache_read: 100,
                cache_write: 0,
            },
            key: "k1".into(),
        };
        assert_eq!(ledger.cursor_sync_since(at, 1000), at - 1000);
        ledger.book_cursor(std::slice::from_ref(&event), at + 5);
        ledger.book_cursor(std::slice::from_ref(&event), at + 9);
        assert_eq!(ledger.cursor_sync_since(at + 9, 1000), at + 9 - 3_600_000);
        let report = ledger.report(at, 1, Vec::new());
        assert_eq!(report.days[0].by_source[SOURCE_CURSOR].total(), 112);
        assert_eq!(report.routes[0].id, "cursor");
    }

    #[test]
    fn devin_daily_keeps_the_lowest_reading_of_each_day() {
        let mut ledger = Ledger::default();
        let at = parse_rfc3339_ms("2026-09-26T01:00:00Z").unwrap();
        ledger.record_devin_daily(100.0, at);
        ledger.record_devin_daily(62.0, at + 3_600_000);
        ledger.record_devin_daily(80.0, at + 7_200_000);
        let report = ledger.report(at + 7_200_000, 7, Vec::new());
        assert_eq!(report.devin_daily, vec![("2026-09-26".to_string(), 38.0)]);
    }

    /// Devin's real counts come out of `sessions.db` `metadata.metrics`,
    /// bucketed by model and ledger day, and a row-id cursor keeps rescan
    /// incremental (rows are append-only).
    #[test]
    fn dim_scan_books_runs_by_who_pays_and_resumes_from_rowid() {
        let dir = std::env::temp_dir().join(format!("dango-dim-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("dimcode.sqlite");
        let _ = std::fs::remove_file(&db_path);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE providers(providerId TEXT PRIMARY KEY, baseUrl TEXT, defaultBaseUrl TEXT, credential TEXT);
             INSERT INTO providers VALUES('dimcode-api-oauth', NULL, 'https://dimagent.cn/v1', 'secret');
             INSERT INTO providers VALUES('zhipuai-coding-plan', 'https://open.bigmodel.cn/api/coding/paas/v4', NULL, 'secret');
             INSERT INTO providers VALUES('custom-local', 'http://127.0.0.1:9123/v1', NULL, NULL);
             CREATE TABLE usage_ledger(ledgerId TEXT PRIMARY KEY, sessionId TEXT, runId TEXT,
               providerId TEXT NOT NULL, modelId TEXT NOT NULL, usage TEXT NOT NULL, cost REAL, createdAt TEXT NOT NULL);",
        )
        .unwrap();
        let insert = |id: &str,
                      provider: &str,
                      model: &str,
                      prompt: u64,
                      out: u64,
                      cache: u64,
                      at: &str| {
            conn.execute(
                "INSERT INTO usage_ledger VALUES(?1,'s','r',?2,?3,?4,NULL,?5)",
                rusqlite::params![
                    id,
                    provider,
                    model,
                    format!("{{\"promptTokens\":{prompt},\"completionTokens\":{out},\"totalTokens\":{},\"cacheReadTokens\":{cache}}}", prompt + out),
                    at
                ],
            )
            .unwrap();
        };
        insert(
            "a",
            "dimcode-api-oauth",
            "glm-5.3",
            1000,
            50,
            800,
            "2026-09-17T10:02:22.853Z",
        );
        insert(
            "b",
            "zhipuai-coding-plan",
            "glm-5.3-flash",
            500,
            20,
            100,
            "2026-09-17T11:00:00Z",
        );
        insert(
            "c",
            "custom-local",
            "bonsai",
            900,
            9,
            0,
            "2026-09-18T15:26:22.456",
        );
        insert(
            "d",
            "gone-provider",
            "gemini-3.7-flash",
            10,
            1,
            0,
            "2026-09-18T01:00:00Z",
        );
        drop(conn);

        let mut ledger = Ledger::default();
        let status = ledger.scan_dim(&db_path);
        assert!(status.found);
        assert_eq!(status.files, 3, "the local-port run is not kept");
        let at = parse_rfc3339_ms("2026-09-18T12:00:00Z").unwrap();
        let report = ledger.report(at, 7, Vec::new());
        let own = report
            .models
            .iter()
            .find(|m| m.source == SOURCE_DIM && m.model == "glm-5.3")
            .unwrap();
        assert_eq!(own.route, "dim");
        assert_eq!(own.counts.input, 200, "promptTokens minus cache reads");
        assert_eq!(own.counts.cache_read, 800);
        assert_eq!(own.counts.output, 50);
        assert!(report
            .models
            .iter()
            .any(|m| m.model == "glm-5.3-flash" && m.route.starts_with("host:")));
        assert!(report
            .models
            .iter()
            .any(|m| m.model == "gemini-3.7-flash" && m.route == "unknown"));
        assert!(!report.models.iter().any(|m| m.model == "bonsai"));
        assert!(report
            .routes
            .iter()
            .any(|r| r.id == "dim" && r.label == "DimAgent 订阅"));

        // Rescan: cursor holds, nothing booked twice.
        assert_eq!(ledger.scan_dim(&db_path).files, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn devin_scan_books_metrics_and_resumes_from_rowid() {
        let dir = std::env::temp_dir().join(format!("dango-devin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("sessions.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message_nodes(
               row_id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id TEXT NOT NULL,
               node_id INTEGER NOT NULL,
               parent_node_id INTEGER,
               chat_message TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               metadata TEXT
             );",
        )
        .unwrap();
        let insert = |msg: &str, at_secs: i64| {
            conn.execute(
                "INSERT INTO message_nodes(session_id,node_id,chat_message,created_at)
                 VALUES('s',0,?1,?2)",
                rusqlite::params![msg, at_secs],
            )
            .unwrap();
        };
        let assistant = |model: &str, input: u64, output: u64, cache_read: u64| {
            format!(
                "{{\"role\":\"assistant\",\"content\":\"x\",\"metadata\":{{\"generation_model\":\"{model}\",\"metrics\":{{\"input_tokens\":{input},\"output_tokens\":{output},\"cache_read_tokens\":{cache_read},\"cache_creation_tokens\":null}}}}}}"
            )
        };
        // 2026-09-26T00:30Z is still 09-25 in the ledger's UTC+8 day.
        insert(&assistant("swe-2-max", 100, 20, 800), 1790335800);
        insert(&assistant("swe-2-max", 300, 40, 0), 1790413200); // 09-26T09:00Z
        insert("{\"role\":\"user\",\"content\":\"no metrics\"}", 1790413300);
        insert(&assistant("claude-fable-5-1-medium", 50, 5, 10), 1790413400);
        drop(conn);

        let mut ledger = Ledger::default();
        let status = ledger.scan_devin(&db_path);
        assert!(status.found);
        assert_eq!(status.files, 3, "metrics-bearing rows only");
        let at = parse_rfc3339_ms("2026-09-26T12:00:00Z").unwrap();
        let report = ledger.report(at, 7, Vec::new());
        let devin_days: BTreeMap<_, _> = report
            .days
            .iter()
            .map(|day| (day.date.clone(), day.by_source.get(SOURCE_DEVIN).copied()))
            .collect();
        let first = devin_days["2026-09-25"].unwrap();
        assert_eq!(first.input, 100);
        assert_eq!(first.output, 20);
        assert_eq!(first.cache_read, 800);
        let second = devin_days["2026-09-26"].unwrap();
        assert_eq!(second.input, 350);
        assert_eq!(second.output, 45);
        assert_eq!(second.cache_read, 10);
        assert!(
            report.models.iter().any(|m| m.source == SOURCE_DEVIN
                && m.model == "claude-fable-5-1-medium"
                && m.route == "devin"),
            "models: {:?}",
            report.models
        );
        assert!(report.routes.iter().any(|r| r.id == "devin"));

        // Rescan: nothing new booked.
        let status = ledger.scan_devin(&db_path);
        assert_eq!(status.files, 0);
        // A new row lands → only it is added.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO message_nodes(session_id,node_id,chat_message,created_at)
             VALUES('s',1,?1,?2)",
            rusqlite::params![assistant("swe-2-max", 7, 3, 0), 1790413500],
        )
        .unwrap();
        drop(conn);
        let status = ledger.scan_devin(&db_path);
        assert_eq!(status.files, 1);
        let report = ledger.report(at, 7, Vec::new());
        let today = report
            .days
            .iter()
            .find(|day| day.date == "2026-09-26")
            .unwrap()
            .by_source[SOURCE_DEVIN];
        assert_eq!(today.input, 357);
        assert_eq!(today.output, 48);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Grok Build rewrites `usage.json` after each turn; only turns past the
    /// last one booked count, and cache reads come out of `inputTokens`.
    #[test]
    fn grok_books_each_turn_once() {
        let dir = std::env::temp_dir().join(format!("dango-grok-test-{}", std::process::id()));
        let session = dir.join("%2Ftmp").join("s1");
        std::fs::create_dir_all(&session).unwrap();
        let turn = |n: u64, at: &str| {
            format!(
                r#"{{"turnNumber":{n},"endedAt":"{at}","inputTokens":100,"outputTokens":7,
                "cachedReadTokens":60,"cacheCreationTokens":0,"primaryModelId":"grok-4.6-build",
                "modelUsage":{{"grok-4.6-build":{{"inputTokens":100,"outputTokens":7,
                "cachedReadTokens":60,"cacheCreationTokens":0}}}}}}"#
            )
        };
        let write = |turns: &[String]| {
            std::fs::write(
                session.join("usage.json"),
                format!(r#"{{"sessionId":"s1","turns":[{}]}}"#, turns.join(",")),
            )
            .unwrap();
        };
        let mut ledger = Ledger::default();
        write(&[turn(1, "2026-09-27T01:00:00Z")]);
        let status = ledger.scan_grok(&dir);
        assert_eq!((status.files, status.error), (1, None));
        write(&[
            turn(1, "2026-09-27T01:00:00Z"),
            turn(2, "2026-09-27T02:00:00Z"),
        ]);
        ledger.scan_grok(&dir);
        ledger.scan_grok(&dir);
        let now = parse_rfc3339_ms("2026-09-27T03:00:00Z").unwrap();
        let report = ledger.report(now, 1, Vec::new());
        let today = report.days[0].by_source[SOURCE_GROK];
        assert_eq!((today.input, today.cache_read, today.output), (80, 120, 14));
        assert_eq!(report.routes[0].id, "grok");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
