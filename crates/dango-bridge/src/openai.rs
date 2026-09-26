//! OpenAI 协议这一侧：`/v1/chat/completions` 的请求转换与响应翻译。
//!
//! 和 anthropic 那侧共用同一套基础设施（签名仓库、泄漏修复、跳哨兵），
//! 只是外形不同：content 是字符串、工具走 tool_calls、流式 chunk 是 chat.completion.chunk。
//!
//! 移植自 `src/bridge/openai.mjs`，行为基准是 JS 版与 `test/openai.test.mjs`。
//! 请求侧（`build_gemini_request_from_openai`）只做形状转换、不碰 IO；
//! 流式侧（`OpenAiTranslator`）逐 chunk 翻译，收尾的那个 `data: [DONE]` 由服务层补
//! （JS 版也是这样分的：openai.mjs 只给 `sse_data_frame`）。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::leak_repair::{LeakEvent, LeakFilter};
use crate::quota::ModelLimit;
use crate::signatures::{sig_get, sig_put, sig_put_trailing, SharedSignatures, SIGNATURE_SENTINEL};
use crate::types::now_millis;

/// 拿不到模型表时的兜底输出上限（对应 JS 的 `MAX_OUTPUT_TOKENS`）。
const MAX_OUTPUT_TOKENS: i64 = 65536;
/// 思考预算抬高输出上限时留的余量（对应 JS 的 `MAX_TOKENS_BUMP`）。
const MAX_TOKENS_BUMP: i64 = 8192;

/// 上游 id 的形状照抄 JS：`chatcmpl-` + 24 位十六进制。
fn new_completion_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("chatcmpl-{}", &hex[..24])
}

/// 工具调用 id：`call_` + 22 位十六进制。
fn new_call_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("call_{}", &hex[..22])
}

/// OpenAI 的 `reasoning_effort` → 上游的思考预算。取值表照抄 JS。
fn effort_budget(effort: &str) -> Option<i64> {
    match effort {
        "minimal" => Some(0),
        "low" => Some(1024),
        "medium" => Some(8192),
        "high" => Some(24576),
        _ => None,
    }
}

/// JS 的真值判断。`0` / `NaN` / `""` / `null` / `false` 为假，其余为真。
/// JSON 里出不了 NaN，所以数字只看「是不是 0」。
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// 「字符串而且非空」才算有值 —— 用来复刻 JS 里 `a || b` 这种短路。
fn truthy_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// `Number(v)`。只认 JS 真的认的形状：
/// 缺失/对象/多元素数组 → 不是数（`None`）；`null`/空串/空白串 → 0；
/// 布尔 → 1/0；`Infinity` 也认（调用方再用 `is_finite` 挡掉）。
fn js_number(v: &Value) -> Option<f64> {
    match v {
        Value::Null => Some(0.0),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_js_number_string(s),
        // JS: Number([]) === 0，Number([x]) === Number(String(x))，多元素 → NaN
        Value::Array(items) => parse_js_number_string(&join_js(items, ",")),
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
    // JS 的 Number 认无符号的 0x/0o/0b 字面量（带符号的 "-0x1" 反而是 NaN）。
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

/// `String(v)`。只在「JS 会把非字符串值拼进正文」的边角路径上用得到。
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n
            .as_f64()
            .map(js_num_to_string)
            .unwrap_or_else(|| n.to_string()),
        Value::String(s) => s.clone(),
        Value::Array(items) => join_js(items, ","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

fn join_js(items: &[Value], sep: &str) -> String {
    items.iter().map(js_string).collect::<Vec<_>>().join(sep)
}

/// 整数值的 f64 退回整数类型再进 JSON：JS 会把 `1.0` 写成 `1`，
/// `serde_json::Value::from(1.0)` 会写成 `1.0`，直接放会让两边输出对不上。
fn json_number(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 {
        return Value::from(f as i64);
    }
    Value::from(f)
}

/// `typeof content === "string"` 就用它；是数组就按 `b?.text ?? ""` 取文本再拼
/// （对应 JS 的 `content.map(...).join("")`）；其余一律空串。
fn text_of_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|b| match b {
                Value::String(s) => s.clone(),
                other => match other.get("text") {
                    None | Some(Value::Null) => String::new(),
                    Some(v) => js_string(v),
                },
            })
            .collect(),
        _ => String::new(),
    }
}

/// 相邻的同角色消息合并成一条 content（对应 JS 的 `push`）。
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

/// `data:<mime>;base64,<data>` → `(mime, data)`，对应 JS 的
/// `/^data:([^;]+);base64,(.+)$/`。base64 正文至少一个字符，且不含行终止符
/// （JS 的 `.` 不匹配行终止符；结尾恰好一个换行时 `$` 仍会匹配，这里照放）。
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let semi = rest.find(';')?;
    let mime = &rest[..semi];
    if mime.is_empty() {
        return None;
    }
    let data = rest[semi + 1..].strip_prefix("base64,")?;
    let data = data.strip_suffix('\n').unwrap_or(data);
    if data.is_empty()
        || data
            .chars()
            .any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
    {
        return None;
    }
    Some((mime.to_string(), data.to_string()))
}

// ------------------------------------------------------------------ 请求侧

/// 请求侧的统计。和 Anthropic 侧同形，服务层把两边的命中率记到一起。
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildStats {
    pub tool_signature_hits: usize,
    pub tool_signature_misses: usize,
}

/// 描述这次翻译能拿到的外部资源。和 `anthropic_request::BuildOptions` 各自定义、
/// 互不依赖（服务层按协议分别调用）。
pub struct BuildOptions<'a> {
    pub signatures: Option<&'a SharedSignatures>,
    pub session_key: &'a str,
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

/// OpenAI `/v1/chat/completions` 请求体 → v1internal 的 generateContent 请求体。
/// 这一层只做形状转换，不做 IO。
pub fn build_gemini_request_from_openai(body: &Value, opts: BuildOptions<'_>) -> BuiltRequest {
    let mut warnings: Vec<String> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut stats = BuildStats::default();
    let mut tool_name_by_id: HashMap<String, Option<String>> = HashMap::new();

    let empty: Vec<Value> = Vec::new();
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    // 先扫一遍：tool 消息里只有 tool_call_id，名字要从 assistant 的 tool_calls 里还原。
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for tc in calls {
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        tool_name_by_id.insert(id.to_string(), name);
                    }
                }
            }
        }
    }

    let mut system_texts: Vec<String> = Vec::new();

    for msg in messages {
        let Some(msg) = msg.as_object() else {
            // JS 的 `!msg || typeof msg !== "object"`：非对象一律跳过。
            continue;
        };
        match msg.get("role").and_then(Value::as_str) {
            Some("system") | Some("developer") => {
                let t = text_of_content(msg.get("content"));
                if !t.is_empty() {
                    system_texts.push(t);
                }
            }
            Some("tool") => {
                // tool 消息的名字：优先自己的 `name`，否则按 tool_call_id 还原，再不行兜底。
                let name = truthy_str(msg.get("name"))
                    .or_else(|| {
                        msg.get("tool_call_id")
                            .and_then(Value::as_str)
                            .and_then(|id| tool_name_by_id.get(id).cloned().flatten())
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or_else(|| "unknown_tool".to_string());
                push_parts(
                    &mut contents,
                    "user",
                    vec![json!({
                        "functionResponse": {
                            "name": name,
                            "response": { "result": text_of_content(msg.get("content")) },
                        }
                    })],
                );
            }
            Some("assistant") => {
                let mut parts: Vec<Value> = Vec::new();
                let text = text_of_content(msg.get("content"));
                if !text.is_empty() {
                    parts.push(json!({ "text": text }));
                }
                // 助手回合里的推理内容（各家习惯叫 reasoning_content / reasoning）
                let reasoning = msg
                    .get("reasoning_content")
                    .filter(|v| !v.is_null())
                    .or_else(|| msg.get("reasoning").filter(|v| !v.is_null()));
                if let Some(r) = reasoning.and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    let signature = truthy_str(msg.get("reasoning_signature"))
                        .unwrap_or_else(|| SIGNATURE_SENTINEL.to_string());
                    parts
                        .push(json!({ "text": r, "thought": true, "thoughtSignature": signature }));
                }

                if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for (i, tc) in calls.iter().enumerate() {
                        let function = tc.get("function");
                        let mut args = Value::Object(Map::new());
                        match function.and_then(|f| f.get("arguments")) {
                            Some(Value::String(raw)) if !raw.trim().is_empty() => {
                                match serde_json::from_str::<Value>(raw) {
                                    Ok(parsed) => args = parsed,
                                    Err(_) => {
                                        args = json!({ "__raw": raw });
                                        warnings.push("tool_arguments_not_json".to_string());
                                    }
                                }
                            }
                            Some(v @ Value::Object(_)) => args = v.clone(),
                            _ => {}
                        }

                        let signature = tc
                            .get("id")
                            .and_then(Value::as_str)
                            .and_then(|id| opts.signatures.and_then(|store| sig_get(store, id)));
                        if signature.is_some() {
                            stats.tool_signature_hits += 1;
                        } else {
                            stats.tool_signature_misses += 1;
                        }

                        let name = match function.and_then(|f| f.get("name")) {
                            Some(v) if !v.is_null() => v.clone(),
                            _ => Value::String("unknown_tool".to_string()),
                        };
                        let mut part = Map::new();
                        part.insert(
                            "functionCall".to_string(),
                            json!({ "name": name, "args": args }),
                        );
                        // 和 Anthropic 侧同样的规矩：这组调用只给第一个 part 带签名（真签名或哨兵）
                        if i == 0 {
                            part.insert(
                                "thoughtSignature".to_string(),
                                Value::String(
                                    signature.unwrap_or_else(|| SIGNATURE_SENTINEL.to_string()),
                                ),
                            );
                        }
                        parts.push(Value::Object(part));
                    }
                }
                push_parts(&mut contents, "model", parts);
            }
            _ => {
                // user（含图片）
                let owned_blocks;
                let blocks: &[Value] = match msg.get("content") {
                    Some(Value::Array(items)) => items,
                    other => {
                        owned_blocks =
                            vec![json!({ "type": "text", "text": text_of_content(other) })];
                        &owned_blocks
                    }
                };
                let mut parts: Vec<Value> = Vec::new();
                for b in blocks {
                    if let Value::String(s) = b {
                        if !s.is_empty() {
                            parts.push(json!({ "text": s }));
                        }
                        continue;
                    }
                    let Some(obj) = b.as_object() else {
                        continue;
                    };
                    match obj.get("type").and_then(Value::as_str) {
                        Some("text") if truthy(obj.get("text")) => {
                            parts.push(
                                json!({ "text": obj.get("text").cloned().unwrap_or(Value::Null) }),
                            );
                        }
                        Some("image_url") => {
                            let url = obj
                                .get("image_url")
                                .and_then(|i| i.get("url"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            match parse_data_url(url) {
                                Some((mime_type, data)) => {
                                    parts.push(json!({ "inlineData": { "mimeType": mime_type, "data": data } }));
                                }
                                None => warnings.push("image_url_unsupported".to_string()),
                            }
                        }
                        _ => {
                            if let Some(t) = obj
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                            {
                                parts.push(json!({ "text": t }));
                            }
                        }
                    }
                }
                push_parts(&mut contents, "user", parts);
            }
        }
    }

    let mut declarations: Vec<Value> = Vec::new();
    let mut dropped_schema_keys: BTreeSet<String> = BTreeSet::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for t in tools {
            let function = match t.get("type").and_then(Value::as_str) {
                Some("function") => t.get("function"),
                _ => Some(t),
            };
            let Some(function) = function else { continue };
            let Some(name) = function.get("name").filter(|v| truthy(Some(v))) else {
                continue;
            };
            let description = match function.get("description") {
                None | Some(Value::Null) => Value::String(String::new()),
                Some(v) => v.clone(),
            };
            declarations.push(json!({
                "name": name.clone(),
                "description": description,
                "parameters": normalize_schema(function.get("parameters"), &mut dropped_schema_keys, true),
            }));
        }
    }
    let tools_value = if declarations.is_empty() {
        None
    } else {
        Some(json!([{ "functionDeclarations": declarations }]))
    };

    let tool_config = if tools_value.is_some() {
        let choice = body.get("tool_choice");
        let mut mode = "AUTO";
        let mut allowed: Option<Value> = None;
        match choice {
            Some(Value::String(s)) if s == "none" => mode = "NONE",
            Some(Value::String(s)) if s == "required" => mode = "ANY",
            Some(v @ (Value::Object(_) | Value::Array(_))) => {
                mode = "ANY";
                let name = v
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .filter(|n| !n.is_null())
                    .or_else(|| v.get("name").filter(|n| !n.is_null()));
                if let Some(name) = name.filter(|n| truthy(Some(n))) {
                    allowed = Some(name.clone());
                }
            }
            _ => {}
        }
        let mut config = Map::new();
        config.insert("mode".to_string(), Value::String(mode.to_string()));
        if let Some(name) = allowed {
            config.insert("allowedFunctionNames".to_string(), json!([name]));
        }
        Some(json!({ "functionCallingConfig": Value::Object(config) }))
    } else {
        None
    };

    // 上限按上游模型表收（limits 来自 fetchAvailableModels），拿不到模型表时才退回硬编码的 65536。
    let cap = opts
        .limits
        .and_then(|l| l.max_output_tokens)
        .unwrap_or(MAX_OUTPUT_TOKENS);
    let mut generation_config = Map::new();
    let max_tokens = body
        .get("max_tokens")
        .filter(|v| !v.is_null())
        .or_else(|| body.get("max_completion_tokens").filter(|v| !v.is_null()))
        .and_then(js_number);
    if let Some(max_tokens) = max_tokens {
        if max_tokens.is_finite() && max_tokens > 0.0 {
            let wanted = max_tokens.floor();
            generation_config.insert(
                "maxOutputTokens".to_string(),
                json_number(wanted.min(cap as f64)),
            );
            if wanted > cap as f64 {
                warnings.push(format!(
                    "max_tokens_clamped:{}->{}",
                    js_num_to_string(wanted),
                    cap
                ));
            }
        }
    }
    // JS 用的是 `Number.isFinite`，它不做隐式转换：只有真的是数字才收。
    // JSON 里的数字不可能是 NaN/Infinity，所以「是数字」就够了。
    for key in ["temperature", "top_p"] {
        if let Some(v) = body.get(key) {
            if v.is_number() {
                let target = if key == "temperature" {
                    "temperature"
                } else {
                    "topP"
                };
                generation_config.insert(target.to_string(), v.clone());
            }
        }
    }
    if let Some(stop) = body.get("stop").filter(|v| truthy(Some(v))) {
        let sequences = match stop {
            Value::Array(items) => items.iter().take(5).cloned().collect(),
            other => vec![other.clone()],
        };
        generation_config.insert("stopSequences".to_string(), Value::Array(sequences));
    }

    let effort = body.get("reasoning_effort").and_then(Value::as_str);
    if let Some(base) = effort.and_then(effort_budget) {
        let floor = opts.limits.and_then(|l| l.min_thinking_budget).unwrap_or(1);
        let budget = base.max(if base > 0 { floor } else { 0 });
        if budget > 0 {
            if base < floor {
                warnings.push(format!("thinking_budget_raised:{base}->{budget}"));
            }
            // 思考预算要占输出额度：不够就先按预算+余量抬一抬，抬到顶还是不够才报警。
            let needs_bump = match generation_config
                .get("maxOutputTokens")
                .and_then(Value::as_f64)
            {
                None => true,
                Some(current) => current <= budget as f64,
            };
            if needs_bump {
                let bumped = cap.min(budget + MAX_TOKENS_BUMP);
                generation_config.insert("maxOutputTokens".to_string(), json_number(bumped as f64));
                if bumped <= budget {
                    warnings.push("thinking_budget_exceeds_output_cap".to_string());
                }
            }
            generation_config.insert(
                "thinkingConfig".to_string(),
                json!({ "thinkingBudget": budget, "includeThoughts": true }),
            );
        } else {
            generation_config.insert("thinkingConfig".to_string(), json!({ "thinkingBudget": 0 }));
        }
    }

    if let Some(rf) = body.get("response_format") {
        match rf.get("type").and_then(Value::as_str) {
            Some("json_object") => {
                generation_config.insert("responseMimeType".to_string(), json!("application/json"));
            }
            Some("json_schema") => {
                generation_config.insert("responseMimeType".to_string(), json!("application/json"));
                if let Some(schema) = rf.get("json_schema").and_then(|s| s.get("schema")) {
                    if truthy(Some(schema)) {
                        generation_config.insert(
                            "responseSchema".to_string(),
                            normalize_schema(Some(schema), &mut dropped_schema_keys, true),
                        );
                    }
                }
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
    if !system_texts.is_empty() {
        inner.insert(
            "systemInstruction".to_string(),
            json!({ "parts": [{ "text": system_texts.join("\n\n") }] }),
        );
    }
    inner.insert(
        "generationConfig".to_string(),
        Value::Object(generation_config),
    );

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

    BuiltRequest {
        inner: Value::Object(inner),
        stats,
        warnings,
    }
}

// ------------------------------------------------------------------ 流式侧

/// 上游 `usageMetadata` 归一化出来的计数。
/// `completion` 已经把思考合进去了（和 Anthropic 侧同口径）；
/// `thoughts` 单留一份，报警阈值要用。
#[derive(Debug, Clone, Copy, Default)]
struct Usage {
    prompt: f64,
    /// Part of `prompt` served from the context cache.
    cached: f64,
    thoughts: f64,
    completion: f64,
    total: f64,
}

impl Usage {
    fn value(self) -> Value {
        json!({
            "prompt_tokens": json_number(self.prompt),
            "completion_tokens": json_number(self.completion),
            "total_tokens": json_number(self.total),
        })
    }
}

fn zero_usage() -> Value {
    json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 })
}

/// 对应 JS 的 `usageOf(meta)`：`meta` 假值就直接没有（返回 `None`）。
fn usage_of(meta: Option<&Value>) -> Option<Usage> {
    let meta = meta?;
    if !truthy(Some(meta)) {
        return None;
    }
    let count = |key: &str| meta.get(key).and_then(js_number).unwrap_or(0.0);
    let prompt = count("promptTokenCount");
    let cached = count("cachedContentTokenCount");
    let candidates = count("candidatesTokenCount");
    let thoughts = count("thoughtsTokenCount");
    let completion = candidates + thoughts;
    let total = meta
        .get("totalTokenCount")
        .and_then(js_number)
        .unwrap_or(prompt + completion);
    Some(Usage {
        prompt,
        cached,
        thoughts,
        completion,
        total,
    })
}

/// 一帧 chat.completion.chunk。`finish_reason` 和 JS 一样总是出现（没有就是 null）。
fn frame(id: &str, created: i64, model: &str, delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": match finish_reason {
                Some(reason) => Value::String(reason.to_string()),
                None => Value::Null,
            },
        }],
    })
}

/// 上游 finishReason → OpenAI 的 finish_reason。
fn map_finish(finish_reason: Option<&str>, saw_tool_use: bool) -> &'static str {
    match finish_reason {
        Some("MAX_TOKENS") => "length",
        Some("SAFETY")
        | Some("RECITATION")
        | Some("PROHIBITED_CONTENT")
        | Some("BLOCKLIST")
        | Some("SPII") => "content_filter",
        _ => {
            if saw_tool_use {
                "tool_calls"
            } else {
                "stop"
            }
        }
    }
}

/// 翻译器里不随请求变的那部分（id 由构造时生成一次，整个流的 chunk 共用）。
struct Config {
    id: String,
    created: i64,
    model: String,
    /// 客户端要不要流式最后那条 usage（对应 JS 的 `includeUsage`）
    include_usage: bool,
    /// 这次请求声明过的工具名；非空时，泄漏修复只认名单里的名字
    declared_tools: Option<HashSet<String>>,
    signatures: SharedSignatures,
    session_key: String,
}

/// 每个流的可变状态。单独拎出来，是为了让 `to_completion(&self)` 也能借它
/// （JS 的 `toCompletion` 就是复用同一个 state —— 服务层整包路径随后要读 `stats()`）。
struct State {
    filter: LeakFilter,
    saw_tool_use: bool,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    /// 从 -1 起：第一次 `++` 得到 0（OpenAI 的 tool_calls 下标从 0 开始）
    tool_index: i64,
    leaked_calls: usize,
    leaks_ignored: usize,
    role_sent: bool,
}

impl State {
    fn new() -> Self {
        Self {
            filter: LeakFilter::new(),
            saw_tool_use: false,
            finish_reason: None,
            usage: None,
            tool_index: -1,
            leaked_calls: 0,
            leaks_ignored: 0,
            role_sent: false,
        }
    }
}

/// 翻译器的构造参数。服务层按协议分别造（Anthropic 侧有它自己的同名结构）。
pub struct TranslatorOptions {
    pub model: String,
    pub signatures: SharedSignatures,
    pub session_key: String,
    /// 客户端要不要流式最后那条 usage（对应 JS 的 includeUsage）
    pub include_usage: bool,
    pub declared_tools: Option<Vec<String>>,
}

/// 上游 chunk → OpenAI chat.completion.chunk。
/// include_usage 对应 stream_options.include_usage（客户端要最后补一个带 usage 的 chunk）。
pub struct OpenAiTranslator {
    config: Config,
    state: RefCell<State>,
}

impl OpenAiTranslator {
    pub fn new(opts: TranslatorOptions) -> Self {
        Self {
            config: Config {
                id: new_completion_id(),
                // JS 的 `Math.floor(Date.now() / 1000)`
                created: now_millis() / 1000,
                model: opts.model,
                include_usage: opts.include_usage,
                declared_tools: opts.declared_tools.map(|names| names.into_iter().collect()),
                signatures: opts.signatures,
                session_key: opts.session_key,
            },
            state: RefCell::new(State::new()),
        }
    }

    /// 流开始：先给一个只带 role 的 chunk。
    pub fn start(&mut self) -> Vec<Value> {
        let mut state = self.state.borrow_mut();
        start_events(&self.config, &mut state)
    }

    /// 一个上游 chunk → 若干 OpenAI chunk。
    pub fn push(&mut self, chunk: &Value) -> Vec<Value> {
        let mut state = self.state.borrow_mut();
        push_events(&self.config, &mut state, chunk)
    }

    /// 流结束：放掉扣住的尾巴、补 finish_reason，客户端要的话再补一条 usage。
    pub fn finish(&mut self) -> Vec<Value> {
        let mut state = self.state.borrow_mut();
        finish_events(&self.config, &mut state)
    }

    /// 非流式：拿**上游 chunk** 拼一个完整的 chat.completion（JS 里叫 toCompletion）。
    ///
    /// JS 的 toCompletion 复用翻译器自身的 state，服务层整包路径随后就靠 `stats()`
    /// 记日志，所以这里也用同一份 state（`&self` + RefCell），不能另起一份。
    pub fn to_completion(&self, chunks: &[Value]) -> Value {
        let mut state = self.state.borrow_mut();
        let mut events = start_events(&self.config, &mut state);
        for chunk in chunks {
            events.extend(push_events(&self.config, &mut state, chunk));
        }
        events.extend(finish_events(&self.config, &mut state));
        completion_from(&self.config, &events, state.usage)
    }

    /// 给日志/告警用的统计快照。
    pub fn stats(&self) -> crate::anthropic_stream::TranslatorStats {
        let state = self.state.borrow();
        crate::anthropic_stream::TranslatorStats {
            finish_reason: state.finish_reason.clone(),
            saw_tool_use: state.saw_tool_use,
            leaked_calls: state.leaked_calls,
            leaks_ignored: state.leaks_ignored,
            input_tokens: state.usage.map(|u| u.prompt as i64),
            output_tokens: state.usage.map(|u| u.completion as i64),
            thoughts_tokens: state.usage.map(|u| u.thoughts as i64),
            cached_tokens: state.usage.map(|u| u.cached as i64),
        }
    }
}

/// 和 `OpenAiTranslator::new` 等价；保留这个名字是为了对上模块桩里写的接口。
pub fn create_openai_translator(opts: TranslatorOptions) -> OpenAiTranslator {
    OpenAiTranslator::new(opts)
}

fn start_events(config: &Config, state: &mut State) -> Vec<Value> {
    state.role_sent = true;
    vec![frame(
        &config.id,
        config.created,
        &config.model,
        json!({ "role": "assistant", "content": "" }),
        None,
    )]
}

fn push_events(config: &Config, state: &mut State, chunk: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if !state.role_sent {
        state.role_sent = true;
        out.push(frame(
            &config.id,
            config.created,
            &config.model,
            json!({ "role": "assistant", "content": "" }),
            None,
        ));
    }
    // 上游可能给 { response: <CaGenerateContentResponse> }，也可能直接给内层。
    let payload = chunk
        .get("response")
        .filter(|v| !v.is_null())
        .unwrap_or(chunk);
    let candidate = payload
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|list| list.first());
    if let Some(reason) = candidate
        .and_then(|c| c.get("finishReason"))
        .filter(|v| truthy(Some(v)))
    {
        state.finish_reason = reason
            .as_str()
            .map(str::to_string)
            .or_else(|| Some(js_string(reason)));
    }
    if let Some(usage) = usage_of(payload.get("usageMetadata")) {
        state.usage = Some(usage);
    }
    if let Some(parts) = candidate
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
    {
        events_for_parts(config, parts, &mut out, state);
    }
    out
}

fn finish_events(config: &Config, state: &mut State) -> Vec<Value> {
    let mut chunks = Vec::new();
    for event in state.filter.finish() {
        if let LeakEvent::Text { text } = event {
            if !text.is_empty() {
                chunks.push(frame(
                    &config.id,
                    config.created,
                    &config.model,
                    json!({ "content": text }),
                    None,
                ));
            }
        }
    }
    let reason = map_finish(state.finish_reason.as_deref(), state.saw_tool_use);
    chunks.push(frame(
        &config.id,
        config.created,
        &config.model,
        json!({}),
        Some(reason),
    ));
    if config.include_usage {
        chunks.push(json!({
            "id": config.id,
            "object": "chat.completion.chunk",
            "created": config.created,
            "model": config.model,
            "choices": [],
            "usage": state.usage.map(Usage::value).unwrap_or_else(zero_usage),
        }));
    }
    chunks
}

/// 挨个 part 翻成 delta。functionCall 直接变 tool_calls，正文走泄漏过滤器。
fn events_for_parts(config: &Config, parts: &[Value], out: &mut Vec<Value>, state: &mut State) {
    for part in parts {
        let Some(part) = part.as_object() else {
            continue;
        };

        if let Some(function_call) = part.get("functionCall").filter(|v| truthy(Some(v))) {
            let call_id = new_call_id();
            let name = match function_call.get("name") {
                Some(v) if !v.is_null() => v.clone(),
                _ => Value::String("unknown_tool".to_string()),
            };
            let args = match function_call.get("args") {
                Some(v) if !v.is_null() => v.clone(),
                _ => Value::Object(Map::new()),
            };
            let arguments = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
            state.tool_index += 1;
            let call = json!({
                "index": state.tool_index,
                "id": call_id,
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            });
            out.push(frame(
                &config.id,
                config.created,
                &config.model,
                json!({ "tool_calls": [call] }),
                None,
            ));
            state.saw_tool_use = true;
            // 真签名按调用 id 存起来，Factory 回传 tool_calls 时才有得带。
            if let Some(signature) = part
                .get("thoughtSignature")
                .filter(|v| truthy(Some(v)))
                .and_then(Value::as_str)
            {
                sig_put(&config.signatures, &call_id, signature);
            }
            continue;
        }

        let text = match part.get("text") {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let signature = part.get("thoughtSignature").filter(|v| truthy(Some(v)));
        if truthy(part.get("thought")) {
            if !text.is_empty() {
                out.push(frame(
                    &config.id,
                    config.created,
                    &config.model,
                    json!({ "reasoning_content": text }),
                    None,
                ));
            }
            if let Some(signature) = signature.and_then(Value::as_str) {
                sig_put_trailing(&config.signatures, &config.session_key, signature);
            }
            continue;
        }
        // 只有签名、没有正文的 part：属于整个回合，按 session 存。
        if text.is_empty() {
            if let Some(signature) = signature.and_then(Value::as_str) {
                sig_put_trailing(&config.signatures, &config.session_key, signature);
            }
            continue;
        }

        for event in state.filter.push(&text) {
            match event {
                // 能确定的正文（含伪调用前后夹着的文字）直接吐
                LeakEvent::Text { text } => {
                    out.push(frame(
                        &config.id,
                        config.created,
                        &config.model,
                        json!({ "content": text }),
                        None,
                    ));
                }
                LeakEvent::ToolCall { name, input, raw } => {
                    // 和 Anthropic 侧同一条保险：没声明过的工具名，不当调用
                    //（正文里可能只是长得像）
                    if let Some(allowed) = &config.declared_tools {
                        if !allowed.is_empty() && !allowed.contains(&name) {
                            state.leaks_ignored += 1;
                            out.push(frame(
                                &config.id,
                                config.created,
                                &config.model,
                                json!({ "content": raw }),
                                None,
                            ));
                            continue;
                        }
                    }
                    state.leaked_calls += 1;
                    // 先如实回显原文，再补一个真调用（客户端两边都看得到）
                    out.push(frame(
                        &config.id,
                        config.created,
                        &config.model,
                        json!({ "content": raw }),
                        None,
                    ));
                    let call_id = new_call_id();
                    let args = if input.is_null() {
                        Value::Object(Map::new())
                    } else {
                        input
                    };
                    let arguments =
                        serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                    state.tool_index += 1;
                    let call = json!({
                        "index": state.tool_index,
                        "id": call_id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    });
                    out.push(frame(
                        &config.id,
                        config.created,
                        &config.model,
                        json!({ "tool_calls": [call] }),
                        None,
                    ));
                    state.saw_tool_use = true;
                }
            }
        }
    }
}

/// 把一路流出来的事件拼成一个 chat.completion。
fn completion_from(config: &Config, events: &[Value], fallback_usage: Option<Usage>) -> Value {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: BTreeMap<usize, Value> = BTreeMap::new();
    let mut finish = "stop".to_string();
    let mut usage: Option<Value> = None;

    for event in events {
        if let Some(choices) = event.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if let Some(reason) = choice.get("finish_reason").filter(|v| truthy(Some(v))) {
                    finish = reason
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| js_string(reason));
                }
                let delta = choice.get("delta");
                if let Some(text) = delta.and_then(|d| d.get("content")).and_then(Value::as_str) {
                    content.push_str(text);
                }
                if let Some(text) = delta
                    .and_then(|d| d.get("reasoning_content"))
                    .and_then(Value::as_str)
                {
                    reasoning.push_str(text);
                }
                if let Some(calls) = delta
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(Value::as_array)
                {
                    for call in calls {
                        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let function = call.get("function");
                        tool_calls.insert(
                            index,
                            json!({
                                "id": call.get("id").cloned().unwrap_or(Value::Null),
                                "type": "function",
                                "function": {
                                    "name": function.and_then(|f| f.get("name")).cloned().unwrap_or(Value::Null),
                                    "arguments": function
                                        .and_then(|f| f.get("arguments"))
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                },
                            }),
                        );
                    }
                }
            }
        }
        if let Some(ev_usage) = event.get("usage").filter(|v| truthy(Some(v))) {
            usage = Some(ev_usage.clone());
        }
    }

    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    // content 为空时按 OpenAI 习惯给 null
    message.insert(
        "content".to_string(),
        if content.is_empty() {
            Value::Null
        } else {
            Value::String(content)
        },
    );
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert(
            "tool_calls".to_string(),
            Value::Array(tool_calls.into_values().collect()),
        );
    }

    json!({
        "id": config.id,
        "object": "chat.completion",
        "created": config.created,
        "model": config.model,
        "choices": [{ "index": 0, "message": Value::Object(message), "finish_reason": finish }],
        // 整包一定要给 usage（哪怕客户端没要 stream_options.include_usage）
        "usage": usage.or_else(|| fallback_usage.map(Usage::value)).unwrap_or_else(zero_usage),
    })
}

/// OpenAI 的 SSE 帧：只有 data: 行，收尾是 `data: [DONE]`（由服务层补）。
pub fn sse_data_frame(payload: &Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signatures::shared_signatures;

    /// 对应 JS 测试里的 `chunkWith(parts, {finishReason, usage})`。
    fn chunk_with(parts: Value, finish_reason: Option<&str>, usage: Option<Value>) -> Value {
        let mut candidate = json!({ "content": { "role": "model", "parts": parts } });
        if let Some(reason) = finish_reason {
            candidate["finishReason"] = json!(reason);
        }
        let mut response = json!({ "candidates": [candidate] });
        if let Some(usage) = usage {
            response["usageMetadata"] = usage;
        }
        json!({ "response": response })
    }

    /// 所有 chunk 的 delta 串起来，方便断言。
    fn deltas(events: &[Value]) -> Vec<Value> {
        events
            .iter()
            .filter_map(|e| e.get("choices"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|c| c.get("delta"))
            .cloned()
            .collect()
    }

    fn joined_content(events: &[Value]) -> String {
        deltas(events)
            .iter()
            .filter_map(|d| d.get("content"))
            .filter_map(Value::as_str)
            .collect()
    }

    fn finish_reason_of(events: &[Value]) -> Option<Value> {
        events
            .iter()
            .filter_map(|e| e.get("choices"))
            .filter_map(Value::as_array)
            .flatten()
            .find(|c| {
                c.get("finish_reason")
                    .map(|v| !v.is_null())
                    .unwrap_or(false)
            })
            .and_then(|c| c.get("finish_reason"))
            .cloned()
    }

    fn translator(model: &str) -> OpenAiTranslator {
        OpenAiTranslator::new(TranslatorOptions {
            model: model.to_string(),
            signatures: shared_signatures(),
            session_key: "default".to_string(),
            include_usage: false,
            declared_tools: None,
        })
    }

    #[test]
    fn request_system_goes_to_system_instruction_and_messages_to_contents() {
        let built = build_gemini_request_from_openai(
            &json!({
                "model": "gpt-x",
                "max_tokens": 512,
                "messages": [
                    { "role": "system", "content": "你是助手" },
                    { "role": "user", "content": "你好" },
                    { "role": "assistant", "content": "在" },
                    { "role": "user", "content": "再问一句" },
                ],
            }),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["systemInstruction"],
            json!({ "parts": [{ "text": "你是助手" }] })
        );
        assert_eq!(
            built.inner["contents"],
            json!([
                { "role": "user", "parts": [{ "text": "你好" }] },
                { "role": "model", "parts": [{ "text": "在" }] },
                { "role": "user", "parts": [{ "text": "再问一句" }] },
            ])
        );
        assert_eq!(
            built.inner["generationConfig"]["maxOutputTokens"],
            json!(512)
        );
    }

    #[test]
    fn request_tool_calls_and_tool_messages_round_trip() {
        let built = build_gemini_request_from_openai(
            &json!({
                "messages": [
                    { "role": "user", "content": "查一下" },
                    {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "c1",
                            "type": "function",
                            "function": { "name": "Grep", "arguments": "{\"pattern\":\"x\"}" },
                        }],
                    },
                    { "role": "tool", "tool_call_id": "c1", "content": "命中 2 处" },
                ],
            }),
            BuildOptions::default(),
        );
        let call = &built.inner["contents"][1]["parts"][0];
        assert_eq!(
            call["functionCall"],
            json!({ "name": "Grep", "args": { "pattern": "x" } })
        );
        assert_eq!(call["thoughtSignature"], json!(SIGNATURE_SENTINEL));
        assert_eq!(
            built.inner["contents"][2]["parts"][0]["functionResponse"],
            json!({ "name": "Grep", "response": { "result": "命中 2 处" } })
        );
    }

    #[test]
    fn request_reuses_stored_signature_only_on_the_first_parallel_call() {
        let signatures = shared_signatures();
        sig_put(&signatures, "c1", "SIG-1");
        let built = build_gemini_request_from_openai(
            &json!({
                "messages": [{
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        { "id": "c1", "type": "function", "function": { "name": "get_time", "arguments": "{}" } },
                        { "id": "c2", "type": "function", "function": { "name": "get_time", "arguments": "{}" } },
                    ],
                }],
            }),
            BuildOptions {
                signatures: Some(&signatures),
                ..Default::default()
            },
        );
        let parts = built.inner["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["thoughtSignature"], json!("SIG-1"));
        assert!(parts[1].get("thoughtSignature").is_none());
        assert_eq!(built.stats.tool_signature_hits, 1);
        assert_eq!(built.stats.tool_signature_misses, 1);
    }

    #[test]
    fn request_tools_tool_choice_reasoning_effort_and_response_format() {
        let built = build_gemini_request_from_openai(
            &json!({
                "messages": [{ "role": "user", "content": "x" }],
                "tools": [{
                    "type": "function",
                    "function": { "name": "Grep", "description": "搜", "parameters": { "type": "object" } },
                }],
                "tool_choice": { "type": "function", "function": { "name": "Grep" } },
                "reasoning_effort": "medium",
                "response_format": { "type": "json_object" },
            }),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["tools"][0]["functionDeclarations"][0]["name"],
            json!("Grep")
        );
        assert_eq!(
            built.inner["toolConfig"],
            json!({ "functionCallingConfig": { "mode": "ANY", "allowedFunctionNames": ["Grep"] } })
        );
        assert_eq!(
            built.inner["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(8192)
        );
        assert_eq!(
            built.inner["generationConfig"]["responseMimeType"],
            json!("application/json")
        );
    }

    #[test]
    fn schema_keywords_outside_the_allowlist_are_stripped_and_warned() {
        let built = build_gemini_request_from_openai(
            &json!({
                "messages": [{ "role": "user", "content": "x" }],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "Task",
                        "parameters": {
                            "type": "object",
                            "properties": { "priority": { "type": "integer", "exclusiveMinimum": 0 } },
                            "additionalProperties": false
                        }
                    }
                }],
            }),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["tools"][0]["functionDeclarations"][0]["parameters"],
            json!({ "type": "object", "properties": { "priority": { "type": "integer" } } })
        );
        assert!(built.warnings.contains(
            &"schema_keywords_dropped:additionalProperties,exclusiveMinimum".to_string()
        ));
    }

    #[test]
    fn request_tool_choice_string_form_and_images() {
        let built = build_gemini_request_from_openai(
            &json!({
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "看图" },
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
                    ],
                }],
                "tools": [{ "type": "function", "function": { "name": "T" } }],
                "tool_choice": "required",
            }),
            BuildOptions::default(),
        );
        assert_eq!(
            built.inner["toolConfig"]["functionCallingConfig"]["mode"],
            json!("ANY")
        );
        assert_eq!(
            built.inner["contents"][0]["parts"][1],
            json!({ "inlineData": { "mimeType": "image/png", "data": "AAAA" } })
        );
    }

    #[test]
    fn stream_role_then_content_then_tool_calls_then_finish() {
        let store = shared_signatures();
        let mut t = OpenAiTranslator::new(TranslatorOptions {
            model: "m".to_string(),
            signatures: store.clone(),
            session_key: "s".to_string(),
            include_usage: false,
            declared_tools: None,
        });
        let mut events = t.start();
        events.extend(t.push(&chunk_with(
            json!([{ "text": "你好" }]),
            None,
            Some(json!({ "promptTokenCount": 3, "candidatesTokenCount": 1 })),
        )));
        events.extend(t.push(&chunk_with(
            json!([{ "thoughtSignature": "SIG-2", "functionCall": { "name": "Grep", "args": { "pattern": "x" } } }]),
            Some("STOP"),
            None,
        )));
        events.extend(t.finish());

        assert_eq!(events[0]["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(joined_content(&events), "你好");

        let ds = deltas(&events);
        let call = ds
            .iter()
            .filter_map(|d| d.get("tool_calls"))
            .filter_map(Value::as_array)
            .flatten()
            .next()
            .expect("a tool call");
        assert_eq!(call["function"]["name"], json!("Grep"));
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
            json!({ "pattern": "x" })
        );
        assert_eq!(
            crate::signatures::sig_get(&store, call["id"].as_str().unwrap()),
            Some("SIG-2".to_string())
        );

        assert_eq!(finish_reason_of(&events), Some(json!("tool_calls")));
        assert_eq!(
            events.last().unwrap()["object"],
            json!("chat.completion.chunk")
        );
    }

    #[test]
    fn stream_include_usage_appends_a_usage_only_chunk() {
        let mut t = OpenAiTranslator::new(TranslatorOptions {
            model: "m".to_string(),
            signatures: shared_signatures(),
            session_key: "default".to_string(),
            include_usage: true,
            declared_tools: None,
        });
        t.start();
        t.push(&chunk_with(
            json!([{ "text": "x" }]),
            None,
            Some(
                json!({ "promptTokenCount": 7, "candidatesTokenCount": 3, "totalTokenCount": 10 }),
            ),
        ));
        let events = t.finish();
        let usage_chunk = events.last().unwrap();
        assert_eq!(usage_chunk["choices"], json!([]));
        assert_eq!(
            usage_chunk["usage"],
            json!({ "prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10 })
        );
    }

    #[test]
    fn stream_thinking_goes_to_reasoning_content() {
        let mut t = translator("m");
        t.start();
        let events = t.push(&chunk_with(
            json!([{ "text": "我在想", "thought": true, "thoughtSignature": "TS" }]),
            None,
            None,
        ));
        assert_eq!(
            events[0]["choices"][0]["delta"]["reasoning_content"],
            json!("我在想")
        );
    }

    #[test]
    fn stream_leaked_pseudo_call_becomes_content_plus_a_real_tool_call() {
        let mut t = translator("m");
        t.start();
        let events = t.push(&chunk_with(
            json!([{ "text": "<call:default_api:Grep{pattern:foo}" }]),
            Some("STOP"),
            None,
        ));
        let ds = deltas(&events);
        assert_eq!(
            ds[0]["content"],
            json!("<call:default_api:Grep{pattern:foo}")
        );
        assert_eq!(ds[1]["tool_calls"][0]["function"]["name"], json!("Grep"));
        assert_eq!(t.stats().leaked_calls, 1);
    }

    #[test]
    fn completion_packs_chunks_into_a_chat_completion() {
        let store = shared_signatures();
        let t = OpenAiTranslator::new(TranslatorOptions {
            model: "m".to_string(),
            signatures: store.clone(),
            session_key: "default".to_string(),
            include_usage: false,
            declared_tools: None,
        });
        let completion = t.to_completion(&[chunk_with(
            json!([
                { "text": "半句" },
                { "functionCall": { "name": "Read", "args": { "file_path": "/a" } } },
            ]),
            Some("STOP"),
            Some(json!({ "promptTokenCount": 4, "candidatesTokenCount": 2, "totalTokenCount": 6 })),
        )]);
        assert_eq!(completion["object"], json!("chat.completion"));
        assert_eq!(
            completion["choices"][0]["message"]["content"],
            json!("半句")
        );
        let call = &completion["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["function"]["name"], json!("Read"));
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
            json!({ "file_path": "/a" })
        );
        assert_eq!(
            completion["choices"][0]["finish_reason"],
            json!("tool_calls")
        );
        assert_eq!(completion["usage"]["total_tokens"], json!(6));
        // 回传时签名能对上：这一发的 functionCall 没有签名，所以库里也没有。
        assert_eq!(
            crate::signatures::sig_get(&store, call["id"].as_str().unwrap()),
            None
        );
    }

    #[test]
    fn finish_reason_maps_length_and_content_filter() {
        let mut t1 = translator("m");
        t1.start();
        t1.push(&chunk_with(
            json!([{ "text": "x" }]),
            Some("MAX_TOKENS"),
            None,
        ));
        assert_eq!(finish_reason_of(&t1.finish()), Some(json!("length")));

        let mut t2 = translator("m");
        t2.start();
        t2.push(&chunk_with(
            json!([{ "text": "x" }]),
            Some("PROHIBITED_CONTENT"),
            None,
        ));
        assert_eq!(
            finish_reason_of(&t2.finish()),
            Some(json!("content_filter"))
        );
    }

    #[test]
    fn sse_data_frame_only_has_a_data_line() {
        assert_eq!(sse_data_frame(&json!({ "a": 1 })), "data: {\"a\":1}\n\n");
    }
}
