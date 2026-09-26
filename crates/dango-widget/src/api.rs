//! Localhost-only control API on 127.0.0.1:8049.
//!
//! Endpoints: `GET /snapshot`, `POST /refresh`, `GET|PUT /settings`,
//! `GET /proxy-detail/{planId}`, `GET /tokens?days=N`, `GET /events` (SSE), `POST /clipboard`, and the
//! settings page under `/ui/`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{sse, IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use dango_lib::settings::Settings;
use futures_util::Stream;
use tokio::sync::mpsc;

use crate::data::{self, DataEvent, DataState};

const MAX_BODY_BYTES: usize = 64 * 1024;
pub const DEFAULT_PORT: u16 = dango_lib::ports::CONTROL_API;

#[derive(Clone)]
pub struct ApiState {
    pub data: Arc<DataState>,
    pub events: mpsc::UnboundedSender<DataEvent>,
    pub clipboard: mpsc::UnboundedSender<ClipboardRequest>,
    pub clipboard_notify: Arc<dyn Fn() + Send + Sync>,
    /// The port this server is bound to; the clipboard Origin check compares
    /// against it instead of a hardcoded 8049 so `--control-port` keeps working.
    pub port: u16,
}

pub struct ClipboardRequest {
    pub text: String,
    pub result: tokio::sync::oneshot::Sender<Result<(), String>>,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/ui/settings.html", get(settings_html))
        .route("/ui/settings.css", get(settings_css))
        .route("/ui/settings.js", get(settings_js))
        .route("/ui/common.css", get(common_css))
        .route("/ui/common.js", get(common_js))
        .route("/ui/bridge.js", get(bridge_js))
        .route("/ui/grok-ball.js", get(grok_ball_js))
        .route("/snapshot", get(snapshot))
        .route("/refresh", post(refresh))
        .route("/clipboard", post(clipboard))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/proxy-detail/{planId}", get(proxy_detail))
        .route("/tokens", get(token_report))
        .route("/app-memory", get(app_memory))
        .route("/connect/{planId}", post(connect_plan))
        .route("/proxy-test/{planId}", post(proxy_test))
        .route(
            "/proxy-login/{planId}",
            post(proxy_login_start).get(proxy_login_status),
        )
        .route("/credentials", get(list_credentials).put(set_credential))
        .route(
            "/credentials/{planId}",
            axum::routing::delete(delete_credential),
        )
        .route("/events", get(events))
        .layer(axum::middleware::from_fn(require_local_host))
        .with_state(state)
}

/// Binding to loopback does not stop DNS rebinding: a web page can point its own
/// domain at 127.0.0.1 and then call us same-origin. Browsers always send the
/// page's host, so refusing foreign `Host` values closes that hole. A missing
/// `Host` header is foreign too: HTTP/1.1 requires it, so rejecting is safe
/// (all real clients — WKWebView, curl, reqwest — always send one).
async fn require_local_host(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let foreign = !request
        .headers()
        .get(header::HOST)
        .is_some_and(|value| value.to_str().is_ok_and(is_local_host));
    if foreign {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

fn is_local_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

async fn settings_html() -> Response {
    static_file(
        include_str!("../../../ui/settings.html"),
        "text/html; charset=utf-8",
    )
}

async fn settings_css() -> Response {
    static_file(
        include_str!("../../../ui/settings.css"),
        "text/css; charset=utf-8",
    )
}

async fn settings_js() -> Response {
    static_file(
        include_str!("../../../ui/settings.js"),
        "application/javascript; charset=utf-8",
    )
}

async fn common_css() -> Response {
    static_file(
        include_str!("../../../ui/common.css"),
        "text/css; charset=utf-8",
    )
}

async fn common_js() -> Response {
    static_file(
        include_str!("../../../ui/common.js"),
        "application/javascript; charset=utf-8",
    )
}

async fn bridge_js() -> Response {
    static_file(
        include_str!("../../../ui/bridge.js"),
        "application/javascript; charset=utf-8",
    )
}

async fn grok_ball_js() -> Response {
    static_file(
        include_str!("../../../ui/grok-ball.js"),
        "application/javascript; charset=utf-8",
    )
}

fn static_file(contents: &'static str, content_type: &'static str) -> Response {
    let mut response = Response::new(axum::body::Body::from(contents));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Serve the API until the listener dies. Localhost only.
pub async fn serve(addr: std::net::SocketAddr, state: ApiState) -> std::io::Result<()> {
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "control API can only bind to loopback",
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("[control-api] listening on http://{addr}");
    axum::serve(listener, router(state)).await
}

async fn snapshot(State(state): State<ApiState>) -> Json<serde_json::Value> {
    let snapshot = state
        .data
        .snapshot_watch()
        .borrow()
        .as_ref()
        .map(|snapshot| (**snapshot).clone())
        .unwrap_or_else(data::placeholder_snapshot);
    Json(serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null))
}

async fn refresh(State(state): State<ApiState>) -> Json<serde_json::Value> {
    let snapshot = data::refresh_now(&state.data, &state.events).await;
    Json(serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null))
}

#[derive(serde::Deserialize)]
struct ClipboardPayload {
    text: String,
}

/// Writing the clipboard is a side effect, so it gets one check beyond the
/// loopback `Host` gate: `Origin` must point back at this server's own port.
/// The settings page (WKWebView on http://127.0.0.1:<port>) sends exactly
/// that; a random same-host process is less likely to preflight like a browser.
fn is_self_origin(headers: &HeaderMap, port: u16) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let (host, origin_port) = match origin.rsplit_once(':') {
        Some((host, origin_port)) => (host, origin_port),
        None => return false,
    };
    let host_local = matches!(
        host,
        "http://127.0.0.1" | "http://localhost" | "http://[::1]"
    );
    host_local && origin_port == port.to_string()
}

async fn clipboard(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !is_json_content_type(&headers) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if body.len() > MAX_BODY_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    if !is_self_origin(&headers, state.port) {
        return (
            StatusCode::FORBIDDEN,
            "clipboard writes must come from this app's own page",
        )
            .into_response();
    }
    let payload: ClipboardPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid clipboard JSON: {error}"),
            )
                .into_response()
        }
    };
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    if state
        .clipboard
        .send(ClipboardRequest {
            text: payload.text,
            result: result_tx,
        })
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "clipboard handler is unavailable",
        )
            .into_response();
    }
    (state.clipboard_notify)();
    match result_rx.await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => (StatusCode::BAD_GATEWAY, error).into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "clipboard handler stopped").into_response(),
    }
}

async fn get_settings(State(state): State<ApiState>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(state.data.settings().await).unwrap_or(serde_json::Value::Null))
}

async fn put_settings(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !is_json_content_type(&headers) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if body.len() > MAX_BODY_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let parsed: Result<Settings, _> = serde_json::from_slice(&body);
    match parsed {
        Ok(settings) => match state.data.save_settings(settings.clone()).await {
            Ok(saved) => {
                let _ = state.events.send(DataEvent::Settings(saved.clone()));
                Json(serde_json::to_value(&saved).unwrap_or(serde_json::Value::Null))
                    .into_response()
            }
            Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
        },
        Err(error) => (
            StatusCode::BAD_REQUEST,
            format!("invalid settings JSON: {error}"),
        )
            .into_response(),
    }
}

/// `GET /credentials` — which manual credential slots are filled. Presence
/// only; the token itself is never served back out.
async fn list_credentials() -> Json<serde_json::Value> {
    let set = |plan: &str| {
        tokio::task::spawn_blocking({
            let plan = plan.to_string();
            move || dango_lib::manual_creds::has(&plan)
        })
    };
    let (haze, devin, factory, dim) =
        tokio::join!(set("haze"), set("devin"), set("factory"), set("dim"));
    Json(serde_json::json!({
        "dim": dim.unwrap_or(false),
        "haze": haze.unwrap_or(false),
        "devin": devin.unwrap_or(false),
        "factory": factory.unwrap_or(false),
    }))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialPayload {
    plan: String,
    token: String,
}

/// `PUT /credentials` — store a user-pasted token in the `dango`
/// keychain service. Same side-effect bar as `/clipboard`: self-origin only.
async fn set_credential(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !is_json_content_type(&headers) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if body.len() > MAX_BODY_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    if !is_self_origin(&headers, state.port) {
        return (
            StatusCode::FORBIDDEN,
            "credential writes must come from this app's own page",
        )
            .into_response();
    }
    let payload: CredentialPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid credential JSON: {error}"),
            )
                .into_response();
        }
    };
    let token = payload.token.trim().to_string();
    let outcome =
        tokio::task::spawn_blocking(move || dango_lib::manual_creds::set(&payload.plan, &token))
            .await;
    match outcome {
        Ok(Ok(())) => {
            // Pull fresh quota right away so the new token's effect is visible
            // instead of arriving up to a minute later.
            data::refresh_now(&state.data, &state.events).await;
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, error).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "credential task failed").into_response(),
    }
}

/// `DELETE /credentials/{plan}` — drop the manual slot, back to vendor reads.
async fn delete_credential(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
) -> Response {
    if !is_self_origin(&headers, state.port) {
        return (
            StatusCode::FORBIDDEN,
            "credential deletes must come from this app's own page",
        )
            .into_response();
    }
    let outcome =
        tokio::task::spawn_blocking(move || dango_lib::manual_creds::delete(&plan_id)).await;
    match outcome {
        Ok(Ok(())) => {
            data::refresh_now(&state.data, &state.events).await;
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, error).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "credential task failed").into_response(),
    }
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

async fn proxy_detail(State(state): State<ApiState>, Path(plan_id): Path<String>) -> Response {
    match data::proxy_detail(&state.data, &plan_id).await {
        Ok(Some(detail)) => {
            Json(serde_json::to_value(detail).unwrap_or(serde_json::Value::Null)).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            format!("no proxy detail for plan '{plan_id}'"),
        )
            .into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, error).into_response(),
    }
}

/// `POST /connect/{planId}` — start the vendor's own login (official CLI in
/// Terminal, or its App). Launches programs, so self-origin only.
async fn connect_plan(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
) -> Response {
    if !is_self_origin(&headers, state.port) {
        return (
            StatusCode::FORBIDDEN,
            "connect must come from this app's own page",
        )
            .into_response();
    }
    match tokio::task::spawn_blocking(move || crate::connect::connect(&plan_id)).await {
        Ok(Ok(outcome)) => Json(outcome).into_response(),
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, error).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "connect task failed").into_response(),
    }
}

#[derive(serde::Deserialize)]
struct TokensQuery {
    days: Option<u32>,
}

/// Token ledger report (local tool logs, rescanned incrementally per call).
async fn token_report(axum::extract::Query(query): axum::extract::Query<TokensQuery>) -> Response {
    match crate::token_ledger::scan(query.days.unwrap_or(30)).await {
        Ok(report) => Json(report).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AppMemoryResponse {
    bytes: Option<u64>,
    formatted: String,
}

#[cfg(target_os = "macos")]
fn get_process_memory_bytes() -> Option<u64> {
    use std::mem::MaybeUninit;

    // Matches macOS task_vm_info struct from mach/task_info.h
    #[repr(C)]
    #[allow(non_camel_case_types)]
    struct task_vm_info_data_t {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
        min_address: u64,
        max_address: u64,
        ledger_tag_free: i64,
        ledger_tag_purgeable: i64,
        ledger_tag_media_footprint: i64,
        ledger_tag_media_nofootprint: i64,
        ledger_tag_graphics_footprint: i64,
        ledger_tag_graphics_nofootprint: i64,
        ledger_tag_neural_footprint: i64,
        ledger_tag_neural_nofootprint: i64,
    }

    const TASK_VM_INFO: i32 = 22;
    const TASK_VM_INFO_COUNT: u32 =
        (std::mem::size_of::<task_vm_info_data_t>() / std::mem::size_of::<i32>()) as u32;

    extern "C" {
        fn mach_task_self() -> u32;
        fn task_info(
            target_task: u32,
            flavor: i32,
            task_info_out: *mut i32,
            task_info_outCnt: *mut u32,
        ) -> i32;
    }

    unsafe {
        let mut info = MaybeUninit::<task_vm_info_data_t>::zeroed();
        let mut count = TASK_VM_INFO_COUNT;
        let kr = task_info(
            mach_task_self(),
            TASK_VM_INFO,
            info.as_mut_ptr() as *mut i32,
            &mut count,
        );
        if kr == 0 {
            let info = info.assume_init();
            Some(info.phys_footprint)
        } else {
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn get_process_memory_bytes() -> Option<u64> {
    None
}

async fn app_memory() -> Json<AppMemoryResponse> {
    let bytes = get_process_memory_bytes();
    let formatted = match bytes {
        Some(b) => {
            let mb = b as f64 / (1024.0 * 1024.0);
            if mb >= 1000.0 {
                format!("{:.2} GB", mb / 1024.0)
            } else {
                format!("{:.1} MB", mb)
            }
        }
        None => "—".to_string(),
    };
    Json(AppMemoryResponse { bytes, formatted })
}

#[derive(serde::Deserialize, Default)]
struct ProxyTestPayload {
    model: Option<String>,
}

/// Run the connectivity test. It spends a few upstream tokens, so like the
/// credential writes it only answers the app's own settings page.
async fn proxy_test(
    State(state): State<ApiState>,
    Path(plan_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !is_self_origin(&headers, state.port) {
        return (
            StatusCode::FORBIDDEN,
            "proxy tests must come from this app's own page",
        )
            .into_response();
    }
    let payload: ProxyTestPayload = if body.is_empty() {
        ProxyTestPayload::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(payload) => payload,
            Err(error) => {
                return (StatusCode::BAD_REQUEST, format!("invalid JSON: {error}")).into_response()
            }
        }
    };
    match data::proxy_test(&state.data, &plan_id, payload.model).await {
        Some(result) => Json(result).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            format!("no proxy for plan '{plan_id}'"),
        )
            .into_response(),
    }
}

/// `POST /proxy-login/{plan}` → 上游反代的 `/control/login`（启动 OAuth 回环）。
/// `GET  /proxy-login/{plan}` → 轮它的状态。两条都只认本应用页面。
/// 反代实例配了 api-key 时我们不带 key，403 如实回给前端。
async fn proxy_login_start(
    State(state): State<ApiState>,
    Path(plan_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !is_self_origin(&headers, state.port) {
        return (
            StatusCode::FORBIDDEN,
            "login must come from this app's own page",
        )
            .into_response();
    }
    let Some(base) = proxy_base_for(&plan_id) else {
        return (
            StatusCode::NOT_FOUND,
            format!("plan '{plan_id}' 没有可加账号的反代"),
        )
            .into_response();
    };
    match reqwest::Client::new()
        .post(format!("{base}/control/login"))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(upstream) => passthrough(upstream).await,
        Err(error) => (StatusCode::BAD_GATEWAY, format!("反代够不着：{error}")).into_response(),
    }
}

async fn proxy_login_status(
    State(state): State<ApiState>,
    Path(plan_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !is_self_origin(&headers, state.port) {
        return (
            StatusCode::FORBIDDEN,
            "login status must come from this app's own page",
        )
            .into_response();
    }
    let Some(base) = proxy_base_for(&plan_id) else {
        return (
            StatusCode::NOT_FOUND,
            format!("plan '{plan_id}' 没有可加账号的反代"),
        )
            .into_response();
    };
    match reqwest::Client::new()
        .get(format!("{base}/control/login"))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(upstream) => passthrough(upstream).await,
        Err(error) => (StatusCode::BAD_GATEWAY, format!("反代够不着：{error}")).into_response(),
    }
}

/// `/control/login` 目前只有 antigravity 池有；按 plan id 从反代表里找 base
/// （`http://…:8050/healthz` → `http://…:8050`），别的套餐一律 404。
fn proxy_base_for(plan_id: &str) -> Option<String> {
    if plan_id != "antigravity" {
        return None;
    }
    dango_lib::proxies::PLAN_PROXIES
        .iter()
        .find(|(id, _)| *id == plan_id)
        .map(|(_, url)| url.trim_end_matches("/healthz").to_string())
}

async fn passthrough(response: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = response.text().await.unwrap_or_default();
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

/// SSE stream of `snapshot` and `settings-changed` events.
///
/// Implemented with `futures_util::stream::unfold` so the crate does not need
/// the `async-stream` dependency.
async fn events(
    State(state): State<ApiState>,
) -> Sse<impl Stream<Item = Result<sse::Event, std::convert::Infallible>>> {
    let snapshots = state.data.snapshot_watch();
    let settings = state.data.settings_watch();
    let stream = futures_util::stream::unfold(
        (snapshots, settings),
        |(mut snapshots, mut settings)| async move {
            loop {
                tokio::select! {
                    changed = snapshots.changed() => {
                        if changed.is_err() {
                            return None;
                        }
                        let snapshot = snapshots.borrow_and_update().clone();
                        if let Some(snapshot) = snapshot {
                            return Some((Ok(event("snapshot", &*snapshot)), (snapshots, settings)));
                        }
                    }
                    changed = settings.changed() => {
                        if changed.is_err() {
                            return None;
                        }
                        let settings_val = settings.borrow_and_update().clone();
                        return Some((Ok(event("settings-changed", &*settings_val)), (snapshots, settings)));
                    }
                }
            }
        },
    );
    Sse::new(stream).keep_alive(sse::KeepAlive::new())
}

fn event(name: &str, payload: &impl serde::Serialize) -> sse::Event {
    sse::Event::default()
        .event(name)
        .data(serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body as AxumBody;
    use axum::http::{header, Request};
    use dango_lib::models::{PlanQuota, ProxyStatus, Snapshot};
    use tower::ServiceExt;

    fn state() -> ApiState {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (clipboard, _clipboard_rx) = mpsc::unbounded_channel();
        ApiState {
            data: data::test_state(),
            events: tx,
            clipboard,
            clipboard_notify: Arc::new(|| {}),
            port: DEFAULT_PORT,
        }
    }

    fn app() -> Router {
        router(state())
    }

    #[tokio::test]
    async fn snapshot_endpoint_returns_json() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/snapshot")
                    .header(header::HOST, "127.0.0.1:8049")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["plans"], serde_json::json!([]));
        assert!(value.get("fetchedAt").is_some());
    }

    #[tokio::test]
    async fn app_memory_endpoint_returns_json_and_positive_bytes_on_macos() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/app-memory")
                    .header(header::HOST, "127.0.0.1:8049")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("formatted").is_some());
        #[cfg(target_os = "macos")]
        {
            let b = value["bytes"].as_u64().unwrap();
            assert!(b > 1024 * 1024); // at least 1MB
        }
    }

    #[tokio::test]
    async fn foreign_host_header_is_rejected() {
        for (host, expected) in [
            ("127.0.0.1:8049", StatusCode::OK),
            ("localhost:8049", StatusCode::OK),
            ("[::1]:8049", StatusCode::OK),
            ("evil.example:8049", StatusCode::FORBIDDEN),
            ("127.0.0.1.evil.example", StatusCode::FORBIDDEN),
        ] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .uri("/snapshot")
                        .header(header::HOST, host)
                        .body(AxumBody::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{host}");
        }
    }

    #[tokio::test]
    async fn missing_host_header_is_rejected() {
        // A request with no `Host` header used to slip past `require_local_host`
        // (`is_some_and` only checked when present), leaving the control API
        // open to non-browser clients that omit the header entirely.
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/snapshot")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn settings_ui_assets_are_served_without_caching_or_cors() {
        for (path, content_type, marker) in [
            (
                "/ui/settings.html",
                "text/html; charset=utf-8",
                "Dango 设置",
            ),
            ("/ui/settings.css", "text/css; charset=utf-8", "plan-row"),
            (
                "/ui/settings.js",
                "application/javascript; charset=utf-8",
                "renderBallsTab",
            ),
            ("/ui/common.css", "text/css; charset=utf-8", "--ink:"),
            (
                "/ui/common.js",
                "application/javascript; charset=utf-8",
                "function formatTime",
            ),
            (
                "/ui/bridge.js",
                "application/javascript; charset=utf-8",
                "new EventSource('/events')",
            ),
            (
                "/ui/grok-ball.js",
                "application/javascript; charset=utf-8",
                "GrokBall",
            ),
        ] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::HOST, "127.0.0.1:8049")
                        .body(AxumBody::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some(content_type),
                "{path}"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::CACHE_CONTROL)
                    .and_then(|value| value.to_str().ok()),
                Some("no-store"),
                "{path}"
            );
            assert!(
                response
                    .headers()
                    .get("access-control-allow-origin")
                    .is_none(),
                "{path}"
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(body.contains(marker), "{path} missing {marker}");
        }
    }

    #[tokio::test]
    async fn settings_put_validates_and_persists() {
        let body = serde_json::json!({
            "version": 1,
            "order": ["cursor"],
            "balls": {},
            "perfMode": "smooth"
        })
        .to_string();
        let response = app()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/settings")
                    .header(header::HOST, "127.0.0.1:8049")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(AxumBody::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["perfMode"], serde_json::json!("smooth"));
    }

    #[tokio::test]
    async fn settings_put_persists_theme() {
        let body = serde_json::json!({
            "version": 1,
            "order": ["cursor"],
            "balls": {},
            "theme": "dark"
        })
        .to_string();
        let response = app()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/settings")
                    .header(header::HOST, "127.0.0.1:8049")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(AxumBody::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["theme"], serde_json::json!("dark"));
    }

    #[tokio::test]
    async fn invalid_settings_are_rejected() {
        let body = serde_json::json!({
            "version": 1,
            "order": ["cursor"],
            "balls": { "cursor": { "shape": "octagon" } }
        })
        .to_string();
        let response = app()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/settings")
                    .header(header::HOST, "127.0.0.1:8049")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(AxumBody::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn get_settings_returns_json() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/settings")
                    .header(header::HOST, "127.0.0.1:8049")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["version"], 1);
    }

    #[tokio::test]
    async fn post_refresh_returns_json() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/refresh")
                    .header(header::HOST, "127.0.0.1:8049")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("plans").is_some());
    }

    #[tokio::test]
    async fn clipboard_endpoint_forwards_text_and_waits_for_native_result() {
        let (events, _events_rx) = mpsc::unbounded_channel();
        let (clipboard, mut clipboard_rx) = mpsc::unbounded_channel();
        let api = router(ApiState {
            data: data::test_state(),
            events,
            clipboard,
            clipboard_notify: Arc::new(|| {}),
            port: DEFAULT_PORT,
        });
        let request = tokio::spawn(async move {
            api.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/clipboard")
                    .header(header::HOST, "127.0.0.1:8049")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "http://127.0.0.1:8049")
                    .body(AxumBody::from(r#"{"text":"copy me"}"#))
                    .unwrap(),
            )
            .await
            .unwrap()
        });

        let forwarded = clipboard_rx.recv().await.unwrap();
        assert_eq!(forwarded.text, "copy me");
        forwarded.result.send(Ok(())).unwrap();
        assert_eq!(request.await.unwrap().status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn clipboard_endpoint_rejects_simple_cross_origin_content_type() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/clipboard")
                    .header(header::HOST, "127.0.0.1:8049")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(AxumBody::from(r#"{"text":"copy me"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn events_endpoint_returns_sse_stream() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/events")
                    .header(header::HOST, "127.0.0.1:8049")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
    }

    #[tokio::test]
    async fn malformed_settings_body_returns_400() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/settings")
                    .header(header::HOST, "127.0.0.1:8049")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(AxumBody::from("this is not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn proxy_test_refuses_foreign_origins() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/proxy-test/antigravity")
                    .header(header::HOST, "127.0.0.1:8049")
                    .header(header::ORIGIN, "https://evil.example")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn proxy_detail_reports_missing_plan() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/proxy-detail/not-a-plan")
                    .header(header::HOST, "127.0.0.1:8049")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_rejects_non_loopback() {
        let non_loopback = "192.168.1.1:8049".parse().unwrap();
        let result = serve(non_loopback, state()).await;
        assert!(result.is_err());
    }

    #[test]
    fn snapshot_serialises_with_camel_case_contract() {
        let snapshot = Snapshot {
            plans: vec![PlanQuota {
                id: "cursor".into(),
                name: "Cursor Pro".into(),
                ok: true,
                error: None,
                remaining_percent: Some(42.0),
                buckets: vec![],
                note: None,
                proxy: None,
                headline_label: None,
                resets_at: None,
            }],
            fetched_at: 1_700_000_000,
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["plans"][0]["remainingPercent"], 42.0);
        assert_eq!(value["fetchedAt"], 1_700_000_000_u64);
    }

    #[test]
    fn proxy_status_fields_stay_camel_case() {
        let status = ProxyStatus {
            name: "Gemini 反代".into(),
            url: "http://127.0.0.1:8050/healthz".into(),
            ok: true,
            latency_ms: Some(12),
            detail: None,
            summary: Some("上游 200".into()),
            upstream_status: Some(200),
            requests_today: Some(3),
            requests_total: None,
            in_flight: Some(0),
            last_upstream_at: Some(1_700_000_000),
            token_available: Some(true),
            accounts_available: None,
            accounts_cooling: None,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["latencyMs"], 12);
        assert_eq!(value["upstreamStatus"], 200);
        assert_eq!(value["requestsToday"], 3);
        assert!(value.get("requestsTotal").is_none());
    }

    #[test]
    fn recent_request_exposes_camel_case_fields() {
        let request = dango_lib::RecentRequest {
            at: 1_700_000_000_000,
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            model: Some("grok".into()),
            status: Some(200),
            ms: Some(120),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["path"], "/v1/chat/completions");
        assert_eq!(value["status"], 200);
        assert_eq!(value["ms"], 120);
    }

    #[tokio::test]
    async fn connect_refuses_foreign_origins() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/connect/cursor")
                    .header(header::HOST, "127.0.0.1:8049")
                    .header(header::ORIGIN, "https://evil.example")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
