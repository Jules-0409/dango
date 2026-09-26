//! Qoder 的 SSE 信封解析 + 上游 chunk → Gemini 形状的翻译。
//!
//! 信封形状（PoC 实测）：每个 `data:` 行的 JSON 是
//!   { statusCodeValue?:200, body: "<OpenAI 风格 chunk 的 JSON 文本>" }
//! `body` 里的 chunk 是 `choices[0].delta.{content,reasoning_content,tool_calls}`、
//! `usage{billable,credits,prompt_tokens,completion_tokens,total_tokens}`、`finish_reason`。
//! `[DONE]` 有裸的，也有包在信封里 `body:"[DONE]"`，两种都是结束。
//!
//! 桥的 pump/翻译器只认 Gemini 形状（`{response:{candidates:[{content:{parts},finishReason}],
//! usageMetadata}}`），所以这里必须翻成那个形状；否则客户端拿到的是空回合。
//!
//! 已知未验证：工具调用（`delta.tool_calls` 是逐段 JSON 字符串）只能缓冲到流末尾再吐一个
//! `functionCall` part；没有真实网关的带工具用例对拍过。

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::types::UpstreamError;

/// SSE 解出的一条事件。
#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    /// 信封里掉出来的内层 chunk（OpenAI 风格）
    Chunk(Value),
    /// 回复结束（裸 `[DONE]` 或 `body:"[DONE]"`）
    Done,
}

/// 按行攒 buffer 的 SSE 解码器。只认 `data:` 行；坏行跳过（对齐 PoC 的 `try/catch continue`）。
#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段原始字节（TCP 会任意切分，所以要在字节层按 `\n` 切，别按字符切）。
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, UpstreamError> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(position) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]);
            let text = text.trim_end_matches('\r').trim();
            if let Some(event) = parse_data_line(text)? {
                out.push(event);
                if matches!(out.last(), Some(SseEvent::Done)) {
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// 解析一条 `data:` 行。不是 data 行 / JSON 坏了都返回 `Ok(None)`（跳过）；
/// 只有上游明确报 `statusCodeValue != 200` 才返回 `Err`。
pub fn parse_data_line(line: &str) -> Result<Option<SseEvent>, UpstreamError> {
    let Some(rest) = line.strip_prefix("data:") else {
        return Ok(None);
    };
    let payload = rest.trim();
    if payload.is_empty() {
        return Ok(None);
    }
    if payload == "[DONE]" {
        return Ok(Some(SseEvent::Done));
    }
    let Ok(envelope) = serde_json::from_str::<Value>(payload) else {
        return Ok(None);
    };
    if let Some(status) = envelope.get("statusCodeValue").and_then(Value::as_u64) {
        if status != 200 {
            let body = envelope
                .get("body")
                .map(|body| match body {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            return Err(UpstreamError::http(
                status as u16,
                "qoder_stream",
                crate::server::truncate(&body, 300),
            ));
        }
    }
    let Some(body) = envelope.get("body") else {
        return Ok(None);
    };
    let body = match body {
        Value::String(text) => text.as_str(),
        Value::Null => return Ok(None),
        // 信封的 body 偶尔直接是对象（不走字符串），也认
        _ => return Ok(Some(SseEvent::Chunk(body.clone()))),
    };
    if body == "[DONE]" {
        return Ok(Some(SseEvent::Done));
    }
    if body.is_empty() {
        return Ok(None);
    }
    let Ok(inner) = serde_json::from_str::<Value>(body) else {
        return Ok(None);
    };
    Ok(Some(SseEvent::Chunk(inner)))
}

/// 上游 `finish_reason` → Gemini 的 `finishReason`（桥的两条协议都按 Gemini 取值判断）。
fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "length" | "max_tokens" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        // stop / tool_calls / 其它：有工具调用时由翻译器按 saw_tool_use 收口
        _ => "STOP",
    }
}

/// Qoder `usage` → Gemini `usageMetadata`。
///
/// `completion_tokens` 含思考；桥里 Anthropic 侧会把 thoughts 再加一次，所以能拿到
/// `reasoning_tokens` 时要从 candidates 里减掉，避免重复计数。
fn usage_metadata(usage: &Value) -> Value {
    let number = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    let prompt = number("prompt_tokens");
    let completion = number("completion_tokens");
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(prompt + completion);
    let candidates = if reasoning > 0 {
        (completion - reasoning).max(0)
    } else {
        completion
    };
    json!({
        "promptTokenCount": prompt,
        "candidatesTokenCount": candidates,
        "thoughtsTokenCount": reasoning,
        "totalTokenCount": total,
    })
}

#[derive(Default)]
struct ToolCallBuffer {
    id: String,
    name: String,
    arguments: String,
}

/// 把上游 chunk 翻成 Gemini chunk 的有状态翻译器（工具调用要跨 chunk 攒）。
#[derive(Default)]
pub struct QoderStream {
    tool_calls: BTreeMap<u64, ToolCallBuffer>,
    usage: Option<Value>,
    model: Option<String>,
}

impl QoderStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// 处理一个内层 chunk，追加若干要发给 pump 的 Gemini 形状 chunk。
    pub fn ingest(&mut self, inner: &Value, out: &mut Vec<Value>) {
        if let Some(model) = inner.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_string());
        }
        if let Some(usage) = inner.get("usage").filter(|value| !value.is_null()) {
            self.usage = Some(usage.clone());
        }

        let choice = inner
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        let delta = choice.and_then(|choice| choice.get("delta"));

        let mut parts: Vec<Value> = Vec::new();
        let mut finish: Option<&str> = None;

        if let Some(delta) = delta {
            if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
                if !text.is_empty() {
                    parts.push(json!({ "text": text, "thought": true }));
                }
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    parts.push(json!({ "text": text }));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let buffer = self.tool_calls.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        if !id.is_empty() {
                            buffer.id = id.to_string();
                        }
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            if !name.is_empty() {
                                buffer.name = name.to_string();
                            }
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                            buffer.arguments.push_str(arguments);
                        }
                    }
                }
            }
        }
        if let Some(reason) = choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
        {
            finish = Some(reason);
        }

        let usage_here = inner.get("usage").filter(|value| !value.is_null());
        if parts.is_empty() && finish.is_none() && usage_here.is_none() {
            return;
        }

        let mut response = Map::new();
        let mut candidates: Vec<Value> = Vec::new();
        if !parts.is_empty() || finish.is_some() {
            let mut candidate = json!({
                "content": { "role": "model", "parts": parts },
            });
            if let Some(reason) = finish {
                candidate
                    .as_object_mut()
                    .expect("candidate 是对象")
                    .insert("finishReason".to_string(), json!(map_finish_reason(reason)));
            }
            candidates.push(candidate);
        }
        response.insert("candidates".to_string(), Value::Array(candidates));
        if let Some(usage) = usage_here {
            response.insert("usageMetadata".to_string(), usage_metadata(usage));
        }
        out.push(json!({ "response": Value::Object(response) }));
    }

    /// 流结束：把缓冲的并行工具调用吐成 `functionCall` part（参数解析失败当空对象）。
    pub fn flush(&mut self, out: &mut Vec<Value>) {
        if self.tool_calls.is_empty() {
            return;
        }
        let mut parts: Vec<Value> = Vec::new();
        for buffer in self.tool_calls.values() {
            let name = if buffer.name.is_empty() {
                "unknown_tool".to_string()
            } else {
                buffer.name.clone()
            };
            let arguments = if buffer.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&buffer.arguments).unwrap_or_else(|_| json!({}))
            };
            parts.push(json!({ "functionCall": { "name": name, "args": arguments } }));
        }
        self.tool_calls.clear();
        out.push(json!({
            "response": {
                "candidates": [{ "content": { "role": "model", "parts": parts } }],
            },
        }));
    }

    /// 最后一次看到的 usage（服务层用不到，测试要它）。
    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(text: &str) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        decoder.push(text.as_bytes()).expect("解码")
    }

    #[test]
    fn decodes_envelope_and_bare_done() {
        let events = decode_all(
            "data: {\"statusCodeValue\":200,\"body\":\"{\\\"id\\\":1,\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"你\\\"}}]}\"}\n\
             data: [DONE]\n",
        );
        assert_eq!(events.len(), 2);
        let SseEvent::Chunk(inner) = &events[0] else {
            panic!("第一条应是 chunk");
        };
        assert_eq!(inner["choices"][0]["delta"]["content"], json!("你"));
        assert_eq!(events[1], SseEvent::Done);
    }

    #[test]
    fn body_wrapped_done_is_also_the_end() {
        let events = decode_all("data: {\"statusCodeValue\":200,\"body\":\"[DONE]\"}\n");
        assert_eq!(events, vec![SseEvent::Done]);
    }

    #[test]
    fn error_envelope_surfaces_the_status() {
        let err = parse_data_line("data: {\"statusCodeValue\":403,\"body\":\"forbidden\"}")
            .expect_err("403 必须报错");
        assert_eq!(err.status, 403);
        assert_eq!(err.reason, "qoder_stream");
        assert!(err.message.contains("forbidden"));
    }

    #[test]
    fn malformed_and_non_data_lines_are_skipped() {
        let events = decode_all(
            "event: ping\n\
             data: {not json}\n\
             data: \n\
             : comment\n\
             data: {\"statusCodeValue\":200,\"body\":\"\"}\n",
        );
        assert!(events.is_empty(), "坏行不该变成事件：{events:?}");
    }

    #[test]
    fn chunks_split_mid_line_are_reassembled() {
        let whole = "data: {\"statusCodeValue\":200,\"body\":\"{\\\"choices\\\":[{\\\"delta\\\":{\\\"content\\\":\\\"好\\\"}}]}\"}\n";
        let bytes = whole.as_bytes();
        let mut decoder = SseDecoder::new();
        // 在 UTF-8 多字节字符中间切开也要能拼回来
        let mut events = decoder.push(&bytes[..40]).expect("前半");
        events.extend(decoder.push(&bytes[40..]).expect("后半"));
        assert_eq!(events.len(), 1);
        let SseEvent::Chunk(inner) = &events[0] else {
            panic!("应是 chunk");
        };
        assert_eq!(inner["choices"][0]["delta"]["content"], json!("好"));
    }

    #[test]
    fn text_and_thinking_become_gemini_parts() {
        let mut stream = QoderStream::new();
        let mut out = Vec::new();
        stream.ingest(
            &json!({ "choices": [{ "delta": { "reasoning_content": "想", "content": "答" } }] }),
            &mut out,
        );
        assert_eq!(out.len(), 1);
        let parts = &out[0]["response"]["candidates"][0]["content"]["parts"];
        assert_eq!(parts[0], json!({ "text": "想", "thought": true }));
        assert_eq!(parts[1], json!({ "text": "答" }));
    }

    #[test]
    fn usage_is_mapped_without_double_counting_thoughts() {
        let mut stream = QoderStream::new();
        let mut out = Vec::new();
        stream.ingest(
            &json!({
                "choices": [{ "delta": {}, "finish_reason": "stop" }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 8,
                    "total_tokens": 18,
                    "billable": false,
                    "credits": 0,
                    "completion_tokens_details": { "reasoning_tokens": 3 },
                },
            }),
            &mut out,
        );
        let meta = &out[0]["response"]["usageMetadata"];
        assert_eq!(meta["promptTokenCount"], json!(10));
        // 8 - 3 = 5，思考另计
        assert_eq!(meta["candidatesTokenCount"], json!(5));
        assert_eq!(meta["thoughtsTokenCount"], json!(3));
        assert_eq!(meta["totalTokenCount"], json!(18));
        assert_eq!(
            out[0]["response"]["candidates"][0]["finishReason"],
            json!("STOP")
        );
        // billable/credits 不丢：留在原始 usage 里
        assert_eq!(stream.usage().unwrap()["billable"], json!(false));
    }

    #[test]
    fn length_finish_maps_to_max_tokens() {
        let mut stream = QoderStream::new();
        let mut out = Vec::new();
        stream.ingest(
            &json!({ "choices": [{ "delta": { "content": "x" }, "finish_reason": "length" }] }),
            &mut out,
        );
        assert_eq!(
            out[0]["response"]["candidates"][0]["finishReason"],
            json!("MAX_TOKENS")
        );
    }

    #[test]
    fn tool_calls_are_buffered_then_flushed() {
        let mut stream = QoderStream::new();
        let mut out = Vec::new();
        stream.ingest(
            &json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "id": "c1", "function": { "name": "Read", "arguments": "{\"pa" } }
            ] } }] }),
            &mut out,
        );
        // 第一段只有工具调用的开头（没有正文/结束标记）→ 不吐任何 chunk
        assert!(out.is_empty());
        stream.ingest(
            &json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "function": { "arguments": "th\":\"a\"}" } }
            ] }, "finish_reason": "tool_calls" }] }),
            &mut out,
        );
        stream.flush(&mut out);
        // 第二段一个 finishReason chunk + flush 的一个 functionCall chunk
        assert_eq!(out.len(), 2);
        let call = &out.last().unwrap()["response"]["candidates"][0]["content"]["parts"][0];
        assert_eq!(call["functionCall"]["name"], json!("Read"));
        assert_eq!(call["functionCall"]["args"], json!({ "path": "a" }));
    }
}
