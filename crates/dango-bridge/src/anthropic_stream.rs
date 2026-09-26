//! 上游 chunk → Anthropic SSE 事件流。
//!
//! 移植自 `src/bridge/anthropic-stream.mjs`（测试：`test/anthropic-stream.test.mjs`）。
//! JS 版是行为基准：同一批 chunk 必须翻出同一串事件，不许「顺手改好」。
//! 注释写的是「为什么」，JS 里那些看起来奇怪的判断都连着原因一起搬过来。
//!
//! 上游每个 data: 行是一整个 CaGenerateContentResponse（不是增量 patch），形状是
//!   { response: { candidates: [{ content: { parts: [...] }, finishReason }], usageMetadata }, traceId }
//! 我们要把它翻成 Anthropic 的 message_start / content_block_* / message_delta / message_stop。
//!
//! 两个必须在流里做完的修复（否则客户端看到的是一个空转的回合）：
//!   1. 泄漏成文本的伪工具调用（<call:…>）→ 真正的 tool_use 块，键名照抄模型给的。
//!   2. 缺签名的 functionCall → 回传时补哨兵。发射时把真签名按 tool_use.id 存起来。

use serde_json::{json, Map, Value};

use crate::leak_repair::{LeakEvent, LeakFilter};
use crate::signatures::{sig_put, sig_put_trailing, SharedSignatures, SIGNATURE_SENTINEL};

/// Anthropic 停止原因取值有限，这里做一次映射。
///
/// `STOP` / `MALFORMED_FUNCTION_CALL` / `UNEXPECTED_TOOL_CALL` 都落在默认分支
/// （有调用就是 tool_use，否则 end_turn），和 JS 的 switch 一致。
pub fn map_stop_reason(finish_reason: Option<&str>, saw_tool_use: bool) -> &'static str {
    match finish_reason {
        Some("MAX_TOKENS") => "max_tokens",
        // 安全/版权类的收尾，Anthropic 侧没有对应语义，按正常结束回。
        Some("SAFETY")
        | Some("RECITATION")
        | Some("PROHIBITED_CONTENT")
        | Some("BLOCKLIST")
        | Some("SPII") => "end_turn",
        _ => {
            if saw_tool_use {
                "tool_use"
            } else {
                "end_turn"
            }
        }
    }
}

/// 上游 usageMetadata → Anthropic usage 字段。
///
/// JS 版在没有 meta 时返回 `null`；这里同样返回 `Value::Null`，调用方按 `is_null()` 判缺。
pub fn map_usage(meta: &Value) -> Value {
    if meta.is_null() {
        return Value::Null;
    }
    let input = meta
        .get("promptTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let candidates = meta
        .get("candidatesTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let thoughts = meta
        .get("thoughtsTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let mut out = Map::new();
    out.insert("input_tokens".to_string(), json!(input));
    // Anthropic 把思考也算进 output_tokens；上游是分开计的，这里合上。
    out.insert("output_tokens".to_string(), json!(candidates + thoughts));
    if meta
        .get("cachedContentTokenCount")
        .is_some_and(Value::is_number)
    {
        if let Some(value) = meta.get("cachedContentTokenCount") {
            out.insert("cache_read_input_tokens".to_string(), value.clone());
        }
    }
    if meta.get("thoughtsTokenCount").is_some_and(Value::is_number) && thoughts > 0 {
        out.insert("thoughts_token_count".to_string(), json!(thoughts));
    }
    Value::Object(out)
}

/// 翻译器的构造入参。字段名与 JS 解构的参数对齐（`signatureStore` 在这边是必给的共享仓库）。
#[derive(Debug, Clone)]
pub struct TranslatorOptions {
    pub model: String,
    pub signatures: SharedSignatures,
    pub session_key: String,
    pub message_id: String,
    /// 这次请求声明过的工具名（泄漏修复的白名单）；`None` = 没白名单就忽略泄漏。
    pub declared_tools: Option<Vec<String>>,
}

/// 服务层的 pump 用它做日志与预算诊断。
///
/// 对应 JS 的 `translator.stats()`：那边给 `{ blockCount, sawToolUse, leakedCalls,
/// leaksIgnored, finishReason, usage }`，这边按服务层的诊断口径把 `usage` 摊平成三个
/// token 字段（`blockCount` 由事件流本身体现，不在这里重复）。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslatorStats {
    pub finish_reason: Option<String>,
    pub saw_tool_use: bool,
    pub leaked_calls: usize,
    pub leaks_ignored: usize,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub thoughts_tokens: Option<i64>,
    /// Part of `input_tokens` served from the upstream context cache.
    pub cached_tokens: Option<i64>,
}

/// 当前打开的块属于哪一类。JS 里靠 `open.kind === "text" | "thinking"` 判，
/// 这边用枚举省掉字符串比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

#[derive(Debug)]
struct OpenBlock {
    kind: BlockKind,
    index: i64,
    /// 只有 thinking 块会用到；缺签名时是 `None` / 空串（JS 里 falsy）。
    signature: Option<String>,
}

#[derive(Debug)]
struct TranslatorState {
    block_index: i64,
    open: Option<OpenBlock>,
    saw_tool_use: bool,
    finish_reason: Option<String>,
    usage: Option<Value>,
    leaked_calls: usize,
    leaks_ignored: usize,
    thinking_signature_emitted: bool,
}

impl Default for TranslatorState {
    fn default() -> Self {
        Self {
            // JS 从 -1 起：第一个块 ++ 之后是 0。
            block_index: -1,
            open: None,
            saw_tool_use: false,
            finish_reason: None,
            usage: None,
            leaked_calls: 0,
            leaks_ignored: 0,
            thinking_signature_emitted: false,
        }
    }
}

/// 逐块翻译器。事件是 Anthropic SSE 事件对象，序列化交给调用方。
pub struct AnthropicTranslator {
    opts: TranslatorOptions,
    message_id: String,
    filter: LeakFilter,
    state: TranslatorState,
}

impl AnthropicTranslator {
    pub fn new(opts: TranslatorOptions) -> Self {
        let message_id = if opts.message_id.is_empty() {
            new_message_id()
        } else {
            opts.message_id.clone()
        };
        Self {
            opts,
            message_id,
            filter: LeakFilter::new(),
            state: TranslatorState::default(),
        }
    }

    /// JS 把 `messageId` 暴露在翻译器上；服务层记日志时要它。
    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    /// 流开始：Anthropic 要求先给一个 message_start（usage 先占位）。
    pub fn start(&mut self) -> Vec<Value> {
        vec![json!({
            "type": "message_start",
            "message": {
                "id": self.message_id.clone(),
                "type": "message",
                "role": "assistant",
                "model": self.opts.model.clone(),
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            },
        })]
    }

    /// 一个上游 chunk → 若干 Anthropic 事件。
    pub fn push(&mut self, chunk: &Value) -> Vec<Value> {
        let mut events = Vec::new();

        // `chunk?.response ?? chunk`：response 为 null/缺省时退回整个 chunk。
        let payload = match chunk.get("response") {
            Some(response) if !response.is_null() => response,
            _ => chunk,
        };

        // `payload?.candidates?.[0] ?? payload?.candidate`：candidates 空/缺时退回 candidate。
        let candidate = payload
            .get("candidates")
            .and_then(|candidates| candidates.get(0usize))
            .filter(|value| !value.is_null())
            .or_else(|| payload.get("candidate"))
            .filter(|value| !value.is_null());

        if let Some(finish_reason) = candidate.and_then(|c| c.get("finishReason")) {
            if truthy(Some(finish_reason)) {
                self.state.finish_reason = as_js_string(finish_reason);
            }
        }

        if let Some(meta) = payload.get("usageMetadata") {
            let usage = map_usage(meta);
            if !usage.is_null() {
                self.state.usage = Some(usage);
            }
        }

        if let Some(parts) = candidate
            .and_then(|c| c.get("content"))
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            self.handle_parts(&mut events, parts);
        }

        events
    }

    /// 流结束：放掉扣住的尾巴、补 stop 事件。
    pub fn finish(&mut self) -> Vec<Value> {
        let mut events = Vec::new();

        // JS 的 `filter.flush()` 返回一个字符串（可能扣着半截 marker 或没闭合的调用），
        // Rust 的 `finish()` 把它作为一条 Text 事件放出来，语义一致：不完整的调用当正文，不猜。
        let rest = self.filter.finish();
        for event in rest {
            if let LeakEvent::Text { text } = event {
                self.emit_text(&mut events, &text);
            }
        }
        self.close_open(&mut events);

        events.push(json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": map_stop_reason(self.state.finish_reason.as_deref(), self.state.saw_tool_use),
                "stop_sequence": Value::Null,
            },
            "usage": self.state.usage.clone().unwrap_or_else(|| json!({"input_tokens": 0, "output_tokens": 0})),
        }));
        events.push(json!({"type": "message_stop"}));
        events
    }

    /// 上游报错/连接断了：给客户端一个收尾，不要留半截流。
    pub fn fail(&mut self, message: &str, kind: &str) -> Vec<Value> {
        let mut events = Vec::new();

        let rest = self.filter.finish();
        for event in rest {
            if let LeakEvent::Text { text } = event {
                self.emit_text(&mut events, &text);
            }
        }
        self.close_open(&mut events);

        events.push(json!({"type": "error", "error": {"type": kind, "message": message}}));
        events
    }

    /// 非流式：把上游所有 chunk 一次性翻成一条消息。
    pub fn to_message(&mut self, chunks: &[Value]) -> Value {
        let mut events = self.start();
        for chunk in chunks {
            events.extend(self.push(chunk));
        }
        events.extend(self.finish());
        self.events_to_message(events)
    }

    /// 非流式路径用同一批「翻译后的事件」，把事件挪成消息体。
    ///
    /// 注意入参是翻译后的事件（start/push/finish 的产物），**不是上游 chunk**。
    pub fn events_to_message(&self, events: Vec<Value>) -> Value {
        let mut content: Vec<Value> = Vec::new();
        let mut usage: Option<Value> = None;
        let mut stop_reason: Option<Value> = None;

        for event in &events {
            match event.get("type").and_then(Value::as_str) {
                Some("content_block_start") => {
                    let Some(block) = event.get("content_block") else {
                        continue;
                    };
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => content.push(json!({"type": "text", "text": ""})),
                        Some("thinking") => content
                            .push(json!({"type": "thinking", "thinking": "", "signature": ""})),
                        Some("tool_use") => {
                            // 对齐 JSON.stringify：JS 里 b.id / b.name 是 undefined 时键会被省掉。
                            let mut block_out = Map::new();
                            block_out.insert("type".to_string(), json!("tool_use"));
                            if let Some(id) = block.get("id") {
                                block_out.insert("id".to_string(), id.clone());
                            }
                            if let Some(name) = block.get("name") {
                                block_out.insert("name".to_string(), name.clone());
                            }
                            block_out.insert("input".to_string(), json!({}));
                            content.push(Value::Object(block_out));
                        }
                        _ => {}
                    }
                }
                Some("content_block_delta") => {
                    let Some(last) = content.last_mut() else {
                        continue;
                    };
                    let Some(delta) = event.get("delta") else {
                        continue;
                    };
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            let add = delta
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            if let Some(obj) = last.as_object_mut() {
                                let prev = obj
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                obj.insert(
                                    "text".to_string(),
                                    Value::String(format!("{prev}{add}")),
                                );
                            }
                        }
                        Some("thinking_delta") => {
                            let add = delta
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            if let Some(obj) = last.as_object_mut() {
                                let prev = obj
                                    .get("thinking")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                obj.insert(
                                    "thinking".to_string(),
                                    Value::String(format!("{prev}{add}")),
                                );
                            }
                        }
                        Some("signature_delta") => {
                            if let Some(signature) = delta.get("signature") {
                                if let Some(obj) = last.as_object_mut() {
                                    obj.insert("signature".to_string(), signature.clone());
                                }
                            }
                        }
                        Some("input_json_delta") => {
                            let add = delta
                                .get("partial_json")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            if let Some(obj) = last.as_object_mut() {
                                let prev = obj
                                    .get("__json")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                obj.insert(
                                    "__json".to_string(),
                                    Value::String(format!("{prev}{add}")),
                                );
                            }
                        }
                        _ => {}
                    }
                }
                Some("message_delta") => {
                    if let Some(u) = event.get("usage") {
                        if !u.is_null() {
                            usage = Some(u.clone());
                        }
                    }
                    if let Some(sr) = event
                        .get("delta")
                        .and_then(|delta| delta.get("stop_reason"))
                    {
                        if !sr.is_null() {
                            stop_reason = Some(sr.clone());
                        }
                    }
                }
                _ => {}
            }
        }

        // tool_use 的 input 是逐段的 partial_json 拼出来的，最后一次性解析（失败就当空对象）。
        for block in content.iter_mut() {
            let Some(raw) = block
                .get("__json")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            let parsed = if raw.is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
            };
            if let Some(obj) = block.as_object_mut() {
                obj.insert("input".to_string(), parsed);
                obj.remove("__json");
            }
        }

        json!({
            "id": self.message_id.clone(),
            "type": "message",
            "role": "assistant",
            "model": self.opts.model.clone(),
            "content": content,
            "stop_reason": stop_reason.unwrap_or(Value::Null),
            "stop_sequence": Value::Null,
            "usage": usage.unwrap_or_else(|| json!({"input_tokens": 0, "output_tokens": 0})),
        })
    }

    pub fn stats(&self) -> TranslatorStats {
        let usage = self.state.usage.as_ref();
        TranslatorStats {
            finish_reason: self.state.finish_reason.clone(),
            saw_tool_use: self.state.saw_tool_use,
            leaked_calls: self.state.leaked_calls,
            leaks_ignored: self.state.leaks_ignored,
            input_tokens: usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(Value::as_i64),
            output_tokens: usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_i64),
            thoughts_tokens: usage
                .and_then(|u| u.get("thoughts_token_count"))
                .and_then(Value::as_i64),
            cached_tokens: usage
                .and_then(|u| u.get("cache_read_input_tokens"))
                .and_then(Value::as_i64),
        }
    }

    // ---------------- 内部：内容块生命周期 ----------------

    /// 关掉当前打开的块。thinking 块在关之前要把签名作为 delta 补出去。
    fn close_open(&mut self, events: &mut Vec<Value>) {
        let Some(open) = self.state.open.take() else {
            return;
        };
        if open.kind == BlockKind::Thinking && !self.state.thinking_signature_emitted {
            // 思考块必须带签名：客户端要拿签名才有资格把思考块留住（也靠它回传上游），
            // 没签名的思考块在客户端那边等于不存在 —— 实测：上游偶尔会给出没有
            // thoughtSignature 的思考 part，这种块发出去会被客户端直接丢掉，面板上就是
            // 「模型没思考」。上游给了真签名就用真的；没给就补哨兵，和请求侧的兜底
            // （`anthropic_request.rs` 里的 SIGNATURE_SENTINEL）同一个值。
            let signature = open
                .signature
                .clone()
                .filter(|signature| !signature.is_empty())
                .unwrap_or_else(|| SIGNATURE_SENTINEL.to_string());
            events.push(json!({
                "type": "content_block_delta",
                "index": open.index,
                "delta": {"type": "signature_delta", "signature": signature},
            }));
            self.state.thinking_signature_emitted = true;
        }
        events.push(json!({"type": "content_block_stop", "index": open.index}));
    }

    /// 开一个新块：先关掉上一个，索引自增，记下块类型（thinking 还要带上签名槽位）。
    fn open_block(
        &mut self,
        events: &mut Vec<Value>,
        content_block: Value,
        signature: Option<String>,
    ) {
        self.close_open(events);
        let kind = match content_block.get("type").and_then(Value::as_str) {
            Some("thinking") => BlockKind::Thinking,
            Some("tool_use") => BlockKind::ToolUse,
            _ => BlockKind::Text,
        };
        self.state.block_index += 1;
        let index = self.state.block_index;
        events.push(json!({
            "type": "content_block_start",
            "index": index,
            "content_block": content_block,
        }));
        self.state.open = Some(OpenBlock {
            kind,
            index,
            signature,
        });
        self.state.thinking_signature_emitted = false;
    }

    fn emit_text(&mut self, events: &mut Vec<Value>, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.state.open.as_ref().map(|open| open.kind) != Some(BlockKind::Text) {
            self.open_block(events, json!({"type": "text", "text": ""}), None);
        }
        let index = self.state.open.as_ref().map(|open| open.index).unwrap_or(0);
        events.push(json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": text},
        }));
    }

    fn emit_thinking(&mut self, events: &mut Vec<Value>, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.state.open.as_ref().map(|open| open.kind) != Some(BlockKind::Thinking) {
            self.open_block(
                events,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
                Some(String::new()),
            );
        }
        let index = self.state.open.as_ref().map(|open| open.index).unwrap_or(0);
        events.push(json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "thinking_delta", "thinking": text},
        }));
    }

    /// 一次 functionCall（上游的正式形态）→ 一个 tool_use 块；签名按 id 存下来供回传。
    fn emit_tool_use(
        &mut self,
        events: &mut Vec<Value>,
        name: &str,
        input: &Value,
        signature: Option<&str>,
    ) -> String {
        let tool_id = new_tool_id();
        self.open_block(
            events,
            json!({"type": "tool_use", "id": tool_id.as_str(), "name": name, "input": {}}),
            None,
        );
        let index = self.state.open.as_ref().map(|open| open.index).unwrap_or(0);
        // `JSON.stringify(input ?? {})`
        let effective = if input.is_null() {
            json!({})
        } else {
            input.clone()
        };
        let partial_json = serde_json::to_string(&effective).unwrap_or_else(|_| "{}".to_string());
        events.push(json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": partial_json},
        }));
        self.state.saw_tool_use = true;
        if let Some(signature) = signature {
            if !signature.is_empty() {
                sig_put(&self.opts.signatures, &tool_id, signature);
            }
        }
        self.close_open(events);
        tool_id
    }

    fn handle_parts(&mut self, events: &mut Vec<Value>, parts: &[Value]) {
        for part in parts {
            // JS: `if (!part || typeof part !== "object") continue;`（null 已被 `!part` 挡掉，
            // 数组也算 object）。
            if !part.is_object() && !part.is_array() {
                continue;
            }

            if truthy(part.get("functionCall")) {
                let function_call = part.get("functionCall").expect("checked above");
                let name = function_call
                    .get("name")
                    .filter(|value| !value.is_null())
                    .and_then(as_js_string)
                    .unwrap_or_else(|| "unknown_tool".to_string());
                let input = function_call
                    .get("args")
                    .filter(|value| !value.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let signature = part.get("thoughtSignature").and_then(Value::as_str);
                self.emit_tool_use(events, &name, &input, signature);
                continue;
            }

            let signature = part.get("thoughtSignature").and_then(Value::as_str);
            let text = part.get("text").and_then(Value::as_str).unwrap_or("");

            if truthy(part.get("thought")) {
                self.emit_thinking(events, text);
                // 只有非空签名、且当前正开着 thinking 块时才贴上去。
                if let Some(signature) = signature {
                    if !signature.is_empty() {
                        if let Some(open) = self.state.open.as_mut() {
                            if open.kind == BlockKind::Thinking {
                                open.signature = Some(signature.to_string());
                            }
                        }
                        if text.is_empty() {
                            // 空正文的 thought part 不会建块（`emit_thinking` 因为空正文
                            // 提前返回），如果此刻也没有开着的 thinking 块，签名就没处可贴；
                            // 而这条分支马上 `continue`，走不到下面「只有签名」那条 trailing
                            // 存储 —— 真签名会在这里白丢，下一发只能补哨兵。所以这里补一次
                            // 存储，和下面那条路径一致。
                            sig_put_trailing(
                                &self.opts.signatures,
                                &self.opts.session_key,
                                signature,
                            );
                        }
                    }
                }
                continue;
            }

            if text.is_empty() && signature.is_some_and(|signature| !signature.is_empty()) {
                // 只有签名、没有正文（也没有调用）：这是整个回合的尾部签名。
                // 在上游自己的会话里它会跟着回合回去，我们按会话存着。
                let signature = signature.expect("non-empty");
                if let Some(open) = self.state.open.as_mut() {
                    if open.kind == BlockKind::Thinking
                        && open.signature.as_deref().map_or(true, str::is_empty)
                    {
                        open.signature = Some(signature.to_string());
                    }
                }
                sig_put_trailing(&self.opts.signatures, &self.opts.session_key, signature);
                continue;
            }

            if !text.is_empty() {
                let leaked = self.filter.push(text);
                for event in leaked {
                    match event {
                        LeakEvent::Text { text } => self.emit_text(events, &text),
                        LeakEvent::ToolCall { name, input, raw } => {
                            // 保险：只有「这次请求真的声明过」的工具名才当调用。
                            // 正文里恰好出现一段长得像调用的文本（比如用户在让模型复述它）
                            // 不该变成 tool_use。
                            let allowed = match &self.opts.declared_tools {
                                Some(declared) => {
                                    declared.is_empty() || declared.iter().any(|tool| tool == &name)
                                }
                                None => true,
                            };
                            if !allowed {
                                self.state.leaks_ignored += 1;
                                self.emit_text(events, &raw);
                                continue;
                            }
                            self.state.leaked_calls += 1;
                            // 先如实回显原文，再补一个真调用（客户端两边都看得到）。
                            self.emit_text(events, &raw);
                            self.emit_tool_use(events, &name, &input, None);
                        }
                    }
                }
            }
        }
    }
}

/// `createAnthropicTranslator` 的兼容别名（旧注释/调用点用的是这个名字）。
pub fn create_anthropic_translator(opts: TranslatorOptions) -> AnthropicTranslator {
    AnthropicTranslator::new(opts)
}

/// SSE 序列化：Anthropic 的每条事件都要 `event:` 和 `data:` 两行。
pub fn sse_frame(event: &Value) -> String {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    let data = serde_json::to_string(event).unwrap_or_default();
    format!("event: {event_type}\ndata: {data}\n\n")
}

/// 出错时的收尾帧（`type` 缺省在 JS 是 api_error，这里由调用方给）。
pub fn error_frame(message: &str, kind: &str) -> String {
    sse_frame(&json!({"type": "error", "error": {"type": kind, "message": message}}))
}

fn new_tool_id() -> String {
    format!("toolu_{}", &uuid::Uuid::new_v4().simple().to_string()[..22])
}

fn new_message_id() -> String {
    format!("msg_{}", &uuid::Uuid::new_v4().simple().to_string()[..24])
}

/// JS 的 truthiness：null/undefined/false/0/NaN/"" 为假，对象和数组恒为真。
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => match number.as_f64() {
            Some(flag) => flag != 0.0,
            None => true,
        },
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// 尽力把 JSON 值当字符串用（对齐 JS 往对象里塞任意值时的行为）。
fn as_js_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signatures::{shared_signatures, sig_get, sig_take_trailing, SharedSignatures};

    /// `test/anthropic-stream.test.mjs` 的 `chunkWith`：candidates[0] + usageMetadata。
    fn chunk(parts: Value, finish: Option<&str>, usage: Option<Value>) -> Value {
        json!({
            "response": {
                "candidates": [{
                    "content": {"role": "model", "parts": parts},
                    "finishReason": finish
                        .map(|value| Value::String(value.to_string()))
                        .unwrap_or(Value::Null),
                }],
                "usageMetadata": usage.unwrap_or(Value::Null),
            },
            "traceId": "t",
        })
    }

    fn options(model: &str) -> TranslatorOptions {
        options_with(model, shared_signatures(), "default")
    }

    fn options_with(
        model: &str,
        signatures: SharedSignatures,
        session_key: &str,
    ) -> TranslatorOptions {
        TranslatorOptions {
            model: model.to_string(),
            signatures,
            session_key: session_key.to_string(),
            message_id: String::new(),
            declared_tools: None,
        }
    }

    fn collect(translator: &mut AnthropicTranslator, chunks: &[Value]) -> Vec<Value> {
        let mut events = translator.start();
        for chunk in chunks {
            events.extend(translator.push(chunk));
        }
        events.extend(translator.finish());
        events
    }

    fn deltas_of(events: &[Value], index: i64) -> Vec<Value> {
        events
            .iter()
            .filter(|event| {
                event.get("type").and_then(Value::as_str) == Some("content_block_delta")
                    && event.get("index").and_then(Value::as_i64) == Some(index)
            })
            .cloned()
            .collect()
    }

    fn count_type(events: &[Value], kind: &str) -> usize {
        events
            .iter()
            .filter(|event| event.get("type").and_then(Value::as_str) == Some(kind))
            .count()
    }

    #[test]
    fn plain_text_stream() {
        let mut translator = AnthropicTranslator::new(options("m"));
        let events = collect(
            &mut translator,
            &[
                chunk(json!([{"text": "你好"}]), None, None),
                chunk(
                    json!([{"text": "，世界"}]),
                    Some("STOP"),
                    Some(json!({"promptTokenCount": 10, "candidatesTokenCount": 3})),
                ),
            ],
        );
        assert_eq!(events[0]["type"], "message_start");
        assert_eq!(events[0]["message"]["model"], "m");
        let texts: Vec<String> = events
            .iter()
            .filter(|event| event["type"] == "content_block_delta")
            .map(|event| {
                let delta = &event["delta"];
                delta["text"]
                    .as_str()
                    .or_else(|| delta["thinking"].as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        assert_eq!(texts, vec!["你好".to_string(), "，世界".to_string()]);
        assert_eq!(count_type(&events, "content_block_start"), 1);
        assert_eq!(count_type(&events, "content_block_stop"), 1);
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(
            delta["usage"],
            json!({"input_tokens": 10, "output_tokens": 3})
        );
        assert_eq!(events.last().unwrap()["type"], "message_stop");
    }

    #[test]
    fn thinking_block_and_signature() {
        let mut translator = AnthropicTranslator::new(options("m"));
        let events = collect(
            &mut translator,
            &[
                chunk(
                    json!([{"text": "我在想", "thought": true, "thoughtSignature": "TSIG-1"}]),
                    None,
                    None,
                ),
                chunk(
                    json!([{"text": "结论"}]),
                    Some("STOP"),
                    Some(
                        json!({"promptTokenCount": 7, "candidatesTokenCount": 2, "thoughtsTokenCount": 5}),
                    ),
                ),
            ],
        );
        let starts: Vec<&str> = events
            .iter()
            .filter(|event| event["type"] == "content_block_start")
            .map(|event| event["content_block"]["type"].as_str().unwrap())
            .collect();
        assert_eq!(starts, vec!["thinking", "text"]);

        let thinking_index = events
            .iter()
            .find(|event| {
                event["type"] == "content_block_start"
                    && event["content_block"]["type"] == "thinking"
            })
            .unwrap()["index"]
            .as_i64()
            .unwrap();
        let signature = deltas_of(&events, thinking_index)
            .into_iter()
            .find(|event| event["delta"]["type"] == "signature_delta")
            .unwrap();
        assert_eq!(signature["delta"]["signature"], "TSIG-1");

        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["usage"]["output_tokens"], 7);
    }

    /// 上游偶尔给没有签名的思考 part（实测 2026-09-18 04:48 的那发「你好」就是这样）。
    /// 这种块必须补哨兵签名：客户端要拿签名才有资格把思考块留住，没签名的在它那边会被
    /// 直接丢掉 —— 表现就是「模型没思考」。流式与非流式共用同一个 `close_open`。
    #[test]
    fn thinking_block_without_signature_gets_the_sentinel() {
        let mut translator = AnthropicTranslator::new(options("m"));
        let events = collect(
            &mut translator,
            &[
                chunk(json!([{"text": "我先想想", "thought": true}]), None, None),
                chunk(json!([{"text": "答案"}]), Some("STOP"), None),
            ],
        );
        let thinking_index = events
            .iter()
            .find(|event| {
                event["type"] == "content_block_start"
                    && event["content_block"]["type"] == "thinking"
            })
            .unwrap()["index"]
            .as_i64()
            .unwrap();
        let signature = deltas_of(&events, thinking_index)
            .into_iter()
            .find(|event| event["delta"]["type"] == "signature_delta")
            .expect("没签名的思考块也要有一发 signature_delta");
        assert_eq!(signature["delta"]["signature"], SIGNATURE_SENTINEL);

        // 非流式那条路复用同一批事件，聚合出来的块里也要带着这个签名
        let message = translator.events_to_message(events);
        assert_eq!(message["content"][0]["type"], "thinking");
        assert_eq!(message["content"][0]["signature"], SIGNATURE_SENTINEL);
    }

    /// 上游有时把回合签名单独放在一个空正文的 thought part 里：
    /// `{"text": "", "thought": true, "thoughtSignature": …}`。它不会建 thinking 块
    /// （空正文），此刻若没有开着的块，签名只能走 trailing 存储 —— 这条分支不能因为
    /// 不建块就把签名白丢，否则下一发回传时只能拿哨兵顶包。
    #[test]
    fn empty_thought_part_stores_signature_as_trailing() {
        let store = shared_signatures();
        let mut translator =
            AnthropicTranslator::new(options_with("m", store.clone(), "sess-thought"));
        let events = collect(
            &mut translator,
            &[chunk(
                json!([{"text": "", "thought": true, "thoughtSignature": "THOUGHT-TAIL-1"}]),
                Some("STOP"),
                None,
            )],
        );
        assert_eq!(
            sig_take_trailing(&store, "sess-thought").as_deref(),
            Some("THOUGHT-TAIL-1")
        );
        // 空正文不开 thinking 块（别把行为扩大到「空正文也建块」）
        assert_eq!(count_type(&events, "content_block_start"), 0);
    }

    #[test]
    fn function_call_becomes_tool_use_and_stores_signature() {
        let store = shared_signatures();
        let mut translator = AnthropicTranslator::new(options_with("m", store.clone(), "s1"));
        let events = collect(
            &mut translator,
            &[chunk(
                json!([{
                    "thoughtSignature": "REAL-SIG",
                    "functionCall": {"name": "get_time", "args": {"tz": "Asia/Shanghai"}},
                }]),
                Some("STOP"),
                None,
            )],
        );
        let start = events
            .iter()
            .find(|event| {
                event["type"] == "content_block_start"
                    && event["content_block"]["type"] == "tool_use"
            })
            .unwrap()
            .clone();
        assert_eq!(start["content_block"]["name"], "get_time");
        let id = start["content_block"]["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("toolu_"));
        let json_delta = deltas_of(&events, start["index"].as_i64().unwrap())
            .into_iter()
            .find(|event| event["delta"]["type"] == "input_json_delta")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(json_delta["delta"]["partial_json"].as_str().unwrap())
                .unwrap(),
            json!({"tz": "Asia/Shanghai"})
        );
        assert_eq!(sig_get(&store, &id).as_deref(), Some("REAL-SIG"));
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn parallel_tool_calls_only_first_has_signature() {
        let store = shared_signatures();
        let mut translator = AnthropicTranslator::new(options_with("m", store.clone(), "default"));
        let events = collect(
            &mut translator,
            &[chunk(
                json!([
                    {"thoughtSignature": "SIG-A", "functionCall": {"name": "get_time", "args": {"tz": "Asia/Shanghai"}}},
                    {"functionCall": {"name": "get_time", "args": {"tz": "America/New_York"}}},
                ]),
                None,
                None,
            )],
        );
        let ids: Vec<String> = events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_start"
                    && event["content_block"]["type"] == "tool_use"
            })
            .map(|event| event["content_block"]["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(sig_get(&store, &ids[0]).as_deref(), Some("SIG-A"));
        assert_eq!(sig_get(&store, &ids[1]), None);
    }

    #[test]
    fn trailing_signature_is_stored_per_session() {
        let store = shared_signatures();
        let mut translator = AnthropicTranslator::new(options_with("m", store.clone(), "sess-9"));
        collect(
            &mut translator,
            &[
                chunk(json!([{"text": "回答"}]), None, None),
                chunk(
                    json!([{"text": "", "thoughtSignature": "TAIL-1"}]),
                    Some("STOP"),
                    None,
                ),
            ],
        );
        assert_eq!(
            sig_take_trailing(&store, "sess-9").as_deref(),
            Some("TAIL-1")
        );
    }

    #[test]
    fn leaked_pseudo_call_is_echoed_then_repaired() {
        let mut translator =
            AnthropicTranslator::new(options_with("m", shared_signatures(), "default"));
        let events = collect(
            &mut translator,
            &[
                chunk(
                    json!([{"text": "我先看看 <call:default_api:Grep{pattern:"}]),
                    None,
                    None,
                ),
                chunk(json!([{"text": "foo}"}]), Some("STOP"), None),
            ],
        );
        let text: String = events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
            })
            .map(|event| event["delta"]["text"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(text, "我先看看 <call:default_api:Grep{pattern:foo}");
        let tool = events
            .iter()
            .find(|event| {
                event["type"] == "content_block_start"
                    && event["content_block"]["type"] == "tool_use"
            })
            .unwrap();
        assert_eq!(tool["content_block"]["name"], "Grep");
        let index = tool["index"].as_i64().unwrap();
        let json_delta = deltas_of(&events, index)
            .into_iter()
            .find(|event| event["delta"]["type"] == "input_json_delta")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(json_delta["delta"]["partial_json"].as_str().unwrap())
                .unwrap(),
            json!({"pattern": "foo"})
        );
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
        assert_eq!(translator.stats().leaked_calls, 1);
    }

    #[test]
    fn marker_split_across_chunks_is_repaired() {
        let mut translator =
            AnthropicTranslator::new(options_with("m", shared_signatures(), "default"));
        let events = collect(
            &mut translator,
            &[
                chunk(json!([{"text": "看 <ca"}]), None, None),
                chunk(json!([{"text": "ll:Read{file_path:/a.ts}"}]), None, None),
            ],
        );
        let tool = events
            .iter()
            .find(|event| {
                event["type"] == "content_block_start"
                    && event["content_block"]["type"] == "tool_use"
            })
            .unwrap();
        assert_eq!(tool["content_block"]["name"], "Read");
    }

    #[test]
    fn fail_closes_the_stream_with_an_error_event() {
        let mut translator = AnthropicTranslator::new(options("m"));
        translator.start();
        translator.push(&chunk(json!([{"text": "半句话"}]), None, None));
        let events = translator.fail("上游 500", "overloaded_error");
        assert_eq!(events.last().unwrap()["type"], "error");
        assert_eq!(events.last().unwrap()["error"]["type"], "overloaded_error");
        assert_eq!(count_type(&events, "content_block_stop"), 1);
    }

    #[test]
    fn events_to_message_builds_a_full_message() {
        let mut translator =
            AnthropicTranslator::new(options_with("m", shared_signatures(), "default"));
        let message = translator.to_message(&[chunk(
            json!([{"text": "半句"}, {"functionCall": {"name": "Grep", "args": {"pattern": "x"}}}]),
            Some("STOP"),
            Some(json!({"promptTokenCount": 5, "candidatesTokenCount": 4})),
        )]);
        assert_eq!(message["type"], "message");
        assert_eq!(message["content"][0]["type"], "text");
        assert_eq!(message["content"][0]["text"], "半句");
        assert_eq!(message["content"][1]["type"], "tool_use");
        assert_eq!(message["content"][1]["input"], json!({"pattern": "x"}));
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(
            message["usage"],
            json!({"input_tokens": 5, "output_tokens": 4})
        );
    }

    #[test]
    fn stop_reason_and_usage_edges() {
        assert_eq!(map_stop_reason(Some("MAX_TOKENS"), true), "max_tokens");
        assert_eq!(map_stop_reason(Some("STOP"), false), "end_turn");
        assert_eq!(map_stop_reason(Some("SAFETY"), false), "end_turn");
        assert_eq!(map_stop_reason(None, true), "tool_use");
        assert_eq!(map_usage(&Value::Null), Value::Null);
        assert_eq!(
            map_usage(
                &json!({"promptTokenCount": 1, "candidatesTokenCount": 2, "cachedContentTokenCount": 3})
            ),
            json!({"input_tokens": 1, "output_tokens": 2, "cache_read_input_tokens": 3})
        );
    }

    #[test]
    fn sse_frame_has_event_and_data_lines() {
        let frame = sse_frame(&json!({"type": "ping"}));
        assert_eq!(frame, "event: ping\ndata: {\"type\":\"ping\"}\n\n");
    }

    #[test]
    fn empty_chunks_do_not_panic() {
        let mut translator = AnthropicTranslator::new(options("m"));
        let events = collect(
            &mut translator,
            &[
                json!({}),
                json!({"response": {}}),
                json!({"response": {"candidates": []}}),
            ],
        );
        assert_eq!(events.last().unwrap()["type"], "message_stop");
    }

    /// JS 测试没有覆盖翻译器这一层的白名单判断（那边的 leak-repair 测试覆盖了过滤器），
    /// 但这是「正文里恰好长得像调用」的关键保险，补一条。
    #[test]
    fn undeclared_leaked_tool_is_left_as_text() {
        let mut opts = options_with("m", shared_signatures(), "default");
        opts.declared_tools = Some(vec!["Read".to_string()]);
        let mut translator = AnthropicTranslator::new(opts);
        let events = collect(
            &mut translator,
            &[chunk(
                json!([{"text": "<call:default_api:Grep{pattern:hello}"}]),
                Some("STOP"),
                None,
            )],
        );
        assert_eq!(count_type(&events, "content_block_start"), 1);
        assert!(!events
            .iter()
            .any(|event| event["content_block"]["type"] == "tool_use"));
        let text: String = events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
            })
            .map(|event| event["delta"]["text"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(text, "<call:default_api:Grep{pattern:hello}");
        let stats = translator.stats();
        assert_eq!(stats.leaked_calls, 0);
        assert_eq!(stats.leaks_ignored, 1);
    }

    /// 白名单里的工具照修（对应过滤器测试里的「声明过这个工具」）。
    #[test]
    fn declared_leaked_tool_is_repaired() {
        let mut opts = options_with("m", shared_signatures(), "default");
        opts.declared_tools = Some(vec!["Grep".to_string()]);
        let mut translator = AnthropicTranslator::new(opts);
        let events = collect(
            &mut translator,
            &[chunk(
                json!([{"text": "<call:default_api:Grep{pattern:hello}"}]),
                Some("STOP"),
                None,
            )],
        );
        let tool = events
            .iter()
            .find(|event| {
                event["type"] == "content_block_start"
                    && event["content_block"]["type"] == "tool_use"
            })
            .expect("repaired");
        assert_eq!(tool["content_block"]["name"], "Grep");
        let stats = translator.stats();
        assert_eq!(stats.leaked_calls, 1);
        assert_eq!(stats.leaks_ignored, 0);
        assert!(stats.saw_tool_use);
    }

    #[test]
    fn error_frame_matches_the_sse_shape() {
        let frame = error_frame("boom", "api_error");
        let expected = json!({"type": "error", "error": {"type": "api_error", "message": "boom"}});
        assert_eq!(
            frame,
            format!(
                "event: error\ndata: {}\n\n",
                serde_json::to_string(&expected).unwrap()
            )
        );
    }
}
