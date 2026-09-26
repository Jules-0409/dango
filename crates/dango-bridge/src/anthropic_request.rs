//! Anthropic `/v1/messages` 请求 → Cloud Code v1internal 的 request 体。
//!
//! 这一层只做「形状转换」，不做 IO。移植自 `src/bridge/anthropic-request.mjs`，
//! 行为基准是 JS 版与 `test/anthropic-request.test.mjs`。所有「为什么这么写」的原因
//! 照抄 JS 注释：预算钳制、思考下限抬升、签名哨兵、会话指纹。
//!
//! 需要认识的 Anthropic 内容块：text / image / tool_use / tool_result / thinking /
//! redacted_thinking。需要认识的请求字段：system / messages / tools / tool_choice /
//! max_tokens / temperature / top_p / top_k / stop_sequences / thinking / metadata。
//!
//! 返回形状：`BuiltRequest`。JS 的 `buildGeminiRequest` 返回
//! `{ contents, tools?, toolConfig?, systemInstruction?, generationConfig, warnings, stats }`，
//! 其中前五项就是上游要的 `request` 体，Rust 版装进 `inner`；`warnings` / `stats`
//! 是给服务层做日志与收口的说明，和 `inner` 平级（JS 里也是平级）。

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::quota::ModelLimit;
use crate::signatures::{sig_get, SharedSignatures, SIGNATURE_SENTINEL};

/// 上游实测上限（超过会被拒或截断）。拿不到模型表时才退回它。
pub const MAX_OUTPUT_TOKENS: i64 = 65536;
/// 思考预算托底输出上限时给正文留的余量（对应 JS 的 `MAX_TOKENS_BUMP`）。
const MAX_TOKENS_BUMP: i64 = 8192;

/// 请求侧统计。和 OpenAI 侧（`openai::BuildStats`）同形 —— 服务层把两边的命中率记到一起。
///
/// JS 的 stats 还带 `thinkingBlocks` / `toolResults` 两个计数，但服务层从不读它们，
/// 所以 Rust 版按接口只留签名这两个（见 `core/src/openai.rs` 的同款结构）。
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildStats {
    pub tool_signature_hits: usize,
    pub tool_signature_misses: usize,
}

/// 描述这次翻译能拿到的外部资源。
pub struct BuildOptions<'a> {
    /// 签名仓库（按会话共享）。没有就全部走哨兵。
    pub signatures: Option<&'a SharedSignatures>,
    /// 真会话的 key。JS 的 `buildGeminiRequest` 在这一层其实不读它（尾部签名在流式侧处理），
    /// 保留是为了和服务层的调用形状对齐。
    pub session_key: &'a str,
    /// 上游模型表里这个模型的硬上限；`None` 表示拿不到表（退回硬编码上限）。
    pub limits: Option<&'a ModelLimit>,
}

impl Default for BuildOptions<'_> {
    fn default() -> Self {
        Self {
            signatures: None,
            session_key: "default",
            limits: None,
        }
    }
}

/// 一次请求转换的结果：`inner` 是 v1internal 的 `request` 体，
/// `warnings` 是「我们替客户端收了口」的说明，`stats` 给日志用。
#[derive(Debug, Clone)]
pub struct BuiltRequest {
    pub inner: Value,
    pub stats: BuildStats,
    pub warnings: Vec<String>,
}

/// 从请求里挑一个稳定的会话键：签名缓存按会话隔离，避免串号。
pub fn derive_session_key(body: &Value) -> String {
    let meta = body.get("metadata");
    if let Some(m) = meta.and_then(Value::as_object) {
        if let Some(s) = m
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return s.to_string();
        }
    }
    let user_id = meta.and_then(|m| m.get("user_id"));
    if let Some(uid) = user_id.and_then(Value::as_str).filter(|s| !s.is_empty()) {
        if let Some(session_id) = session_id_from_user_id(uid) {
            return session_id;
        }
        // JS 的 `uid.slice(0, 200)` 按 UTF-16 码元切；这里按 Unicode 标量切。
        // 只有非 BMP 字符（emoji 等）在 200 位边界上才可能差一个字符，实际 key 用不到。
        return uid.chars().take(200).collect();
    }
    if let Some(obj) = user_id.and_then(Value::as_object) {
        if let Some(sid) = obj.get("session_id").filter(|v| is_truthy(v)) {
            return js_string(sid);
        }
    }
    "default".to_string()
}

/// 建请求体。对应 JS 的 `buildGeminiRequest`。
pub fn build_gemini_request(body: &Value, opts: BuildOptions<'_>) -> BuiltRequest {
    let mut warnings: Vec<String> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut stats = BuildStats::default();

    let empty: Vec<Value> = Vec::new();
    let messages: &[Value] = body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&empty);

    // tool_result 只用 id 引用工具名，先从全部 assistant 回合里建一张 id → name 表。
    let mut tool_name_by_id: HashMap<String, Option<String>> = HashMap::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for b in content {
            if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            // 工具 id 实际都是字符串（Anthropic 协议）；非字符串 id 这条路径不存在。
            if let Some(id) = b
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let name = b.get("name").and_then(Value::as_str).map(str::to_string);
                tool_name_by_id.insert(id.to_string(), name);
            }
        }
    }

    // ---- system → systemInstruction
    let system = body.get("system");
    let wants_system = match system {
        Some(Value::Array(items)) => !items.is_empty(),
        other => truthy(other),
    };
    let mut system_instruction: Option<Value> = None;
    if wants_system {
        let text = text_of(system);
        if !text.trim().is_empty() {
            system_instruction = Some(json!({ "parts": [{ "text": text }] }));
        }
    }

    // ---- messages
    for msg_value in messages {
        let Some(msg) = msg_value.as_object() else {
            // JS 的 `!msg || typeof msg !== "object"`：非对象一律跳过。
            continue;
        };
        let role = if msg.get("role").and_then(Value::as_str) == Some("assistant") {
            "model"
        } else {
            "user"
        };
        let owned_blocks;
        let blocks: &[Value] = match msg.get("content") {
            Some(Value::Array(items)) => items,
            // 非数组 content 包成一个 text 块（对应 JS 的 `[{type:"text", text: textOf(content)}]`）。
            other => {
                owned_blocks = vec![json!({ "type": "text", "text": text_of(other) })];
                &owned_blocks
            }
        };

        let mut parts: Vec<Value> = Vec::new();
        // 连续的一组 tool_use 视为「一组并行调用」：上游只给这组里的第一个 part 发签名，
        // 回传时也只能给第一个 part 带（Phase 0 实测，PROTOCOL.md §7）。
        let mut call_group: Vec<CallGroup> = Vec::new();

        for block in blocks {
            // JS 的 `!block || typeof block !== "object"`：数组/字符串/数字块一律跳过。
            let Some(obj) = block.as_object() else {
                continue;
            };
            match obj.get("type").and_then(Value::as_str) {
                Some("text") => {
                    flush_call_group(&mut call_group, &mut parts, &mut stats, opts.signatures);
                    if let Some(t) = obj.get("text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            parts.push(json!({ "text": t }));
                        }
                    }
                }
                Some("thinking") => {
                    flush_call_group(&mut call_group, &mut parts, &mut stats, opts.signatures);
                    if let Some(thinking) = obj.get("thinking").filter(|v| is_truthy(v)) {
                        let signature = truthy_str(obj.get("signature"))
                            .unwrap_or_else(|| SIGNATURE_SENTINEL.to_string());
                        parts.push(json!({
                            "text": thinking.clone(),
                            "thought": true,
                            "thoughtSignature": signature,
                        }));
                    }
                }
                Some("redacted_thinking") => {
                    // 上游没有对应形态，丢掉并记账（不静默）。
                    flush_call_group(&mut call_group, &mut parts, &mut stats, opts.signatures);
                    warnings.push("redacted_thinking_dropped".to_string());
                }
                Some("image") => {
                    flush_call_group(&mut call_group, &mut parts, &mut stats, opts.signatures);
                    match image_part(block) {
                        Some(part) => parts.push(part),
                        None => warnings.push("image_source_unsupported".to_string()),
                    }
                }
                Some("tool_use") => {
                    // 先攒着，等 flush 时整组一起处理（不 flush）。
                    if let Some(name) = obj.get("name").filter(|v| is_truthy(v)) {
                        call_group.push(CallGroup {
                            id: obj
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string),
                            name: name.clone(),
                            input: obj.get("input").cloned(),
                            signature: truthy_str(obj.get("signature")),
                        });
                    }
                }
                Some("tool_result") => {
                    flush_call_group(&mut call_group, &mut parts, &mut stats, opts.signatures);
                    // 名字：优先块自带的，其次按 tool_use_id 还原，最后兜底。
                    let name = truthy_str(obj.get("name"))
                        .or_else(|| {
                            obj.get("tool_use_id")
                                .and_then(Value::as_str)
                                .and_then(|id| tool_name_by_id.get(id))
                                .and_then(|name| name.clone())
                                .filter(|s| !s.is_empty())
                        })
                        .unwrap_or_else(|| "unknown_tool".to_string());
                    let text = text_of(obj.get("content"));
                    let payload = if truthy(obj.get("is_error")) {
                        let err = if text.is_empty() {
                            "tool_error".to_string()
                        } else {
                            text
                        };
                        json!({ "error": err })
                    } else {
                        json!({ "result": text })
                    };
                    parts
                        .push(json!({ "functionResponse": { "name": name, "response": payload } }));
                }
                _ => {
                    // JS 的 default：只有 text 是字符串（空串也算）才转发，且只有这时才 flush。
                    if let Some(t) = obj.get("text").and_then(Value::as_str) {
                        flush_call_group(&mut call_group, &mut parts, &mut stats, opts.signatures);
                        parts.push(json!({ "text": t }));
                    }
                }
            }
        }
        flush_call_group(&mut call_group, &mut parts, &mut stats, opts.signatures);
        push_parts(&mut contents, role, parts);
    }

    // ---- tools / tool_choice
    let mut declarations: Vec<Value> = Vec::new();
    let mut dropped_schema_keys: BTreeSet<String> = BTreeSet::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for t in tools {
            let Some(name) = t.get("name").filter(|v| is_truthy(v)) else {
                continue;
            };
            let description = match t.get("description") {
                None | Some(Value::Null) => Value::String(String::new()),
                Some(v) => v.clone(),
            };
            declarations.push(json!({
                "name": name.clone(),
                "description": description,
                "parameters": normalize_schema(t.get("input_schema"), &mut dropped_schema_keys, true),
            }));
        }
    }
    if !dropped_schema_keys.is_empty() {
        warnings.push(format!(
            "schema_keywords_dropped:{}",
            dropped_schema_keys
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    let tools_value = if declarations.is_empty() {
        None
    } else {
        Some(json!([{ "functionDeclarations": declarations }]))
    };

    let tool_config = if tools_value.is_some() {
        match body.get("tool_choice").filter(|v| is_truthy(v)) {
            Some(choice) => {
                let ty = choice.get("type").and_then(Value::as_str);
                let mode = match ty {
                    Some("any") | Some("tool") => "ANY",
                    Some("none") => "NONE",
                    _ => "AUTO",
                };
                let mut config = Map::new();
                config.insert("mode".to_string(), Value::String(mode.to_string()));
                if ty == Some("tool") {
                    if let Some(name) = choice.get("name").filter(|v| is_truthy(v)) {
                        config.insert("allowedFunctionNames".to_string(), json!([name.clone()]));
                    }
                }
                Some(json!({ "functionCallingConfig": Value::Object(config) }))
            }
            None => None,
        }
    } else {
        None
    };

    // ---- generationConfig
    // 上限按上游模型表收（limits 来自 fetchAvailableModels），拿不到模型表时才退回硬编码的 65536。
    let cap = opts
        .limits
        .and_then(|l| l.max_output_tokens)
        .unwrap_or(MAX_OUTPUT_TOKENS);
    let mut generation_config = Map::new();
    if let Some(max_tokens) = body.get("max_tokens").and_then(js_number) {
        if max_tokens.is_finite() && max_tokens > 0.0 {
            let wanted = max_tokens.floor();
            generation_config.insert(
                "maxOutputTokens".to_string(),
                json_number(wanted.min(cap as f64)),
            );
            if wanted > cap as f64 {
                warnings.push(format!(
                    "max_tokens_clamped:{}->{cap}",
                    js_num_to_string(wanted)
                ));
            }
        }
    }
    // JS 用的是 `Number.isFinite`，不做隐式转换：只有真的是数字才收。
    // JSON 里的数字不可能是 NaN/Infinity，所以「是数字」就够了。
    for (key, target) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
    ] {
        if let Some(v) = body.get(key) {
            if v.is_number() {
                generation_config.insert(target.to_string(), v.clone());
            }
        }
    }
    if let Some(items) = body.get("stop_sequences").and_then(Value::as_array) {
        if !items.is_empty() {
            generation_config.insert(
                "stopSequences".to_string(),
                Value::Array(items.iter().take(5).cloned().collect()),
            );
        }
    }

    if let Some(thinking) = body.get("thinking").filter(|v| is_truthy(v)) {
        match thinking.get("type").and_then(Value::as_str) {
            Some("enabled") => {
                // `Number(budget_tokens) || 1024`：0/NaN 都当没给。
                let raw = thinking
                    .get("budget_tokens")
                    .and_then(js_number)
                    .filter(|n| *n != 0.0)
                    .unwrap_or(1024.0);
                let wanted = raw.floor().max(1.0);
                let floor = opts.limits.and_then(|l| l.min_thinking_budget).unwrap_or(1) as f64;
                let budget = wanted.max(floor);
                if budget != wanted {
                    warnings.push(format!(
                        "thinking_budget_raised:{}->{}",
                        js_num_to_string(wanted),
                        js_num_to_string(budget)
                    ));
                }
                // 上游思考预算和输出上限是分开算的：maxOutputTokens 太小会把正文挤掉，这里托一下底。
                let needs_bump = match generation_config
                    .get("maxOutputTokens")
                    .and_then(Value::as_f64)
                {
                    None => true,
                    Some(current) => current <= budget,
                };
                if needs_bump {
                    let bumped = (cap as f64).min(budget + MAX_TOKENS_BUMP as f64);
                    generation_config.insert("maxOutputTokens".to_string(), json_number(bumped));
                    if bumped <= budget {
                        warnings.push("thinking_budget_exceeds_output_cap".to_string());
                    }
                }
                generation_config.insert(
                    "thinkingConfig".to_string(),
                    json!({ "thinkingBudget": json_number(budget), "includeThoughts": true }),
                );
            }
            Some("disabled") => {
                generation_config
                    .insert("thinkingConfig".to_string(), json!({ "thinkingBudget": 0 }));
            }
            _ => {}
        }
    }

    let mut inner = Map::new();
    inner.insert("contents".to_string(), Value::Array(contents));
    if let Some(tools) = tools_value {
        inner.insert("tools".to_string(), tools);
    }
    if let Some(config) = tool_config {
        inner.insert("toolConfig".to_string(), config);
    }
    if let Some(system) = system_instruction {
        inner.insert("systemInstruction".to_string(), system);
    }
    inner.insert(
        "generationConfig".to_string(),
        Value::Object(generation_config),
    );

    BuiltRequest {
        inner: Value::Object(inner),
        stats,
        warnings,
    }
}

// ------------------------------------------------------------------ 内部辅助

/// 一组待处理的并行 `tool_use`。
struct CallGroup {
    id: Option<String>,
    name: Value,
    input: Option<Value>,
    signature: Option<String>,
}

/// 收尾一组并行调用：整组只认一个签名，且只贴在第一个 part 上。
fn flush_call_group(
    call_group: &mut Vec<CallGroup>,
    parts: &mut Vec<Value>,
    stats: &mut BuildStats,
    signatures: Option<&SharedSignatures>,
) {
    if call_group.is_empty() {
        return;
    }
    // 组内谁有签名就用谁的：先看块自带的，再从仓库里按 id 找回传的。
    let mut signature: Option<String> = None;
    for g in call_group.iter() {
        if let Some(inline) = g.signature.as_ref() {
            signature = Some(inline.clone());
            break;
        }
        if let Some(store) = signatures {
            if let Some(stored) = sig_get(store, g.id.as_deref().unwrap_or("")) {
                signature = Some(stored);
                break;
            }
        }
    }
    if signature.is_some() {
        stats.tool_signature_hits += 1;
    } else {
        stats.tool_signature_misses += 1;
    }

    for (i, g) in call_group.iter().enumerate() {
        let args = match g.input.as_ref() {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(v) => v.clone(),
        };
        let mut part = Map::new();
        part.insert(
            "functionCall".to_string(),
            json!({ "name": g.name.clone(), "args": args }),
        );
        // 只给这组的第一个 part 带签名：真签名是上游自己的行为，哨兵版也实测过 200。
        // 后面的并行调用不带，和上游自己发出来的形状一致。
        if i == 0 {
            part.insert(
                "thoughtSignature".to_string(),
                Value::String(
                    signature
                        .clone()
                        .unwrap_or_else(|| SIGNATURE_SENTINEL.to_string()),
                ),
            );
        }
        parts.push(Value::Object(part));
    }
    call_group.clear();
}

/// `image` 块 → `inlineData`。只认 base64；其余返回 `None`（调用方记 warning）。
fn image_part(block: &Value) -> Option<Value> {
    let src = match block.get("source") {
        Some(Value::Object(obj)) => obj,
        // JS 的 `block.source ?? {}`：缺失/非对象都取不到 `type`，视作不支持。
        _ => return None,
    };
    if src.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    // `src.data` 要有值（JS 的真值判断），为空/缺失就走 warning。
    let data = src.get("data").filter(|v| is_truthy(v))?;
    let mime_type = match src.get("media_type") {
        None | Some(Value::Null) => Value::String("image/png".to_string()),
        Some(v) => v.clone(),
    };
    Some(json!({ "inlineData": { "mimeType": mime_type, "data": data.clone() } }))
}

/// Gemini `Schema` proto 实际认得的字段（白名单）。
/// 上游对不认的字段是整包 400（不是忽略），所以这里只能白名单——
/// 客户端工具定义里冒出的任何新 JSON Schema 关键字都不该再让整个会话死掉。
const SCHEMA_KEYWORDS: [&str; 16] = [
    "type",
    "format",
    "title",
    "description",
    "nullable",
    "default",
    "items",
    "minItems",
    "maxItems",
    "enum",
    "properties",
    "required",
    "minimum",
    "maximum",
    "anyOf",
    "propertyOrdering",
];

/// 白名单剥掉上游不认的 JSON Schema 关键字（对应 JS 的 `normalizeSchema`），
/// 并递归进 `properties` / `items` / `anyOf`；只有顶层补 `type`（老行为），
/// 嵌套层保持原样——上游不要求，乱补反而会改掉 anyOf 的语义。
/// 被剥掉的键名收进 `dropped`，调用方记一条 warning（不静默）。
fn normalize_schema(schema: Option<&Value>, dropped: &mut BTreeSet<String>, top: bool) -> Value {
    let Some(Value::Object(obj)) = schema else {
        // JS 对「非对象 schema」直接给空壳（注：数组在 JS 里也算 typeof object，
        // 那种输入没有实际意义，这里按空壳处理）。
        return json!({ "type": "object", "properties": {} });
    };
    let mut clean = Map::new();
    for (k, v) in obj {
        if !SCHEMA_KEYWORDS.contains(&k.as_str()) {
            dropped.insert(k.clone());
            continue;
        }
        match k.as_str() {
            "properties" => match v {
                Value::Object(props) => {
                    let mut out = Map::new();
                    for (name, sub) in props {
                        out.insert(name.clone(), normalize_schema(Some(sub), dropped, false));
                    }
                    clean.insert(k.clone(), Value::Object(out));
                }
                _ => {
                    dropped.insert(k.clone());
                }
            },
            "items" => {
                let sub = match v {
                    Value::Array(list) => list.first(),
                    other => Some(other),
                };
                clean.insert(k.clone(), normalize_schema(sub, dropped, false));
            }
            "anyOf" => match v {
                Value::Array(list) => {
                    let out: Vec<Value> = list
                        .iter()
                        .map(|sub| normalize_schema(Some(sub), dropped, false))
                        .collect();
                    clean.insert(k.clone(), Value::Array(out));
                }
                _ => {
                    dropped.insert(k.clone());
                }
            },
            "required" => match v {
                Value::Array(list) => {
                    let out: Vec<Value> = list.iter().filter(|x| x.is_string()).cloned().collect();
                    clean.insert(k.clone(), Value::Array(out));
                }
                _ => {
                    dropped.insert(k.clone());
                }
            },
            _ => {
                clean.insert(k.clone(), v.clone());
            }
        }
    }
    if top && !truthy(clean.get("type")) {
        let t = if truthy(clean.get("properties")) {
            "object"
        } else {
            "string"
        };
        clean.insert("type".to_string(), Value::String(t.to_string()));
    }
    Value::Object(clean)
}

/// 相邻的同角色消息合并成一个 content（上游不接受不交替的回合）。
fn push_parts(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(arr) = last.get_mut("parts").and_then(Value::as_array_mut) {
                arr.extend(parts);
                return;
            }
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

/// 对应 JS 的 `textOf(content)`：把任意 content（字符串 / 内容块数组 / 其它）压成一段文本。
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut parts: Vec<String> = Vec::new();
            for b in items {
                if let Value::String(s) = b {
                    parts.push(s.clone());
                    continue;
                }
                let ty = b.get("type").and_then(Value::as_str);
                if ty == Some("text") {
                    if let Some(Value::String(t)) = b.get("text") {
                        parts.push(t.clone());
                        continue;
                    }
                }
                if ty == Some("image") {
                    parts.push("[image]".to_string());
                    continue;
                }
                // 其余块（含「type:text 但 text 不是字符串」）按 JSON 原文带上，别丢信息。
                if !b.is_null() {
                    parts.push(b.to_string());
                }
            }
            parts.join("\n")
        }
        // JS 的 `content == null ? "" : JSON.stringify(content)`
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// 从 `user_id` 字符串里的 JSON（形如 `{"session_id":"..."}`）抽会话 id。
/// 对应 JS 的 `/"session_id"\s*:\s*"([^"]+)"/`：无锚点搜索，`\s` 只认 ASCII 空白
/// （真实取值里没有全角空格这类输入，这里不做全 Unicode 空白匹配）。
fn session_id_from_user_id(uid: &str) -> Option<String> {
    let bytes = uid.as_bytes();
    let needle = b"\"session_id\"";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b':' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'"' {
                    let start = j + 1;
                    let mut k = start;
                    while k < bytes.len() && bytes[k] != b'"' {
                        k += 1;
                    }
                    // `+` 要求至少一个字符；空串不算匹配，继续往后找。
                    if k > start {
                        // `"` 是 ASCII 单字节，两端都落在字符边界上。
                        return uid.get(start..k).map(str::to_string);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// JS 的真值判断。`0` / `NaN` / `""` / `null` / `false` 为假，其余为真。
/// JSON 里出不了 NaN，所以数字只看「是不是 0」。
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        // serde_json 的数字一定能转 f64，None 分支实际到不了；真值只看「是不是 0」。
        Value::Number(n) => match n.as_f64() {
            Some(f) => f != 0.0,
            None => true,
        },
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn truthy(v: Option<&Value>) -> bool {
    v.map(is_truthy).unwrap_or(false)
}

/// 「字符串而且非空」才算有值 —— 用来复刻 JS 里 `a || b` 这种短路。
fn truthy_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// `Number(v)`。只认 JS 真的认的形状：缺失/对象 → 不是数（`None`）；
/// `null`/空串/空白串 → 0；布尔 → 1/0；数组按 `Number(String(v))` 走。
fn js_number(v: &Value) -> Option<f64> {
    match v {
        Value::Null => Some(0.0),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_js_number_string(s),
        Value::Array(items) => {
            let joined = items.iter().map(js_string).collect::<Vec<_>>().join(",");
            parse_js_number_string(&joined)
        }
        Value::Object(_) => None,
    }
}

fn parse_js_number_string(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return Some(0.0);
    }
    // JS 认带符号的 Infinity；`inf`/`nan` 这些它不认。
    match t {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok().map(|n| n as f64);
    }
    if let Some(oct) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return i64::from_str_radix(oct, 8).ok().map(|n| n as f64);
    }
    if let Some(bin) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return i64::from_str_radix(bin, 2).ok().map(|n| n as f64);
    }
    // Rust 的 f64 解析认 "inf"/"nan"，JS 不认：先把字母挡掉（只剩科学计数法的 e）。
    if t.chars()
        .any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E')
    {
        return None;
    }
    t.parse::<f64>().ok()
}

/// JS 里 `${number}` 的样子。整数不带小数点，好让 warnings 的文案和 JS 对得上。
fn js_num_to_string(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 {
        return format!("{}", f as i64);
    }
    format!("{f}")
}

/// `String(v)`。只在会话指纹那种「JS 会把非字符串值拼成字符串」的边角路径上用得到。
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n
            .as_f64()
            .map(js_num_to_string)
            .unwrap_or_else(|| n.to_string()),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// 整数值的 f64 退回整数类型再进 JSON：JS 会把 `1.0` 写成 `1`，
/// `serde_json::Value::from(1.0)` 会写成 `1.0`，直接放会让两边输出对不上。
fn json_number(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 {
        return Value::from(f as i64);
    }
    Value::from(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signatures::{shared_signatures, sig_put};

    /// 对应 JS 测试里的 `base(extra)`：先铺最小请求，再让 `extra` 覆盖/追加。
    fn base(extra: Value) -> Value {
        let mut v = json!({
            "model": "gemini-3.6-flash-high",
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": "你好" }],
        });
        if let (Some(base_obj), Some(extra_obj)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in extra_obj {
                base_obj.insert(k.clone(), val.clone());
            }
        }
        v
    }

    #[test]
    fn minimal_request_is_one_user_message() {
        let built = build_gemini_request(&base(json!({})), BuildOptions::default());
        assert_eq!(
            built.inner["contents"],
            json!([{ "role": "user", "parts": [{ "text": "你好" }] }])
        );
        assert_eq!(
            built.inner["generationConfig"]["maxOutputTokens"],
            json!(1024)
        );
        assert!(built.inner.get("tools").is_none());
        assert!(built.inner.get("systemInstruction").is_none());
    }

    #[test]
    fn max_tokens_is_clamped_to_the_model_table_and_reported() {
        // 拿不到模型表：退回硬编码上限
        let plain = build_gemini_request(
            &base(json!({ "max_tokens": 999_999 })),
            BuildOptions::default(),
        );
        assert_eq!(
            plain.inner["generationConfig"]["maxOutputTokens"],
            json!(MAX_OUTPUT_TOKENS)
        );
        assert!(plain
            .warnings
            .contains(&format!("max_tokens_clamped:999999->{MAX_OUTPUT_TOKENS}")));

        // 拿到模型表：按表里的 maxOutputTokens 收
        let limits = ModelLimit {
            max_output_tokens: Some(8192),
            ..Default::default()
        };
        let clamped = build_gemini_request(
            &base(json!({ "max_tokens": 999_999 })),
            BuildOptions {
                limits: Some(&limits),
                ..Default::default()
            },
        );
        assert_eq!(
            clamped.inner["generationConfig"]["maxOutputTokens"],
            json!(8192)
        );
        assert!(clamped
            .warnings
            .contains(&"max_tokens_clamped:999999->8192".to_string()));

        // 客户端要得比上限小：原样放行，不警告
        let small = build_gemini_request(
            &base(json!({ "max_tokens": 512 })),
            BuildOptions {
                limits: Some(&limits),
                ..Default::default()
            },
        );
        assert_eq!(
            small.inner["generationConfig"]["maxOutputTokens"],
            json!(512)
        );
        assert!(small.warnings.is_empty());
    }

    #[test]
    fn thinking_budget_is_raised_to_the_model_floor_and_reported() {
        let raised_limits = ModelLimit {
            min_thinking_budget: Some(1024),
            max_output_tokens: Some(65536),
            ..Default::default()
        };
        let raised = build_gemini_request(
            &base(json!({ "thinking": { "type": "enabled", "budget_tokens": 128 } })),
            BuildOptions {
                limits: Some(&raised_limits),
                ..Default::default()
            },
        );
        assert_eq!(
            raised.inner["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(1024)
        );
        assert!(raised
            .warnings
            .contains(&"thinking_budget_raised:128->1024".to_string()));
        // 预算托底后输出上限也跟着抬（正文不能被思考挤掉）
        assert_eq!(
            raised.inner["generationConfig"]["maxOutputTokens"],
            json!(1024 + 8192)
        );

        let tight_limits = ModelLimit {
            min_thinking_budget: Some(1024),
            max_output_tokens: Some(8192),
            ..Default::default()
        };
        let capped = build_gemini_request(
            &base(
                json!({ "max_tokens": 9000, "thinking": { "type": "enabled", "budget_tokens": 8192 } }),
            ),
            BuildOptions {
                limits: Some(&tight_limits),
                ..Default::default()
            },
        );
        assert_eq!(
            capped.inner["generationConfig"]["maxOutputTokens"],
            json!(8192)
        );
        assert!(capped
            .warnings
            .contains(&"thinking_budget_exceeds_output_cap".to_string()));
    }

    #[test]
    fn consecutive_same_role_messages_merge_into_one_content() {
        let built = build_gemini_request(
            &base(json!({
                "messages": [
                    { "role": "user", "content": "第一句" },
                    { "role": "user", "content": [{ "type": "text", "text": "第二句" }] },
                    { "role": "assistant", "content": "回答" },
                ],
            })),
            BuildOptions::default(),
        );
        assert_eq!(built.inner["contents"].as_array().unwrap().len(), 2);
        assert_eq!(
            built.inner["contents"][0]["parts"],
            json!([{ "text": "第一句" }, { "text": "第二句" }])
        );
        assert_eq!(built.inner["contents"][1]["role"], json!("model"));
    }

    #[test]
    fn tool_use_and_tool_result_map_to_function_call_and_response() {
        let built = build_gemini_request(
            &base(json!({
                "messages": [
                    { "role": "user", "content": "查一下" },
                    { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "Grep", "input": { "pattern": "x" } }] },
                    { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "没找到" }] },
                ],
            })),
            BuildOptions::default(),
        );
        let call = &built.inner["contents"][1]["parts"][0];
        assert_eq!(
            call["functionCall"],
            json!({ "name": "Grep", "args": { "pattern": "x" } })
        );
        // 没有签名可回传时必须给哨兵，否则上游 400（Phase 0 实测）
        assert_eq!(call["thoughtSignature"], json!(SIGNATURE_SENTINEL));
        assert_eq!(
            built.inner["contents"][2]["parts"][0]["functionResponse"],
            json!({ "name": "Grep", "response": { "result": "没找到" } })
        );
        assert_eq!(built.stats.tool_signature_misses, 1);
    }

    #[test]
    fn stored_signature_is_echoed_instead_of_the_sentinel() {
        let signatures = shared_signatures();
        sig_put(&signatures, "t1", "REAL-SIG");
        let built = build_gemini_request(
            &base(json!({
                "messages": [
                    { "role": "user", "content": "查一下" },
                    { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "Grep", "input": {} }] },
                ],
            })),
            BuildOptions {
                signatures: Some(&signatures),
                ..Default::default()
            },
        );
        assert_eq!(
            built.inner["contents"][1]["parts"][0]["thoughtSignature"],
            json!("REAL-SIG")
        );
        assert_eq!(built.stats.tool_signature_hits, 1);
    }

    #[test]
    fn parallel_calls_only_carry_the_signature_on_the_first_part() {
        let signatures = shared_signatures();
        sig_put(&signatures, "t1", "REAL-SIG");
        let built = build_gemini_request(
            &base(json!({
                "messages": [
                    { "role": "user", "content": "同时查两个时间" },
                    {
                        "role": "assistant",
                        "content": [
                            { "type": "tool_use", "id": "t1", "name": "get_time", "input": { "tz": "Asia/Shanghai" } },
                            { "type": "tool_use", "id": "t2", "name": "get_time", "input": { "tz": "America/New_York" } },
                        ],
                    },
                ],
            })),
            BuildOptions {
                signatures: Some(&signatures),
                ..Default::default()
            },
        );
        let parts = built.inner["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["thoughtSignature"], json!("REAL-SIG"));
        assert!(parts[1].get("thoughtSignature").is_none());
    }

    #[test]
    fn tool_result_error_uses_response_error() {
        let built = build_gemini_request(
            &base(json!({
                "messages": [
                    { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "Grep", "input": {} }] },
                    { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "炸了", "is_error": true }] },
                ],
            })),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["contents"][1]["parts"][0]["functionResponse"]["response"],
            json!({ "error": "炸了" })
        );
    }

    #[test]
    fn thinking_block_becomes_a_thought_part_with_signature() {
        let built = build_gemini_request(
            &base(json!({
                "thinking": { "type": "enabled", "budget_tokens": 2048 },
                "messages": [
                    { "role": "user", "content": "想想" },
                    { "role": "assistant", "content": [{ "type": "thinking", "thinking": "我在想", "signature": "TSIG" }] },
                ],
            })),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["contents"][1]["parts"][0],
            json!({ "text": "我在想", "thought": true, "thoughtSignature": "TSIG" })
        );
        assert_eq!(
            built.inner["generationConfig"]["thinkingConfig"],
            json!({ "thinkingBudget": 2048, "includeThoughts": true })
        );
    }

    #[test]
    fn thinking_raises_max_tokens_so_text_is_not_squeezed_out() {
        let built = build_gemini_request(
            &base(
                json!({ "thinking": { "type": "enabled", "budget_tokens": 30000 }, "max_tokens": 4096 }),
            ),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(30000)
        );
        let max_output = built.inner["generationConfig"]["maxOutputTokens"]
            .as_i64()
            .unwrap();
        assert!(max_output > 30000);
        assert!(max_output <= MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn max_tokens_over_the_upstream_cap_is_clamped() {
        let built = build_gemini_request(
            &base(json!({ "max_tokens": 999_999 })),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["generationConfig"]["maxOutputTokens"],
            json!(MAX_OUTPUT_TOKENS)
        );
    }

    #[test]
    fn tools_become_function_declarations_and_tool_choice_becomes_config() {
        let built = build_gemini_request(
            &base(json!({
                "tools": [{ "name": "Grep", "description": "搜", "input_schema": { "type": "object", "properties": { "pattern": { "type": "string" } } } }],
                "tool_choice": { "type": "tool", "name": "Grep" },
            })),
            BuildOptions::default(),
        );
        assert_eq!(built.inner["tools"].as_array().unwrap().len(), 1);
        assert_eq!(
            built.inner["tools"][0]["functionDeclarations"][0]["name"],
            json!("Grep")
        );
        assert_eq!(
            built.inner["toolConfig"],
            json!({ "functionCallingConfig": { "mode": "ANY", "allowedFunctionNames": ["Grep"] } })
        );
    }

    #[test]
    fn schema_keywords_outside_the_allowlist_are_stripped_recursively() {
        let built = build_gemini_request(
            &base(json!({
                "tools": [{
                    "name": "Task",
                    "input_schema": {
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": "object",
                        "properties": {
                            "priority": { "type": "integer", "exclusiveMinimum": 0, "minimum": 1, "description": "优先级" },
                            "list": { "type": "array", "items": { "type": "string", "minLength": 1 } },
                            "pick": { "anyOf": [{ "type": "string", "const": "a" }, { "type": "number" }], "additionalProperties": false }
                        },
                        "required": ["priority", 42],
                        "additionalProperties": false
                    }
                }],
            })),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["tools"][0]["functionDeclarations"][0]["parameters"],
            json!({
                "type": "object",
                "properties": {
                    "priority": { "type": "integer", "minimum": 1, "description": "优先级" },
                    "list": { "type": "array", "items": { "type": "string" } },
                    "pick": { "anyOf": [{ "type": "string" }, { "type": "number" }] }
                },
                "required": ["priority"]
            })
        );
        assert!(built
            .warnings
            .contains(&"schema_keywords_dropped:$schema,additionalProperties,const,exclusiveMinimum,minLength".to_string()));
    }

    #[test]
    fn non_object_schema_becomes_shell_and_nested_schemas_keep_no_type() {
        let arr = build_gemini_request(
            &base(json!({ "tools": [{ "name": "T", "input_schema": [1, 2] }] })),
            BuildOptions::default(),
        );
        assert_eq!(
            arr.inner["tools"][0]["functionDeclarations"][0]["parameters"],
            json!({ "type": "object", "properties": {} })
        );
        let bare = build_gemini_request(
            &base(
                json!({ "tools": [{ "name": "T", "input_schema": { "type": "object", "properties": { "note": { "description": "无类型" } } } }] }),
            ),
            BuildOptions::default(),
        );
        assert_eq!(
            bare.inner["tools"][0]["functionDeclarations"][0]["parameters"]["properties"]["note"],
            json!({ "description": "无类型" })
        );
    }

    #[test]
    fn tool_choice_without_tools_is_ignored_and_auto_maps_to_auto() {
        let ignored = build_gemini_request(
            &base(json!({ "tool_choice": { "type": "auto" } })),
            BuildOptions::default(),
        );
        assert!(ignored.inner.get("toolConfig").is_none());
        let auto = build_gemini_request(
            &base(
                json!({ "tools": [{ "name": "T", "input_schema": {} }], "tool_choice": { "type": "auto" } }),
            ),
            BuildOptions::default(),
        );
        assert_eq!(
            auto.inner["toolConfig"]["functionCallingConfig"]["mode"],
            json!("AUTO")
        );
    }

    #[test]
    fn images_become_inline_data_and_redacted_thinking_is_dropped() {
        let built = build_gemini_request(
            &base(json!({
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "看这张图" },
                        { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
                        { "type": "redacted_thinking", "data": "加密的思考" },
                    ],
                }],
            })),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["contents"][0]["parts"][1],
            json!({ "inlineData": { "mimeType": "image/png", "data": "AAAA" } })
        );
        assert!(built
            .warnings
            .contains(&"redacted_thinking_dropped".to_string()));
    }

    #[test]
    fn temperature_top_p_and_stop_sequences_pass_through() {
        let built = build_gemini_request(
            &base(json!({ "temperature": 0.3, "top_p": 0.9, "stop_sequences": ["STOP"] })),
            BuildOptions::default(),
        );
        assert_eq!(built.inner["generationConfig"]["temperature"], json!(0.3));
        assert_eq!(built.inner["generationConfig"]["topP"], json!(0.9));
        assert_eq!(
            built.inner["generationConfig"]["stopSequences"],
            json!(["STOP"])
        );
    }

    #[test]
    fn derive_session_key_prefers_session_id_then_user_id_payload() {
        assert_eq!(
            derive_session_key(&json!({ "metadata": { "session_id": "s1" } })),
            "s1"
        );
        assert_eq!(
            derive_session_key(
                &json!({ "metadata": { "user_id": "{\"session_id\":\"s2\",\"device\":\"x\"}" } })
            ),
            "s2"
        );
        assert_eq!(derive_session_key(&json!({})), "default");
    }
}
