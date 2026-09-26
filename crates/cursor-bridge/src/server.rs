//! HTTP 层：路由、账号轮换、把 CLI 的事件流翻成 SSE。
//!
//! 端点：
//! - `GET  /healthz`                  自检（含认证方式、账号数、CLI 路径）
//! - `GET  /v1/models`                模型清单（问 CLI，缓存 10 分钟）
//! - `POST /v1/messages`              Anthropic Messages（流式 + 非流式）
//! - `POST /v1/messages/count_tokens` 粗略估 token（不是精确值，文档里写明）
//! - `POST /v1/chat/completions`      OpenAI Chat Completions（流式 + 非流式）

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::accounts::{self, Account, Pool};
use crate::cli::{AgentCmd, TurnRequest};
use crate::config::{Backend, Config};
use crate::error::{Error, Result};
use crate::models::ModelInfo;
use crate::native::Native;
use crate::protocol::{self, AnthropicStream, Collected, OpenAiStream};
use crate::turn::{self, Step, TurnOutcome};
use crate::types::{build_prompt, AnthropicRequest, OpenAiRequest, UnifiedRequest};

/// 历史正文的字符预算：CLI 是「单请求 + 一段 prompt」，带太多历史又慢又贵。
const HISTORY_BUDGET: usize = 24_000;
/// 模型清单的缓存时长。
const MODELS_TTL: Duration = Duration::from_secs(600);

pub struct App {
    pub cfg: Config,
    /// CLI 路径。装了 CLI 才有；`backend=native` 时允许没有。
    pub cmd: Option<AgentCmd>,
    pub pool: Pool,
    pub started: Instant,
    /// `CURSOR_BRIDGE_BACKEND=native` 时才有：直接和 agent 服务说话，不拉 CLI。
    pub native: Option<Native>,
    models: tokio::sync::Mutex<Option<(Instant, Vec<ModelInfo>)>>,
}

type Shared = Arc<App>;

/// 起服务。返回时说明监听结束（正常情况不会返回）。
pub async fn serve(cfg: Config) -> Result<()> {
    // 原生后端不拉 CLI，所以 CLI 不在也能跑；CLI 后端必须找得到它
    let cmd = match AgentCmd::locate() {
        Ok(cmd) => Some(cmd),
        Err(err) if cfg.backend == Backend::Cli => return Err(err),
        Err(_) => None,
    };
    let accounts = accounts::load(&cfg.accounts_file)?;
    let pool = Pool::new(accounts);
    std::fs::create_dir_all(&cfg.workspace).map_err(Error::Io)?;

    let auth = if cfg.single_api_key.is_some() {
        "单 key（CURSOR_BRIDGE_API_KEY）"
    } else if pool.is_empty() {
        "CLI 当前登录态（不碰钥匙串）"
    } else {
        "账号池（API key 轮换）"
    };

    // 原生后端只认 API key（换 access token 那条路），CLI 登录态用不了
    let native = match cfg.backend {
        Backend::Native => {
            if pool.is_empty() && cfg.single_api_key.is_none() {
                eprintln!(
                    "注意：      native 后端需要 API key（accounts.json 或 CURSOR_BRIDGE_API_KEY），\
                     现在只有 CLI 登录态，每个请求都会失败"
                );
            }
            Some(Native::new(&cfg.api_endpoint, &cfg.agent_endpoint)?)
        }
        Backend::Cli => None,
    };

    println!("cursor-bridge {}", env!("CARGO_PKG_VERSION"));
    println!("监听：      http://{}", cfg.addr());
    println!("后端：      {}", cfg.backend.as_str());
    if let Some(native) = &native {
        println!("agent 端点：{}", native.agent_endpoint());
        println!("客户端版本：{}", native.client_version());
    }
    match &cmd {
        Some(cmd) => println!("CLI：       {}", cmd.bin.display()),
        None => println!("CLI：       没找到（原生后端不需要它）"),
    }
    println!("工作区：    {}", cfg.workspace.display());
    println!(
        "模式：      {}",
        cfg.mode.as_deref().unwrap_or("（CLI 默认）")
    );
    println!("认证：      {auth}");
    if !pool.is_empty() {
        println!("账号：      {}", pool.names().join(", "));
    } else {
        println!(
            "账号：      没有 {}（想要日抛号就把 API key 写进去，见 README）",
            cfg.accounts_file.display()
        );
    }
    for note in &cfg.notes {
        println!("注意：      {note}");
    }

    let app = Arc::new(App {
        cfg: cfg.clone(),
        cmd,
        pool,
        started: Instant::now(),
        native,
        models: tokio::sync::Mutex::new(None),
    });

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(models))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(cfg.addr())
        .await
        .map_err(Error::Io)?;
    println!("\n准备好了。Anthropic 端点 http://{}/v1/messages，OpenAI 端点 http://{}/v1/chat/completions", cfg.addr(), cfg.addr());
    axum::serve(listener, router).await.map_err(Error::Io)
}

async fn healthz(State(app): State<Shared>) -> Response {
    let auth = if app.cfg.single_api_key.is_some() {
        "api-key"
    } else if app.pool.is_empty() {
        "cli-login"
    } else {
        "api-key-pool"
    };
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "backend": app.cfg.backend.as_str(),
        "agentEndpoint": app.native.as_ref().map(|n| n.agent_endpoint().to_string()),
        "agent": app.cmd.as_ref().map(|c| c.bin.display().to_string()),
        "mode": app.cfg.mode.clone(),
        "workspace": app.cfg.workspace.display().to_string(),
        "auth": auth,
        "accounts": app.pool.len(),
        "accountNames": app.pool.names(),
        "uptimeSecs": app.started.elapsed().as_secs(),
    }))
    .into_response()
}

async fn models(State(app): State<Shared>) -> Response {
    // 原生后端不依赖 CLI：没装 CLI 就给空清单，别把 /v1/models 变成 5xx
    let Some(cmd) = app.cmd.as_ref() else {
        return Json(json!({"object": "list", "data": []})).into_response();
    };
    // 先看缓存
    {
        let guard = app.models.lock().await;
        if let Some((at, list)) = guard.as_ref() {
            if at.elapsed() < MODELS_TTL {
                return Json(models_body(list)).into_response();
            }
        }
    }
    match crate::models::list(cmd).await {
        Ok(list) => {
            let mut guard = app.models.lock().await;
            *guard = Some((Instant::now(), list.clone()));
            Json(models_body(&list)).into_response()
        }
        Err(err) => {
            let guard = app.models.lock().await;
            if let Some((_, list)) = guard.as_ref() {
                // 问不到就用旧清单，别让客户端的模型列表突然空白
                return Json(models_body(list)).into_response();
            }
            error_response(&err, Dialect::OpenAi, StatusCode::BAD_GATEWAY)
        }
    }
}

fn models_body(list: &[ModelInfo]) -> Value {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let data: Vec<Value> = list
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "created": created,
                "owned_by": "cursor",
                "display_name": m.name,
            })
        })
        .collect();
    json!({"object": "list", "data": data})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Anthropic,
    OpenAi,
}

async fn messages(State(app): State<Shared>, Json(body): Json<Value>) -> Response {
    let req = match AnthropicRequest::from_value(body) {
        Ok(req) => req,
        Err(err) => {
            return error_response(&err, Dialect::Anthropic, StatusCode::BAD_REQUEST);
        }
    };
    let stream = req.stream.unwrap_or(false);
    let wants_thinking = req.wants_thinking();
    let model = req.model.clone();
    let unified = req.into_unified();
    run(
        &app,
        unified,
        stream,
        wants_thinking,
        model,
        Dialect::Anthropic,
        true,
    )
    .await
}

async fn chat_completions(State(app): State<Shared>, Json(body): Json<Value>) -> Response {
    let req = match OpenAiRequest::from_value(body) {
        Ok(req) => req,
        Err(err) => {
            return error_response(&err, Dialect::OpenAi, StatusCode::BAD_REQUEST);
        }
    };
    let stream = req.stream.unwrap_or(false);
    let wants_thinking = req.wants_thinking();
    let wants_usage = req.stream_options_wants_usage();
    let model = req.model.clone();
    let unified = req.into_unified();
    run(
        &app,
        unified,
        stream,
        wants_thinking,
        model,
        Dialect::OpenAi,
        wants_usage,
    )
    .await
}

/// 粗略估 token：字符数 / 4 再压一点。**不是精确值**，客户端拿它做预算参考可以，
/// 拿来对账不行 —— 真正的用量在响应里的 usage 字段（来自 CLI 的 result 事件）。
async fn count_tokens(Json(body): Json<Value>) -> Response {
    let req = match AnthropicRequest::from_value(body) {
        Ok(req) => req,
        Err(err) => {
            return error_response(&err, Dialect::Anthropic, StatusCode::BAD_REQUEST);
        }
    };
    let unified = req.into_unified();
    let chars: usize = unified.system.as_deref().map(str::len).unwrap_or(0)
        + unified.turns.iter().map(|t| t.text.len()).sum::<usize>();
    Json(json!({"input_tokens": chars / 4 + 8})).into_response()
}

/// 两个端点共用的主干。
async fn run(
    app: &Shared,
    unified: UnifiedRequest,
    stream: bool,
    wants_thinking: bool,
    requested_model: Option<String>,
    dialect: Dialect,
    wants_usage: bool,
) -> Response {
    let prompt = match build_prompt(&unified, HISTORY_BUDGET) {
        Ok(prompt) => prompt,
        Err(err) => return error_response(&err, dialect, StatusCode::BAD_REQUEST),
    };
    let account = app.pool.next().or_else(|| {
        app.cfg.single_api_key.clone().map(|api_key| Account {
            name: "env".into(),
            api_key,
        })
    });
    let turn_req = TurnRequest {
        prompt,
        model: pick_model(requested_model.as_deref()),
        mode: app.cfg.mode.clone(),
        workspace: app.cfg.workspace.clone(),
        trust: app.cfg.trust,
        api_key: account.as_ref().map(|a| a.api_key.clone()),
    };
    let started = Instant::now();
    let account_name = account
        .as_ref()
        .map(|a| a.name.clone())
        .unwrap_or_else(|| "cli-login".into());

    if !stream {
        let mut collected = Collected::default();
        let result = run_backend(app, &turn_req, |step| {
            collected.absorb(&step);
        })
        .await;
        match result {
            Ok(outcome) => {
                log_line(
                    app,
                    dialect,
                    &account_name,
                    &turn_req,
                    &outcome,
                    started,
                    None,
                );
                let model = requested_model.clone().or(outcome.model.clone());
                let body = match dialect {
                    Dialect::Anthropic => {
                        protocol::anthropic_message(&collected, model.as_deref(), wants_thinking)
                    }
                    Dialect::OpenAi => protocol::openai_completion(&collected, model.as_deref()),
                };
                Json(body).into_response()
            }
            Err(err) => {
                log_line(
                    app,
                    dialect,
                    &account_name,
                    &turn_req,
                    &TurnOutcome::default(),
                    started,
                    Some(&err.to_string()),
                );
                error_response(&err, dialect, status_for(&err))
            }
        }
    } else {
        let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(64);
        let keepalive_secs = app.cfg.keepalive_secs.max(1);
        let app_for_log = app.clone();
        let requested = requested_model.clone();

        let producer = tokio::spawn(async move {
            let mut tx = tx;
            // 心跳：CLI 一轮可能跑几十秒，中间不发东西有的客户端会判死
            let keepalive_tx = tx.clone();
            let keepalive = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(keepalive_secs));
                tick.tick().await;
                loop {
                    tick.tick().await;
                    if keepalive_tx
                        .send(Ok(Bytes::from(protocol::keepalive())))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });

            let mut closed = Arc::new(AtomicBool::new(false));
            let error_text;
            let outcome = match dialect {
                Dialect::Anthropic => {
                    let mut tr = AnthropicStream::new(requested.clone(), wants_thinking);
                    let closed_ref = closed.clone();
                    let tx_ref: &mut mpsc::Sender<std::result::Result<Bytes, std::io::Error>> =
                        &mut tx;
                    let result = run_backend(&app_for_log, &turn_req, |step| {
                        if closed_ref.load(Ordering::Relaxed) {
                            return;
                        }
                        for frame in tr.on_step(&step) {
                            if tx_ref.try_send(Ok(Bytes::from(frame))).is_err() {
                                // 客户端走了：收工的活交给 run_turn 的 kill_on_drop
                                closed_ref.store(true, Ordering::Relaxed);
                            }
                        }
                    })
                    .await;
                    error_text = result.as_ref().err().map(|e| e.to_string());
                    let frames = tr.finish(error_text.as_deref());
                    for frame in frames {
                        if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                            break;
                        }
                    }
                    result.ok()
                }
                Dialect::OpenAi => {
                    let mut tr = OpenAiStream::new(requested.clone(), wants_usage);
                    let closed_ref = closed.clone();
                    let tx_ref: &mut mpsc::Sender<std::result::Result<Bytes, std::io::Error>> =
                        &mut tx;
                    let result = run_backend(&app_for_log, &turn_req, |step| {
                        if closed_ref.load(Ordering::Relaxed) {
                            return;
                        }
                        for frame in tr.on_step(&step) {
                            if tx_ref.try_send(Ok(Bytes::from(frame))).is_err() {
                                closed_ref.store(true, Ordering::Relaxed);
                            }
                        }
                    })
                    .await;
                    error_text = result.as_ref().err().map(|e| e.to_string());
                    let frames = tr.finish(error_text.as_deref());
                    for frame in frames {
                        if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                            break;
                        }
                    }
                    result.ok()
                }
            };
            keepalive.abort();
            let _ = &mut closed;

            let logged = outcome.clone().unwrap_or_default();
            log_line(
                &app_for_log,
                dialect,
                &account_name,
                &turn_req,
                &logged,
                started,
                error_text.as_deref(),
            );
            drop(tx);
        });

        let body = Body::from_stream(stream_of(rx, producer));
        let content_type = match dialect {
            Dialect::Anthropic | Dialect::OpenAi => "text/event-stream; charset=utf-8",
        };
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no")
            .body(body)
            .unwrap_or_else(|_| Response::new(Body::empty()))
    }
}

/// 后端分发：原生（不拉进程）或 CLI。两条路都吐同一套 [`Step`]，上层不分叉。
async fn run_backend<F>(app: &Shared, req: &TurnRequest, on_step: F) -> Result<TurnOutcome>
where
    F: FnMut(Step) + Send,
{
    match &app.native {
        Some(native) => native.run_turn(req, app.cfg.timeout_secs, on_step).await,
        None => match app.cmd.as_ref() {
            Some(cmd) => turn::run_turn(cmd, req, app.cfg.timeout_secs, on_step).await,
            None => Err(Error::AgentNotFound { tried: Vec::new() }),
        },
    }
}

/// 把接收端变成 body 流，并保证流一被丢掉（客户端断开）就掐掉生产任务。
fn stream_of(
    rx: mpsc::Receiver<std::result::Result<Bytes, std::io::Error>>,
    producer: tokio::task::JoinHandle<()>,
) -> impl futures_util::Stream<Item = std::result::Result<Bytes, std::io::Error>> {
    struct Guard(tokio::task::JoinHandle<()>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    stream::unfold((rx, Guard(producer)), |(mut rx, guard)| async move {
        let item = rx.recv().await?;
        Some((item, (rx, guard)))
    })
}
fn pick_model(requested: Option<&str>) -> Option<String> {
    let model = requested?.trim();
    if model.is_empty() {
        return None;
    }
    // 客户端如果想标明「这个模型是走 cursor 桥的」，允许 cursor/<id> 这种写法
    let model = model.strip_prefix("cursor/").unwrap_or(model).trim();
    if model.is_empty()
        || model.eq_ignore_ascii_case("auto")
        || model.eq_ignore_ascii_case("default")
    {
        return None;
    }
    Some(model.to_string())
}

fn status_for(err: &Error) -> StatusCode {
    match err {
        Error::EmptyPrompt | Error::Json(_) => StatusCode::BAD_REQUEST,
        Error::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
        Error::AgentNotFound { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        Error::Spawn(_) | Error::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        Error::EmptyTurn { .. } | Error::Upstream { .. } | Error::Native { .. } => {
            StatusCode::BAD_GATEWAY
        }
        // 请求路径上不会出现（那是 service 子命令的事）
        Error::Service { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_response(err: &Error, dialect: Dialect, status: StatusCode) -> Response {
    let message = err.to_string();
    let body = match dialect {
        Dialect::Anthropic => json!({
            "type": "error",
            "error": {"type": "api_error", "message": message},
        }),
        Dialect::OpenAi => json!({
            "error": {"message": message, "type": "api_error", "code": null},
        }),
    };
    (status, Json(body)).into_response()
}

/// 一个请求一行日志。key 只留指纹，正文长度只记数字。
fn log_line(
    app: &App,
    dialect: Dialect,
    account: &str,
    req: &TurnRequest,
    outcome: &TurnOutcome,
    started: Instant,
    error: Option<&str>,
) {
    let dialect = match dialect {
        Dialect::Anthropic => "anthropic",
        Dialect::OpenAi => "openai",
    };
    let model = req.model.as_deref().unwrap_or("auto");
    let ms = started.elapsed().as_millis();
    let where_ = match (app.native.is_some(), app.cfg.mode.as_deref()) {
        (true, _) => "native",
        (false, Some("ask")) => "ask",
        _ => "cli",
    };
    match error {
        Some(err) => eprintln!(
            "[{dialect}] {where_} model={model} account={account} {ms}ms 失败：{err}"
        ),
        None => eprintln!(
            "[{dialect}] {where_} model={model} account={account} {ms}ms ok 正文 {} 字 / 思考 {} 字 in={} out={} cache={}+{}",
            outcome.text.chars().count(),
            outcome.thinking.chars().count(),
            outcome.usage.input_tokens,
            outcome.usage.output_tokens,
            outcome.usage.cache_read_tokens,
            outcome.usage.cache_write_tokens,
        ),
    }
    let _ = Mutex::new(());
    if !outcome.notes.is_empty() {
        for note in &outcome.notes {
            eprintln!("  备注：{note}");
        }
    }
}
