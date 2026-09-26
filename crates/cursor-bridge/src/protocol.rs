//! 事件流 → 两个协议的报文：Anthropic `/v1/messages` 与 OpenAI `/v1/chat/completions`。
//!
//! 这里只做「组装」，不碰网络：每个方法返回要写出去的字符串帧，好让测试直接断言帧内容。
//! 两边的取数口径一致（都来自 [`crate::turn::TurnOutcome`]）：
//!
//! - 思考 → Anthropic 的 `thinking` 块 / OpenAI 的 `reasoning_content`；CLI 不给签名，
//!   所以按 antigravity-bridge 那次的结论补哨兵签名 `skip_thought_signature_validator`，
//!   否则客户端会把整个思考块丢掉（实测 Droid 就会丢）。
//! - 用量 → `input_tokens` / `output_tokens` / `cache_read_input_tokens` /
//!   `cache_creation_input_tokens`，OpenAI 侧折成 `prompt_tokens` / `completion_tokens`。

use serde_json::{json, Value};
use uuid::Uuid;

use crate::cli::Usage;
use crate::turn::{Step, TurnOutcome};

/// 上游不给签名时用的哨兵：客户端见到它就知道不用校验。和 antigravity-bridge 同一个值。
pub const SIGNATURE_SENTINEL: &str = "skip_thought_signature_validator";

/// 拼一帧 SSE。
pub fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// SSE 注释帧，用来保活（客户端会忽略内容，但连接不会被判死）。
pub fn keepalive() -> String {
    ": keepalive\n\n".to_string()
}

pub fn anthropic_usage(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_input_tokens": usage.cache_read_tokens,
        "cache_creation_input_tokens": usage.cache_write_tokens,
    })
}

pub fn openai_usage(usage: &Usage) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
        "prompt_tokens_details": {"cached_tokens": usage.cache_read_tokens},
    })
}

/// 把 `Step` 折进两种协议的公共结果（非流式直接用这个）。
#[derive(Debug, Default, Clone)]
pub struct Collected {
    pub thinking: String,
    pub text: String,
    pub outcome: Option<TurnOutcome>,
}

impl Collected {
    pub fn absorb(&mut self, step: &Step) {
        match step {
            Step::Thinking(delta) => self.thinking.push_str(delta),
            Step::Text(delta) => self.text.push_str(delta),
            Step::Done(outcome) => {
                self.thinking = outcome.thinking.clone();
                self.text = outcome.text.clone();
                self.outcome = Some(outcome.clone());
            }
            Step::Init { .. } | Step::ThinkingDone | Step::Note(_) => {}
        }
    }
}

/// Anthropic 侧的流式组装。
#[derive(Debug)]
pub struct AnthropicStream {
    id: String,
    model: Option<String>,
    wants_thinking: bool,
    started: bool,
    thinking_open: bool,
    text_open: bool,
    next_index: usize,
    usage: Usage,
}

impl AnthropicStream {
    pub fn new(requested_model: Option<String>, wants_thinking: bool) -> Self {
        Self {
            id: format!("msg_{}", Uuid::new_v4().simple()),
            model: requested_model,
            wants_thinking,
            started: false,
            thinking_open: false,
            text_open: false,
            next_index: 0,
            usage: Usage::default(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn model_name(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| "cursor-auto".to_string())
    }

    /// 开一条消息。CLI 的 init 事件会顺手把真实模型名带进来。
    fn ensure_started(&mut self) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model_name(),
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }
            }),
        )]
    }

    pub fn on_step(&mut self, step: &Step) -> Vec<String> {
        let mut out = self.ensure_started();
        match step {
            Step::Init { model, .. } => {
                // init 先到的话用它的模型名；客户端指定过就不覆盖
                if self.model.is_none() {
                    if let Some(m) = model {
                        self.model = Some(m.clone());
                        let _ = m;
                    }
                }
            }
            Step::Thinking(delta) => {
                if !self.wants_thinking || delta.is_empty() {
                    return out;
                }
                if !self.thinking_open {
                    let index = self.next_index;
                    self.next_index += 1;
                    self.thinking_open = true;
                    out.push(frame(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "thinking", "thinking": "", "signature": ""}
                        }),
                    ));
                }
                out.push(frame(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.next_index - 1,
                        "delta": {"type": "thinking_delta", "thinking": delta}
                    }),
                ));
            }
            Step::ThinkingDone => out.extend(self.close_thinking()),
            Step::Text(delta) => {
                if delta.is_empty() {
                    return out;
                }
                out.extend(self.close_thinking());
                if !self.text_open {
                    let index = self.next_index;
                    self.next_index += 1;
                    self.text_open = true;
                    out.push(frame(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    ));
                }
                out.push(frame(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.next_index - 1,
                        "delta": {"type": "text_delta", "text": delta}
                    }),
                ));
            }
            Step::Note(_) => {}
            Step::Done(outcome) => {
                self.usage = outcome.usage.clone();
            }
        }
        out
    }

    /// 关思考块：**总是**补一条 signature_delta（真签名没有就给哨兵），
    /// 客户端缺签名会整块丢掉，这是实测过的坑。
    fn close_thinking(&mut self) -> Vec<String> {
        if !self.thinking_open {
            return Vec::new();
        }
        self.thinking_open = false;
        let index = self.next_index - 1;
        vec![
            frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "signature_delta", "signature": SIGNATURE_SENTINEL}
                }),
            ),
            frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            ),
        ]
    }

    /// 收尾：正常就 message_delta + message_stop，出错就一条 error 事件。
    pub fn finish(&mut self, error: Option<&str>) -> Vec<String> {
        let mut out = self.ensure_started();
        out.extend(self.close_thinking());
        if self.text_open {
            let index = self.next_index - 1;
            self.text_open = false;
            out.push(frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            ));
        }
        match error {
            Some(message) => out.push(frame(
                "error",
                &json!({"type": "error", "error": {"type": "api_error", "message": message}}),
            )),
            None => {
                out.push(frame(
                    "message_delta",
                    &json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": "end_turn", "stop_sequence": Value::Null},
                        "usage": anthropic_usage(&self.usage),
                    }),
                ));
            }
        }
        out.push(frame("message_stop", &json!({"type": "message_stop"})));
        out
    }
}

/// Anthropic 非流式的整包响应。
pub fn anthropic_message(
    collected: &Collected,
    model: Option<&str>,
    wants_thinking: bool,
) -> Value {
    let mut content: Vec<Value> = Vec::new();
    if wants_thinking && !collected.thinking.is_empty() {
        content.push(json!({
            "type": "thinking",
            "thinking": collected.thinking,
            "signature": SIGNATURE_SENTINEL,
        }));
    }
    content.push(json!({"type": "text", "text": collected.text}));
    let usage = collected
        .outcome
        .as_ref()
        .map(|o| o.usage.clone())
        .unwrap_or_default();
    json!({
        "id": format!("msg_{}", Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "model": model.unwrap_or("cursor-auto"),
        "content": content,
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": anthropic_usage(&usage),
    })
}

/// OpenAI 侧的流式组装。
#[derive(Debug)]
pub struct OpenAiStream {
    id: String,
    model: Option<String>,
    created: u64,
    role_sent: bool,
    /// 有没有 `stream_options.include_usage`；要了才在最后补一条用量块。
    wants_usage: bool,
    usage: Usage,
}

impl OpenAiStream {
    pub fn new(requested_model: Option<String>, wants_usage: bool) -> Self {
        Self {
            id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
            model: requested_model,
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            role_sent: false,
            wants_usage,
            usage: Usage::default(),
        }
    }

    fn model_name(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| "cursor-auto".to_string())
    }

    fn chunk(&self, delta: Value, finish_reason: Value) -> String {
        let data = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model_name(),
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        });
        format!("data: {data}\n\n")
    }

    /// 第一条业务帧之前必须先发 `role`：OpenAI 的客户端靠它判断这条流里是谁在说话。
    fn ensure_role(&mut self) -> Vec<String> {
        if self.role_sent {
            return Vec::new();
        }
        self.role_sent = true;
        vec![self.chunk(json!({"role": "assistant", "content": ""}), Value::Null)]
    }

    pub fn on_step(&mut self, step: &Step) -> Vec<String> {
        let mut out = Vec::new();
        match step {
            Step::Init { model, .. } => {
                if self.model.is_none() {
                    self.model = model.clone();
                }
            }
            Step::Thinking(delta) => {
                if !delta.is_empty() {
                    out.extend(self.ensure_role());
                    out.push(self.chunk(json!({"reasoning_content": delta}), Value::Null));
                }
            }
            Step::Text(delta) => {
                if !delta.is_empty() {
                    out.extend(self.ensure_role());
                    out.push(self.chunk(json!({"content": delta}), Value::Null));
                }
            }
            Step::ThinkingDone | Step::Note(_) => {}
            Step::Done(outcome) => self.usage = outcome.usage.clone(),
        }
        out
    }

    pub fn finish(&mut self, error: Option<&str>) -> Vec<String> {
        let mut out = self.ensure_role();
        match error {
            Some(message) => {
                out.push(format!(
                    "data: {}\n\n",
                    json!({"error": {"message": message, "type": "api_error"}})
                ));
            }
            None => {
                out.push(self.chunk(json!({}), json!("stop")));
                if self.wants_usage {
                    out.push(format!(
                        "data: {}\n\n",
                        json!({
                            "id": self.id,
                            "object": "chat.completion.chunk",
                            "created": self.created,
                            "model": self.model_name(),
                            "choices": [],
                            "usage": openai_usage(&self.usage),
                        })
                    ));
                }
            }
        }
        out.push("data: [DONE]\n\n".to_string());
        out
    }
}

/// OpenAI 非流式的整包响应。
pub fn openai_completion(collected: &Collected, model: Option<&str>) -> Value {
    let mut message = json!({"role": "assistant", "content": collected.text});
    if !collected.thinking.is_empty() {
        message["reasoning_content"] = json!(collected.thinking);
    }
    let usage = collected
        .outcome
        .as_ref()
        .map(|o| o.usage.clone())
        .unwrap_or_default();
    json!({
        "id": format!("chatcmpl-{}", Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "model": model.unwrap_or("cursor-auto"),
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": openai_usage(&usage),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::probe::LINES;
    use crate::turn::TurnState;

    /// 真实抓包喂出来的 Step 序列。
    fn real_steps() -> Vec<Step> {
        let mut state = TurnState::default();
        let mut steps = Vec::new();
        for line in LINES {
            if let Some(ev) = crate::cli::parse_line(line) {
                steps.extend(state.apply(&ev));
            }
        }
        steps
    }

    /// 把 SSE 帧里的 data 行解析出来（帧形如 `event: x\ndata: {…}\n\n`）。
    fn payloads(frames: &[String]) -> Vec<Value> {
        frames
            .iter()
            .flat_map(|f| f.lines())
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .collect()
    }

    fn text_of(frames: &[String]) -> String {
        payloads(frames)
            .iter()
            .filter(|v| v["type"] == "content_block_delta" && v["delta"]["type"] == "text_delta")
            .filter_map(|v| v["delta"]["text"].as_str().map(str::to_string))
            .collect()
    }

    fn events(frames: &[String]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| f.lines().next().map(str::to_string))
            .map(|l| l.trim_start_matches("event: ").to_string())
            .collect()
    }

    #[test]
    fn anthropic_stream_has_thinking_block_with_sentinel_signature_then_text() {
        let mut stream = AnthropicStream::new(Some("gpt-5.3-codex".into()), true);
        let mut frames = Vec::new();
        for step in real_steps() {
            frames.extend(stream.on_step(&step));
        }
        frames.extend(stream.finish(None));

        let evs = events(&frames);
        assert_eq!(evs[0], "message_start");
        assert_eq!(evs[1], "content_block_start");
        assert_eq!(evs.last().unwrap(), "message_stop");
        assert!(evs.contains(&"message_delta".to_string()));

        let payloads = payloads(&frames);

        // 思考块的 index 是 0，正文块是 1
        let blocks: Vec<&Value> = payloads
            .iter()
            .filter(|v| v["type"] == "content_block_start")
            .collect();
        assert_eq!(blocks.len(), 2, "应有思考块 + 正文块：{frames:?}");
        assert_eq!(blocks[0]["index"], 0);
        assert_eq!(blocks[0]["content_block"]["type"], "thinking");
        assert_eq!(blocks[1]["index"], 1);
        assert_eq!(blocks[1]["content_block"]["type"], "text");

        // 思考块关闭前必须有哨兵签名
        let sig = payloads
            .iter()
            .find(|v| v["delta"]["type"] == "signature_delta")
            .expect("少了 signature_delta，客户端会把思考丢掉");
        assert_eq!(sig["delta"]["signature"], SIGNATURE_SENTINEL, "{sig}");
        assert_eq!(sig["index"], 0);

        // 正文恰好一份
        assert_eq!(text_of(&frames), "1+1 等于 2。");

        // 用量折算
        let delta = payloads
            .iter()
            .find(|v| v["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["usage"]["input_tokens"], 5851);
        assert_eq!(delta["usage"]["output_tokens"], 56);
        assert_eq!(delta["usage"]["cache_read_input_tokens"], 7808);
    }

    #[test]
    fn anthropic_stream_without_thinking_request_hides_thinking() {
        let mut stream = AnthropicStream::new(None, false);
        let mut frames = Vec::new();
        for step in real_steps() {
            frames.extend(stream.on_step(&step));
        }
        frames.extend(stream.finish(None));
        assert!(!frames.iter().any(|f| f.contains("\"thinking\"")));
        assert_eq!(text_of(&frames), "1+1 等于 2。");
        assert_eq!(stream.model_name(), "Auto", "init 的模型名应按需回填");
    }

    #[test]
    fn anthropic_error_path_emits_error_event_and_stops() {
        let mut stream = AnthropicStream::new(Some("auto".into()), true);
        let frames = stream.finish(Some("CLI 挂了"));
        let evs = events(&frames);
        assert_eq!(evs, ["message_start", "error", "message_stop"]);
        assert!(frames[1].contains("CLI 挂了"));
    }

    #[test]
    fn openai_stream_maps_thinking_to_reasoning_content() {
        let mut stream = OpenAiStream::new(Some("auto".into()), true);
        let mut frames = Vec::new();
        for step in real_steps() {
            frames.extend(stream.on_step(&step));
        }
        frames.extend(stream.finish(None));

        let payloads = payloads(&frames);
        let reasoning: String = payloads
            .iter()
            .filter_map(|v| v["choices"][0]["delta"]["reasoning_content"].as_str())
            .collect();
        assert_eq!(reasoning, "用户在询问 1+1 的结果。\n\n1+1 等于 2。");
        let content: String = payloads
            .iter()
            .filter_map(|v| v["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(content, "1+1 等于 2。");
        assert_eq!(frames.last().unwrap(), "data: [DONE]\n\n");
        let usage = payloads.iter().find_map(|v| v.get("usage")).unwrap();
        assert_eq!(usage["prompt_tokens"], 5851);
        assert_eq!(usage["total_tokens"], 5851 + 56);
    }

    #[test]
    fn openai_stream_without_usage_request_omits_the_usage_chunk() {
        let mut stream = OpenAiStream::new(None, false);
        let mut frames = stream.on_step(&Step::Text("hi".into()));
        frames.extend(stream.finish(None));
        assert!(!frames.iter().any(|f| f.contains("\"usage\"")));
        // 首帧带 role，末帧带 finish_reason
        let payloads = payloads(&frames);
        assert_eq!(payloads[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(
            payloads.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
    }

    #[test]
    fn non_streaming_bodies_carry_text_thinking_and_usage() {
        let mut collected = Collected::default();
        for step in real_steps() {
            collected.absorb(&step);
        }
        let anthropic = anthropic_message(&collected, Some("gpt-5.3-codex"), true);
        assert_eq!(anthropic["type"], "message");
        assert_eq!(anthropic["content"][0]["type"], "thinking");
        assert_eq!(anthropic["content"][0]["signature"], SIGNATURE_SENTINEL);
        assert_eq!(anthropic["content"][1]["text"], "1+1 等于 2。");
        assert_eq!(anthropic["usage"]["output_tokens"], 56);

        let openai = openai_completion(&collected, Some("gpt-5.3-codex"));
        assert_eq!(openai["choices"][0]["message"]["content"], "1+1 等于 2。");
        assert!(openai["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .unwrap()
            .contains("1+1 等于 2。"));
        assert_eq!(openai["usage"]["completion_tokens"], 56);
    }

    #[test]
    fn collected_without_done_still_has_partial_text() {
        let mut collected = Collected::default();
        collected.absorb(&Step::Text("一".into()));
        collected.absorb(&Step::Text("二".into()));
        assert_eq!(collected.text, "一二");
        assert!(collected.outcome.is_none());
        let body = anthropic_message(&collected, None, false);
        assert_eq!(body["model"], "cursor-auto");
        assert_eq!(body["usage"]["input_tokens"], 0);
    }
}
