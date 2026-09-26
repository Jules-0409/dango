//! 请求侧修复：清掉「孤儿 tool 消息」、补齐「悬空 tool_use」。
//!
//! 移植自 `src/bridge/sanitize.mjs`（测试：`test/request-repair.test.mjs`）。
//!
//! 为什么需要这些修复：客户端（Factory）回传历史时会出现两类坏形状——
//!
//!   1. **孤儿 tool_result**：没有对应 tool_use 的 tool 结果；
//!   2. **悬空 tool_use**：最后一条 assistant 发了 tool_use，却没有对应的 tool_result。
//!
//! 上游对这两种都会 400，而且重试也没用。修复只发生在「送去上游之前」，
//! 语义不动：只动坏的那一条，其余原样。
//!
//! 真实故障：压缩/翻译弄丢前置的 tool_calls 之后，历史里留下一条没有归属的 tool 结果，
//! 这类请求本身发不出去（两台机器都撞过）。协议逻辑不依赖上游，所以能本地修。
//!
//! 返回值：JS 版这几个函数返回 `{ changed, removed|filled }`；Rust 版按桩里的约定
//! 用 `usize` 表达「改动了几处」——
//!   - `sanitize_*` 返回删掉的块/消息条数（= JS 的 `removed.length`，`changed ⇔ > 0`；
//!     整条消息只因孤儿被删时，不再额外计数，与 JS 一致）；
//!   - `fill_missing_*` 返回补出的占位结果条数（= JS 的 `filled.length`）。
//!
//! body 形状不对（没有 messages 数组等）时返回 0（对应 JS 的 `changed: false`）。

use serde_json::{json, Map, Value};

/// 上游没收到工具结果时补的占位文案。两种协议的占位内容字面量相同（JS 版就是同一个串）。
const MISSING_TOOL_RESULT_TEXT: &str =
    "[[bridge]] 上游没有收到这个工具的结果（客户端在调用后中断了），请据此继续。";

/// JS 的 truthiness：`if (x)`。只用于「id / tool_call_id 是否存在且有值」这类判断，
/// 得和 JS 对齐（空串、0、null、false 都是假）。
fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => match n.as_f64() {
            Some(f) => f != 0.0,
            None => true,
        },
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `b?.type === ty`：b 不是对象时取不到 type，按不匹配处理。
fn block_is(b: &Value, ty: &str) -> bool {
    b.get("type").and_then(Value::as_str) == Some(ty)
}

/// 按形状分派：anthropic（content 块）或 openai（role:"tool"）。
/// JS 的判据是「任意一条消息 role === "tool"」→ 走 OpenAI 版。
pub fn sanitize_messages(body: &mut Value) -> usize {
    // 没有 messages 数组 → 原样返回 0（JS: {changed:false, removed:[]}）。
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return 0;
    };
    let has_tool_role = messages
        .iter()
        .any(|m| m.get("role").and_then(Value::as_str) == Some("tool"));
    if has_tool_role {
        sanitize_openai(body)
    } else {
        sanitize_anthropic(body)
    }
}

/// Anthropic：tool_result 块必须能对上前面的 tool_use，孤儿块删掉；
/// 整条消息只剩孤儿时，整条消息删掉。
///
/// 扫描是单趟顺序的：只有**前面**（含当前）assistant 消息里的 tool_use 算「已知」，
/// 和 JS 的 Set 行为一致——顺序不能改，否则会误判。
pub fn sanitize_anthropic(body: &mut Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    // 已知的 tool_use id。JS 用 Set，这里用 Vec；id 实际都是短字符串，线性查足够，
    // 也避免为了 Hash 把 id 转成字符串键（那会改变「值相等」的语义）。
    let mut known: Vec<Value> = Vec::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut removed = 0usize;

    for msg in messages.iter() {
        let Some(obj) = msg.as_object() else {
            // 非对象（null / 字符串 / 数组……）：JS 里同样原样保留。
            out.push(msg.clone());
            continue;
        };

        // assistant + content 数组：登记其中的 tool_use id，消息本身原样保留。
        if obj.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(content) = obj.get("content").and_then(Value::as_array) {
                for b in content {
                    if block_is(b, "tool_use") {
                        if let Some(id) = b.get("id") {
                            if js_truthy(id) {
                                known.push(id.clone());
                            }
                        }
                    }
                }
                out.push(msg.clone());
                continue;
            }
        }

        // 其它带 content 数组的消息：逐块过滤孤儿 tool_result。
        if let Some(content) = obj.get("content").and_then(Value::as_array) {
            let mut kept: Vec<Value> = Vec::with_capacity(content.len());
            for b in content {
                let is_orphan = block_is(b, "tool_result")
                    && b.get("tool_use_id").is_some_and(js_truthy)
                    && !known.iter().any(|k| Some(k) == b.get("tool_use_id"));
                if is_orphan {
                    removed += 1;
                    continue;
                }
                kept.push(b.clone());
            }
            if kept.is_empty() && !content.is_empty() {
                // 整条只有孤儿 → 删掉（removed 里已逐块计过数，不再重复计数）。
                continue;
            }
            if kept.len() != content.len() {
                // 一条消息里既有好的也有孤儿：只删孤儿块，其余保留。
                let mut fixed = obj.clone();
                fixed.insert("content".to_string(), Value::Array(kept));
                out.push(Value::Object(fixed));
                continue;
            }
        }

        out.push(msg.clone());
    }

    if removed > 0 {
        *messages = out;
    }
    removed
}

/// OpenAI 风格：role:"tool" 的 tool_call_id 必须能在前面的 assistant.tool_calls 里找到，
/// 找不到就整条删掉。
pub fn sanitize_openai(body: &mut Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut known: Vec<Value> = Vec::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut removed = 0usize;

    for msg in messages.iter() {
        let Some(obj) = msg.as_object() else {
            out.push(msg.clone());
            continue;
        };

        if obj.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(calls) = obj.get("tool_calls").and_then(Value::as_array) {
                for tc in calls {
                    if let Some(id) = tc.get("id") {
                        if js_truthy(id) {
                            known.push(id.clone());
                        }
                    }
                }
                out.push(msg.clone());
                continue;
            }
        }

        if obj.get("role").and_then(Value::as_str) == Some("tool") {
            // `msg.tool_call_id && !known.has(msg.tool_call_id)`：id 为空/缺失的不算孤儿。
            if let Some(id) = obj.get("tool_call_id") {
                if js_truthy(id) && !known.iter().any(|k| k == id) {
                    removed += 1;
                    continue;
                }
            }
        }

        out.push(msg.clone());
    }

    if removed > 0 {
        *messages = out;
    }
    removed
}

/// OpenAI 版的「悬空 tool_use」修复：最后一条 assistant 带 tool_calls，
/// 却没有对应的 role:"tool" 消息（说明客户端在调用后中断了）→ 逐个补占位结果，
/// 让回合能继续。
pub fn fill_missing_openai_tool_results(body: &mut Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let Some(last) = messages.last() else {
        return 0;
    };
    if last.get("role").and_then(Value::as_str) != Some("assistant") {
        return 0;
    }
    let Some(calls) = last.get("tool_calls").and_then(Value::as_array) else {
        return 0;
    };
    if calls.is_empty() {
        return 0;
    }
    // 先拷出来再往 messages 里 push，避免同时借 messages。
    let calls: Vec<Value> = calls.to_vec();

    for tc in &calls {
        // JS 是 `{ role, tool_call_id: tc.id, name: tc.function?.name, content }`：
        // tc.id 为 undefined（字段不存在）时该键会被 JSON 丢掉；存在就原样搬（含 null）。
        let mut m = Map::new();
        m.insert("role".to_string(), Value::String("tool".to_string()));
        if let Some(id) = tc.get("id") {
            m.insert("tool_call_id".to_string(), id.clone());
        }
        if let Some(name) = tc.get("function").and_then(|f| f.get("name")) {
            m.insert("name".to_string(), name.clone());
        }
        m.insert(
            "content".to_string(),
            Value::String(MISSING_TOOL_RESULT_TEXT.to_string()),
        );
        messages.push(Value::Object(m));
    }
    calls.len()
}

/// Anthropic 版的「悬空 tool_use」修复：历史里最后一条是 assistant 的 tool_use，
/// 但没有对应的 tool_result。这种回合一侧能过、一侧会 400（回合没闭合），
/// 补一条占位结果让会话能继续。
pub fn fill_missing_tool_results(body: &mut Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    // 已经是 OpenAI 形状（有 role:"tool"）的不归这里管。
    if messages
        .iter()
        .any(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
    {
        return 0;
    }
    let Some(last) = messages.last() else {
        return 0;
    };
    if last.get("role").and_then(Value::as_str) != Some("assistant") {
        return 0;
    }
    let Some(content) = last.get("content").and_then(Value::as_array) else {
        return 0;
    };

    let ids: Vec<Value> = content
        .iter()
        .filter_map(|b| {
            if !block_is(b, "tool_use") {
                return None;
            }
            let id = b.get("id")?;
            if js_truthy(id) {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();
    if ids.is_empty() {
        return 0;
    }

    let results: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "type": "tool_result",
                "tool_use_id": id.clone(),
                "content": MISSING_TOOL_RESULT_TEXT,
                "is_error": true,
            })
        })
        .collect();
    messages.push(json!({ "role": "user", "content": results }));
    ids.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_orphan_result_message_is_dropped() {
        let mut body = json!({
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "跑一下" }] },
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "Grep", "input": {} }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "ok" }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "gone", "content": "?" }] },
            ],
        });
        let removed = sanitize_anthropic(&mut body);
        assert_eq!(removed, 1);
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn anthropic_mixed_message_only_drops_orphan_block() {
        let mut body = json!({
            "messages": [
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "Grep", "input": {} }] },
                {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "t1", "content": "ok" },
                        { "type": "tool_result", "tool_use_id": "gone", "content": "?" },
                    ],
                },
            ],
        });
        let removed = sanitize_anthropic(&mut body);
        assert_eq!(removed, 1);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], "t1");
    }

    #[test]
    fn sanitize_messages_dispatches_to_openai() {
        let mut body = json!({
            "messages": [
                { "role": "assistant", "tool_calls": [{ "id": "c1" }] },
                { "role": "tool", "tool_call_id": "c1", "content": "ok" },
                { "role": "tool", "tool_call_id": "c2", "content": "孤儿" },
            ],
        });
        let removed = sanitize_messages(&mut body);
        assert_eq!(removed, 1);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn openai_without_orphans_is_untouched() {
        let mut body = json!({
            "messages": [
                { "role": "assistant", "tool_calls": [{ "id": "c1" }] },
                { "role": "tool", "tool_call_id": "c1", "content": "ok" },
            ],
        });
        assert_eq!(sanitize_openai(&mut body), 0);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn anthropic_plain_text_history_is_untouched() {
        let mut body = json!({
            "messages": [
                { "role": "user", "content": "你好" },
                { "role": "assistant", "content": "在" },
            ],
        });
        assert_eq!(sanitize_anthropic(&mut body), 0);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn fill_missing_tool_results_appends_placeholder() {
        let mut body = json!({
            "messages": [
                { "role": "user", "content": "查时间" },
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "get_time", "input": {} }] },
            ],
        });
        let filled = fill_missing_tool_results(&mut body);
        assert_eq!(filled, 1);
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        let filler = &body["messages"][2];
        assert_eq!(filler["role"], "user");
        assert_eq!(filler["content"][0]["tool_use_id"], "t1");
        assert_eq!(filler["content"][0]["is_error"], true);
        assert_eq!(filler["content"][0]["type"], "tool_result");
    }

    #[test]
    fn fill_missing_tool_results_not_needed_when_result_present() {
        let mut body = json!({
            "messages": [
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "x", "input": {} }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "ok" }] },
            ],
        });
        assert_eq!(fill_missing_tool_results(&mut body), 0);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn fill_missing_openai_tool_results_appends_placeholders() {
        let mut body = json!({
            "messages": [
                { "role": "user", "content": "查时间" },
                {
                    "role": "assistant",
                    "tool_calls": [
                        { "id": "c1", "type": "function", "function": { "name": "get_time", "arguments": "{}" } },
                        { "id": "c2", "type": "function", "function": { "name": "get_weather" } },
                    ],
                },
            ],
        });
        let filled = fill_missing_openai_tool_results(&mut body);
        assert_eq!(filled, 2);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "c1");
        assert_eq!(msgs[2]["name"], "get_time");
        assert_eq!(msgs[3]["tool_call_id"], "c2");
        assert_eq!(msgs[3]["name"], "get_weather");
        assert_eq!(msgs[3]["content"], MISSING_TOOL_RESULT_TEXT);
    }

    #[test]
    fn fill_missing_openai_needs_last_assistant_with_calls() {
        // 最后一条不是 assistant（这里是 user）→ 不动。
        let mut body = json!({
            "messages": [
                { "role": "user", "content": "你好" },
            ],
        });
        assert_eq!(fill_missing_openai_tool_results(&mut body), 0);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn non_message_shapes_are_no_ops() {
        let mut body = json!({ "model": "x" });
        assert_eq!(sanitize_messages(&mut body), 0);
        assert_eq!(sanitize_anthropic(&mut body), 0);
        assert_eq!(sanitize_openai(&mut body), 0);
        assert_eq!(fill_missing_tool_results(&mut body), 0);
        assert_eq!(fill_missing_openai_tool_results(&mut body), 0);
    }

    #[test]
    fn tool_result_without_id_is_kept() {
        // JS: `b.tool_use_id` 为空串（falsy）不算孤儿，保留。
        let mut body = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "", "content": "?" }],
                },
            ],
        });
        assert_eq!(sanitize_anthropic(&mut body), 0);
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 1);
    }
}
