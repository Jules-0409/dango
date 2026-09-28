//! Native dango widget.
//!
//! Platform-neutral pieces (theme, geometry, data loop, control API) live here;
//! everything that touches a windowing system lives under `platform/`.
//!
//! # UI Quickstart
//! ```no_run
//! use dango_widget::{start, StartOptions, DataEvent, PerfMode};
//!
//! let handle = start(StartOptions::default());
//! // Frame loop (non-blocking, polled per tick on the main UI thread):
//! while let Some(event) = handle.try_recv() {
//!     match event {
//!         DataEvent::Snapshot(snapshot) => { /* update slot rings & balls */ }
//!         DataEvent::Settings(settings) => { /* update layout / perf mode */ }
//!         DataEvent::ProxyDetail(detail) => { /* update detail card */ }
//!     }
//! }
//! // Dispatch commands from menu/user clicks:
//! handle.refresh();
//! handle.set_perf_mode(PerfMode::Smooth);
//! ```

use std::sync::Arc;

pub mod api;
pub mod app_model;
pub mod connect;
pub mod data;
pub mod geometry;
pub mod phone_feed;
pub mod theme;
pub mod token_ledger;
pub mod window_state;

#[cfg(target_os = "macos")]
pub mod app;
#[cfg(target_os = "macos")]
pub mod platform;

pub use api::DEFAULT_PORT as DEFAULT_API_PORT;
pub use dango_lib::models::{Bucket, PlanQuota, ProxyStatus, RecentRequest, Snapshot};
pub use dango_lib::proxy_detail::ProxyDetail;
pub use dango_lib::settings::{PerfMode, Settings, Theme};
pub use data::{DataConfig, DataEvent};
pub use theme::{emotion_for, palette_color, shape_for};

/// Options used to launch the background runtime and control API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOptions {
    /// Port for the localhost HTTP control API (defaults to 8049). `None` disables the server.
    pub api_port: Option<u16>,
}

impl Default for StartOptions {
    fn default() -> Self {
        Self {
            api_port: Some(api::DEFAULT_PORT),
        }
    }
}

impl StartOptions {
    /// Parse CLI flags: `--no-api`, `--api-port <n>`.
    pub fn from_args<I, T>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        let mut api_port = Some(api::DEFAULT_PORT);
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            let s = arg.as_ref();
            if s == "--no-api" {
                api_port = None;
            } else if s == "--api-port" {
                if let Some(val) = iter.next() {
                    let port = val
                        .as_ref()
                        .parse::<u16>()
                        .map_err(|e| format!("invalid --api-port: {e}"))?;
                    api_port = Some(port);
                } else {
                    return Err("--api-port requires a port argument".into());
                }
            } else if let Some(val) = s.strip_prefix("--api-port=") {
                let port = val
                    .parse::<u16>()
                    .map_err(|e| format!("invalid --api-port: {e}"))?;
                api_port = Some(port);
            }
        }
        Ok(Self { api_port })
    }
}

#[derive(Debug)]
pub enum BackendCommand {
    Refresh,
    SetPerfMode(PerfMode),
    SaveSettings(Settings),
    RequestProxyDetail(String),
}

/// UI-facing handle providing non-blocking access to incoming events and
/// command dispatching to the background tokio runtime.
///
/// Library-mode entry point retained for tests and for embedding the backend
/// without the macOS app shell; the shipped binary (`main.rs`) drives the same
/// pieces through `app::run` instead.
pub struct Handle {
    /// Non-blocking receiver channel for data and state events.
    pub events: std::sync::mpsc::Receiver<DataEvent>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<BackendCommand>,
    _thread: std::thread::JoinHandle<()>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").finish_non_exhaustive()
    }
}

impl Handle {
    /// Non-blocking check for the next backend event. Poll this once or more per UI frame.
    pub fn try_recv(&self) -> Option<DataEvent> {
        self.events.try_recv().ok()
    }

    /// Request an immediate refresh of quotas and proxies.
    pub fn refresh(&self) {
        let _ = self.cmd_tx.send(BackendCommand::Refresh);
    }

    /// Change the animation performance mode and persist it to settings.json.
    pub fn set_perf_mode(&self, mode: PerfMode) {
        let _ = self.cmd_tx.send(BackendCommand::SetPerfMode(mode));
    }

    /// Validate, atomically persist, and apply updated settings.
    pub fn save_settings(&self, settings: Settings) {
        let _ = self.cmd_tx.send(BackendCommand::SaveSettings(settings));
    }

    /// Request proxy detail (accounts, models, recent requests) for a plan.
    pub fn request_proxy_detail(&self, plan_id: impl Into<String>) {
        let _ = self
            .cmd_tx
            .send(BackendCommand::RequestProxyDetail(plan_id.into()));
    }
}

/// Spawn the background Tokio runtime, periodic data loops, and control API.
///
/// Returns a [`Handle`] that gives the UI non-blocking access to incoming events
/// and allows the UI to dispatch commands without blocking the main event loop.
pub fn start(opts: StartOptions) -> Handle {
    let (ui_tx, ui_rx) = std::sync::mpsc::channel();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<BackendCommand>();

    // Seed the UI channel immediately with the saved settings.
    let initial_settings = dango_lib::settings::load();
    let _ = ui_tx.send(DataEvent::Settings(initial_settings));

    let thread = std::thread::Builder::new()
        .name("dango-widget-backend".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(error) => {
                    eprintln!("[dango-widget] failed to build tokio runtime: {error}");
                    return;
                }
            };

            rt.block_on(async move {
                let client = data::http_client();
                let state = data::DataState::new(client);
                let data_config = data::DataConfig;
                let handle = tokio::runtime::Handle::current();
                let mut data_event_rx = data::spawn(Arc::clone(&state), data_config, &handle);

                let (api_event_tx, mut api_event_rx) =
                    tokio::sync::mpsc::unbounded_channel::<DataEvent>();
                let (clipboard_tx, mut clipboard_rx) =
                    tokio::sync::mpsc::unbounded_channel::<api::ClipboardRequest>();
                tokio::spawn(async move {
                    while let Some(request) = clipboard_rx.recv().await {
                        let _ = request
                            .result
                            .send(Err("native clipboard is unavailable in library mode".into()));
                    }
                });

                if let Some(api_port) = opts.api_port {
                    let api_state = api::ApiState {
                        data: Arc::clone(&state),
                        events: api_event_tx,
                        clipboard: clipboard_tx,
                        clipboard_notify: Arc::new(|| {}),
                        port: api_port,
                    };
                    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], api_port));
                    tokio::spawn(async move {
                        if let Err(error) = api::serve(addr, api_state).await {
                            eprintln!("[control-api] serve exited: {error}");
                        }
                    });
                }

                let ui_tx_clone = ui_tx.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            Some(event) = data_event_rx.recv() => {
                                if ui_tx_clone.send(event).is_err() {
                                    break;
                                }
                            }
                            Some(event) = api_event_rx.recv() => {
                                if ui_tx_clone.send(event).is_err() {
                                    break;
                                }
                            }
                            else => break,
                        }
                    }
                });

                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        BackendCommand::Refresh => {
                            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                            let snapshot = data::refresh_now(&state, &tx).await;
                            let _ = ui_tx.send(DataEvent::Snapshot(Arc::new(snapshot)));
                        }
                        BackendCommand::SetPerfMode(perf_mode) => {
                            let mut current = state.settings().await;
                            current.perf_mode = Some(perf_mode);
                            if let Ok(saved) = state.save_settings(current).await {
                                let _ = ui_tx.send(DataEvent::Settings(saved));
                            }
                        }
                        BackendCommand::SaveSettings(settings) => {
                            if let Ok(saved) = state.save_settings(settings).await {
                                let _ = ui_tx.send(DataEvent::Settings(saved));
                            }
                        }
                        BackendCommand::RequestProxyDetail(plan_id) => {
                            if let Ok(Some(detail)) = data::proxy_detail(&state, &plan_id).await {
                                let _ = ui_tx.send(DataEvent::ProxyDetail(detail));
                            }
                        }
                    }
                }
            });
        })
        .expect("spawn backend thread");

    Handle {
        events: ui_rx,
        cmd_tx,
        _thread: thread,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_options_parse_cli_flags() {
        let opts = StartOptions::from_args(["--api-port", "8888"]).unwrap();
        assert_eq!(opts.api_port, Some(8888));

        let opts2 = StartOptions::from_args(["--no-api"]).unwrap();
        assert_eq!(opts2.api_port, None);
    }

    #[test]
    fn start_spawns_and_delivers_initial_settings() {
        let opts = StartOptions { api_port: None };
        let handle = start(opts);
        let event = handle.try_recv().expect("initial event");
        match event {
            DataEvent::Settings(_) => {}
            other => panic!("expected Settings event, got {other:?}"),
        }
    }
}
