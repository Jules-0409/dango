//! Cloud Code Assistant（v1internal）的最小客户端。
//!
//! 调用风格：POST {base}:{method}，流式再加 ?alt=sse。三个端点按 sandbox → daily → prod 回退，
//! 这个顺序是从 Antigravity Tools 的运行日志里实测出来的（见 docs/PROTOCOL.md §1）。

use std::time::Duration;

use serde_json::Value;

use crate::types::UpstreamError;

pub const ENDPOINTS: [&str; 3] = [
    "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal",
    "https://daily-cloudcode-pa.googleapis.com/v1internal",
    "https://cloudcode-pa.googleapis.com/v1internal",
];

/// 实测结论（2026-09-17，见 docs/PROTOCOL.md §5）：这三个头是硬要求。
/// 少了它们，同一个 token、同一个请求体会得到 403 PERMISSION_DENIED / SUBSCRIPTION_REQUIRED，
/// 报错文案还误导成"没有许可证"。加上之后 fetchAvailableModels / retrieveUserQuotaSummary /
/// streamGenerateContent 全部 200。
pub const ANTIGRAVITY_HEADERS: [(&str, &str); 3] = [
    ("User-Agent", "antigravity/1.11.5 windows/amd64"),
    (
        "X-Goog-Api-Client",
        "google-cloud-sdk vscode_cloudshelleditor/0.1",
    ),
    (
        "Client-Metadata",
        r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#,
    ),
];

/// 一次非流式调用的结果。不抛错：端点回退要靠调用方看 `ok` 决定下一步。
#[derive(Debug, Clone)]
pub struct PostResult {
    pub ok: bool,
    pub status: u16,
    pub url: String,
    pub json: Option<Value>,
    pub error: Option<UpstreamError>,
}

/// 从 Google RPC error 里抽一个给人看的摘要（reason / message）。
///
/// JS 版还抽了个 `uiMessage` 标志位，实际没有消费方（那只是"这段文案是给人看的、别信"的记号），
/// 所以 Rust 版不带它；原始 error 对象在 `PostResult::json` 里，排查时看得到。
pub fn summarize_error(status: u16, json: &Value) -> UpstreamError {
    let err = json.get("error");
    let reason = err
        .and_then(|e| e.get("details"))
        .and_then(Value::as_array)
        .and_then(|details| {
            details.iter().find_map(|d| {
                let is_info = d
                    .get("@type")
                    .and_then(Value::as_str)
                    .map(|t| t.contains("ErrorInfo"))
                    .unwrap_or(false);
                if !is_info {
                    return None;
                }
                d.get("reason").and_then(Value::as_str).map(str::to_string)
            })
        })
        .or_else(|| {
            err.and_then(|e| e.get("status"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(|m| m.chars().take(300).collect::<String>())
        .unwrap_or_default();

    UpstreamError {
        status,
        reason,
        message,
    }
}

/// 非 JSON 响应体（网关页面之类）也让它有个形状，别把正文吞掉。
fn parse_json_safe(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(
        |_| serde_json::json!({ "__nonJson": text.chars().take(500).collect::<String>() }),
    )
}

fn base_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in ANTIGRAVITY_HEADERS {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// 非流式调用一个方法。返回 `PostResult` —— 不抛，让调用方决定回退。
pub async fn post_method(
    client: &reqwest::Client,
    endpoint: &str,
    method: &str,
    access_token: &str,
    body: &Value,
    timeout_ms: u64,
) -> PostResult {
    let url = format!("{endpoint}:{method}");
    let mut headers = base_headers();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    let req = client
        .post(&url)
        .bearer_auth(access_token)
        .headers(headers)
        .timeout(Duration::from_millis(timeout_ms))
        .json(body);

    match req.send().await {
        Err(err) => PostResult {
            ok: false,
            status: 0,
            url,
            json: None,
            error: Some(UpstreamError::network(reason_of(&err), err.to_string())),
        },
        Ok(res) => {
            let status = res.status().as_u16();
            let text = res.text().await.unwrap_or_default();
            let json = parse_json_safe(&text);
            if (200..300).contains(&status) {
                PostResult {
                    ok: true,
                    status,
                    url,
                    json: Some(json),
                    error: None,
                }
            } else {
                let error = summarize_error(status, &json);
                PostResult {
                    ok: false,
                    status,
                    url,
                    json: Some(json),
                    error: Some(error),
                }
            }
        }
    }
}

/// 开流的结果：拿到响应头就算成功，正文交给调用方逐块喂给 `SseDecoder`。
///
/// 为什么拆成两步：服务层要在「还没往客户端写任何字节」的时候换端点/换账号重试，
/// 所以必须能在流开始前知道这个端点行不行。
pub struct OpenStream {
    pub ok: bool,
    pub status: u16,
    pub url: String,
    pub error: Option<UpstreamError>,
    /// 响应体字节流（成功时才有）
    pub body: Option<reqwest::Response>,
}

pub async fn open_stream(
    client: &reqwest::Client,
    endpoint: &str,
    method: &str,
    access_token: &str,
    body: &Value,
    timeout_ms: u64,
) -> OpenStream {
    // 注意 ?alt=sse：不要这个参数上游回的是整包 JSON，不是事件流。
    let url = format!("{endpoint}:{method}?alt=sse");
    let mut headers = base_headers();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("text/event-stream"),
    );

    let req = client
        .post(&url)
        .bearer_auth(access_token)
        .headers(headers)
        .timeout(Duration::from_millis(timeout_ms))
        .json(body);

    match req.send().await {
        Err(err) => OpenStream {
            ok: false,
            status: 0,
            url,
            error: Some(UpstreamError::network(reason_of(&err), err.to_string())),
            body: None,
        },
        Ok(res) => {
            let status = res.status().as_u16();
            if (200..300).contains(&status) {
                OpenStream {
                    ok: true,
                    status,
                    url,
                    error: None,
                    body: Some(res),
                }
            } else {
                let text = res.text().await.unwrap_or_default();
                let json = parse_json_safe(&text);
                let error = summarize_error(status, &json);
                OpenStream {
                    ok: false,
                    status,
                    url,
                    error: Some(error),
                    body: None,
                }
            }
        }
    }
}

fn reason_of(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "TimeoutError".to_string()
    } else if err.is_connect() {
        "ConnectError".to_string()
    } else if err.is_body() || err.is_decode() {
        "BodyError".to_string()
    } else {
        "network_error".to_string()
    }
}

/// UTF-8 增量解码：网络分块可能把一个多字节字符切成两半，不能每块单独解。
#[derive(Debug, Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    out.push_str(text);
                    self.pending.clear();
                    break;
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    if valid > 0 {
                        // 上面刚验证过这段是合法 UTF-8，unwrap 不会炸
                        out.push_str(
                            std::str::from_utf8(&self.pending[..valid]).unwrap_or_default(),
                        );
                    }
                    match err.error_len() {
                        // 真的遇到非法字节：用替换字符吃掉，别卡死在坏数据上
                        Some(len) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid + len);
                        }
                        // 序列被截断：留着等下一块
                        None => {
                            self.pending.drain(..valid);
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}

/// SSE 行解析：和 JS 版 `openStream().iterate()` 的行为一致。
///
/// 两个要点：
///
///   1. 按 `\n` 切行，行尾的 `\r` 去掉（上游有时用 CRLF）；
///   2. 只认 `data:` 开头的行；解析失败的行原样包成 `{__unparsed: …}`，别丢数据。
///
/// 另外记着「有没有见过 data: 行」和「响应体开头长什么样」——上游对已下线的模型会回
/// 200 + 纯文本，一个 data: 行都没有，上层要拿这段正文才能报出真正的原因。
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
    saw_data: bool,
    raw_text: String,
}

impl SseDecoder {
    /// 喂一段文本，吐出这段里解析出来的所有事件对象。
    pub fn feed(&mut self, text: &str) -> Vec<Value> {
        if !self.saw_data {
            let mut head = self.raw_text.clone();
            head.push_str(text);
            self.raw_text = head.chars().take(800).collect();
        }
        self.buffer.push_str(text);

        let mut events = Vec::new();
        while let Some(idx) = self.buffer.find('\n') {
            let line: String = self.buffer[..idx].trim_end_matches('\r').to_string();
            self.buffer.drain(..idx + 1);
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            self.saw_data = true;
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            events.push(
                serde_json::from_str(payload).unwrap_or_else(|_| {
                    serde_json::json!({ "__unparsed": payload.chars().take(200).collect::<String>() })
                }),
            );
        }
        events
    }

    pub fn saw_data(&self) -> bool {
        self.saw_data
    }

    pub fn raw_text(&self) -> &str {
        &self.raw_text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarize_error_pulls_reason_and_message() {
        let json = json!({"error":{"code":403,"status":"PERMISSION_DENIED","message":"You do not have a valid license",
            "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SUBSCRIPTION_REQUIRED","metadata":{"uiMessage":"true"}}]}});
        let e = summarize_error(403, &json);
        assert_eq!(e.status, 403);
        assert_eq!(e.reason, "SUBSCRIPTION_REQUIRED");
        assert_eq!(e.message, "You do not have a valid license");
    }

    #[test]
    fn summarize_error_falls_back_to_status_field() {
        let e = summarize_error(
            429,
            &json!({"error":{"status":"RESOURCE_EXHAUSTED","message":"quota"}}),
        );
        assert_eq!(e.reason, "RESOURCE_EXHAUSTED");
        assert_eq!(e.message, "quota");
        // 啥都没有也不炸
        let empty = summarize_error(500, &json!({}));
        assert_eq!(empty.reason, "");
        assert_eq!(empty.message, "");
    }

    #[test]
    fn parse_json_safe_keeps_non_json_head() {
        let v = parse_json_safe("<html>oops</html>");
        assert!(v
            .get("__nonJson")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("oops"));
    }

    #[test]
    fn sse_decoder_handles_split_lines_and_crlf() {
        let mut d = SseDecoder::default();
        assert!(d.feed("data: {\"a\":").is_empty());
        // 第二块把第一个事件补全，并且带上第二个（CRLF 结尾）
        let events = d.feed("1}\r\ndata: {\"b\":2}\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], json!({"a": 1}));
        assert_eq!(events[1], json!({"b": 2}));
        assert!(d.saw_data());
    }

    #[test]
    fn sse_decoder_marks_unparsed_and_ignores_other_lines() {
        let mut d = SseDecoder::default();
        let events = d.feed("event: ping\n: comment\n\ndata: 不是JSON\n");
        assert_eq!(events.len(), 1);
        assert!(events[0].get("__unparsed").is_some());
        assert!(d.saw_data());
    }

    #[test]
    fn sse_decoder_remembers_raw_text_when_no_data_lines() {
        let mut d = SseDecoder::default();
        let events = d.feed("这个模型已经下线了，请换一个");
        assert!(events.is_empty());
        assert!(!d.saw_data());
        assert!(d.raw_text().contains("已经下线"));
    }

    #[test]
    fn utf8_stream_survives_split_multibyte() {
        let text = "收到好";
        let bytes = text.as_bytes();
        let mut stream = Utf8Stream::default();
        // 把「好」的三个字节切成两块
        let split = bytes.len() - 1;
        let first = stream.push(&bytes[..split]);
        let second = stream.push(&bytes[split..]);
        assert_eq!(format!("{first}{second}"), text);
    }

    #[test]
    fn utf8_stream_replaces_invalid_bytes() {
        let mut stream = Utf8Stream::default();
        // 0xFF 永远不是合法 UTF-8 起始字节
        let out = stream.push(&[0x41, 0xFF, 0x42]);
        assert_eq!(out, "A\u{FFFD}B");
    }

    #[test]
    fn antigravity_headers_are_exactly_three() {
        // 少一个就是 403，这条别被"顺手清理"掉
        assert_eq!(ANTIGRAVITY_HEADERS.len(), 3);
        assert_eq!(ENDPOINTS.len(), 3);
        assert!(ENDPOINTS[0].contains("sandbox"));
        assert!(ENDPOINTS[2].starts_with("https://cloudcode-pa"));
    }
}
