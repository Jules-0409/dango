//! 原生后端：直接和 Cursor 的 agent 服务对话，不拉起 CLI。
//!
//! 协议是抓包实测算出来的（`--agent-endpoint` 指到本地监听 + 一个 h2c 转发）：
//!
//! - 传输：HTTP/2 + Connect 流式协议（`content-type: application/connect+proto`）。每个消息是
//!   `[flag u8][len u32 大端][body]`；`flag & 1` 表示 body 是 gzip，`flag & 2` 是结束帧，
//!   结束帧的 body 是 JSON（正常 `{}`，出错 `{"error":…}`）。
//! - 方法：`POST {agent_endpoint}/agent.v1.AgentService/Run`（双向流）。
//! - 认证：先用 API key 换 access token（`POST {api_endpoint}/auth/exchange_user_api_key`，
//!   `Authorization: Bearer <API key>`，body `{}`），之后每请求带 `Bearer <access token>`。
//! - 请求第一帧（proto）：`1 { 1: "", 2 { 1 { 1 { 1: 用户消息, 2: 消息 id, 4: 1 } } }, 4: "",
//!   5/16: 会话 id, 9: 模型, 12: 0, 14: 模型清单, 25: 请求 id }`。
//! - 响应：正文增量 `1.1.1`，思考 `1.4.1`，用量 `1.14`（1 输入 / 2 输出 / 3 缓存读 / 4 缓存写）。
//!
//! 抓到的完整往返在 `tests/` 里有字节级夹具。
//!
//! **边界（很重要）**：这一层拿到的仍然不是「纯模型」。系统提示、工具清单、agent 循环都在
//! 服务端 —— 抓包里能直接看到服务端把 `You are an AI coding assistant, powered by Cursor
//! Grok 4.6…` 这段 system prompt 发回来。所以它对外依然是个聊天反代，区别只是更轻（没有
//! 进程启动）、能逐字出（正文本来就是增量帧）。
//!
//! 已知取舍：客户端工具调用不做（服务端要什么工具我们就当没看见，`--mode ask` 那套只读语义
//! 在这里由服务端自己决定）；图片暂不支持。

pub mod proto;

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::cli::{TurnRequest, Usage};
use crate::error::{Error, Result};
use crate::turn::{Step, TurnOutcome};

use self::proto::{bytes_field, message_field, string_field, varint_field, Field, Reader};

/// 响应里我们关心的字段号（抓包实测）。
const R_EVENT: u32 = 1;
const R_TEXT: u32 = 1;
/// 正文增量的那一层（`1.1.1`）。
const R_TEXT_DELTA: u32 = 1;
const R_THINKING: u32 = 4;
const R_THINKING_TEXT: u32 = 1;
const R_USAGE: u32 = 14;

/// Connect 帧头的标志位。
const FLAG_COMPRESSED: u8 = 0b0000_0001;
const FLAG_END: u8 = 0b0000_0010;

/// 心跳间隔：CLI 每几秒发一个空 `field 7`，不然后面的中间层会掐流。
const HEARTBEAT: Duration = Duration::from_secs(10);
/// access token 缓存时长。
const TOKEN_TTL: Duration = Duration::from_secs(25 * 60);
/// 没装 CLI 时兜底用的客户端版本号（服务端按它做能力开关）。
const FALLBACK_CLIENT_VERSION: &str = "cli-2026.09.18-9a7762b";

#[derive(Debug, Clone)]
pub struct Native {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    api_endpoint: String,
    agent_endpoint: String,
    client_version: String,
    /// 请求头里那个 32 字节随机 key（抓包里叫 `x-blob-encryption-key`，64 位十六进制）。
    blob_key: String,
    /// 对 agent 端点：强制 HTTP/2。
    h2: reqwest::Client,
    /// 对 api 端点：普通请求。
    http: reqwest::Client,
    /// API key → (access token, 拿到的时间)。
    tokens: Mutex<HashMap<String, (String, Instant)>>,
}

impl Native {
    pub fn new(api_endpoint: &str, agent_endpoint: &str) -> Result<Self> {
        let h2 = reqwest::Client::builder()
            // agent 服务只讲 HTTP/2，且没有 ALPN 之外的协商余地
            .http2_prior_knowledge()
            .use_rustls_tls()
            .build()
            .map_err(|e| Error::Native {
                message: format!("建 HTTP/2 客户端失败：{e}"),
            })?;
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .map_err(|e| Error::Native {
                message: format!("建 HTTP 客户端失败：{e}"),
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                api_endpoint: api_endpoint.trim_end_matches('/').to_string(),
                agent_endpoint: agent_endpoint.trim_end_matches('/').to_string(),
                client_version: detect_client_version(),
                blob_key: random_hex(32),
                h2,
                http,
                tokens: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn agent_endpoint(&self) -> &str {
        &self.inner.agent_endpoint
    }

    pub fn client_version(&self) -> &str {
        &self.inner.client_version
    }

    /// 用 API key 换 access token（带缓存）。
    async fn token(&self, api_key: &str) -> Result<String> {
        {
            let cache = self.inner.tokens.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((token, at)) = cache.get(api_key) {
                if at.elapsed() < TOKEN_TTL {
                    return Ok(token.clone());
                }
            }
        }
        let url = format!("{}/auth/exchange_user_api_key", self.inner.api_endpoint);
        let resp = self
            .inner
            .http
            .post(url)
            .header("authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", &self.inner.client_version)
            .body("{}")
            .send()
            .await
            .map_err(|e| Error::Native {
                message: format!("换 access token 失败：{e}"),
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Native {
                message: format!(
                    "换 access token 被拒（HTTP {status}）：{}",
                    truncate(&text, 200)
                ),
            });
        }
        let value: Value = serde_json::from_str(&text)?;
        let token = value
            .get("accessToken")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Native {
                message: "换 token 的响应里没有 accessToken".into(),
            })?
            .to_string();
        self.inner
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(api_key.to_string(), (token.clone(), Instant::now()));
        Ok(token)
    }

    fn headers(&self, token: &str, request_id: &str) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        let mut put = |k: &'static str, v: &str| -> Result<()> {
            h.insert(
                k,
                HeaderValue::from_str(v).map_err(|_| Error::Native {
                    message: format!("请求头 {k} 的值不合法"),
                })?,
            );
            Ok(())
        };
        put("content-type", "application/connect+proto")?;
        put("connect-protocol-version", "1")?;
        put("connect-accept-encoding", "gzip")?;
        put("authorization", &format!("Bearer {token}"))?;
        put("user-agent", "connect-es/1.6.1")?;
        put("x-cursor-client-type", "cli")?;
        put("x-cursor-client-version", &self.inner.client_version)?;
        put("x-ghost-mode", "false")?;
        put("x-request-id", request_id)?;
        put("x-blob-encryption-key", &self.inner.blob_key)?;
        Ok(h)
    }

    /// 跑一轮。事件形状和 CLI 后端一致（[`Step`]），所以上层不用分叉。
    pub async fn run_turn<F>(
        &self,
        req: &TurnRequest,
        timeout_secs: u64,
        mut on_step: F,
    ) -> Result<TurnOutcome>
    where
        F: FnMut(Step) + Send,
    {
        let api_key = req.api_key.clone().ok_or_else(|| Error::Native {
            message: "原生后端需要 API key：写进 accounts.json，或设 CURSOR_BRIDGE_API_KEY".into(),
        })?;
        let token = self.token(&api_key).await?;

        let requested = req.model.clone().unwrap_or_else(|| "auto".to_string());
        let (slug, params) = parse_model(&requested);
        let request_id = uuid::Uuid::new_v4().to_string();

        let mut outcome = TurnOutcome {
            model: Some(requested.clone()),
            ..Default::default()
        };
        on_step(Step::Init {
            model: Some(requested.clone()),
            session_id: None,
        });

        // 请求体是一个「先发第一帧、之后按心跳续着」的流
        let (tx, rx) = mpsc::channel::<std::result::Result<Vec<u8>, std::io::Error>>(8);
        let first = connect_frame(&build_run_frame(&req.prompt, &slug, &params, &request_id));
        let feeder = tokio::spawn(async move {
            if tx.send(Ok(first)).await.is_err() {
                return;
            }
            let mut tick = tokio::time::interval(HEARTBEAT);
            tick.tick().await;
            loop {
                tick.tick().await;
                let mut beat = Vec::new();
                bytes_field(&mut beat, 7, &[]);
                if tx.send(Ok(beat)).await.is_err() {
                    return;
                }
            }
        });

        let url = format!("{}/agent.v1.AgentService/Run", self.inner.agent_endpoint);
        let resp = self
            .inner
            .h2
            .post(url)
            .headers(self.headers(&token, &request_id)?)
            .body(reqwest::Body::wrap_stream(ReceiverStream::new(rx)))
            .send()
            .await
            .map_err(|e| Error::Native {
                message: format!("连 agent 服务失败：{e}"),
            })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            feeder.abort();
            return Err(Error::Native {
                message: format!("agent 服务返回 HTTP {status}：{}", truncate(&text, 300)),
            });
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut thinking_open = false;
        let mut trailer_error: Option<String> = None;

        let pump = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| Error::Native {
                    message: format!("读 agent 流失败：{e}"),
                })?;
                buf.extend_from_slice(&chunk);
                while let Some(frame) = take_frame(&mut buf) {
                    if let FrameOutcome::End(err) = consume_frame(
                        &frame.body,
                        frame.flag,
                        &mut outcome,
                        &mut on_step,
                        &mut thinking_open,
                    ) {
                        trailer_error = err;
                        return Ok::<(), Error>(());
                    }
                }
            }
            Ok(())
        };

        let pumped = tokio::time::timeout(Duration::from_secs(timeout_secs), pump).await;
        feeder.abort();
        match pumped {
            Err(_) => return Err(Error::Timeout { secs: timeout_secs }),
            Ok(Err(err)) => return Err(err),
            Ok(Ok(())) => {}
        }

        if thinking_open {
            on_step(Step::ThinkingDone);
        }
        if let Some(err) = trailer_error {
            outcome.is_error = true;
            outcome.notes.push(err.clone());
            on_step(Step::Note(err));
        }
        if !outcome.saw_event {
            return Err(Error::EmptyTurn {
                code: None,
                stderr: "原生后端没拿到任何事件".into(),
            });
        }
        if !outcome.is_error && outcome.text.is_empty() {
            return Err(Error::EmptyTurn {
                code: None,
                stderr: truncate(&outcome.notes.join(" | "), 300),
            });
        }
        on_step(Step::Done(outcome.clone()));
        Ok(outcome)
    }
}

/// 从流缓冲里切出一个完整的 Connect 帧。
struct Frame {
    flag: u8,
    body: Vec<u8>,
}

fn take_frame(buf: &mut Vec<u8>) -> Option<Frame> {
    if buf.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return None;
    }
    let flag = buf[0];
    let body = buf[5..5 + len].to_vec();
    buf.drain(..5 + len);
    Some(Frame { flag, body })
}

enum FrameOutcome {
    Ignored,
    /// 结束帧；`Some` 是里面的错误文本。
    End(Option<String>),
}

/// 吃一帧响应，把正文/思考/用量变成 [`Step`]。
fn consume_frame<F: FnMut(Step)>(
    body: &[u8],
    flag: u8,
    outcome: &mut TurnOutcome,
    on_step: &mut F,
    thinking_open: &mut bool,
) -> FrameOutcome {
    if flag & FLAG_END != 0 {
        // 结束帧的 body 是 JSON：正常 `{}`，出错带 error
        let text = std::str::from_utf8(body).unwrap_or("");
        let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let error = value
            .get("error")
            .filter(|e| !e.is_null())
            .map(|e| format!("agent 服务报错：{}", truncate(&e.to_string(), 300)));
        return FrameOutcome::End(error);
    }
    let decoded;
    let body = if flag & FLAG_COMPRESSED != 0 {
        match gunzip(body) {
            Some(raw) => {
                decoded = raw;
                decoded.as_slice()
            }
            None => body,
        }
    } else {
        body
    };

    let mut r = Reader::new(body);
    while let Some((field, value)) = r.next_field() {
        if field != R_EVENT {
            continue;
        }
        let Some(mut event) = value.reader() else {
            continue;
        };
        while let Some((f, v)) = event.next_field() {
            match f {
                R_TEXT => {
                    let Some(mut text) = v.reader() else { continue };
                    let Some(delta) = text.find(R_TEXT_DELTA).and_then(|d| d.str()) else {
                        continue;
                    };
                    if delta.is_empty() {
                        continue;
                    }
                    if *thinking_open {
                        on_step(Step::ThinkingDone);
                        *thinking_open = false;
                    }
                    outcome.saw_event = true;
                    outcome.text.push_str(delta);
                    on_step(Step::Text(delta.to_string()));
                }
                R_THINKING => {
                    let Some(mut think) = v.reader() else {
                        continue;
                    };
                    let Some(delta) = think.find(R_THINKING_TEXT).and_then(|d| d.str()) else {
                        continue;
                    };
                    if delta.is_empty() {
                        continue;
                    }
                    outcome.saw_event = true;
                    outcome.thinking.push_str(delta);
                    *thinking_open = true;
                    on_step(Step::Thinking(delta.to_string()));
                }
                R_USAGE => {
                    let Some(mut usage) = v.reader() else {
                        continue;
                    };
                    let mut parsed = Usage::default();
                    if let Some(Field::Varint(v)) = usage.find(1) {
                        parsed.input_tokens = v;
                    }
                    if let Some(Field::Varint(v)) = usage.find(2) {
                        parsed.output_tokens = v;
                    }
                    if let Some(Field::Varint(v)) = usage.find(3) {
                        parsed.cache_read_tokens = v;
                    }
                    if let Some(Field::Varint(v)) = usage.find(4) {
                        parsed.cache_write_tokens = v;
                    }
                    outcome.usage = parsed;
                }
                _ => {}
            }
        }
    }
    FrameOutcome::Ignored
}

fn gunzip(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// 给请求体加 Connect 帧头（不压缩，抓包里第一帧就是未压缩的）。
fn connect_frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(0);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// 拼 `Run` 请求的第一帧（字段布局照抓包）。
fn build_run_frame(
    prompt: &str,
    slug: &str,
    params: &[(String, String)],
    request_id: &str,
) -> Vec<u8> {
    let message_id = uuid::Uuid::new_v4().to_string();
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let details = model_details(slug, params);
    let mut root = Vec::new();
    message_field(&mut root, 1, |m| {
        bytes_field(m, 1, &[]);
        message_field(m, 2, |l1| {
            message_field(l1, 1, |l2| {
                message_field(l2, 1, |msg| {
                    string_field(msg, 1, prompt);
                    string_field(msg, 2, &message_id);
                    bytes_field(msg, 3, &[]);
                    varint_field(msg, 4, 1);
                });
            });
        });
        bytes_field(m, 4, &[]);
        string_field(m, 5, &conversation_id);
        bytes_field(m, 9, &details);
        varint_field(m, 12, 0);
        // 抓包里这段是「客户端已知模型清单」，我们只报当前用的那个
        bytes_field(m, 14, &details);
        string_field(m, 16, &conversation_id);
        string_field(m, 25, request_id);
    });
    root
}

/// `RunnerDetails`：`1: 模型 slug`，`3` 是若干 `{1: 参数名, 2: 值}`。
fn model_details(slug: &str, params: &[(String, String)]) -> Vec<u8> {
    let mut md = Vec::new();
    string_field(&mut md, 1, slug);
    for (key, value) in params {
        message_field(&mut md, 3, |p| {
            string_field(p, 1, key);
            string_field(p, 2, value);
        });
    }
    md
}

/// 把桥的模型名翻成上游要的 `(slug, 参数)`。
///
/// `cursor-grok-4.6-high` → `("grok-4.6", [effort=high, fast=false])`；
/// `cursor-grok-4.6-high-fast` → 同上但 `fast=true`。
/// 档位参数名按模型族选（抓包里 grok 用 `effort`、gpt-5 用 `reasoning`、gemini 用
/// `reasoning_effort`），没有档位就只带 `fast`。
pub fn parse_model(id: &str) -> (String, Vec<(String, String)>) {
    let mut rest = id.strip_prefix("cursor-").unwrap_or(id).trim();
    let mut fast = false;
    if let Some(base) = rest.strip_suffix("-fast") {
        rest = base;
        fast = true;
    }
    let mut level = None;
    // 长的放前面：`xhigh` 不能被 `high` 先吃掉
    for candidate in [
        "minimal",
        "extra-high",
        "xhigh",
        "none",
        "medium",
        "low",
        "max",
        "high",
    ] {
        let suffix = format!("-{candidate}");
        if let Some(base) = rest.strip_suffix(&suffix) {
            rest = base;
            level = Some(candidate.to_string());
            break;
        }
    }
    let mut params = Vec::new();
    if let Some(level) = level {
        let key = if rest.starts_with("gemini") {
            "reasoning_effort"
        } else if rest.starts_with("gpt-5") {
            "reasoning"
        } else {
            "effort"
        };
        params.push((key.to_string(), level));
    }
    params.push((
        "fast".to_string(),
        if fast { "true" } else { "false" }.to_string(),
    ));
    let slug = if rest.is_empty() { "auto" } else { rest };
    (slug.to_string(), params)
}

/// 客户端版本号：优先环境变量，其次照抄本机 CLI 的版本目录名。
fn detect_client_version() -> String {
    if let Ok(v) = std::env::var("CURSOR_BRIDGE_CLIENT_VERSION") {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(home) = crate::cli::home_dir() {
        let dir = home.join(".local/share/cursor-agent/versions");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        if let Some(newest) = names.pop() {
            return format!("cli-{newest}");
        }
    }
    FALLBACK_CLIENT_VERSION.to_string()
}

fn random_hex(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes * 2);
    // 用 uuid 的随机源拼够位数，省一个依赖
    while out.len() < bytes * 2 {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(bytes * 2);
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 抓包里 res-f10.bin：一帧正文增量（`1.1.1 = "在"`）。
    const CAPTURED_TEXT: &[u8] = &[
        0x0a, 0x0f, 0x0a, 0x05, 0x0a, 0x03, 0xe5, 0x9c, 0xa8, 0xc8, 0x01, 0xac, 0xb9, 0xcb, 0xe3,
        0x8b, 0x34,
    ];
    /// 抓包里 res-f7.bin：一帧思考（`1.4.1`）。
    const CAPTURED_THINKING: &[u8] = &[
        0x0a, 0x32, 0x22, 0x28, 0x0a, 0x24, 0xe7, 0x94, 0xa8, 0xe6, 0x88, 0xb7, 0xe8, 0xa6, 0x81,
        0xe6, 0xb1, 0x82, 0xe4, 0xbb, 0x85, 0xe7, 0x94, 0xa8, 0xe4, 0xb8, 0x89, 0xe4, 0xb8, 0xaa,
        0xe5, 0xad, 0x97, 0xe5, 0x9b, 0x9e, 0xe5, 0xa4, 0x8d, 0xe3, 0x80, 0x82, 0x10, 0x01, 0xc8,
        0x01, 0xaa, 0xb9, 0xcb, 0xe3, 0x8b, 0x34,
    ];

    fn field_of(reader: &mut Reader<'_>, want: u32) -> Vec<u8> {
        match reader.find(want).expect("字段应该在") {
            Field::Bytes(b) => b.to_vec(),
            Field::Varint(v) => v.to_string().into_bytes(),
        }
    }

    #[test]
    fn model_id_parses_into_slug_and_params() {
        assert_eq!(
            parse_model("cursor-grok-4.6-high"),
            (
                "grok-4.6".to_string(),
                vec![
                    ("effort".to_string(), "high".to_string()),
                    ("fast".to_string(), "false".to_string())
                ]
            )
        );
        assert_eq!(
            parse_model("cursor-grok-4.6-xhigh-fast"),
            (
                "grok-4.6".to_string(),
                vec![
                    ("effort".to_string(), "xhigh".to_string()),
                    ("fast".to_string(), "true".to_string())
                ]
            )
        );
        // 档位参数名按模型族走
        assert_eq!(parse_model("cursor-gpt-5.6-sol-medium").1[0].0, "reasoning");
        assert_eq!(
            parse_model("cursor-gemini-3.8-flash-high").1[0].0,
            "reasoning_effort"
        );
        // 没有档位就只带 fast
        assert_eq!(
            parse_model("cursor-composer-2.5-fast"),
            (
                "composer-2.5".to_string(),
                vec![("fast".to_string(), "true".to_string())]
            )
        );
        // auto 也要能用
        assert_eq!(parse_model("auto").0, "auto");
        assert_eq!(parse_model("cursor-").0, "auto");
    }

    /// 请求帧结构要和抓包一致：正文落在 `1.2.1.1.1`，模型落在 `1.9`。
    #[test]
    fn run_frame_matches_captured_layout() {
        let frame = build_run_frame(
            "只回答三个字：在的",
            "grok-4.6",
            &[
                ("effort".to_string(), "high".to_string()),
                ("fast".to_string(), "false".to_string()),
            ],
            "3bb3ad65-c469-45ef-bb1e-9c350c47123f",
        );
        let mut root = Reader::new(&frame);
        // 字段必须按顺序读（Reader 只往前走）
        let one = field_of(&mut root, 1);
        let mut m = Reader::new(&one);

        // 1.1 空
        assert!(matches!(m.find(1), Some(Field::Bytes(b)) if b.is_empty()));
        // 1.2.1.1.1 = 正文
        let mut l1 = m.find(2).unwrap().reader().unwrap();
        let mut l2 = l1.find(1).unwrap().reader().unwrap();
        let mut msg = l2.find(1).unwrap().reader().unwrap();
        assert_eq!(msg.find(1).unwrap().str().unwrap(), "只回答三个字：在的");
        // 空的占位字段也在
        assert!(matches!(m.find(4), Some(Field::Bytes(b)) if b.is_empty()));
        // 1.5 / 1.16 是同一个会话 id
        let conv = m.find(5).unwrap().str().unwrap().to_string();
        assert!(!conv.is_empty());
        // 1.9 模型（顺序上在 12/14/16 之前）
        let mut details = m.find(9).unwrap().reader().unwrap();
        assert_eq!(details.find(1).unwrap().str().unwrap(), "grok-4.6");
        assert_eq!(m.find(16).unwrap().str().unwrap(), conv);
        // 1.25 请求 id
        assert_eq!(
            m.find(25).unwrap().str().unwrap(),
            "3bb3ad65-c469-45ef-bb1e-9c350c47123f"
        );
    }

    #[test]
    fn connect_frame_prefix_is_big_endian_length() {
        let framed = connect_frame(&[0x0a, 0x00]);
        assert_eq!(framed, vec![0x00, 0x00, 0x00, 0x00, 0x02, 0x0a, 0x00]);
    }

    #[test]
    fn take_frame_handles_partial_and_multiple() {
        let mut buf = connect_frame(&[1, 2, 3]);
        buf.extend_from_slice(&connect_frame(&[4, 5]));
        let first = take_frame(&mut buf).unwrap();
        assert_eq!(first.body, vec![1, 2, 3]);
        let second = take_frame(&mut buf).unwrap();
        assert_eq!(second.body, vec![4, 5]);
        assert!(take_frame(&mut buf).is_none());
        // 半帧要等到齐
        let mut partial = connect_frame(&[9, 9, 9]);
        partial.truncate(6);
        assert!(take_frame(&mut partial).is_none());
    }

    /// 抓包里的真实正文帧要走通 `consume_frame`。
    #[test]
    fn real_text_frame_becomes_a_text_step() {
        let mut outcome = TurnOutcome::default();
        let mut steps = Vec::new();
        let mut thinking_open = false;
        consume_frame(
            CAPTURED_TEXT,
            0,
            &mut outcome,
            &mut |s| steps.push(s),
            &mut thinking_open,
        );
        assert!(matches!(steps.as_slice(), [Step::Text(t)] if t == "在"));
        assert_eq!(outcome.text, "在");
        assert!(outcome.saw_event);
    }

    /// 思考先到、正文后到：正文之前必须关掉 thinking 块（否则 Anthropic 侧会串块）。
    #[test]
    fn thinking_is_closed_before_first_text() {
        let mut outcome = TurnOutcome::default();
        let mut steps = Vec::new();
        let mut open = false;
        for frame in [CAPTURED_THINKING, CAPTURED_TEXT] {
            consume_frame(frame, 0, &mut outcome, &mut |s| steps.push(s), &mut open);
        }
        let order: Vec<&str> = steps
            .iter()
            .map(|s| match s {
                Step::Thinking(_) => "thinking",
                Step::ThinkingDone => "done",
                Step::Text(_) => "text",
                _ => "other",
            })
            .collect();
        assert_eq!(order, vec!["thinking", "done", "text"]);
        assert_eq!(outcome.thinking, "用户要求仅用三个字回复。");
    }

    /// 用量帧：输入 9489 / 输出 35。
    #[test]
    fn real_usage_frame_fills_outcome() {
        let mut outcome = TurnOutcome::default();
        let mut open = false;
        let usage: &[u8] = &[
            0x0a, 0x0d, 0x72, 0x0b, 0x08, 0x91, 0x4a, 0x10, 0x23, 0x18, 0x00, 0x20, 0x00, 0x28,
            0x21,
        ];
        consume_frame(usage, 0, &mut outcome, &mut |_| {}, &mut open);
        assert_eq!(outcome.usage.input_tokens, 9489);
        assert_eq!(outcome.usage.output_tokens, 35);
    }

    /// 结束帧：`{}` 是正常结束，带 error 的要能认出来。
    #[test]
    fn trailer_frames_are_recognized() {
        let mut outcome = TurnOutcome::default();
        let mut open = false;
        let ok = consume_frame(b"{}", FLAG_END, &mut outcome, &mut |_| {}, &mut open);
        assert!(matches!(ok, FrameOutcome::End(None)));
        let bad = consume_frame(
            br#"{"error":{"code":"permission_denied","message":"no money"}}"#,
            FLAG_END,
            &mut outcome,
            &mut |_| {},
            &mut open,
        );
        match bad {
            FrameOutcome::End(Some(text)) => assert!(text.contains("no money")),
            _ => panic!("应该认出错误"),
        }
    }
}
