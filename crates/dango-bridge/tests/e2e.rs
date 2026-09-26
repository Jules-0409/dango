//! 端到端：真的起 HTTP 服务、真的走 SSE，上游换成假的。
//!
//! 逐条移植 `test/server.e2e.test.mjs`（JS 版是行为基准）。假上游不是 HTTP 服务，
//! 而是一个实现 `Session` 的假会话 —— 和 JS 那边注入假 upstream 对象是同一招，
//! 这样测的是桥自己的翻译与路由，不会碰到任何真实凭据或网络。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dango_bridge::accounts::{AccountPool, AccountPoolOptions, SessionFactory, SingleAccountPool};
use dango_bridge::server::{Bridge, BridgeOptions, Pool};
use dango_bridge::signatures::shared_signatures;
use dango_bridge::types::{
    Account, GenerateRequest, QuotaRow, QuotaSnapshot, ResolvedModel, Session, StreamEvent,
    UpstreamError,
};
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;

/// 上游调用流水：(账号邮箱, 模型)。额度选号用例靠它看「谁被试过几次」。
type CallLog = Arc<Mutex<Vec<(String, String)>>>;

fn chunk(parts: Value, finish: Option<&str>, usage: Option<Value>) -> Value {
    let mut candidate = json!({ "content": { "role": "model", "parts": parts } });
    if let Some(reason) = finish {
        candidate["finishReason"] = json!(reason);
    }
    json!({
        "response": {
            "candidates": [candidate],
            "usageMetadata": usage.unwrap_or_else(|| json!({ "promptTokenCount": 11, "candidatesTokenCount": 2 })),
        },
        "traceId": "trace-1",
    })
}

fn text_chunk(text: &str, finish: Option<&str>) -> Value {
    chunk(json!([{ "text": text }]), finish, None)
}

/// 模型解析的两种模式：原样回显（默认），或「版本匹配 → 换 id」（单个模型那条用例要它）。
#[derive(Clone, Copy, PartialEq)]
enum ResolveMode {
    Echo,
    Tiered,
}

/// 假上游会话：记下收到的请求体，按脚本吐 chunk。
struct FakeSession {
    script: Vec<Value>,
    /// 请求把思考明确关掉（`thinkingBudget: 0`）时改吐这个脚本 ——
    /// 「空回合 → 关思考重试」那条路要能看出来第二发和第一发不一样。空 = 还是走 `script`。
    no_thinking_script: Vec<Value>,
    fail: Option<UpstreamError>,
    /// 只对「模型名包含这个子串」的请求回这个错，别的模型照常走 `script`。
    /// 多账号用例要它：让一个账号在 gemini 上 429、在 claude 上正常，才能验证
    /// 「额度耗尽按家族换号」而不是把整个账号一刀切打死。
    fail_model: Option<(String, UpstreamError)>,
    no_data: Option<String>,
    models: Option<Value>,
    seen: Arc<Mutex<Vec<GenerateRequest>>>,
    /// 把每次 `stream_generate` 收到的 (账号标签, 模型) 记到共享流水里。
    /// 多账号用例断言「谁被试了几次」靠它 —— 光看响应头，看不出选号跳过了谁。
    calls: Option<(String, CallLog)>,
    resolve: ResolveMode,
}

impl FakeSession {
    /// 这一发该吐哪个脚本。看请求体而不是请求次数：同一个 harness 里的多次请求互不干扰。
    fn script_for(&self, req: &GenerateRequest) -> &[Value] {
        let budget = req
            .request
            .get("generationConfig")
            .and_then(|g| g.get("thinkingConfig"))
            .and_then(|t| t.get("thinkingBudget"))
            .and_then(Value::as_i64);
        if budget == Some(0) && !self.no_thinking_script.is_empty() {
            &self.no_thinking_script
        } else {
            &self.script
        }
    }
}

#[async_trait]
impl Session for FakeSession {
    fn identity(&self) -> Value {
        json!({ "email": "te***@gmail.com", "project": "proj-x", "tier": "free-tier" })
    }

    async fn load_code_assist(&self) -> Result<Value, UpstreamError> {
        Ok(Value::Null)
    }

    async fn models(&self) -> Result<Value, UpstreamError> {
        Ok(self.models.clone().unwrap_or_else(|| {
            json!({
                "models": { "gemini-3.6-flash-high": { "displayName": "Flash" } },
                "defaultAgentModelId": "gemini-3.6-flash-high",
            })
        }))
    }

    async fn quota(&self) -> Result<QuotaSnapshot, UpstreamError> {
        Ok(QuotaSnapshot::default())
    }

    async fn resolve_model(&self, requested: &str) -> ResolvedModel {
        match self.resolve {
            ResolveMode::Echo => ResolvedModel {
                model: if requested.is_empty() {
                    "gemini-3.6-flash-high".to_string()
                } else {
                    requested.to_string()
                },
                substituted_from: None,
                reason: "exact".to_string(),
            },
            ResolveMode::Tiered => {
                let model = "gemini-3.8-flash-tiered".to_string();
                if requested == "gemini-3.8-flash-high" {
                    ResolvedModel {
                        model,
                        substituted_from: Some(requested.to_string()),
                        reason: "version_match".to_string(),
                    }
                } else {
                    // 真实的 resolveModel 在谁也没匹配上时：substitutedFrom 是请求名、reason 是 default_fallback
                    ResolvedModel {
                        model,
                        substituted_from: Some(requested.to_string()),
                        reason: "default_fallback".to_string(),
                    }
                }
            }
        }
    }

    async fn stream_generate(
        &self,
        req: GenerateRequest,
        tx: Sender<StreamEvent>,
    ) -> Result<(), UpstreamError> {
        // 脚本先按请求体选好（关思考的修复体走另一个脚本），再记请求 —— req 下面就被 seen 拿走了
        let script = self.script_for(&req);
        let model = req.model.clone();
        if let Some((label, calls)) = &self.calls {
            calls.lock().unwrap().push((label.clone(), model.clone()));
        }
        self.seen.lock().unwrap().push(req);
        let endpoint = "https://example.invalid/v1internal".to_string();
        if let Some((needle, err)) = &self.fail_model {
            if model.contains(needle.as_str()) {
                let _ = tx.send(StreamEvent::Error(err.clone())).await;
                return Ok(());
            }
        }
        if let Some(err) = &self.fail {
            let _ = tx.send(StreamEvent::Error(err.clone())).await;
            return Ok(());
        }
        if let Some(raw) = &self.no_data {
            let _ = tx
                .send(StreamEvent::Open {
                    endpoint: endpoint.clone(),
                })
                .await;
            let _ = tx
                .send(StreamEvent::NoData {
                    endpoint,
                    raw_text: raw.clone(),
                })
                .await;
            return Ok(());
        }
        if !self.script.is_empty() || !self.no_thinking_script.is_empty() {
            let _ = tx.send(StreamEvent::Open { endpoint }).await;
        }
        for item in script {
            if tx.send(StreamEvent::Chunk(item.clone())).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

struct Harness {
    bridge: Arc<Bridge>,
    base: String,
    seen: Arc<Mutex<Vec<GenerateRequest>>>,
}

struct FakeConfig {
    script: Vec<Value>,
    /// 关掉思考那一发（空回合的修复重试）该吐什么；空 = 和 `script` 一样
    no_thinking_script: Vec<Value>,
    fail: Option<UpstreamError>,
    no_data: Option<String>,
    models: Option<Value>,
    resolve: ResolveMode,
    api_key: Option<String>,
    /// 给了就落盘写请求日志（正文相关的用例要读它）
    log_dir: Option<PathBuf>,
    /// 请求日志里记不记正文（默认记；关掉时条目里只留 `"bodies": "off"`）
    log_bodies: bool,
    body_limit: usize,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            script: Vec::new(),
            no_thinking_script: Vec::new(),
            fail: None,
            no_data: None,
            models: None,
            resolve: ResolveMode::Echo,
            api_key: None,
            log_dir: None,
            log_bodies: true,
            body_limit: 0,
        }
    }
}

async fn start(config: FakeConfig) -> Harness {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let session: Arc<dyn Session> = Arc::new(FakeSession {
        script: config.script,
        no_thinking_script: config.no_thinking_script,
        fail: config.fail,
        fail_model: None,
        no_data: config.no_data,
        models: config.models,
        seen: Arc::clone(&seen),
        calls: None,
        resolve: config.resolve,
    });
    let bridge = Arc::new(Bridge::new(BridgeOptions {
        pool: Pool::Single(Arc::new(SingleAccountPool::new(session))),
        qoder: None,
        store: shared_signatures(),
        log_dir: config.log_dir,
        state_dir: None,
        log_bodies: config.log_bodies,
        body_limit: config.body_limit,
        api_key: config.api_key,
        allow_restart: Some(false),
        log: Arc::new(|_msg: String| {}),
        version: "0.0.0-test".to_string(),
    }));
    let addr = dango_bridge::server::serve(Arc::clone(&bridge), "127.0.0.1", 0)
        .await
        .expect("起服务失败");
    Harness {
        bridge,
        base: format!("http://{addr}"),
        seen,
    }
}

async fn post(
    base: &str,
    path: &str,
    body: Value,
    headers: Vec<(&str, &str)>,
) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(&body);
    for (name, value) in headers {
        req = req.header(name, value);
    }
    req.send().await.expect("请求失败")
}

/// `event: x` + `data: {...}` 的帧解析（和 JS 测试里的 parseFrames 同一套）。
fn parse_frames(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
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

fn delta_text(frames: &[(String, Value)]) -> String {
    frames
        .iter()
        .filter(|(_, data)| {
            data.get("delta")
                .and_then(|d| d.get("type"))
                .and_then(Value::as_str)
                == Some("text_delta")
        })
        .filter_map(|(_, data)| {
            data.get("delta")
                .and_then(|d| d.get("text"))
                .and_then(Value::as_str)
        })
        .collect()
}

#[tokio::test]
async fn streaming_anthropic_event_sequence_and_text() {
    let harness = start(FakeConfig {
        script: vec![text_chunk("你好", None), text_chunk("，世界", Some("STOP"))],
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 128,
            "stream": true,
            "messages": [{ "role": "user", "content": "打个招呼" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    assert!(res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("text/event-stream"));
    let text = res.text().await.unwrap();
    let frames = parse_frames(&text);
    assert_eq!(frames[0].0, "message_start");
    assert_eq!(frames.last().unwrap().0, "message_stop");
    assert_eq!(delta_text(&frames), "你好，世界");
    let delta = frames
        .iter()
        .find(|(event, _)| event == "message_delta")
        .unwrap();
    assert_eq!(delta.1["delta"]["stop_reason"], "end_turn");
    assert_eq!(delta.1["usage"]["input_tokens"], 11);
    assert_eq!(delta.1["usage"]["output_tokens"], 2);

    // 上游收到的是 v1internal 形状
    let seen = harness.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].model, "gemini-3.6-flash-high");
    assert_eq!(
        seen[0].request["contents"],
        json!([{ "role": "user", "parts": [{ "text": "打个招呼" }] }])
    );
}

#[tokio::test]
async fn buffered_anthropic_message_carries_bridge_headers() {
    let harness = start(FakeConfig {
        script: vec![text_chunk("整包", Some("STOP"))],
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "gemini-3.6-flash-high", "max_tokens": 64, "stream": false, "messages": [{ "role": "user", "content": "hi" }] }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers().get("x-bridge-model").unwrap(),
        "gemini-3.6-flash-high"
    );
    // 整包也要能看出是谁服务的（流式早就有这两个头，别只给流式）
    assert!(!res.headers().get("x-bridge-account").unwrap().is_empty());
    let message: Value = res.json().await.unwrap();
    assert_eq!(message["type"], "message");
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["content"][0]["text"], "整包");
    assert_eq!(message["stop_reason"], "end_turn");
}

#[tokio::test]
async fn missing_stream_field_means_buffered_json() {
    let harness = start(FakeConfig {
        script: vec![text_chunk("默认整包", Some("STOP"))],
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "m", "max_tokens": 64, "messages": [{ "role": "user", "content": "hi" }] }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    assert!(res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("application/json"));
    let message: Value = res.json().await.unwrap();
    assert_eq!(message["content"][0]["text"], "默认整包");
}

/// 上游回思考正文时，流式与非流式都得看得见 thinking 块。
/// 上游绝大多数回合只给 `thoughtsTokenCount`、不回正文（真机行为），所以这里用假上游
/// 刻意构造：先一个带签名的 thought part，再一段普通正文，把「thinking 渲染」这条路
/// 钉在端到端层面（翻译细节的单测在 `anthropic_stream.rs` 里）。
#[tokio::test]
async fn thought_part_becomes_a_thinking_block_in_both_modes() {
    let harness = start(FakeConfig {
        script: vec![
            chunk(
                json!([{ "text": "想一想", "thought": true, "thoughtSignature": "THOUGHT-SIG-1" }]),
                None,
                None,
            ),
            text_chunk("答案", Some("STOP")),
        ],
        ..Default::default()
    })
    .await;

    // 流式：thinking 块先开，关块前补 signature_delta，正文留在 text 块里
    let res = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "m", "max_tokens": 64, "stream": true, "messages": [{ "role": "user", "content": "想一想再答" }] }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    let frames = parse_frames(&text);
    let thinking_start = frames
        .iter()
        .find(|(event, data)| {
            event == "content_block_start" && data["content_block"]["type"] == "thinking"
        })
        .expect("上游回了思考正文，流里就该有 thinking 块");
    let thinking_index = thinking_start.1["index"].as_i64().unwrap();
    let signature = frames
        .iter()
        .find(|(_, data)| data["delta"]["type"] == "signature_delta")
        .expect("thinking 块收尾必须发 signature_delta");
    assert_eq!(signature.1["index"].as_i64().unwrap(), thinking_index);
    assert_eq!(signature.1["delta"]["signature"], "THOUGHT-SIG-1");
    assert_eq!(delta_text(&frames), "答案");

    // 非流式：同一批事件聚合成 [thinking, text] 两块
    let res = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "m", "max_tokens": 64, "stream": false, "messages": [{ "role": "user", "content": "想一想再答" }] }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let message: Value = res.json().await.unwrap();
    assert_eq!(message["content"][0]["type"], "thinking");
    assert_eq!(message["content"][0]["thinking"], "想一想");
    assert_eq!(message["content"][0]["signature"], "THOUGHT-SIG-1");
    assert_eq!(message["content"][1]["type"], "text");
    assert_eq!(message["content"][1]["text"], "答案");
}

#[tokio::test]
async fn tool_use_round_trip_returns_the_signature_upstream() {
    let harness = start(FakeConfig {
        script: vec![chunk(
            json!([{ "thoughtSignature": "SIG-XYZ", "functionCall": { "name": "Grep", "args": { "pattern": "foo" } } }]),
            Some("STOP"),
            None,
        )],
        ..Default::default()
    })
    .await;

    let first = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "m", "max_tokens": 64, "stream": true, "messages": [{ "role": "user", "content": "搜一下" }] }),
        vec![],
    )
    .await;
    let frames = parse_frames(&first.text().await.unwrap());
    let tool_start = frames
        .iter()
        .find(|(event, data)| {
            event == "content_block_start" && data["content_block"]["type"] == "tool_use"
        })
        .expect("没有 tool_use 块");
    assert_eq!(tool_start.1["content_block"]["name"], "Grep");
    let tool_id = tool_start.1["content_block"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let stop = frames
        .iter()
        .find(|(event, _)| event == "message_delta")
        .unwrap();
    assert_eq!(stop.1["delta"]["stop_reason"], "tool_use");

    // 第二回合：客户端把 tool_use 和 tool_result 传回来，签名必须回到上游
    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "m",
            "max_tokens": 64,
            "stream": true,
            "messages": [
                { "role": "user", "content": "搜一下" },
                { "role": "assistant", "content": [{ "type": "tool_use", "id": tool_id, "name": "Grep", "input": { "pattern": "foo" } }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": tool_id, "content": "命中 3 处" }] },
            ],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let _ = res.text().await.unwrap();

    let seen = harness.seen.lock().unwrap();
    // JS 那边假上游按会话记：第二回合的内容必须带签名与函数回执
    let second = &seen[1];
    let contents = &second.request["contents"];
    assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "SIG-XYZ");
    assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "Grep");
}

#[tokio::test]
async fn leaked_pseudo_call_is_repaired_into_tool_use() {
    let harness = start(FakeConfig {
        script: vec![text_chunk(
            "先看 <call:default_api:Grep{pattern:foo}",
            Some("STOP"),
        )],
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "m", "max_tokens": 64, "stream": true, "messages": [{ "role": "user", "content": "x" }] }),
        vec![],
    )
    .await;
    let frames = parse_frames(&res.text().await.unwrap());
    let tool = frames
        .iter()
        .find(|(event, data)| {
            event == "content_block_start" && data["content_block"]["type"] == "tool_use"
        })
        .expect("泄漏的伪调用应该被修成 tool_use");
    assert_eq!(tool.1["content_block"]["name"], "Grep");
    let stop = frames
        .iter()
        .find(|(event, _)| event == "message_delta")
        .unwrap();
    assert_eq!(stop.1["delta"]["stop_reason"], "tool_use");
}

#[tokio::test]
async fn request_repair_drops_orphans_and_fills_dangling_calls() {
    let harness = start(FakeConfig {
        script: vec![text_chunk("ok", Some("STOP"))],
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "m",
            "max_tokens": 64,
            "stream": false,
            "messages": [
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "t-alive", "name": "Grep", "input": {} }] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t-alive", "content": "好" },
                    { "type": "tool_result", "tool_use_id": "t-orphan", "content": "孤儿" },
                ] },
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "t-dangling", "name": "Read", "input": {} }] },
            ],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let _ = res.text().await.unwrap();

    let seen = harness.seen.lock().unwrap();
    let contents = seen[0].request["contents"].clone();
    let flat = contents.to_string();
    assert!(
        !flat.contains("t-orphan") && !flat.contains("孤儿"),
        "孤儿 tool_result 不该送到上游：{flat}"
    );
    let last = contents.as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["parts"][0]["functionResponse"]["name"], "Read");
    assert!(last["parts"][0]["functionResponse"]["response"]["error"]
        .as_str()
        .unwrap_or("")
        .contains("中断"));
}

#[tokio::test]
async fn upstream_failure_before_streaming_keeps_real_status() {
    let harness = start(FakeConfig {
        fail: Some(UpstreamError::http(
            403,
            "SUBSCRIPTION_REQUIRED",
            "You do not have a valid license of this product",
        )),
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "m", "max_tokens": 64, "stream": true, "messages": [{ "role": "user", "content": "x" }] }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 403);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "permission_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("valid license"));
}

#[tokio::test]
async fn invalid_argument_maps_to_invalid_request_error() {
    let harness = start(FakeConfig {
        fail: Some(UpstreamError::http(
            400,
            "INVALID_ARGUMENT",
            "Function call is missing a thought_signature",
        )),
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "m", "max_tokens": 64, "stream": true, "messages": [{ "role": "user", "content": "x" }] }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn plain_text_upstream_answer_becomes_502_with_the_original_text() {
    let harness = start(FakeConfig {
        no_data: Some("Gemini 3.5 Flash is no longer available".to_string()),
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "gone-model", "max_tokens": 64, "messages": [{ "role": "user", "content": "x" }] }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 502);
    let body: Value = res.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no longer available"));
}

#[tokio::test]
async fn api_key_gate_blocks_without_the_key() {
    let harness = start(FakeConfig {
        script: vec![text_chunk("ok", Some("STOP"))],
        api_key: Some("s3cret".to_string()),
        ..Default::default()
    })
    .await;

    let denied = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "m", "messages": [{ "role": "user", "content": "x" }] }),
        vec![],
    )
    .await;
    assert_eq!(denied.status(), 401);

    let ok = post(
        &harness.base,
        "/v1/messages",
        json!({ "model": "m", "max_tokens": 8, "stream": false, "messages": [{ "role": "user", "content": "x" }] }),
        vec![("x-api-key", "s3cret")],
    )
    .await;
    assert_eq!(ok.status(), 200);
}

#[tokio::test]
async fn healthz_models_openai_routes_and_404() {
    let harness = start(FakeConfig {
        script: vec![text_chunk("在", Some("STOP"))],
        ..Default::default()
    })
    .await;

    let health: Value = reqwest::get(format!("{}/healthz", harness.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["upstream"]["project"], "proj-x");
    assert!(!health.to_string().contains("access_token"));

    let models: Value = reqwest::get(format!("{}/v1/models", harness.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "gemini-3.6-flash-high");
    assert_eq!(models["data"][0]["object"], "model");
    assert_eq!(models["default_model"], "gemini-3.6-flash-high");

    // OpenAI 流式：data: 帧 + [DONE]
    let streamed = post(
        &harness.base,
        "/v1/chat/completions",
        json!({
            "model": "gpt-4o",
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "在吗" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(streamed.status(), 200);
    let raw = streamed.text().await.unwrap();
    assert!(
        raw.ends_with("data: [DONE]\n\n"),
        "流要以 [DONE] 收尾：{raw}"
    );
    let payloads: Vec<Value> = raw
        .split('\n')
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| !line.contains("[DONE]"))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert_eq!(payloads[0]["choices"][0]["delta"]["role"], "assistant");
    let text: String = payloads
        .iter()
        .flat_map(|p| p["choices"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["delta"]["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(text, "在");
    assert_eq!(
        payloads.last().unwrap()["choices"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(payloads.last().unwrap()["usage"].is_object());
    assert_eq!(harness.seen.lock().unwrap()[0].model, "gpt-4o");

    // OpenAI 整包
    let whole = post(
        &harness.base,
        "/v1/chat/completions",
        json!({ "model": "gpt-4o", "messages": [{ "role": "user", "content": "在吗" }] }),
        vec![],
    )
    .await;
    assert_eq!(whole.status(), 200);
    let completion: Value = whole.json().await.unwrap();
    assert_eq!(completion["object"], "chat.completion");
    assert_eq!(completion["choices"][0]["message"]["content"], "在");

    let missing = post(&harness.base, "/v1/nope", json!({}), vec![]).await;
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn panel_and_control_buttons() {
    let harness = start(FakeConfig {
        script: vec![text_chunk("ok", Some("STOP"))],
        ..Default::default()
    })
    .await;

    let panel = reqwest::get(format!("{}/", harness.base)).await.unwrap();
    assert_eq!(panel.status(), 200);
    assert!(panel
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("text/html"));
    let html = panel.text().await.unwrap();
    assert!(html.contains("antigravity-bridge"));
    assert!(
        html.contains("/control/restart"),
        "面板上的重启按钮确实打这个接口"
    );

    // 测试进程不在 launchd 下（allow_restart=false），重启必须被拒绝（不能把自己搞没）
    let restart = reqwest::Client::new()
        .post(format!("{}/control/restart", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(restart.status(), 409);
    let body: Value = restart.json().await.unwrap();
    assert_eq!(body["ok"], false);

    // 没注册桌面壳（headless 进程）时：「把面板窗口拿到前台」要如实拒绝，别装样子
    let show = reqwest::Client::new()
        .post(format!("{}/control/show-panel", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(show.status(), 409);

    // 桌面壳注册之后：接口 200，而且真的喊了那一声（第二个实例靠它把已有窗口拿到前台）
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&hits);
    harness.bridge.set_panel_show(std::sync::Arc::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let show = reqwest::Client::new()
        .post(format!("{}/control/show-panel", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(show.status(), 200);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

    let refresh = reqwest::Client::new()
        .post(format!("{}/control/refresh-quota", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(refresh.status(), 202);
}

#[tokio::test]
async fn single_model_route_and_version() {
    let harness = start(FakeConfig {
        script: vec![text_chunk("ok", Some("STOP"))],
        models: Some(json!({
            "models": { "gemini-3.8-flash-tiered": { "displayName": "Tiered" } },
            "defaultAgentModelId": "gemini-3.8-flash-tiered",
        })),
        resolve: ResolveMode::Tiered,
        ..Default::default()
    })
    .await;

    let direct: Value = reqwest::get(format!(
        "{}/v1/models/gemini-3.8-flash-tiered",
        harness.base
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(direct["id"], "gemini-3.8-flash-tiered");
    assert_eq!(direct["display_name"], "Tiered");

    let resolved: Value = reqwest::get(format!("{}/v1/models/gemini-3.8-flash-high", harness.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["id"], "gemini-3.8-flash-tiered");
    assert_eq!(resolved["resolved_from"], "gemini-3.8-flash-high");
    assert_eq!(resolved["resolve_reason"], "version_match");

    // default_fallback 意味着谁也没匹配上：必须 404，不然客户端拿编错的名字探测也会得到 200
    let unknown = reqwest::get(format!("{}/v1/models/totally-made-up", harness.base))
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let body: Value = unknown.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found_error");

    // /models 是别名（老桥日志里客户端直接打这个路径）
    let alias: Value = reqwest::get(format!("{}/models", harness.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(alias["object"], "list");
    assert_eq!(alias["data"][0]["id"], "gemini-3.8-flash-tiered");

    let version: Value = reqwest::get(format!("{}/version", harness.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(version["name"], "antigravity-bridge");
    assert!(version["version"].is_string());
}

#[tokio::test]
async fn budget_is_clamped_to_the_model_table() {
    let harness = start(FakeConfig {
        script: vec![text_chunk("ok", Some("STOP"))],
        models: Some(json!({
            "models": { "gemini-3.6-flash-high": { "displayName": "Flash", "maxOutputTokens": 8192, "minThinkingBudget": 1024 } },
            "defaultAgentModelId": "gemini-3.6-flash-high",
        })),
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 200_000,
            "thinking": { "type": "enabled", "budget_tokens": 128 },
            "messages": [{ "role": "user", "content": "在吗" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let _ = res.text().await.unwrap();

    let seen = harness.seen.lock().unwrap();
    let config = &seen[0].request["generationConfig"];
    assert_eq!(config["maxOutputTokens"], 8192);
    // thinking 预算抬到模型的最小值
    assert_eq!(config["thinkingConfig"]["thinkingBudget"], 1024);
}

#[tokio::test]
async fn logs_recent_reports_no_log_dir_honestly() {
    let harness = start(FakeConfig::default()).await;
    let body: Value = reqwest::get(format!("{}/logs/recent", harness.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["lines"].as_array().unwrap().len(), 0);
    assert!(body["note"].as_str().unwrap().contains("log-dir"));
    // 桥自己不该对外漏凭据
    let health: Value = reqwest::get(format!("{}/healthz", harness.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(health.get("accounts").is_some());
    drop(harness.bridge);
}

#[tokio::test]
async fn single_account_pool_identity_is_reported_without_credentials() {
    // 账号池为空时 identity 也不能把自己撑不住
    let session: Arc<dyn Session> = Arc::new(FakeSession {
        script: vec![],
        no_thinking_script: Vec::new(),
        fail: None,
        fail_model: None,
        no_data: None,
        models: None,
        seen: Arc::new(Mutex::new(Vec::new())),
        calls: None,
        resolve: ResolveMode::Echo,
    });
    let pool = SingleAccountPool::new(session);
    let account = pool.account("");
    assert!(account.is_none() || account.unwrap().refresh_token.is_none());
    let _ = Account::default();
}

// ---------------------------------------------------------------- 账号禁用（/control/accounts）

/// 多账号测试的假会话工厂：选中的是哪个账号，看响应头 `x-bridge-account`（值 = 掩码邮箱）。
fn fake_factory() -> SessionFactory {
    Arc::new(|_account: Account| {
        Box::pin(async {
            let session: Arc<dyn Session> = Arc::new(FakeSession {
                script: vec![text_chunk("好", Some("STOP"))],
                no_thinking_script: Vec::new(),
                fail: None,
                fail_model: None,
                no_data: None,
                models: None,
                seen: Arc::new(Mutex::new(Vec::new())),
                calls: None,
                resolve: ResolveMode::Echo,
            });
            Ok(session)
        })
    })
}

/// 上游连不上的假会话工厂（每个账号都一样）：用来测「全是网络失败时报什么」。
fn unreachable_factory() -> SessionFactory {
    Arc::new(|_account: Account| {
        Box::pin(async {
            let session: Arc<dyn Session> = Arc::new(FakeSession {
                script: Vec::new(),
                no_thinking_script: Vec::new(),
                fail: Some(UpstreamError::network("fetch failed", "connection refused")),
                fail_model: None,
                no_data: None,
                models: None,
                seen: Arc::new(Mutex::new(Vec::new())),
                calls: None,
                resolve: ResolveMode::Echo,
            });
            Ok(session)
        })
    })
}

/// 带指定 id 的账号（前缀唯一性由测试自己掌握，不靠邮箱撞出来）。
fn multi_account(id: &str, email: &str) -> Account {
    let at = email.find('@').unwrap();
    Account {
        id: id.to_string(),
        email: Some(email.to_string()),
        email_masked: format!("{}***{}", &email[..1], &email[at..]),
        project: Some("proj-x".to_string()),
        refresh_token: Some("rt".to_string()),
        ..Default::default()
    }
}

/// 一座多账号桥（a@x.com / b@x.com）；`state_dir` 决定禁用名单落不落盘。
async fn start_multi(state_dir: Option<PathBuf>) -> Harness {
    start_multi_factory(state_dir, fake_factory()).await
}

/// 同上，但换一套假会话工厂（比如「所有账号都连不上」）。
async fn start_multi_factory(state_dir: Option<PathBuf>, factory: SessionFactory) -> Harness {
    let pool = AccountPool::new(AccountPoolOptions::new(
        factory,
        vec![
            multi_account("bf00c418aaaa", "a@x.com"),
            multi_account("cc11dd22eeee", "b@x.com"),
        ],
    ));
    let bridge = Arc::new(Bridge::new(BridgeOptions {
        pool: Pool::Multi(Arc::new(pool)),
        qoder: None,
        store: shared_signatures(),
        log_dir: None,
        state_dir,
        log_bodies: true,
        body_limit: 0,
        api_key: None,
        allow_restart: Some(false),
        log: Arc::new(|_msg: String| {}),
        version: "0.0.0-test".to_string(),
    }));
    let addr = dango_bridge::server::serve(Arc::clone(&bridge), "127.0.0.1", 0)
        .await
        .expect("起服务失败");
    Harness {
        bridge,
        base: format!("http://{addr}"),
        seen: Arc::new(Mutex::new(Vec::new())),
    }
}

/// 多账号用例的每账号脚本：正常时吐 `script`；模型名包含 `fail_model` 里那段的请求回那个错。
struct AccountPlan {
    email: &'static str,
    script: Vec<Value>,
    fail_model: Option<(&'static str, UpstreamError)>,
}
/// 按账号分派的假上游工厂 + 调用流水。
///
/// `plans` 里没列的账号一律给空脚本、从不失败（用来占位）。`calls` 记下每次
/// `stream_generate` 的 (邮箱, 模型)：断言「额度耗尽后有没有换号、有没有误伤同家族」靠它 ——
/// 响应头只说最后是谁服务的，说不出中间跳过了谁。
fn routing_factory(plans: Vec<AccountPlan>) -> (SessionFactory, CallLog) {
    let plans = Arc::new(plans);
    let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
    let calls_for_factory = Arc::clone(&calls);
    let factory: SessionFactory = Arc::new(move |account: Account| {
        let email = account.email.clone().unwrap_or_default();
        let plans = Arc::clone(&plans);
        let calls = Arc::clone(&calls_for_factory);
        Box::pin(async move {
            let plan = plans.iter().find(|p| p.email == email.as_str());
            let script = plan.map(|p| p.script.clone()).unwrap_or_default();
            let fail_model = plan
                .and_then(|p| p.fail_model.as_ref())
                .map(|(needle, err)| (needle.to_string(), err.clone()));
            let session: Arc<dyn Session> = Arc::new(FakeSession {
                script,
                no_thinking_script: Vec::new(),
                fail: None,
                fail_model,
                no_data: None,
                models: None,
                seen: Arc::new(Mutex::new(Vec::new())),
                calls: Some((email, calls)),
                resolve: ResolveMode::Echo,
            });
            Ok(session)
        })
    });
    (factory, calls)
}
/// 调用流水里某个邮箱被上游调过几次、每次是什么模型。
fn calls_for(calls: &CallLog, email: &str) -> Vec<String> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(e, _)| e == email)
        .map(|(_, model)| model.clone())
        .collect()
}
/// 两个家族的额度摘要：gemini / 3p 各给一个剩余比例（形状与 `accounts` 单测一致）。
fn quota_summary(gemini: f64, three_p: f64) -> QuotaSnapshot {
    QuotaSnapshot {
        summary: vec![
            QuotaRow {
                group: "Gemini Models".to_string(),
                window: "weekly".to_string(),
                remaining_percent: Some(gemini),
                reset_time: None,
            },
            QuotaRow {
                group: "Claude and GPT models".to_string(),
                window: "weekly".to_string(),
                remaining_percent: Some(three_p),
                reset_time: None,
            },
        ],
        ..Default::default()
    }
}

/// 临时状态目录（进程号 + 用例名：几个用例并行跑也不撞车）。
fn temp_state_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("agb-e2e-{}-{name}", std::process::id()))
}

async fn get_json(base: &str, path: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .send()
        .await
        .expect("请求失败")
        .json()
        .await
        .expect("不是 JSON")
}

fn message_body() -> Value {
    json!({
        "model": "gemini-3.8-flash-tiered",
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": "hi" }],
    })
}
/// 指定模型 + 正文的整包请求体（额度选号用例要按家族换模型，正文变了才看得出是另一发）。
fn body_for(model: &str, content: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 16,
        "stream": false,
        "messages": [{ "role": "user", "content": content }],
    })
}

/// `/healthz` 里某个账号的掩码邮箱（响应头就是它，别在测试里自己拼格式）。
fn account_email(health: &Value, id_prefix: &str) -> String {
    health["pool"]
        .as_array()
        .expect("pool 不是数组")
        .iter()
        .find(|a| a["id"] == json!(id_prefix))
        .unwrap_or_else(|| panic!("池子里没有 {id_prefix}：{health}"))["email"]
        .as_str()
        .expect("email 不是字符串")
        .to_string()
}

#[tokio::test]
async fn control_accounts_disables_enables_and_persists() {
    let dir = temp_state_dir("accounts");
    std::fs::create_dir_all(&dir).expect("建临时目录失败");
    let harness = start_multi(Some(dir.clone())).await;

    // 开局：两个都在、都没被禁，落盘开着
    let list = get_json(&harness.base, "/control/accounts").await;
    let accounts = list["accounts"].as_array().unwrap().clone();
    assert_eq!(accounts.len(), 2);
    assert!(accounts.iter().all(|a| a["disabled"] == json!(false)));
    assert_eq!(list["persisted"], json!(true));

    // 面板给的是 8 位前缀，照样能禁
    let res = post(
        &harness.base,
        "/control/accounts",
        json!({ "id": "bf00c418", "disabled": true }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["id"], json!("bf00c418aaaa")); // 回的是完整 id
    assert_eq!(body["persisted"], json!(true));
    let raw = std::fs::read_to_string(dir.join("disabled-accounts.json")).unwrap();
    assert!(raw.contains("bf00c418aaaa"), "{raw}");

    // /healthz：被禁的账号还在池子里，只是标着 disabled（面板要能再启用回来）
    let health = get_json(&harness.base, "/healthz").await;
    let pool = health["pool"].as_array().unwrap().clone();
    assert_eq!(pool.len(), 2);
    let disabled: Vec<&Value> = pool
        .iter()
        .filter(|a| a["disabled"] == json!(true))
        .collect();
    assert_eq!(disabled.len(), 1);
    assert_eq!(disabled[0]["id"], json!("bf00c418"));
    let b_email = account_email(&health, "cc11dd22");

    // 选号只剩 b：响应头会说是谁干的
    let res = post(&harness.base, "/v1/messages", message_body(), vec![]).await;
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers()["x-bridge-account"].to_str().unwrap(),
        b_email.as_str()
    );

    // 同一个 state_dir 再起一座桥 = 重启：名单被读回来
    let restarted = start_multi(Some(dir.clone())).await;
    let list = get_json(&restarted.base, "/control/accounts").await;
    let disabled: Vec<&Value> = list["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["disabled"] == json!(true))
        .collect();
    assert_eq!(disabled.len(), 1);
    assert_eq!(disabled[0]["id"], json!("bf00c418"));

    // 启用回来，文件里也清掉
    let res = post(
        &restarted.base,
        "/control/accounts",
        json!({ "id": "bf00c418", "disabled": false }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let raw = std::fs::read_to_string(dir.join("disabled-accounts.json")).unwrap();
    assert!(!raw.contains("bf00c418aaaa"), "{raw}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn control_accounts_without_state_dir_works_in_memory_only() {
    let harness = start_multi(None).await;
    let health = get_json(&harness.base, "/healthz").await;
    let a_email = account_email(&health, "bf00c418");

    let res = post(
        &harness.base,
        "/control/accounts",
        json!({ "id": "cc11dd22", "disabled": true }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["persisted"], json!(false)); // 没配 state_dir：只在内存里，如实说
    assert_eq!(body["stateFile"], Value::Null);

    // 选号真的绕开了被禁的 b
    let res = post(&harness.base, "/v1/messages", message_body(), vec![]).await;
    assert_eq!(
        res.headers()["x-bridge-account"].to_str().unwrap(),
        a_email.as_str()
    );
}

#[tokio::test]
async fn control_accounts_rejects_bad_input_with_a_reason() {
    let harness = start_multi(None).await;

    let res = post(
        &harness.base,
        "/control/accounts",
        json!({ "id": "nope", "disabled": true }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("没有这个账号"),
        "{body}"
    );

    // 少了 disabled，或者给的不是布尔
    for bad in [
        json!({ "id": "bf00c418" }),
        json!({ "id": "bf00c418", "disabled": "yes" }),
    ] {
        let res = post(&harness.base, "/control/accounts", bad.clone(), vec![]).await;
        assert_eq!(res.status(), 400, "{bad}");
        let body: Value = res.json().await.unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("disabled"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn control_accounts_is_honest_in_single_account_mode() {
    let harness = start(FakeConfig::default()).await;
    let res = post(
        &harness.base,
        "/control/accounts",
        json!({ "id": "default", "disabled": true }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("单账号"),
        "{body}"
    );
    // GET 也说得清：单账号没有池子
    let list = get_json(&harness.base, "/control/accounts").await;
    assert_eq!(list["accounts"].as_array().unwrap().len(), 0);
}

/// 请求日志是请求收尾之后才写的，用例里等它一会儿（最多 2 秒）。
async fn wait_for_entry(path: &std::path::Path) -> Value {
    for _ in 0..200 {
        if let Ok(text) = tokio::fs::read_to_string(path).await {
            if let Some(entry) = text
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .next_back()
            {
                return entry;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{} 一直没等到日志条目", path.display());
}

/// 正文进日志：条目带 `id` 与 `request`/`response`；`/logs/recent` 为了轻量不带正文，
/// 按 `id` 去 `/logs/entry` 才拿得到全文。
#[tokio::test]
async fn log_bodies_land_in_the_entry_and_come_back_by_id() {
    let dir = temp_state_dir("log-bodies");
    let harness = start(FakeConfig {
        script: vec![text_chunk("你好", None), text_chunk("，世界", Some("STOP"))],
        log_dir: Some(dir.clone()),
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 128,
            "stream": true,
            "messages": [{ "role": "user", "content": "打个招呼" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let _ = res.text().await.unwrap();

    let entry = wait_for_entry(&dir.join("requests.jsonl")).await;
    let id = entry["id"].as_str().expect("条目要有 id").to_string();
    assert!(
        entry["request"]["json"]
            .as_str()
            .unwrap()
            .contains("打个招呼"),
        "请求正文该是客户端发来的内容：{entry}"
    );
    assert_eq!(entry["request"]["truncated"], json!(false));
    assert_eq!(entry["response"]["text"], json!("你好，世界"));
    assert_eq!(entry["response"]["truncated"], json!(false));

    // 列表不带正文，只留一个记号
    let recent = get_json(&harness.base, "/logs/recent?n=5").await;
    let line = recent["lines"].as_array().unwrap().last().unwrap();
    assert!(line.get("request").is_none(), "列表不该带正文：{line}");
    assert!(line.get("response").is_none(), "列表不该带正文：{line}");
    assert_eq!(line["bodiesOmitted"], json!(true));

    // 按 id 取全文
    let one = get_json(&harness.base, &format!("/logs/entry?id={id}")).await;
    assert_eq!(one["line"]["id"], json!(id));
    assert_eq!(one["line"]["response"]["text"], json!("你好，世界"));

    // 找不到就 404 说清楚，不编一条出来
    let res = reqwest::get(format!("{}/logs/entry?id=nope", harness.base))
        .await
        .expect("请求失败");
    assert_eq!(res.status(), 404);
    let missing: Value = res.json().await.unwrap();
    assert!(
        missing["error"].as_str().unwrap().contains("id=nope"),
        "{missing}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 关了正文开关：条目里没有 `request`/`response`，只留 `"bodies": "off"` 的记号。
#[tokio::test]
async fn log_bodies_off_leaves_a_marker() {
    let dir = temp_state_dir("log-bodies-off");
    let harness = start(FakeConfig {
        script: vec![text_chunk("秘密", Some("STOP"))],
        log_dir: Some(dir.clone()),
        log_bodies: false,
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "别记下来" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let _ = res.text().await.unwrap();

    let entry = wait_for_entry(&dir.join("requests.jsonl")).await;
    assert_eq!(entry["bodies"], json!("off"));
    assert!(entry.get("request").is_none(), "关了就不该有正文：{entry}");
    assert!(entry.get("response").is_none(), "关了就不该有正文：{entry}");
    // 关了就一个字都不留（拿原文里那句真人话验，不只看字段在不在）
    let raw = tokio::fs::read_to_string(dir.join("requests.jsonl"))
        .await
        .unwrap();
    assert!(!raw.contains("别记下来"), "关了还写进去了：{raw}");

    // 没正文可摘，列表里也不该凭空冒出记号
    let recent = get_json(&harness.base, "/logs/recent?n=5").await;
    assert!(recent["lines"][0].get("bodiesOmitted").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 超长正文在配置了上限时只留前一段，并如实报出截断（请求按字节报原文大小，回复按字符）。
#[tokio::test]
async fn oversized_bodies_are_truncated_and_flagged() {
    let dir = temp_state_dir("log-bodies-trunc");
    let harness = start(FakeConfig {
        script: vec![text_chunk("短回复", Some("STOP"))],
        log_dir: Some(dir.clone()),
        body_limit: 8 * 1024,
        ..Default::default()
    })
    .await;

    let long = "噜".repeat(9_000);
    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": long }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let _ = res.text().await.unwrap();

    let entry = wait_for_entry(&dir.join("requests.jsonl")).await;
    assert_eq!(entry["request"]["truncated"], json!(true));
    let kept = entry["request"]["json"].as_str().unwrap();
    assert!(
        kept.chars().count() < 9_000,
        "截断后不该还是原来的长度：{}",
        kept.chars().count()
    );
    assert!(kept.contains("噜"), "留下的该是前面那一段");
    // 回复没超上限，就不该报截断
    assert_eq!(entry["response"]["truncated"], json!(false));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 默认不设限（body_limit = 0）时，超长正文完整保留不截断。
#[tokio::test]
async fn oversized_bodies_are_kept_full_when_unlimited() {
    let dir = temp_state_dir("log-bodies-unlimited");
    let harness = start(FakeConfig {
        script: vec![text_chunk("短回复", Some("STOP"))],
        log_dir: Some(dir.clone()),
        body_limit: 0,
        ..Default::default()
    })
    .await;

    let long = "噜".repeat(9_000);
    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": long }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let _ = res.text().await.unwrap();

    let entry = wait_for_entry(&dir.join("requests.jsonl")).await;
    assert_eq!(entry["request"]["truncated"], json!(false));
    let kept = entry["request"]["json"].as_str().unwrap();
    assert!(
        kept.chars().count() >= 9_000,
        "不截断时应当保留全部字符：{}",
        kept.chars().count()
    );
    assert_eq!(entry["response"]["truncated"], json!(false));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 空回合：失败后的修复

/// 上游 200、一个可见块都没有（实测：思考把 `max_tokens` 吃满时就是这个形状）。
/// 这不能算成功 —— 客户端拿到的是「模型什么都没说」。客户端没要过思考时，
/// 桥该拿同一个账号关掉思考再试一发；修好了就照常回话。
#[tokio::test]
async fn empty_turn_gets_one_thinking_off_retry() {
    let dir = temp_state_dir("empty-turn-retry");
    let _ = std::fs::remove_dir_all(&dir);
    let harness = start(FakeConfig {
        // 第一发：只有收尾与用量，正文一个字没有
        script: vec![chunk(
            json!([]),
            Some("MAX_TOKENS"),
            Some(json!({
                "promptTokenCount": 9,
                "candidatesTokenCount": 0,
                "thoughtsTokenCount": 61
            })),
        )],
        // 关掉思考那一发：正常回话
        no_thinking_script: vec![text_chunk("核聚变。", Some("STOP"))],
        log_dir: Some(dir.clone()),
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "为什么太阳会发光" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("不是 JSON");
    assert_eq!(body["content"][0]["text"], json!("核聚变。"));
    assert_eq!(body["stop_reason"], json!("end_turn"));

    // 上游收到两发：第二发是关掉思考的修复体，第一发一个字没动
    {
        // 锁不跨 await（后面还要等日志落盘）
        let seen = harness.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "应该正好两发（原始 + 修复）");
        assert!(
            seen[0].request["generationConfig"]
                .get("thinkingConfig")
                .is_none(),
            "第一发不该被改：{}",
            seen[0].request
        );
        assert_eq!(
            seen[1].request["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(0)
        );
    }

    // 日志留下痕迹：note 说明修过，warnings 说明怎么修的，正文记的是最后那一发
    let entry = wait_for_entry(&dir.join("requests.jsonl")).await;
    assert_eq!(entry["note"], json!("empty_turn_repaired"));
    let warnings: Vec<&str> = entry["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        warnings.contains(&"empty_turn_retry:thinking_off"),
        "{warnings:?}"
    );
    assert!(warnings.contains(&"empty_turn_repaired"), "{warnings:?}");
    assert_eq!(entry["ok"], json!(true));
    assert_eq!(entry["response"]["text"], json!("核聚变。"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 客户端明确要了思考：桥不许偷偷关掉，所以空回合只能如实报错 ——
/// 不能让客户端拿到一个「什么都没说」的 200。
#[tokio::test]
async fn empty_turn_without_repair_is_an_honest_error() {
    let dir = temp_state_dir("empty-turn-honest");
    let _ = std::fs::remove_dir_all(&dir);
    let empty = chunk(
        json!([]),
        Some("MAX_TOKENS"),
        Some(json!({
            "promptTokenCount": 9,
            "candidatesTokenCount": 0,
            "thoughtsTokenCount": 61
        })),
    );
    let harness = start(FakeConfig {
        script: vec![empty],
        log_dir: Some(dir.clone()),
        ..Default::default()
    })
    .await;

    let wants_thinking = json!({
        "model": "gemini-3.6-flash-high",
        "max_tokens": 2048,
        "thinking": { "type": "enabled", "budget_tokens": 1024 },
        "messages": [{ "role": "user", "content": "慢慢想" }],
    });
    // 整包：还没写过响应头，能给真正的状态码
    let res = post(
        &harness.base,
        "/v1/messages",
        wants_thinking.clone(),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 502);
    let body: Value = res.json().await.expect("不是 JSON");
    assert_eq!(body["type"], json!("error"));
    assert_eq!(body["error"]["type"], json!("api_error"));
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("空回合"),
        "{body}"
    );

    // 流式：200 已经写出去了（思考那几块虽不可见，也开了流），错误只能塞进流里
    let mut streamed = wants_thinking;
    streamed["stream"] = json!(true);
    let res = post(&harness.base, "/v1/messages", streamed, vec![]).await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(text.contains("event: error"), "{text}");
    assert!(text.contains("空回合"), "{text}");

    let entry = wait_for_entry(&dir.join("requests.jsonl")).await;
    assert_eq!(entry["note"], json!("empty_turn"));
    assert_eq!(entry["ok"], json!(false));
    assert_eq!(entry["error"]["http"], json!(502));
    assert_eq!(entry["error"]["reason"], json!("empty_turn"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 流式的空回合：头已经开出去了（上游第一块就到了），修复重试的正文必须接在
/// **同一个 message** 里 —— 不能冒出第二个 `message_start`，也不能先来一个 stop。
#[tokio::test]
async fn streaming_empty_turn_repair_keeps_one_message() {
    let harness = start(FakeConfig {
        script: vec![chunk(
            json!([]),
            Some("MAX_TOKENS"),
            Some(json!({
                "promptTokenCount": 9,
                "candidatesTokenCount": 0,
                "thoughtsTokenCount": 61
            })),
        )],
        no_thinking_script: vec![text_chunk("接上了。", Some("STOP"))],
        ..Default::default()
    })
    .await;

    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 64,
            "stream": true,
            "messages": [{ "role": "user", "content": "为什么太阳会发光" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    let events: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("event: "))
        .collect();
    assert_eq!(
        events.iter().filter(|e| **e == "message_start").count(),
        1,
        "{events:?}"
    );
    assert_eq!(
        events.iter().filter(|e| **e == "message_delta").count(),
        1,
        "{events:?}"
    );
    assert!(!events.contains(&"error"), "{events:?}");
    assert!(text.contains("接上了。"), "{text}");
    assert!(text.contains("\"end_turn\""), "{text}");
    // 正文确实在收尾之前到
    let body_at = text.find("接上了。").unwrap();
    let stop_at = text.find("message_delta").unwrap();
    assert!(body_at < stop_at, "正文该在 message_delta 之前：{text}");
    assert_eq!(harness.seen.lock().unwrap().len(), 2);
}

/// 所有账号都连不上：这是「连不上」，不是「被限流」——
/// 以前会报 429 rate_limit_error，客户端会白退避很久。
#[tokio::test]
async fn transport_failures_report_api_error_not_rate_limit() {
    let harness = start_multi_factory(None, unreachable_factory()).await;
    let res = post(&harness.base, "/v1/messages", message_body(), vec![]).await;
    let retry_after = res.headers().get("retry-after").cloned();
    assert_eq!(res.status(), 502);
    let body: Value = res.json().await.expect("不是 JSON");
    assert_eq!(body["type"], json!("error"));
    assert_eq!(body["error"]["type"], json!("api_error"));
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("连不上上游"),
        "{body}"
    );
    assert!(retry_after.is_none(), "纯网络失败不该给 retry-after");
}

// ---------------------------------------------------------------- 额度耗尽 → 换号

/// 额度耗尽（上游明确 QUOTA_EXHAUSTED）之后的选号：当发换号、后续同家族请求也避开它，
/// 但不跨家族放大 —— 3p 家族照样还能轮到 A。
#[tokio::test]
async fn quota_exhausted_429_switches_account_and_scopes_to_family() {
    let quota_429 = UpstreamError::http(429, "QUOTA_EXHAUSTED", "You exceeded your current quota");
    let (factory, calls) = routing_factory(vec![
        AccountPlan {
            email: "a@x.com",
            script: vec![text_chunk("A 答", Some("STOP"))],
            fail_model: Some(("gemini", quota_429)),
        },
        AccountPlan {
            email: "b@x.com",
            script: vec![text_chunk("B 答", Some("STOP"))],
            fail_model: None,
        },
    ]);
    let harness = start_multi_factory(None, factory).await;
    let model = "gemini-3.8-flash-tiered";

    // 第一发：A 按 id 序先被选，gemini 上额度耗尽 429 → pump 当场换成 B。
    let first = post(
        &harness.base,
        "/v1/messages",
        body_for(model, "第一发"),
        vec![],
    )
    .await;
    assert_eq!(first.status(), 200);
    assert_eq!(
        first.headers()["x-bridge-account"].to_str().unwrap(),
        "b***@x.com"
    );
    assert_eq!(
        calls_for(&calls, "a@x.com"),
        vec![model.to_string()],
        "A 该只被试一次"
    );
    assert_eq!(calls_for(&calls, "b@x.com"), vec![model.to_string()]);

    // 第二发（同模型）：A 的 gemini 家族已被标耗尽，选号直接跳过它。
    let second = post(
        &harness.base,
        "/v1/messages",
        body_for(model, "第二发"),
        vec![],
    )
    .await;
    assert_eq!(second.status(), 200);
    assert_eq!(
        second.headers()["x-bridge-account"].to_str().unwrap(),
        "b***@x.com"
    );
    assert_eq!(
        calls_for(&calls, "a@x.com"),
        vec![model.to_string()],
        "第二发不该再派给额度耗尽的 A"
    );

    // 同家族另一个模型：也得避开 A —— 额度是家族级的，不是模型级的。
    let sibling = "gemini-3.6-flash-high";
    let third = post(
        &harness.base,
        "/v1/messages",
        body_for(sibling, "换个 gemini"),
        vec![],
    )
    .await;
    assert_eq!(third.status(), 200);
    assert_eq!(
        third.headers()["x-bridge-account"].to_str().unwrap(),
        "b***@x.com"
    );
    assert_eq!(
        calls_for(&calls, "a@x.com"),
        vec![model.to_string()],
        "同家族的另一个 gemini 也不该再打 A"
    );

    // 3p 家族不受影响：A 的 3p 额度是好的，应该还能轮到它。
    let claude = "claude-sonnet-4-6";
    let fourth = post(
        &harness.base,
        "/v1/messages",
        body_for(claude, "换个家族"),
        vec![],
    )
    .await;
    assert_eq!(fourth.status(), 200);
    assert_eq!(
        fourth.headers()["x-bridge-account"].to_str().unwrap(),
        "a***@x.com"
    );
    assert_eq!(
        calls_for(&calls, "a@x.com"),
        vec![model.to_string(), claude.to_string()],
        "gemini 的耗尽标记不该把 A 的 3p 也连坐"
    );
}

/// 纯限流（RATE_LIMIT）只记到具体模型：只有那个模型会换号，同家族别的模型照常用 A。
#[tokio::test]
async fn pure_rate_limit_429_stays_scoped_to_the_model() {
    let limit = UpstreamError::http(429, "RATE_LIMIT", "rate limit exceeded");
    let (factory, calls) = routing_factory(vec![
        AccountPlan {
            email: "a@x.com",
            script: vec![text_chunk("A 答", Some("STOP"))],
            fail_model: Some(("gemini-3.8-flash-tiered", limit)),
        },
        AccountPlan {
            email: "b@x.com",
            script: vec![text_chunk("B 答", Some("STOP"))],
            fail_model: None,
        },
    ]);
    let harness = start_multi_factory(None, factory).await;
    let limited = "gemini-3.8-flash-tiered";

    // A 在这个模型上吃纯限流：当发换到 B。
    let first = post(
        &harness.base,
        "/v1/messages",
        body_for(limited, "限流一发"),
        vec![],
    )
    .await;
    assert_eq!(first.status(), 200);
    assert_eq!(
        first.headers()["x-bridge-account"].to_str().unwrap(),
        "b***@x.com"
    );
    assert_eq!(calls_for(&calls, "a@x.com"), vec![limited.to_string()]);

    // 同一个模型再来：A 还在模型级熔断冷却里，B 顶上，A 不该再被叫。
    let second = post(
        &harness.base,
        "/v1/messages",
        body_for(limited, "限流二发"),
        vec![],
    )
    .await;
    assert_eq!(second.status(), 200);
    assert_eq!(
        second.headers()["x-bridge-account"].to_str().unwrap(),
        "b***@x.com"
    );
    assert_eq!(calls_for(&calls, "a@x.com"), vec![limited.to_string()]);

    // 同家族另一个模型：纯限流不封家族，A 应该还能被选中并答上来。
    let sibling = "gemini-3.6-flash-high";
    let third = post(
        &harness.base,
        "/v1/messages",
        body_for(sibling, "同家族另一发"),
        vec![],
    )
    .await;
    assert_eq!(third.status(), 200);
    assert_eq!(
        third.headers()["x-bridge-account"].to_str().unwrap(),
        "a***@x.com",
        "同家族的另一个模型该还能用 A"
    );
    assert_eq!(
        calls_for(&calls, "a@x.com"),
        vec![limited.to_string(), sibling.to_string()]
    );
}

/// 两个号都没额度：不许空手，也不许假装成功 —— 如实地按「候选都被上游拒了」回 429。
#[tokio::test]
async fn all_accounts_quota_exhausted_reports_rate_limit_honestly() {
    let quota_429 = UpstreamError::http(429, "QUOTA_EXHAUSTED", "quota exceeded");
    let (factory, calls) = routing_factory(vec![
        AccountPlan {
            email: "a@x.com",
            script: vec![],
            fail_model: Some(("gemini", quota_429.clone())),
        },
        AccountPlan {
            email: "b@x.com",
            script: vec![],
            fail_model: Some(("gemini", quota_429.clone())),
        },
    ]);
    let harness = start_multi_factory(None, factory).await;
    let model = "gemini-3.8-flash-tiered";

    let res = post(
        &harness.base,
        "/v1/messages",
        body_for(model, "都没额度"),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 429);
    let body: Value = res.json().await.expect("不是 JSON");
    assert_eq!(body["type"], json!("error"));
    assert_eq!(body["error"]["type"], json!("rate_limit_error"));
    // 全都试过，不是只撞第一个就放弃。
    assert_eq!(calls_for(&calls, "a@x.com"), vec![model.to_string()]);
    assert_eq!(calls_for(&calls, "b@x.com"), vec![model.to_string()]);
}

/// 额度快照说「这个家族一点不剩」时，选号直接跳过它，连上游都不打。
#[tokio::test]
async fn zero_remaining_snapshot_avoids_the_account_without_upstream_call() {
    let (factory, calls) = routing_factory(vec![
        AccountPlan {
            email: "a@x.com",
            script: vec![text_chunk("A 答", Some("STOP"))],
            fail_model: None,
        },
        AccountPlan {
            email: "b@x.com",
            script: vec![text_chunk("B 答", Some("STOP"))],
            fail_model: None,
        },
    ]);
    let harness = start_multi_factory(None, factory).await;
    // A 的 gemini 组剩余 0%：A 该被排到最后，选号直接找没有快照的 B（未知不算耗尽）。
    harness
        .bridge
        .pool()
        .note_quota("bf00c418aaaa", &quota_summary(0.0, 50.0));
    let model = "gemini-3.8-flash-tiered";

    let res = post(
        &harness.base,
        "/v1/messages",
        body_for(model, "快照没额度"),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers()["x-bridge-account"].to_str().unwrap(),
        "b***@x.com"
    );
    assert!(
        calls_for(&calls, "a@x.com").is_empty(),
        "快照说 A 的 gemini 只剩 0%，就不该再往上游打它"
    );
    assert_eq!(calls_for(&calls, "b@x.com"), vec![model.to_string()]);
}

// ---------------------------------------------------------------- Qoder 上游（`qoder/` 前缀路由）

/// 假 Qoder 会话：吐 Gemini 形状的 chunk（pump 与两条协议的翻译器只认这个形状），
/// 并记下收到的请求，好断言「qoder 前缀的请求只进 Qoder 池」。
struct FakeQoderSession {
    seen: Arc<Mutex<Vec<GenerateRequest>>>,
    script: Vec<Value>,
    quota: Value,
}

impl FakeQoderSession {
    fn new(seen: Arc<Mutex<Vec<GenerateRequest>>>, script: Vec<Value>, quota: Value) -> Self {
        Self {
            seen,
            script,
            quota,
        }
    }
}

#[async_trait]
impl Session for FakeQoderSession {
    fn identity(&self) -> Value {
        json!({
            "provider": "qoder",
            "email": "q***@qoder.com",
            "uid": "uid-1",
            "jobTokenValid": true,
            "expiresAt": "2026-10-01T00:00:00Z",
        })
    }

    async fn load_code_assist(&self) -> Result<Value, UpstreamError> {
        Ok(Value::Null)
    }

    async fn models(&self) -> Result<Value, UpstreamError> {
        Ok(json!({
            "models": { "auto": { "displayName": "Auto", "isReasoning": true } },
            "defaultAgentModelId": "auto",
        }))
    }

    async fn quota(&self) -> Result<QuotaSnapshot, UpstreamError> {
        Ok(QuotaSnapshot::default())
    }

    async fn quota_raw(&self) -> Result<Value, UpstreamError> {
        Ok(self.quota.clone())
    }

    async fn resolve_model(&self, requested: &str) -> ResolvedModel {
        ResolvedModel {
            model: requested.to_string(),
            substituted_from: None,
            reason: "exact".to_string(),
        }
    }

    async fn stream_generate(
        &self,
        req: GenerateRequest,
        tx: Sender<StreamEvent>,
    ) -> Result<(), UpstreamError> {
        self.seen.lock().unwrap().push(req);
        let _ = tx
            .send(StreamEvent::Open {
                endpoint:
                    "https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation"
                        .to_string(),
            })
            .await;
        for item in &self.script {
            if tx.send(StreamEvent::Chunk(item.clone())).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

struct QoderHarness {
    base: String,
    antigravity_seen: Arc<Mutex<Vec<GenerateRequest>>>,
    qoder_seen: Arc<Mutex<Vec<GenerateRequest>>>,
    /// 桥要活着：drop 了服务就没了
    _bridge: Arc<Bridge>,
}

/// 一座同时挂着 Antigravity 与 Qoder 两个单账号池的桥。
async fn start_qoder() -> QoderHarness {
    let antigravity_seen = Arc::new(Mutex::new(Vec::new()));
    let qoder_seen = Arc::new(Mutex::new(Vec::new()));
    let antigravity: Arc<dyn Session> = Arc::new(FakeSession {
        script: vec![text_chunk("来自 Antigravity", Some("STOP"))],
        no_thinking_script: Vec::new(),
        fail: None,
        fail_model: None,
        no_data: None,
        models: Some(json!({
            "models": { "gemini-3.6-flash-high": { "displayName": "Flash" } },
            "defaultAgentModelId": "gemini-3.6-flash-high",
        })),
        seen: Arc::clone(&antigravity_seen),
        calls: None,
        resolve: ResolveMode::Echo,
    });
    let qoder: Arc<dyn Session> = Arc::new(FakeQoderSession::new(
        Arc::clone(&qoder_seen),
        vec![text_chunk("来自 Qoder", Some("STOP"))],
        json!({
            "userQuota": { "total": 2000, "used": 653, "remaining": 1347, "unit": "credits" },
            "orgResourcePackage": { "total": 0, "used": 0, "remaining": 0 },
            "totalUsagePercentage": 32.65,
            "isQuotaExceeded": false,
            "expiresAt": "2026-10-01T00:00:00Z",
        }),
    ));
    let bridge = Arc::new(Bridge::new(BridgeOptions {
        pool: Pool::Single(Arc::new(SingleAccountPool::new(antigravity))),
        qoder: Some(Pool::Single(Arc::new(SingleAccountPool::new(qoder)))),
        store: shared_signatures(),
        log_dir: None,
        state_dir: None,
        log_bodies: true,
        body_limit: 0,
        api_key: None,
        allow_restart: Some(false),
        log: Arc::new(|_msg: String| {}),
        version: "0.0.0-test".to_string(),
    }));
    let addr = dango_bridge::server::serve(Arc::clone(&bridge), "127.0.0.1", 0)
        .await
        .expect("起服务失败");
    QoderHarness {
        base: format!("http://{addr}"),
        antigravity_seen,
        qoder_seen,
        _bridge: bridge,
    }
}

#[tokio::test]
async fn qoder_prefix_routes_only_to_the_qoder_pool() {
    let harness = start_qoder().await;

    // `qoder/` 前缀 → 只进 Qoder 池
    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "qoder/auto",
            "max_tokens": 32,
            "stream": true,
            "messages": [{ "role": "user", "content": "你好" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.expect("读响应");
    assert_eq!(delta_text(&parse_frames(&text)), "来自 Qoder");
    assert_eq!(harness.qoder_seen.lock().unwrap().len(), 1);
    assert!(
        harness.antigravity_seen.lock().unwrap().is_empty(),
        "qoder 前缀的请求不该碰 Antigravity 池"
    );

    // 普通模型 → 只进 Antigravity 池
    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 32,
            "stream": true,
            "messages": [{ "role": "user", "content": "你好" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.expect("读响应");
    assert_eq!(delta_text(&parse_frames(&text)), "来自 Antigravity");
    assert_eq!(harness.antigravity_seen.lock().unwrap().len(), 1);
    assert_eq!(
        harness.qoder_seen.lock().unwrap().len(),
        1,
        "普通模型不该再进 Qoder 池"
    );
}

#[tokio::test]
async fn quota_qoder_passes_through_the_upstream_shape() {
    let harness = start_qoder().await;
    let body: Value = reqwest::get(format!("{}/quota/qoder", harness.base))
        .await
        .expect("请求 /quota/qoder")
        .json()
        .await
        .expect("JSON");
    assert_eq!(body["provider"], json!("qoder"));
    assert_eq!(body["enabled"], json!(true));
    // 上游原始字段原样透传
    assert_eq!(body["userQuota"]["remaining"], json!(1347));
    assert_eq!(body["isQuotaExceeded"], json!(false));
    // 外加一条归一化摘要：1347/2000 = 67.4%
    assert_eq!(body["summary"][0]["group"], json!("Qoder"));
    assert_eq!(body["summary"][0]["remainingPercent"], json!(67.4));
}

#[tokio::test]
async fn models_merge_antigravity_and_qoder_with_prefix_and_owner() {
    let harness = start_qoder().await;
    let body: Value = reqwest::get(format!("{}/v1/models", harness.base))
        .await
        .expect("请求 /v1/models")
        .json()
        .await
        .expect("JSON");
    let data = body["data"].as_array().expect("data 数组");
    let qoder = data
        .iter()
        .find(|m| m["id"] == json!("qoder/auto"))
        .expect("Qoder 模型要在表里");
    assert_eq!(qoder["owned_by"], json!("qoder"));
    let antigravity = data
        .iter()
        .find(|m| m["id"] == json!("gemini-3.6-flash-high"))
        .expect("Antigravity 模型要在表里");
    assert_eq!(antigravity["owned_by"], json!("antigravity"));

    // 单个模型：路径里的 `/` 要转义，`%2F` 解出来还是 `qoder/auto`
    let one: Value = reqwest::get(format!("{}/v1/models/qoder%2Fauto", harness.base))
        .await
        .expect("请求单个模型")
        .json()
        .await
        .expect("JSON");
    assert_eq!(one["id"], json!("qoder/auto"));
    assert_eq!(one["owned_by"], json!("qoder"));
}

#[tokio::test]
async fn qoder_prefix_without_config_reports_not_enabled() {
    // 默认 harness（qoder: None）：前缀请求要明确报「未启用」，不能悄悄落到 Antigravity
    let harness = start(FakeConfig::default()).await;
    let res = post(
        &harness.base,
        "/v1/messages",
        json!({
            "model": "qoder/auto",
            "max_tokens": 16,
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }],
        }),
        vec![],
    )
    .await;
    assert_eq!(res.status(), 502);
    let body: Value = res.json().await.expect("JSON");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Qoder"),
        "要说清 Qoder 没启用：{body}"
    );
}
