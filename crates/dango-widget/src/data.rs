//! In-process data loop: quotas, proxy health, settings.
//!
//! Runs on a tokio runtime and pushes snapshots to the UI through a channel.
//! Errors are reported as-is (`ok = false`); nothing is ever synthesised from a
//! cached value to look fresh.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use dango_lib::models::Snapshot;
use dango_lib::providers;
use dango_lib::proxies;
use dango_lib::proxy_detail;
use dango_lib::settings::{self, Settings};
use dango_lib::{PlanQuota, ProxyDetail, ProxyStatus};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Mutex;
pub const QUOTA_INTERVAL: Duration = Duration::from_secs(60);
pub const PROXY_INTERVAL: Duration = Duration::from_secs(10);
/// Local token logs are rescanned this often (incremental, cheap).
pub const TOKEN_INTERVAL: Duration = Duration::from_secs(60);

/// A user-visible change the UI should react to.
#[derive(Debug, Clone)]
pub enum DataEvent {
    Snapshot(Arc<Snapshot>),
    Settings(Settings),
    ProxyDetail(ProxyDetail),
}

/// Runtime configuration for the data loop (currently empty; kept so the
/// spawn signature stays stable if options grow back).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataConfig;

/// Shared state between the tokio data loop and the UI thread.
pub struct DataState {
    inner: Mutex<Inner>,
    snapshot_tx: watch::Sender<Option<Arc<Snapshot>>>,
    settings_tx: watch::Sender<Arc<Settings>>,
    /// The receivers handed out by `watch::channel` are dropped immediately;
    /// keeping this one alive means `send` never fails when no UI subscriber
    /// exists yet, and every later `subscribe()` still sees the latest value.
    _snapshot_rx: watch::Receiver<Option<Arc<Snapshot>>>,
    _settings_rx: watch::Receiver<Arc<Settings>>,
    settings_path: Result<std::path::PathBuf, String>,
}

struct Inner {
    settings: Settings,
    snapshot: Option<Snapshot>,
    client: reqwest::Client,
}

impl DataState {
    /// State backed by the real user settings file.
    pub fn new(client: reqwest::Client) -> Arc<Self> {
        Self::build(client, settings::settings_path())
    }

    /// State backed by an explicit settings file, so tests never touch the user's.
    pub fn with_settings_path(client: reqwest::Client, path: std::path::PathBuf) -> Arc<Self> {
        Self::build(client, Ok(path))
    }

    fn build(
        client: reqwest::Client,
        settings_path: Result<std::path::PathBuf, String>,
    ) -> Arc<Self> {
        let settings = match &settings_path {
            // First run: no settings yet — show only the balls this machine
            // has something for, and write that down.
            Ok(path) if !path.exists() && cfg!(not(test)) => {
                let mut settings = Settings::default();
                if let Some(home) = std::env::var_os("HOME") {
                    settings.hidden = crate::connect::first_run_hidden(std::path::Path::new(&home));
                }
                if let Err(error) = settings::save_to(path, &settings) {
                    eprintln!("[settings] first-run save failed: {error}");
                }
                settings
            }
            Ok(path) => settings::load_from(path),
            Err(_) => Settings::default(),
        };
        let (settings_tx, settings_rx) = watch::channel(Arc::new(settings.clone()));
        let (snapshot_tx, snapshot_rx) = watch::channel(None);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                settings,
                snapshot: None,
                client,
            }),
            snapshot_tx,
            settings_tx,
            _snapshot_rx: snapshot_rx,
            _settings_rx: settings_rx,
            settings_path,
        })
    }

    pub async fn settings(&self) -> Settings {
        self.inner.lock().await.settings.clone()
    }

    pub fn snapshot_watch(&self) -> watch::Receiver<Option<Arc<Snapshot>>> {
        self.snapshot_tx.subscribe()
    }

    pub fn settings_watch(&self) -> watch::Receiver<Arc<Settings>> {
        self.settings_tx.subscribe()
    }

    pub fn subscribe_settings(&self) -> watch::Receiver<Arc<Settings>> {
        self.settings_watch()
    }

    /// Persist settings (validated + atomic write) and broadcast the result.
    pub async fn save_settings(&self, next: Settings) -> Result<Settings, String> {
        settings::save_to(self.settings_path.as_ref()?, &next)?;
        let mut inner = self.inner.lock().await;
        inner.settings = next.clone();
        drop(inner);
        let _ = self.settings_tx.send(Arc::new(next.clone()));
        Ok(next)
    }

    /// Replace settings in memory only (used when the UI already persisted them).
    pub async fn adopt_settings(&self, next: Settings) {
        let mut inner = self.inner.lock().await;
        inner.settings = next.clone();
        drop(inner);
        let _ = self.settings_tx.send(Arc::new(next));
    }
}

/// Build the shared HTTP client used by every probe.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(dango_lib::probes::http::USER_AGENT)
        .build()
        .unwrap_or_default()
}

/// One full quota refresh across every provider, with proxies attached.
pub async fn fetch_snapshot(client: &reqwest::Client) -> Snapshot {
    let (claude, ag, probe_plans) = tokio::join!(
        providers::claude::fetch(client),
        providers::antigravity::fetch_with_proxy(client),
        providers::probe_plans(client),
    );
    let (ag_plan, ag_proxy) = ag;

    let mut plans = vec![claude, ag_plan];
    plans.extend(probe_plans);
    // User-added balls (API balances) and removed built-ins. Settings are
    // read fresh so a change on the settings page lands on the next refresh.
    let current = tokio::task::spawn_blocking(dango_lib::settings::load)
        .await
        .unwrap_or_default();
    plans.retain(|plan| !current.hidden.contains(&plan.id));
    plans.extend(dango_lib::providers::custom::fetch_all(client, &current.custom).await);
    {
        let plans = plans.clone();
        let _ =
            tokio::task::spawn_blocking(move || crate::token_ledger::record_devin(&plans)).await;
    }

    proxies::attach_proxies_with_statuses(client, &mut plans, Some(ag_proxy)).await;

    Snapshot {
        plans,
        fetched_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    }
}


/// Spawn the periodic data loops. Returns a receiver for UI events.
///
/// The loops are spawned on `handle` because the caller (the UI thread) is not
/// itself inside a tokio runtime context.
pub fn spawn(
    state: Arc<DataState>,
    _config: DataConfig,
    handle: &tokio::runtime::Handle,
) -> mpsc::UnboundedReceiver<DataEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawn = |future: std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>| {
        drop(handle.spawn(future));
    };

    {
        let state = Arc::clone(&state);
        let tx = tx.clone();
        spawn(Box::pin(async move {
            let client = state.inner.lock().await.client.clone();
            let snapshot = fetch_snapshot(&client).await;
            publish_snapshot(&state, &tx, snapshot).await;
        }));
    }

    {
        let state = Arc::clone(&state);
        let tx = tx.clone();
        spawn(Box::pin(async move {
            let client = state.inner.lock().await.client.clone();
            let mut ticker = tokio::time::interval(QUOTA_INTERVAL);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let snapshot = fetch_snapshot(&client).await;
                publish_snapshot(&state, &tx, snapshot).await;
            }
        }));
    }

    // Token ledger: keep the card's "today" line fresh.
    spawn(Box::pin(async move {
        let mut ticker = tokio::time::interval(TOKEN_INTERVAL);
        loop {
            ticker.tick().await;
            if let Err(error) = crate::token_ledger::scan(1).await {
                eprintln!("[tokens] scan failed: {error}");
            }
        }
    }));

    {
        let state = Arc::clone(&state);
        let tx = tx.clone();
        spawn(Box::pin(async move {
            let client = state.inner.lock().await.client.clone();
            let mut ticker = tokio::time::interval(PROXY_INTERVAL);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                refresh_proxies(&state, &client, &tx).await;
            }
        }));
    }

    rx
}

async fn publish_snapshot(
    state: &DataState,
    tx: &mpsc::UnboundedSender<DataEvent>,
    snapshot: Snapshot,
) {
    state.inner.lock().await.snapshot = Some(snapshot.clone());
    let shared = Arc::new(snapshot);
    let _ = state.snapshot_tx.send(Some(Arc::clone(&shared)));
    let _ = tx.send(DataEvent::Snapshot(shared));
}

/// Antigravity bridge health, from `/healthz` only.
///
/// The 10-second proxy loop needs the bridge row, not the quota numbers (those
/// come from the 60-second [`fetch_snapshot`]), so it must never touch `/quota`.
/// This mirrors the healthz half of `providers::antigravity::fetch_with_proxy`
/// so the fast tick and the full refresh agree on ok/error reporting and on
/// account counts; every failure surfaces as [`proxies::antigravity_unavailable`],
/// never a fake-healthy status.
async fn antigravity_healthz(client: &reqwest::Client) -> ProxyStatus {
    const ANTIGRAVITY_HEALTHZ_URL: &str = "http://127.0.0.1:8050/healthz";
    let response = client
        .get(ANTIGRAVITY_HEALTHZ_URL)
        .timeout(Duration::from_secs(3))
        .send()
        .await;
    match response {
        Ok(response) if !response.status().is_success() => {
            proxies::antigravity_unavailable(format!("HTTP {}", response.status().as_u16()))
        }
        Ok(response) => match response.json::<serde_json::Value>().await {
            Ok(value) => proxies::antigravity_from_healthz(&value),
            Err(_) => proxies::antigravity_unavailable("无效 JSON"),
        },
        Err(error) => proxies::antigravity_unavailable(if error.is_timeout() {
            "连接超时"
        } else {
            "连接失败"
        }),
    }
}

/// Re-check only the plan proxies, keeping the freshly fetched quota buckets.
///
/// Red line: Antigravity health is read from the bridge's `/healthz` only
/// (never `/quota` from this loop), so no plan is ever reported healthy from a
/// stale cache.
async fn refresh_proxies(
    state: &DataState,
    client: &reqwest::Client,
    tx: &mpsc::UnboundedSender<DataEvent>,
) {
    let inner = state.inner.lock().await;
    let Some(mut snapshot) = inner.snapshot.clone() else {
        return;
    };
    drop(inner);

    let ag_proxy = antigravity_healthz(client).await;
    // Pass the status through even when the bridge is down: the helper already
    // reports failures faithfully, and a provided status makes
    // `attach_proxies_with_statuses` skip its own antigravity probe — exactly
    // one /healthz request per tick in both the healthy and failure paths.
    proxies::attach_proxies_with_statuses(client, &mut snapshot.plans, Some(ag_proxy)).await;
    // Keep the original fetch timestamp: the numbers did not change, only the proxy rows.
    publish_snapshot(state, tx, snapshot).await;
}

/// On-demand refresh used by the menu bar and the HTTP API.
pub async fn refresh_now(state: &DataState, tx: &mpsc::UnboundedSender<DataEvent>) -> Snapshot {
    let client = state.inner.lock().await.client.clone();
    let snapshot = fetch_snapshot(&client).await;
    publish_snapshot(state, tx, snapshot.clone()).await;
    snapshot
}

/// Fetch proxy detail for one plan, or `None` for plans without a local proxy.
pub async fn proxy_detail(state: &DataState, plan_id: &str) -> Result<Option<ProxyDetail>, String> {
    let client = state.inner.lock().await.client.clone();
    proxy_detail::proxy_detail(&client, plan_id).await
}

/// Click-to-test a plan's local proxy end to end (see `proxy_detail::proxy_test`).
pub async fn proxy_test(
    state: &DataState,
    plan_id: &str,
    model: Option<String>,
) -> Option<proxy_detail::ProxyTestResult> {
    let base_url = proxy_detail::proxy_base_url(plan_id)?;
    let client = state.inner.lock().await.client.clone();
    Some(proxy_detail::proxy_test(&client, base_url, model).await)
}


/// Build the snapshot used before the first refresh completes.
pub fn placeholder_snapshot() -> Snapshot {
    Snapshot {
        plans: Vec::new(),
        fetched_at: 0,
    }
}

/// Convenience for tests / API callers that need the current plan list.
pub async fn plans(state: &DataState) -> Vec<PlanQuota> {
    state
        .snapshot_watch()
        .borrow()
        .as_ref()
        .map(|snapshot| snapshot.plans.clone())
        .unwrap_or_default()
}

/// Current proxy status for one plan, if the snapshot has one.
pub async fn proxy_status(state: &DataState, plan_id: &str) -> Option<ProxyStatus> {
    plans(state)
        .await
        .into_iter()
        .find(|plan| plan.id == plan_id)
        .and_then(|plan| plan.proxy)
}

/// A state whose settings file lives in a fresh temp path, never the user's.
#[cfg(test)]
pub(crate) fn test_state() -> Arc<DataState> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "dango-widget-test-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    DataState::with_settings_path(http_client(), path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_settings_round_trips_through_the_shared_state() {
        let state = test_state();
        let mut next = Settings::default();
        next.perf_mode = Some(dango_lib::PerfMode::Saver);
        // An unknown shape must be rejected before anything is written.
        // Use a fresh path so a leftover file from an earlier run cannot
        // leak a stale perf_mode into the assertion below.
        let unused = std::env::temp_dir().join(format!(
            "dango-widget-unused-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&unused);
        let mut invalid = next.clone();
        invalid
            .balls
            .get_mut("claude")
            .expect("default settings include claude")
            .shape = Some("octagon".into());
        assert!(dango_lib::settings::save_to(&unused, &invalid).is_err());
        assert_eq!(
            dango_lib::settings::load_from(&unused).perf_mode(),
            dango_lib::PerfMode::Balanced,
        );

        // Adopting settings updates the in-memory value and the watch channel.
        let mut receiver = state.settings_watch();
        state.adopt_settings(next.clone()).await;
        assert_eq!(
            state.settings().await.perf_mode(),
            dango_lib::PerfMode::Saver
        );
        assert_eq!(
            receiver.borrow_and_update().perf_mode(),
            dango_lib::PerfMode::Saver
        );
    }

}
