//! 真上游自测（Rust 版，逐项移植自 `src/bridge/smoke.mjs`）。
//!
//! 和 JS 版一样：用 Anthropic / OpenAI 协议打几发，把「服务层 + 上游层」整条链路在真上游上过一遍。
//! 默认干跑，加 `--run` 才真发请求。
//!
//! ```text
//! smoke                                          # 干跑：只打印计划（不发任何请求）
//! smoke --run                                    # 真发请求（会消耗上游额度）
//! smoke --run --only=tool                        # 只跑工具回环
//! smoke --run --base=http://127.0.0.1:8051        # 只当客户端，打已经在跑的那座桥
//! ```
//!
//! 两种形态：不给 `--base` 就在**本进程里**起一座临时桥（随机空闲端口，绝不碰 8050 上那座常驻桥，
//! 跑完主动关掉）；给了 `--base` 就当纯客户端 —— 对面可以是桌面壳或 CLI。
//!
//! 无论哪种形态，计数器 / 签名统计 / 上游身份都从 `/healthz` 读（和面板同源）。
//! JS 版在「自己起的桥」时直接读内存里的 `bridge.counters`，Rust 这边统一走 `/healthz`：
//! 这样「自己起的桥」和「别人跑的桥」走的是同一条判定路径，也才好对这条读法写单测。
//!
//! 检查项（和 JS 版对齐，另加一项模型表）：
//!   1. basic          Anthropic 流式一发：message_start / 正文 / usage / stop_reason。
//!   2. tool-round-1   强制模型调用工具，拿到真签名，并记下 tool_use.id。
//!   3. tool-round-2   把 tool_use 与 tool_result 回传，看上游收不收；签名命中数从 /healthz 读增量。
//!   4. openai-stream  OpenAI 流式一发（data: 帧 + [DONE]）。
//!   5. openai-whole   OpenAI 整包一发（chat.completion 形状）。
//!   6. models         /v1/models 模型表（JS 版只在报告里读它的 resolvedModel，这里也单独当一项）。
//!
//! 退出码：全过 0、有失败 1、参数不对 2。

use std::sync::Arc;

use dango_bridge::runtime;
use dango_bridge::server::{Bridge, BridgeOptions, LogFn};
use dango_bridge::signatures::shared_signatures;
use dango_bridge::types::{iso_from_millis, now_millis};
use serde_json::{json, Value};

/// 和 JS 版同一个默认模型（换模型走 `--model=`）。
const DEFAULT_MODEL: &str = "gemini-3.8-flash-high";

// ---------------------------------------------------------------- 参数

struct Args {
    run: bool,
    /// `None` = 全部跑；`Some` = 只跑列出来的组（basic / tool / openai / models）
    only: Option<Vec<String>>,
    model: String,
    base: Option<String>,
    help: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            run: false,
            only: None,
            model: DEFAULT_MODEL.to_string(),
            base: None,
            help: false,
        }
    }
}

/// 和 JS 版同一套参数。不认识的参数**忽略**（JS 版就是忽略：脚本里多传一个参数不该让自测起不来）。
fn parse_args(argv: Vec<String>) -> Args {
    let mut args = Args::default();
    for item in argv {
        if item == "--run" {
            args.run = true;
        } else if item == "--help" || item == "-h" {
            args.help = true;
        } else if let Some(value) = item.strip_prefix("--only=") {
            let groups: Vec<String> = value
                .split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect();
            args.only = Some(groups);
        } else if let Some(value) = item.strip_prefix("--model=") {
            args.model = value.to_string();
        } else if let Some(value) = item.strip_prefix("--base=") {
            args.base = Some(value.to_string());
        }
    }
    args
}

fn wants(only: &Option<Vec<String>>, group: &str) -> bool {
    match only {
        None => true,
        Some(groups) => groups.iter().any(|g| g == group),
    }
}

fn print_plan(args: &Args) {
    println!("== 干跑（不会发任何请求）==\n");
    println!("会做的事：");
    println!("  1. 起一个本机临时桥（127.0.0.1 随机端口，绝不碰 8050）");
    println!("     ——带了 --base=<url> 就不起桥，只打那座已经在跑的桥");
    println!(
        "  2. basic ：Anthropic 流式一发（system + 一句短文），模型名 {}",
        args.model
    );
    println!("  3. tool  ：第一发强制工具调用 → 第二发回传 tool_result（验证签名回传）");
    println!("  4. openai：OpenAI 侧整包 + 流式各一发");
    println!("  5. models：读一遍 /v1/models 模型表");
    println!(
        "\n要真跑：smoke --run [--base=http://127.0.0.1:8051] [--only=basic,tool,openai,models]"
    );
}

// ---------------------------------------------------------------- 入口

#[tokio::main]
async fn main() {
    let args = parse_args(std::env::args().skip(1).collect());
    if args.help || !args.run {
        print_plan(&args);
        return;
    }

    let log: LogFn = Arc::new(|message: String| eprintln!("{message}"));
    let started = now_millis();

    // 不给 --base 就自己起一座临时桥；给了就当纯客户端先问一句 /version（别把别的服务当桥打）。
    let mut owned: Option<TempBridge> = None;
    let base = match &args.base {
        Some(raw) => raw.trim_end_matches('/').to_string(),
        None => match start_temp_bridge(&log).await {
            Ok((base, handle)) => {
                owned = Some(handle);
                base
            }
            Err(err) => {
                log_line(&log, format!("起临时桥失败：{err}"));
                std::process::exit(1);
            }
        },
    };
    let client = Client::new(base.clone());

    if args.base.is_some() {
        if !verify_bridge(&client, &log).await {
            std::process::exit(1);
        }
    } else {
        log_line(
            &log,
            format!("临时桥起在 {base}（进程 {}）", std::process::id()),
        );
    }

    // ---------------------------------------------------------------- 跑检查项

    // 整套检查抽成 run_checks：这样单测能直接拿一座假桥把「--base 形态」的客户端路径整条跑一遍。
    let steps = run_checks(&client, &args.model, &args.only, &log).await;

    // ---------------------------------------------------------------- 报告 + 汇总

    let mut report = json!({
        "started_at": iso_from_millis(started),
        "model": args.model,
        "base": base,
        "own_bridge": args.base.is_none(),
        "steps": steps.iter().map(step_to_json).collect::<Vec<_>>(),
    });
    // 计数器、签名统计、上游身份都从 /healthz 读（和面板同源）。
    // 注意：Rust 的 Upstream 把 access_token / refresh_token 设成私有字段，JS 版那段
    // 「报告里不许出现 token 原文」的兜底断言没法照搬；这里只序列化 /healthz 给的
    // 掩码身份与计数，本来就不含凭据。
    match client.health().await {
        Ok(info) => {
            report["counters"] = info.get("counters").cloned().unwrap_or(Value::Null);
            report["signatures"] = info.get("signatures").cloned().unwrap_or(Value::Null);
            report["upstream"] = info.get("upstream").cloned().unwrap_or(Value::Null);
        }
        Err(err) => {
            report["health_error"] = json!(err);
        }
    }
    if let Ok(resolved) = client.resolve_model(&args.model).await {
        report["resolvedModel"] = resolved;
    }
    let report_path = write_report(&report);

    let failed = steps.iter().filter(|step| !step.ok).count();
    println!("\n== 汇总 ==");
    for step in &steps {
        if step.ok {
            println!("  ✔ {} {}ms", step.name, step.ms);
        } else {
            println!(
                "  ✘ {} {}",
                step.name,
                step.error.clone().unwrap_or_default()
            );
        }
    }
    let email = report
        .pointer("/upstream/email")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let project = report
        .pointer("/upstream/project")
        .and_then(Value::as_str)
        .unwrap_or("-");
    println!("  上游：{email} / project {project}");
    println!(
        "  计数：{}",
        report.get("counters").cloned().unwrap_or(Value::Null)
    );
    match &report_path {
        Some(path) => println!("  报告：{}", path.display()),
        None => println!("  报告：<写盘失败>"),
    }

    // 跑完关掉自己起的临时桥（别人跑的桥不动）。
    if let Some(handle) = owned.as_mut() {
        handle.shutdown();
    }

    std::process::exit(if failed > 0 { 1 } else { 0 });
}

/// 按 `--only` 跑检查项，收集每一步的结果。抽成函数是为了单测能直接驱动整条客户端路径。
async fn run_checks(
    client: &Client,
    model: &str,
    only: &Option<Vec<String>>,
    log: &LogFn,
) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();

    if wants(only, "basic") {
        let t0 = now_millis();
        let result = check_basic(client, model).await;
        record("basic", result, now_millis() - t0, &mut steps, log);
    }

    if wants(only, "tool") {
        let t0 = now_millis();
        match check_tool_round_1(client, model).await {
            Ok((detail, handoff)) => {
                record(
                    "tool-round-1",
                    Ok(detail),
                    now_millis() - t0,
                    &mut steps,
                    log,
                );
                let t0 = now_millis();
                let result = check_tool_round_2(client, model, &handoff).await;
                record("tool-round-2", result, now_millis() - t0, &mut steps, log);
            }
            Err(err) => {
                record("tool-round-1", Err(err), now_millis() - t0, &mut steps, log);
                // 和 JS 版一致：第一发没拿到 tool_use，第二发没有 id 可用，直接记「跳过」。
                record(
                    "tool-round-2",
                    Err("第一发没有拿到 tool_use，跳过".to_string()),
                    0,
                    &mut steps,
                    log,
                );
            }
        }
    }

    if wants(only, "openai") {
        let t0 = now_millis();
        let result = check_openai_stream(client, model).await;
        record("openai-stream", result, now_millis() - t0, &mut steps, log);

        let t0 = now_millis();
        let result = check_openai_whole(client, model).await;
        record("openai-whole", result, now_millis() - t0, &mut steps, log);
    }

    if wants(only, "models") {
        let t0 = now_millis();
        let result = check_models(client).await;
        record("models", result, now_millis() - t0, &mut steps, log);
    }

    steps
}

/// 临时桥 + 一个能主动关掉的开关（`serve()` 没有返回把手，这里自己 serve）。
struct TempBridge {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TempBridge {
    fn shutdown(&mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
    }
}

/// 在本进程里起一座临时桥：随机空闲端口（端口 0）、只绑 127.0.0.1、不留盘日志。
async fn start_temp_bridge(log: &LogFn) -> Result<(String, TempBridge), String> {
    let setup = runtime::build_pool(None, None, Arc::clone(log)).await?;
    log_line(log, setup.summary);
    let bridge = Arc::new(Bridge::new(BridgeOptions {
        pool: setup.pool,
        qoder: None,
        store: shared_signatures(),
        // 临时桥跑完就关，写日志没有收益；也免得和常驻桥抢同一个 logs/。
        log_dir: None,
        // 临时桥不落盘任何状态（禁用名单只在内存里），随进程一起没。
        state_dir: None,
        log_bodies: true,
        body_limit: 0,
        api_key: None,
        // 临时桥不在 launchd 下：面板的重启按钮该被拒（别把它自己弄没）。
        allow_restart: Some(false),
        log: Arc::clone(log),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }));
    // 关键：端口给 0 拿一个系统分配的空闲端口 —— 绝不碰 8050 上那座常驻桥。
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|err| format!("绑端口失败：{err}"))?;
    let addr = listener
        .local_addr()
        .map_err(|err| format!("取端口失败：{err}"))?;
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    let app = bridge.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = receiver.await;
            })
            .await;
    });
    Ok((
        format!("http://{addr}"),
        TempBridge {
            shutdown: Some(sender),
        },
    ))
}

/// 先问一句 `/version`：对面得像个桥，否则别把它当成桥打。
async fn verify_bridge(client: &Client, log: &LogFn) -> bool {
    match client.get("/version").await {
        Ok((200, body)) => {
            let info: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let text = format!(
                "只当客户端：打 {}（{} {} {}）",
                client.base,
                info.get("name").and_then(Value::as_str).unwrap_or("?"),
                info.get("version").and_then(Value::as_str).unwrap_or(""),
                info.get("runtime").and_then(Value::as_str).unwrap_or(""),
            );
            log_line(log, text.trim_end().to_string());
            true
        }
        Ok((status, _)) => {
            log_line(
                log,
                format!("{}/version 回了 HTTP {status}，不像是桥", client.base),
            );
            false
        }
        Err(err) => {
            log_line(log, format!("{}/version 打不通：{err}", client.base));
            false
        }
    }
}

// ---------------------------------------------------------------- 客户端

struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    fn new(base: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base,
        }
    }

    async fn post(&self, path: &str, body: Value) -> Result<(u16, String), String> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("请求发不出去：{err}"))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|err| format!("读响应失败：{err}"))?;
        Ok((status, text))
    }

    async fn get(&self, path: &str) -> Result<(u16, String), String> {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .map_err(|err| format!("请求发不出去：{err}"))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|err| format!("读响应失败：{err}"))?;
        Ok((status, text))
    }

    async fn health(&self) -> Result<Value, String> {
        let (status, body) = self.get("/healthz").await?;
        if status != 200 {
            return Err(format!("/healthz 回了 HTTP {status}"));
        }
        serde_json::from_str(&body).map_err(|err| format!("/healthz 不是合法 JSON：{err}"))
    }

    /// 签名命中数：从 `/healthz` 的 counters 读（不是从某个进程内的桥对象读）。
    async fn signature_hits(&self) -> Result<u64, String> {
        let info = self.health().await?;
        Ok(info
            .get("counters")
            .and_then(|counters| counters.get("toolSignatureHits"))
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }

    /// 报告用：把请求的模型名解析成上游真正会用的那个（不是断言，拿不到就算了）。
    async fn resolve_model(&self, model: &str) -> Result<Value, String> {
        let (status, body) = self
            .get(&format!("/v1/models/{}", percent_encode(model)))
            .await?;
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }
        serde_json::from_str(&body).map_err(|err| err.to_string())
    }
}

// ---------------------------------------------------------------- 检查项（I/O 包装）

async fn check_basic(client: &Client, model: &str) -> Result<Value, String> {
    let (status, text) = client
        .post(
            "/v1/messages",
            json!({
                "model": model,
                // 512 不是为了要长回答，是给「思考」留地方：思考也吃 max_tokens。
                // JS 版踩过这个坑——给 128 会被思考吃干净（stop_reason=max_tokens、
                // 正文一个 token 都没有），那种情况下断言「正文非空」会误报，桥本身没问题。
                "max_tokens": 512,
                "stream": true,
                "system": "你是一个只会说短句的助手。",
                "messages": [{ "role": "user", "content": "用五个字以内回答：今天天气怎么样" }],
            }),
        )
        .await?;
    judge_basic(status, &text)
}

async fn check_tool_round_1(client: &Client, model: &str) -> Result<(Value, ToolHandoff), String> {
    let (status, text) = client
        .post(
            "/v1/messages",
            json!({
                "model": model,
                "max_tokens": 512,
                "stream": true,
                "tools": [get_time_tool()],
                "tool_choice": { "type": "any" },
                "messages": [{ "role": "user", "content": "用 get_time 查一下 Asia/Shanghai 现在几点。只调用工具。" }],
            }),
        )
        .await?;
    judge_tool_round_1(status, &text)
}

async fn check_tool_round_2(
    client: &Client,
    model: &str,
    handoff: &ToolHandoff,
) -> Result<Value, String> {
    // 命中的「前后」都从 /healthz 读，差值就是这一发造成的命中数。
    let before = client.signature_hits().await?;
    let (status, text) = client
        .post(
            "/v1/messages",
            json!({
                "model": model,
                "max_tokens": 256,
                "stream": true,
                "tools": [get_time_tool()],
                "messages": [
                    { "role": "user", "content": "用 get_time 查一下 Asia/Shanghai 现在几点。只调用工具。" },
                    {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": handoff.tool_id.as_str(),
                            "name": handoff.name.as_str(),
                            "input": handoff.args,
                        }],
                    },
                    {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": handoff.tool_id.as_str(),
                            "content": "2026-09-18 14:05（北京时间）",
                        }],
                    },
                ],
            }),
        )
        .await?;
    let after = client.signature_hits().await?;
    judge_tool_round_2(status, &text, before, after)
}

async fn check_openai_stream(client: &Client, model: &str) -> Result<Value, String> {
    let (status, text) = client
        .post(
            "/v1/chat/completions",
            json!({
                "model": model,
                "stream": true,
                "stream_options": { "include_usage": true },
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "用两个字回答：在吗" }],
            }),
        )
        .await?;
    judge_openai_stream(status, &text)
}

async fn check_openai_whole(client: &Client, model: &str) -> Result<Value, String> {
    let (status, text) = client
        .post(
            "/v1/chat/completions",
            json!({
                "model": model,
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "用两个字回答：在吗" }],
            }),
        )
        .await?;
    judge_openai_whole(status, &text)
}

async fn check_models(client: &Client) -> Result<Value, String> {
    let (status, text) = client.get("/v1/models").await?;
    judge_models(status, &text)
}

/// 工具调用第一发交出来的东西：第二发要把 `tool_use` 原样贴回去，签名才可能命中缓存。
struct ToolHandoff {
    tool_id: String,
    name: String,
    args: Value,
}

fn get_time_tool() -> Value {
    json!({
        "name": "get_time",
        "description": "返回指定时区的当前时间",
        "input_schema": {
            "type": "object",
            "properties": { "tz": { "type": "string" } },
            "required": ["tz"],
        },
    })
}

// ---------------------------------------------------------------- 判定逻辑（纯函数，便于单测）

/// 解析 Anthropic 的 SSE 帧（`event: x` + 紧跟的 `data: {...}`），和 JS 版 parseAnthropicSse 同一套。
fn parse_anthropic_sse(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter(|frame| frame.starts_with("event: "))
        .map(|frame| {
            let mut event = String::new();
            let mut data = Value::Null;
            for line in frame.lines() {
                if let Some(name) = line.strip_prefix("event: ") {
                    event = name.to_string();
                } else if let Some(payload) = line.strip_prefix("data: ") {
                    data = serde_json::from_str(payload).unwrap_or(Value::Null);
                }
            }
            (event, data)
        })
        .collect()
}

fn has_event(events: &[(String, Value)], name: &str) -> bool {
    events.iter().any(|(event, _)| event == name)
}

fn find_event<'a>(events: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
    events
        .iter()
        .find(|(event, _)| event == name)
        .map(|(_, data)| data)
}

/// 把 text_delta 拼起来（和 JS 版 anthropicText 一致）。
fn anthropic_text(events: &[(String, Value)]) -> String {
    events
        .iter()
        .filter_map(|(_, data)| {
            if data.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta") {
                data.pointer("/delta/text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect()
}

/// 所有 `tool_use` 的 content_block（和 JS 版 toolUses 一致）。
fn tool_uses(events: &[(String, Value)]) -> Vec<Value> {
    events
        .iter()
        .filter(|(event, data)| {
            event == "content_block_start"
                && data.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use")
        })
        .map(|(_, data)| data.get("content_block").cloned().unwrap_or(Value::Null))
        .collect()
}

/// 把 `input_json_delta` 的分片拼起来再解析。JS 版只取第一片，这里拼全更稳（单片行为一致）。
fn tool_input(events: &[(String, Value)]) -> Option<Value> {
    let mut raw = String::new();
    for (_, data) in events {
        if data.pointer("/delta/type").and_then(Value::as_str) == Some("input_json_delta") {
            if let Some(part) = data.pointer("/delta/partial_json").and_then(Value::as_str) {
                raw.push_str(part);
            }
        }
    }
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str(&raw).ok()
}

fn judge_basic(status: u16, text: &str) -> Result<Value, String> {
    if status != 200 {
        return Err(format!("HTTP {status}：{}", truncate(text, 300)));
    }
    let events = parse_anthropic_sse(text);
    // JS 版只看正文非空；Rust 版顺手把 SSE 事件序列也钉死——面板和日志都靠这几个事件，
    // 缺了说明桥的流式翻译坏了，比「正文为空」更早暴露问题。
    if !has_event(&events, "message_start") {
        return Err("SSE 里没有 message_start 事件".to_string());
    }
    if !has_event(&events, "message_delta") {
        return Err("SSE 里没有 message_delta 事件".to_string());
    }
    let body = anthropic_text(&events);
    if body.is_empty() {
        let delta = find_event(&events, "message_delta");
        let stop = delta
            .and_then(|d| d.pointer("/delta/stop_reason"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let spent = delta
            .and_then(|d| d.pointer("/usage/thoughts_token_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let hint = if stop == "max_tokens" {
            " —— 预算被思考吃完了，把 max_tokens 调大"
        } else {
            ""
        };
        return Err(format!(
            "正文是空的（stop_reason={stop}，思考用了 {spent} tokens{hint}）"
        ));
    }
    let start = find_event(&events, "message_start");
    let delta = find_event(&events, "message_delta");
    Ok(json!({
        "model": start.and_then(|d| d.pointer("/message/model")).cloned().unwrap_or(Value::Null),
        "text": truncate(&body, 200),
        "stopReason": delta.and_then(|d| d.pointer("/delta/stop_reason")).cloned().unwrap_or(Value::Null),
        "usage": delta.and_then(|d| d.get("usage")).cloned().unwrap_or(Value::Null),
        "eventCount": events.len(),
    }))
}

fn judge_tool_round_1(status: u16, text: &str) -> Result<(Value, ToolHandoff), String> {
    if status != 200 {
        return Err(format!("HTTP {status}：{}", truncate(text, 300)));
    }
    let events = parse_anthropic_sse(text);
    let uses = tool_uses(&events);
    if uses.is_empty() {
        let stop = find_event(&events, "message_delta")
            .and_then(|d| d.pointer("/delta/stop_reason"))
            .cloned()
            .unwrap_or(Value::Null);
        return Err(format!("模型没有调用工具（stop_reason={stop}）"));
    }
    let first = &uses[0];
    let tool_id = first
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = first
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let args = tool_input(&events).unwrap_or(Value::Null);
    let calls: Vec<Value> = uses
        .iter()
        .map(|call| call.get("name").cloned().unwrap_or(Value::Null))
        .collect();
    let stop = find_event(&events, "message_delta")
        .and_then(|d| d.pointer("/delta/stop_reason"))
        .cloned()
        .unwrap_or(Value::Null);
    let handoff = ToolHandoff {
        tool_id,
        name,
        args: args.clone(),
    };
    let detail = json!({
        "calls": calls,
        "toolId": handoff.tool_id.clone(),
        "args": handoff.args.clone(),
        "stopReason": stop,
    });
    Ok((detail, handoff))
}

fn judge_tool_round_2(
    status: u16,
    text: &str,
    hits_before: u64,
    hits_after: u64,
) -> Result<Value, String> {
    if status != 200 {
        return Err(format!(
            "HTTP {status}：{}（签名回传被上游拒了？）",
            truncate(text, 300)
        ));
    }
    let events = parse_anthropic_sse(text);
    let body = anthropic_text(&events);
    // 命中数是 /healthz 计数器的增量；没命中说明回传时贴的不是真签名。
    let hits = hits_after.saturating_sub(hits_before);
    if hits < 1 {
        return Err("签名没有命中缓存 —— 回传时贴的不是真签名".to_string());
    }
    if body.is_empty() {
        return Err("第二发没有正文".to_string());
    }
    Ok(json!({
        "text": truncate(&body, 200),
        "signatureHits": hits,
    }))
}

fn judge_openai_stream(status: u16, text: &str) -> Result<Value, String> {
    if status != 200 {
        return Err(format!("HTTP {status}：{}", truncate(text, 300)));
    }
    if !text.trim_end().ends_with("data: [DONE]") {
        return Err("流没有以 [DONE] 结束".to_string());
    }
    let payloads: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| !line.contains("[DONE]"))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let body: String = payloads
        .iter()
        .flat_map(|payload| {
            payload
                .get("choices")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|choice| {
            choice
                .pointer("/delta/content")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    if body.is_empty() {
        return Err("正文是空的".to_string());
    }
    Ok(json!({
        "text": truncate(&body, 200),
        "chunks": payloads.len(),
        "usage": payloads.last().and_then(|p| p.get("usage")).cloned().unwrap_or(Value::Null),
    }))
}

fn judge_openai_whole(status: u16, text: &str) -> Result<Value, String> {
    if status != 200 {
        return Err(format!("HTTP {status}：{}", truncate(text, 300)));
    }
    let message: Value =
        serde_json::from_str(text).map_err(|err| format!("响应不是合法 JSON：{err}"))?;
    if message.get("object").and_then(Value::as_str) != Some("chat.completion") {
        return Err(format!(
            "object={}",
            message.get("object").unwrap_or(&Value::Null)
        ));
    }
    let body = message
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(|value| truncate(value, 200))
        .unwrap_or_default();
    Ok(json!({
        "text": body,
        "finish": message.pointer("/choices/0/finish_reason").cloned().unwrap_or(Value::Null),
        "usage": message.get("usage").cloned().unwrap_or(Value::Null),
    }))
}

fn judge_models(status: u16, text: &str) -> Result<Value, String> {
    if status != 200 {
        return Err(format!("HTTP {status}：{}", truncate(text, 300)));
    }
    let table: Value =
        serde_json::from_str(text).map_err(|err| format!("响应不是合法 JSON：{err}"))?;
    if table.get("object").and_then(Value::as_str) != Some("list") {
        return Err(format!(
            "object={}",
            table.get("object").unwrap_or(&Value::Null)
        ));
    }
    let data = table
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if data.is_empty() {
        return Err("模型表是空的（上游没给出任何模型？）".to_string());
    }
    Ok(json!({
        "count": data.len(),
        "defaultModel": table.get("default_model").cloned().unwrap_or(Value::Null),
        "first": data[0].get("id").cloned().unwrap_or(Value::Null),
    }))
}

// ---------------------------------------------------------------- 结果 / 报告 / 小工具

struct Step {
    name: String,
    ok: bool,
    ms: i64,
    detail: Option<Value>,
    error: Option<String>,
}

fn record(name: &str, result: Result<Value, String>, ms: i64, out: &mut Vec<Step>, log: &LogFn) {
    match result {
        Ok(detail) => {
            log_line(log, format!("✔ {name}（{ms}ms）"));
            out.push(Step {
                name: name.to_string(),
                ok: true,
                ms,
                detail: Some(detail),
                error: None,
            });
        }
        Err(err) => {
            log_line(log, format!("✘ {name}：{}", truncate(&err, 300)));
            out.push(Step {
                name: name.to_string(),
                ok: false,
                ms,
                detail: None,
                error: Some(err),
            });
        }
    }
}

/// 把一步的结果摊平成 `{ name, ok, ms, ...detail }`，和 JS 报告里的 `{ name, ...results[name] }` 同形。
fn step_to_json(step: &Step) -> Value {
    let mut object = match step.detail.as_ref() {
        Some(Value::Object(map)) => map.clone(),
        Some(other) => {
            let mut map = serde_json::Map::new();
            map.insert("detail".to_string(), other.clone());
            map
        }
        None => serde_json::Map::new(),
    };
    object.insert("name".to_string(), json!(step.name));
    object.insert("ok".to_string(), json!(step.ok));
    object.insert("ms".to_string(), json!(step.ms));
    if let Some(error) = &step.error {
        object.insert("error".to_string(), json!(error));
    }
    Value::Object(object)
}

/// 报告落 `<当前目录>/research/smoke-<时间戳>.json`（和 JS 版同一个目录习惯）。
fn write_report(report: &Value) -> Option<std::path::PathBuf> {
    let dir = std::env::current_dir().ok()?.join("research");
    std::fs::create_dir_all(&dir).ok()?;
    // ISO 里的 `:` `.` 换成 `-`，做成能和 JS 版对上的文件名（`2026-09-18T14-05-00Z`）。
    let stamp = iso_from_millis(now_millis()).replace([':', '.'], "-");
    let path = dir.join(format!("smoke-{stamp}.json"));
    let text = serde_json::to_string_pretty(report).ok()?;
    std::fs::write(&path, text).ok()?;
    Some(path)
}

fn log_line(log: &LogFn, message: impl Into<String>) {
    log(format!("[{}] {}", clock(), message.into()));
}

/// `HH:MM:SS`（日志前缀，和 JS 的 `toISOString().slice(11,19)` 对齐）。
fn clock() -> String {
    let seconds = (now_millis() / 1000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// 只编码模型名里可能出现的保留字符（模型名基本是字母数字加 `-` `.` `_`）。
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let ch = byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------- 单测（不碰真上游）

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use dango_bridge::accounts::SingleAccountPool;
    use dango_bridge::server::Pool;
    use dango_bridge::types::{
        GenerateRequest, QuotaSnapshot, ResolvedModel, Session, StreamEvent, UpstreamError,
    };
    use tokio::sync::mpsc::Sender;

    /// 造一个上游 chunk（v1internal 形状），和 `core/tests/e2e.rs` 的假上游同一招。
    fn chunk(parts: Value, finish: Option<&str>) -> Value {
        let mut candidate = json!({ "content": { "role": "model", "parts": parts } });
        if let Some(reason) = finish {
            candidate["finishReason"] = json!(reason);
        }
        json!({
            "response": {
                "candidates": [candidate],
                "usageMetadata": { "promptTokenCount": 11, "candidatesTokenCount": 2 },
            },
            "traceId": "trace-smoke",
        })
    }

    /// 假会话：按请求内容决定吐什么脚本，覆盖 basic / 工具两回合 / openai。
    /// 全程离线，绝不碰真上游；`seen` 记下收到的请求，便于需要时排查。
    struct FakeSession {
        seen: Arc<Mutex<Vec<GenerateRequest>>>,
    }

    #[async_trait]
    impl Session for FakeSession {
        fn identity(&self) -> Value {
            json!({ "email": "te***@gmail.com", "project": "proj-smoke", "tier": "free-tier" })
        }

        async fn load_code_assist(&self) -> Result<Value, UpstreamError> {
            Ok(Value::Null)
        }

        async fn models(&self) -> Result<Value, UpstreamError> {
            Ok(json!({
                "models": { "gemini-3.8-flash-high": { "displayName": "Flash" } },
                "defaultAgentModelId": "gemini-3.8-flash-high",
            }))
        }

        async fn quota(&self) -> Result<QuotaSnapshot, UpstreamError> {
            Ok(QuotaSnapshot::default())
        }

        async fn resolve_model(&self, requested: &str) -> ResolvedModel {
            ResolvedModel {
                model: if requested.is_empty() {
                    "gemini-3.8-flash-high".to_string()
                } else {
                    requested.to_string()
                },
                substituted_from: None,
                reason: "exact".to_string(),
            }
        }

        async fn stream_generate(
            &self,
            req: GenerateRequest,
            tx: Sender<StreamEvent>,
        ) -> Result<(), UpstreamError> {
            self.seen.lock().unwrap().push(req.clone());
            let flat = req.request.to_string();
            // 第二回合带回了 functionResponse → 给正文；第一回合声明了 get_time → 给带签名的工具调用；
            // 其余（basic / openai）→ 给正文。
            let script: Vec<Value> = if flat.contains("functionResponse") {
                vec![chunk(json!([{ "text": "现在 15:20" }]), Some("STOP"))]
            } else if flat.contains("get_time") {
                vec![chunk(
                    json!([{
                        "thoughtSignature": "SIG-SMOKE",
                        "functionCall": { "name": "get_time", "args": { "tz": "Asia/Shanghai" } },
                    }]),
                    Some("STOP"),
                )]
            } else {
                vec![chunk(json!([{ "text": "晴" }]), Some("STOP"))]
            };
            let endpoint = "https://example.invalid/v1internal".to_string();
            let _ = tx.send(StreamEvent::Open { endpoint }).await;
            for item in &script {
                if tx.send(StreamEvent::Chunk(item.clone())).await.is_err() {
                    break;
                }
            }
            Ok(())
        }
    }

    /// 起一座本地假桥（真 HTTP、真 SSE，上游是假的），随机端口。
    async fn start_fake() -> String {
        let session: Arc<dyn Session> = Arc::new(FakeSession {
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let bridge = Arc::new(Bridge::new(BridgeOptions {
            pool: Pool::Single(Arc::new(SingleAccountPool::new(session))),
            qoder: None,
            store: shared_signatures(),
            log_dir: None,
            // 测试桥也不落盘任何状态。
            state_dir: None,
            log_bodies: true,
            body_limit: 0,
            api_key: None,
            allow_restart: Some(false),
            log: Arc::new(|_msg: String| {}),
            version: "0.0.0-smoke-test".to_string(),
        }));
        let addr = dango_bridge::server::serve(Arc::clone(&bridge), "127.0.0.1", 0)
            .await
            .expect("起假桥失败");
        format!("http://{addr}")
    }

    #[test]
    fn basic_fails_when_body_is_empty() {
        // 事件齐全但一个正文 token 都没有：正是 JS 注释里那个「思考吃干净」的坑。
        let sse = r#"event: message_start
data: {"type":"message_start","message":{"model":"m"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":0}}

event: message_stop
data: {"type":"message_stop"}

"#;
        let err = judge_basic(200, sse).expect_err("正文为空必须判失败");
        assert!(err.contains("正文是空的"), "{err}");
        assert!(err.contains("max_tokens"), "该提示预算被思考吃完了：{err}");
    }

    #[test]
    fn basic_fails_when_sse_event_is_missing() {
        // 有正文但缺 message_delta：事件序列不完整也要判失败。
        let no_delta = r#"event: message_start
data: {"type":"message_start","message":{"model":"m"}}

event: content_block_delta
data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"晴"}}

event: message_stop
data: {"type":"message_stop"}

"#;
        let err = judge_basic(200, no_delta).expect_err("缺 message_delta 必须判失败");
        assert!(err.contains("message_delta"), "{err}");

        // 反过来：缺 message_start 也要失败（正文都在）。
        let no_start = r#"event: content_block_delta
data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"晴"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

"#;
        let err = judge_basic(200, no_start).expect_err("缺 message_start 必须判失败");
        assert!(err.contains("message_start"), "{err}");
    }

    #[test]
    fn basic_passes_on_a_healthy_stream() {
        let sse = r#"event: message_start
data: {"type":"message_start","message":{"model":"m"}}

event: content_block_delta
data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"晴"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":11,"output_tokens":2}}

event: message_stop
data: {"type":"message_stop"}

"#;
        let detail = judge_basic(200, sse).expect("健康的流应当通过");
        assert_eq!(detail["text"], "晴");
        assert_eq!(detail["stopReason"], "end_turn");
        assert_eq!(detail["usage"]["output_tokens"], 2);
    }

    #[tokio::test]
    async fn basic_and_openai_pass_against_fake_bridge() {
        let base = start_fake().await;
        let client = Client::new(base);

        let basic = check_basic(&client, DEFAULT_MODEL)
            .await
            .expect("basic 应当通过假桥");
        assert_eq!(basic["text"], "晴");

        let stream = check_openai_stream(&client, DEFAULT_MODEL)
            .await
            .expect("openai 流式应当通过假桥");
        assert_eq!(stream["text"], "晴");

        let whole = check_openai_whole(&client, DEFAULT_MODEL)
            .await
            .expect("openai 整包应当通过假桥");
        assert_eq!(whole["text"], "晴");
    }

    #[tokio::test]
    async fn models_check_passes_against_fake_bridge() {
        let base = start_fake().await;
        let client = Client::new(base);
        let (status, body) = client.get("/v1/models").await.expect("GET /v1/models 失败");
        let detail = judge_models(status, &body).expect("/v1/models 一项应当能过");
        assert!(detail["count"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(detail["first"], "gemini-3.8-flash-high");
        assert_eq!(detail["defaultModel"], "gemini-3.8-flash-high");
    }

    #[tokio::test]
    async fn signature_hits_are_read_from_healthz() {
        let base = start_fake().await;
        let client = Client::new(base);

        // 第一发：拿到真签名对应的 tool_use。
        let (_detail, handoff) = check_tool_round_1(&client, DEFAULT_MODEL)
            .await
            .expect("第一发应当拿到 tool_use");
        assert_eq!(handoff.name, "get_time");

        // 命中数是 /healthz 里 counters.toolSignatureHits 的增量。
        let before = client
            .signature_hits()
            .await
            .expect("/healthz 应当能读计数");
        let detail = check_tool_round_2(&client, DEFAULT_MODEL, &handoff)
            .await
            .expect("第二发应当命中签名缓存");
        assert_eq!(
            detail["signatureHits"], 1,
            "命中数应当从 /healthz 的增量读出来"
        );

        let after = client
            .signature_hits()
            .await
            .expect("/healthz 应当能读计数");
        assert!(
            after > before,
            "/healthz 的 toolSignatureHits 应当 +1：{before} -> {after}"
        );
    }

    #[tokio::test]
    async fn tool_round_2_without_a_signature_fails() {
        // 直接手造一个假的 handoff（id 从没被桥存过）：第二发应当因为「签名没命中」判失败。
        let base = start_fake().await;
        let client = Client::new(base);
        let handoff = ToolHandoff {
            tool_id: "toolu_never_seen".to_string(),
            name: "get_time".to_string(),
            args: json!({ "tz": "Asia/Shanghai" }),
        };
        let err = check_tool_round_2(&client, DEFAULT_MODEL, &handoff)
            .await
            .expect_err("没命中签名必须判失败");
        assert!(err.contains("签名没有命中缓存"), "{err}");
    }

    #[tokio::test]
    async fn run_checks_passes_end_to_end_against_fake_bridge() {
        // 等价于 `smoke --run --base=<假桥>`：把整条客户端路径（含 /healthz 读计数）走一遍。
        let base = start_fake().await;
        let client = Client::new(base);
        let log: LogFn = Arc::new(|_msg: String| {});
        let steps = run_checks(&client, DEFAULT_MODEL, &None, &log).await;

        let names: Vec<&str> = steps.iter().map(|step| step.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "basic",
                "tool-round-1",
                "tool-round-2",
                "openai-stream",
                "openai-whole",
                "models",
            ]
        );
        for step in &steps {
            assert!(step.ok, "{} 应当通过：{:?}", step.name, step.error);
        }
    }

    #[tokio::test]
    async fn verify_bridge_accepts_the_fake_bridge() {
        // `--base` 形态的第一件事：先问一句 /version，对面得像个桥。
        let base = start_fake().await;
        let client = Client::new(base);
        let log: LogFn = Arc::new(|_msg: String| {});
        assert!(verify_bridge(&client, &log).await);
    }

    #[test]
    fn args_and_only_filter_work() {
        let args = parse_args(vec![
            "--run".to_string(),
            "--only=tool, openai".to_string(),
            "--model=m".to_string(),
            "--base=http://x".to_string(),
        ]);
        assert!(args.run);
        assert_eq!(args.model, "m");
        assert_eq!(args.base.as_deref(), Some("http://x"));
        assert_eq!(
            args.only.as_deref(),
            Some(&["tool".to_string(), "openai".to_string()][..])
        );
        assert!(wants(&args.only, "tool"));
        assert!(wants(&args.only, "openai"));
        assert!(!wants(&args.only, "basic"));

        // 不认识的参数被忽略（JS 版行为），不给 --only 就是全跑。
        let ignored = parse_args(vec!["--bogus".to_string()]);
        assert!(!ignored.run && ignored.only.is_none());
        assert!(wants(&None, "anything"));
    }
}
