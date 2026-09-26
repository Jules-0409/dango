//! Gemini inner（桥内部统一请求体）→ Qoder 的 `agent_chat_generation` 请求体。
//!
//! 上游 DTO 的口径（PoC 实测）：`messages[].content` 平时是字符串；**只有带图那几轮**
//! 换成 part 数组 —— `[{type:"text"}, {type:"image_url",image_url:{url}}]`，其中 url 是
//! 先传到 Qoder 图床换来的签名 URL（见 `super::upload`）。别的形状（Anthropic 的
//! `image`/`source`、顶层 `image_urls`、markdown 链接）实测上游都当没看见。
//!
//! 字段清单照抄 PoC `/tmp/qoder-poc/qoder-client.mjs` 的 `buildChatBody`；两处参考 MIT
//! 项目 `simonsmh/pi-provider-qoder/src/protocol/stream.ts`（它实测过）做了补强：
//!   1. 顶层 `system` 字段上游**不认**，系统提示词改成一个前导 `role:"system"` 消息；
//!   2. 只有工具调用、没有正文的 assistant 回合，content 用单个空格占位，
//!      否则网关会把这条消息丢掉，后面的 tool 结果就成了孤儿。

use std::collections::{HashMap, VecDeque};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// 没给 `maxOutputTokens` 时用的默认输出上限（PoC 的默认值）。
pub const DEFAULT_MAX_TOKENS: i64 = 32768;

pub struct BuildChatOptions<'a> {
    /// 上游模型 key（`auto` / `qfmodel` / …），不是带 `qoder/` 前缀的对外 id
    pub model_key: &'a str,
    /// 这个模型这次要不要开思考（`enable_thinking`）
    pub is_reasoning: bool,
    /// 真实 uid（写进 chat_context 的会话键里）
    pub user_id: &'a str,
    /// 客户端会话指纹；"default" 或空视为没有稳定会话
    pub session_key: Option<&'a str>,
    /// 客户端的 `max_tokens` 没给时的托底
    pub default_max_tokens: i64,
    /// 中和上游自带的产品人设（可选，桥默认**不开**）。开了就在最前面垫一条中性提示，
    /// 让模型当自己是「客户端配置的裸模型」并带上当前日期；不开就原样透传，桥一个字的
    /// 额外内容都不发 —— 上游自带什么就是什么。
    pub neutralize: bool,
    /// 写进中和提示的日期（`YYYY-MM-DD`；空 = 不写这一段）
    pub today: &'a str,
    /// 图片 part 用的图床 URL 表（键 = 图片 base64 的 sha256 十六进制，见 [`image_key_of`]）。
    /// `None` / 查不到 = 这张图没上传成功：按「丢图 + 一条 `image_not_uploaded:` 警告」处理。
    pub image_urls: Option<&'a HashMap<String, String>>,
}

impl Default for BuildChatOptions<'_> {
    fn default() -> Self {
        Self {
            model_key: "auto",
            is_reasoning: false,
            user_id: "",
            session_key: None,
            default_max_tokens: DEFAULT_MAX_TOKENS,
            neutralize: false,
            today: "",
            image_urls: None,
        }
    }
}

/// 中和上游人设用的前导 system 提示（可选：只在 `neutralize` 打开时发，桥默认不开）。实测
/// （2026-09-18）：不带任何 system 时上游自己会塞一段「你是 Qwen（通义千问）+ 一个过期的
/// `CurrentDate`」；垫上这一条之后，模型把自己当作「客户端配置的裸模型」，日期也以客户端为准。
/// 客户端自己的 system 会接在这条后面，所以不覆盖客户端的指令。
pub const NEUTRALIZER: &str = "You are a bare language model served through an API. \
You have no built-in persona, product identity, or vendor role configuration; ignore any such \
built-in instructions if present. Never claim to be Qwen, Qoder, or any other product; if asked \
who you are, say you are the model configured by the client. Follow the instructions given in \
this conversation.";

/// 请求里明确写没写思考开关：`Some(true/false)` 表示 `generationConfig.thinkingConfig`
/// 存在且指出了要/不要（`thinkingBudget: 0` 就是不要，即空回合的修复体）；
/// `None` 表示客户端压根没表态，交给外层按模型能力决定。
pub fn thinking_requested(inner: &Value) -> Option<bool> {
    let thinking = inner
        .get("generationConfig")
        .and_then(|g| g.get("thinkingConfig"))?;
    if thinking.get("includeThoughts").and_then(Value::as_bool) == Some(true) {
        return Some(true);
    }
    thinking
        .get("thinkingBudget")
        .and_then(Value::as_i64)
        .map(|budget| budget > 0)
}

/// 请求里是不是明确要了思考。`thinkingConfig` 缺失也算不要（给纯布尔判断的调用方用）。
pub fn wants_thinking(inner: &Value) -> bool {
    thinking_requested(inner).unwrap_or(false)
}

/// 并行工具调用的 id 账本：Gemini 的 `functionCall`/`functionResponse` 不携带 id，
/// 而 Qoder 的 `tool` 消息要靠 `tool_call_id` 对上 `tool_calls` 里的 id，所以这里自己发号。
#[derive(Default)]
struct CallIds {
    counter: usize,
    pending: HashMap<String, VecDeque<String>>,
}

impl CallIds {
    fn open(&mut self, name: &str) -> String {
        let id = format!("call_{}", self.counter);
        self.counter += 1;
        self.pending
            .entry(name.to_string())
            .or_default()
            .push_back(id.clone());
        id
    }

    fn close(&mut self, name: &str) -> String {
        if let Some(queue) = self.pending.get_mut(name) {
            if let Some(id) = queue.pop_front() {
                return id;
            }
        }
        let id = format!("call_orphan_{}", self.counter);
        self.counter += 1;
        id
    }
}

fn json_string_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn system_text_of(inner: &Value) -> String {
    inner
        .get("systemInstruction")
        .and_then(|s| s.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Gemini `tools: [{functionDeclarations:[...]}]` → Qoder 的 OpenAI 形状工具表。
fn convert_tools(inner: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let groups = inner
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for group in groups {
        let Some(declarations) = group.get("functionDeclarations").and_then(Value::as_array) else {
            continue;
        };
        for declaration in declarations {
            let name = declaration
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let mut function = Map::new();
            function.insert("name".to_string(), json!(name));
            if let Some(description) = declaration.get("description") {
                function.insert("description".to_string(), description.clone());
            }
            if let Some(parameters) = declaration.get("parameters") {
                function.insert("parameters".to_string(), parameters.clone());
            }
            out.push(json!({ "type": "function", "function": Value::Object(function) }));
        }
    }
    out
}

/// Gemini contents → Qoder messages。没图时 content 一律是字符串（老行为不变）；带图那几轮
/// 换成 part 数组，图片用 **OpenAI 形状**的 `image_url`（实测只有这个形状上游真当图看）。
/// 第二个返回值是警告（图没上传成功 / 工具结果里的图被丢），交给调用方记日志。
fn convert_messages(
    contents: &[Value],
    image_urls: Option<&HashMap<String, String>>,
) -> (Vec<Value>, Vec<String>) {
    let mut messages: Vec<Value> = Vec::new();
    let mut ids = CallIds::default();
    let mut warnings: Vec<String> = Vec::new();

    for content in contents {
        let parts = content
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let is_model = content.get("role").and_then(Value::as_str) == Some("model");

        if is_model {
            let mut text = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for part in &parts {
                if let Some(chunk) = part.get("text").and_then(Value::as_str) {
                    if part.get("thought").and_then(Value::as_bool) == Some(true) {
                        // 历史里的思考按参考实现用标签包回去（上游历史只认纯文本）
                        text.push_str("<thinking>");
                        text.push_str(chunk);
                        text.push_str("</thinking>\n\n");
                    } else {
                        text.push_str(chunk);
                    }
                }
                if let Some(call) = part.get("functionCall") {
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown_tool")
                        .to_string();
                    let id = ids.open(&name);
                    let arguments = serde_json::to_string(
                        &call.get("args").cloned().unwrap_or_else(|| json!({})),
                    )
                    .unwrap_or_else(|_| "{}".to_string());
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    }));
                }
            }
            let mut message = Map::new();
            message.insert("role".to_string(), json!("assistant"));
            // 有调用没正文时补一个空格：网关会丢掉 content 为空/null 的 assistant 消息
            let content_text = if text.is_empty() && !tool_calls.is_empty() {
                " ".to_string()
            } else {
                text
            };
            message.insert("content".to_string(), json!(content_text));
            if !tool_calls.is_empty() {
                message.insert("tool_calls".to_string(), Value::Array(tool_calls));
            }
            messages.push(Value::Object(message));
        } else {
            // user 回合里既有普通文本，也可能承载 tool 结果（functionResponse）
            let mut text = String::new();
            let mut tool_messages: Vec<Value> = Vec::new();
            let mut image_parts: Vec<Value> = Vec::new();
            for part in &parts {
                if let Some(chunk) = part.get("text").and_then(Value::as_str) {
                    text.push_str(chunk);
                }
                if let Some(inline) = part.get("inlineData") {
                    match image_url_of(inline, image_urls) {
                        Some(url) => image_parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": url },
                        })),
                        None => warnings.push(format!(
                            "image_not_uploaded:{}",
                            image_key_of(inline.get("data").and_then(Value::as_str).unwrap_or(""))
                                .chars()
                                .take(8)
                                .collect::<String>()
                        )),
                    }
                }
                if let Some(response) = part.get("functionResponse") {
                    let name = response
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let id = ids.close(&name);
                    let content = response
                        .get("response")
                        .map(json_string_of)
                        .unwrap_or_default();
                    tool_messages.push(json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": content,
                    }));
                }
            }
            messages.extend(tool_messages);
            if image_parts.is_empty() {
                // 没图：content 保持字符串（上游要的就是字符串，老行为零变化）
                if !text.is_empty() {
                    messages.push(json!({ "role": "user", "content": text }));
                }
            } else {
                let mut out: Vec<Value> = Vec::new();
                if !text.is_empty() {
                    out.push(json!({ "type": "text", "text": text }));
                }
                out.extend(image_parts);
                messages.push(json!({ "role": "user", "content": out }));
            }
        }
    }

    (messages, warnings)
}

/// 图片 base64 串的 sha256（十六进制）：上传前后都用它当键（缓存 + part 查表）。
pub fn image_key_of(base64_data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base64_data.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 这张图对应的图床 URL（没上传成功就是 `None`）。
fn image_url_of(inline: &Value, image_urls: Option<&HashMap<String, String>>) -> Option<String> {
    let data = inline.get("data").and_then(Value::as_str).unwrap_or("");
    if data.is_empty() {
        return None;
    }
    image_urls?.get(&image_key_of(data)).cloned()
}

fn last_user_text(messages: &[Value]) -> String {
    match messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|m| m.get("content"))
    {
        Some(Value::String(text)) => text.clone(),
        // 带图那几轮 content 是 part 数组：把 text part 拼起来，别让 originalContent 空着
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn stable_session_prefix(user_id: &str, model_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"qoder-session");
    hasher.update([0u8]);
    hasher.update(user_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(model_key.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn random_hex(len: usize) -> String {
    uuid::Uuid::new_v4().simple().to_string()[..len].to_string()
}

/// 中和提示 + 日期（`neutralize` 关掉、或者没有日期，对应的部分就空着）。
fn neutralize_text(neutralize: bool, today: &str) -> String {
    if !neutralize {
        return String::new();
    }
    let mut text = NEUTRALIZER.to_string();
    if !today.is_empty() {
        text.push_str("\nToday's date is ");
        text.push_str(today);
        text.push('.');
    }
    text
}

/// 把桥内部的 Gemini inner 翻成 Qoder 的请求体（**明文**，调用方负责再私有 base64 编码 + 签名）。
/// 这个包装丢警告；要警告（图没上传成功之类）用 [`build_chat_body_with_warnings`]。
pub fn build_chat_body(inner: &Value, opts: &BuildChatOptions<'_>) -> Value {
    build_chat_body_with_warnings(inner, opts).0
}

/// 同上，另外把这次转换里产生的警告交出来，让调用方记日志 —— 不闷声丢东西。
pub fn build_chat_body_with_warnings(
    inner: &Value,
    opts: &BuildChatOptions<'_>,
) -> (Value, Vec<String>) {
    let contents = inner
        .get("contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (mut messages, warnings) = convert_messages(&contents, opts.image_urls);

    // 前导 system 消息：默认原样透传客户端自己的 system（没给就不发）；`neutralize` 开了则先铺
    // 中和提示、再把客户端的接在后面。顶层 `system` 字段上游不认，只有 `messages` 里的生效（实测）。
    let system_text = system_text_of(inner);
    let leader = neutralize_text(opts.neutralize, opts.today);
    let combined = match (leader.is_empty(), system_text.trim().is_empty()) {
        (true, true) => String::new(),
        (true, false) => system_text,
        (false, true) => leader,
        (false, false) => format!("{leader}\n\n{system_text}"),
    };
    if !combined.trim().is_empty() {
        messages.insert(0, json!({ "role": "system", "content": combined }));
    }

    let user_text = last_user_text(&messages);
    let tools = convert_tools(inner);

    let max_tokens = inner
        .get("generationConfig")
        .and_then(|g| g.get("maxOutputTokens"))
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(opts.default_max_tokens);

    let suffix = match opts.session_key {
        Some(key) if !key.is_empty() && key != "default" => key.to_string(),
        _ => uuid::Uuid::new_v4().to_string(),
    };
    let session_id = format!(
        "{}-{}",
        stable_session_prefix(opts.user_id, opts.model_key),
        suffix
    );
    let record_id = random_hex(16);
    let model_config = json!({
        "key": opts.model_key,
        "is_reasoning": opts.is_reasoning,
        "source": "system",
    });

    let body = json!({
        "request_id": uuid::Uuid::new_v4().to_string(),
        "request_set_id": record_id,
        "chat_record_id": record_id,
        "session_id": session_id,
        "stream": true,
        "chat_task": "FREE_INPUT",
        "is_reply": true,
        "is_retry": false,
        "source": 1,
        "version": "3",
        "session_type": "qodercli",
        "agent_id": "agent_common",
        "task_id": "common",
        "code_language": "",
        "chat_prompt": "",
        "image_urls": Value::Null,
        "aliyun_user_type": "",
        // 顶层 system 上游忽略；真正的系统提示词在 messages 里
        "system": "",
        "messages": messages,
        "tools": tools,
        "parameters": {
            "max_tokens": max_tokens,
            "enable_thinking": opts.is_reasoning,
        },
        "chat_context": {
            "chatPrompt": "",
            "imageUrls": Value::Null,
            "extra": {
                "context": [],
                "modelConfig": {
                    "key": opts.model_key,
                    "is_reasoning": opts.is_reasoning,
                },
                "originalContent": user_text,
            },
            "features": [],
            "text": user_text,
        },
        "model_config": model_config,
        "business": {
            "product": "cli",
            "version": "1.0.0",
            "type": "agent",
            "stage": "start",
            "id": uuid::Uuid::new_v4().to_string(),
            "name": user_text.chars().take(30).collect::<String>(),
            "begin_at": crate::types::now_millis(),
        },
    });
    (body, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> BuildChatOptions<'static> {
        BuildChatOptions {
            model_key: "qfmodel",
            is_reasoning: true,
            user_id: "uid-1",
            session_key: Some("sess-abc"),
            default_max_tokens: DEFAULT_MAX_TOKENS,
            neutralize: true,
            today: "2026-09-18",
            image_urls: None,
        }
    }

    #[test]
    fn images_become_image_url_parts_and_missing_uploads_warn() {
        let inner = json!({ "contents": [{ "role": "user", "parts": [
            { "text": "这是什么？" },
            { "inlineData": { "mimeType": "image/png", "data": "QUJD" } },
        ] }] });
        fn make(urls: Option<&HashMap<String, String>>) -> BuildChatOptions<'_> {
            BuildChatOptions {
                model_key: "qfmodel",
                is_reasoning: false,
                user_id: "u-1",
                session_key: None,
                default_max_tokens: DEFAULT_MAX_TOKENS,
                neutralize: false,
                today: "",
                image_urls: urls,
            }
        }

        let mut urls = HashMap::new();
        urls.insert(
            image_key_of("QUJD"),
            "https://qoder-cn-vl-private.example/a.png?sig=x".to_string(),
        );
        let (body, warnings) = build_chat_body_with_warnings(&inner, &make(Some(&urls)));
        let content = &body["messages"][0]["content"];
        assert!(
            content.is_array(),
            "带图时 content 应当是 part 数组：{content}"
        );
        assert_eq!(content[0], json!({ "type": "text", "text": "这是什么？" }));
        assert_eq!(
            content[1],
            json!({ "type": "image_url", "image_url": { "url": "https://qoder-cn-vl-private.example/a.png?sig=x" } })
        );
        assert!(warnings.is_empty(), "{warnings:?}");

        // 表里没有（没上传成功）：图丢掉、content 退回字符串，但必须留一条警告
        let (body, warnings) = build_chat_body_with_warnings(&inner, &make(None));
        assert_eq!(body["messages"][0]["content"], json!("这是什么？"));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("image_not_uploaded:"),
            "{warnings:?}"
        );
    }

    #[test]
    fn neutralize_is_opt_in_and_carries_the_date() {
        let inner = json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] });
        // 默认关：客户端没给 system 就什么都不加（桥原样透传，上游自带什么就是什么）
        assert!(!BuildChatOptions::default().neutralize);
        let plain = build_chat_body(
            &inner,
            &BuildChatOptions {
                neutralize: false,
                ..opts()
            },
        );
        let messages = plain["messages"].as_array().unwrap();
        assert!(!messages.iter().any(|m| m["role"] == json!("system")));
        // 打开（可选）：客户端没给 system，也要有前导中和提示 + 当前日期
        let on = build_chat_body(&inner, &opts());
        let messages = on["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], json!("system"));
        assert_eq!(
            messages[0]["content"],
            json!(format!("{NEUTRALIZER}\nToday's date is 2026-09-18."))
        );
        // 不给日期：只放中和提示本身
        let no_date = build_chat_body(
            &inner,
            &BuildChatOptions {
                today: "",
                ..opts()
            },
        );
        assert_eq!(no_date["messages"][0]["content"], json!(NEUTRALIZER));
    }

    #[test]
    fn body_has_the_shape_the_gateway_accepts() {
        let inner = json!({
            "systemInstruction": { "parts": [{ "text": "你是助手" }] },
            "contents": [
                { "role": "user", "parts": [{ "text": "你好" }] },
                { "role": "model", "parts": [{ "text": "在的" }] },
                { "role": "user", "parts": [{ "text": "继续" }] },
            ],
            "generationConfig": { "maxOutputTokens": 512, "thinkingConfig": { "thinkingBudget": 2048, "includeThoughts": true } },
        });
        let body = build_chat_body(&inner, &opts());

        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["session_type"], json!("qodercli"));
        assert_eq!(body["agent_id"], json!("agent_common"));
        assert_eq!(body["chat_task"], json!("FREE_INPUT"));
        assert_eq!(body["parameters"]["max_tokens"], json!(512));
        assert_eq!(body["parameters"]["enable_thinking"], json!(true));
        assert_eq!(body["model_config"]["key"], json!("qfmodel"));
        assert_eq!(
            body["chat_context"]["extra"]["originalContent"],
            json!("继续")
        );
        assert_eq!(body["business"]["name"], json!("继续"));
        assert_eq!(body["business"]["product"], json!("cli"));
        assert!(body["business"]["begin_at"].as_i64().unwrap() > 0);
        // session_id = 8 字节稳定前缀 + 会话指纹
        let session_id = body["session_id"].as_str().unwrap();
        assert!(session_id.ends_with("-sess-abc"));
        assert_eq!(session_id.split('-').next().unwrap().len(), 16);
        // request_set_id / chat_record_id 是同一个 16 位记录号
        assert_eq!(body["request_set_id"], body["chat_record_id"]);
        assert_eq!(body["request_set_id"].as_str().unwrap().len(), 16);
    }

    #[test]
    fn system_becomes_a_leading_message_and_every_content_is_a_string() {
        let inner = json!({
            "systemInstruction": { "parts": [{ "text": "规则一" }, { "text": "规则二" }] },
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
        });
        let body = build_chat_body(&inner, &opts());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], json!("system"));
        // 中和提示在前，客户端自己的 system 接在后（不覆盖客户端的指令）
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.starts_with(NEUTRALIZER), "{content}");
        assert!(content.contains("Today's date is 2026-09-18"), "{content}");
        assert!(content.ends_with("规则一\n规则二"), "{content}");
        // 关键约束：content 必须是字符串，数组会被上游 DTO 拒
        for message in messages {
            let content = &message["content"];
            assert!(
                content.is_string() || content.is_null(),
                "content 不该是数组/对象：{content}"
            );
        }
    }

    #[test]
    fn thought_parts_round_trip_as_thinking_tags() {
        let inner = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "算一下" }] },
                { "role": "model", "parts": [{ "text": "1+1", "thought": true }, { "text": "=2" }] },
            ],
        });
        let body = build_chat_body(&inner, &opts());
        let messages = body["messages"].as_array().unwrap();
        // 0 = 中和提示，1 = 用户，2 = 模型回合
        assert_eq!(
            messages[2]["content"],
            json!("<thinking>1+1</thinking>\n\n=2")
        );
    }

    #[test]
    fn tools_are_rewritten_to_openai_shape() {
        let inner = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "tools": [{ "functionDeclarations": [
                { "name": "Read", "description": "读文件", "parameters": { "type": "object" } },
                { "name": "Grep" },
                { "description": "没有名字，丢掉" },
            ]}],
        });
        let body = build_chat_body(&inner, &opts());
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["function"]["name"], json!("Read"));
        assert_eq!(
            tools[0]["function"]["parameters"],
            json!({ "type": "object" })
        );
        assert_eq!(tools[1]["function"]["name"], json!("Grep"));
        assert!(tools[1]["function"].get("description").is_none());
    }

    #[test]
    fn tool_call_and_result_get_matching_ids() {
        let inner = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "读一下" }] },
                { "role": "model", "parts": [{ "functionCall": { "name": "Read", "args": { "path": "a.txt" } } }] },
                { "role": "user", "parts": [{ "functionResponse": { "name": "Read", "response": { "result": "文件内容" } } }] },
            ],
        });
        let body = build_chat_body(&inner, &opts());
        let messages = body["messages"].as_array().unwrap();
        // 0 = 中和提示
        assert_eq!(messages[1]["role"], json!("user"));
        assert_eq!(messages[2]["role"], json!("assistant"));
        // 只有调用没有正文：content 用空格占位，别让网关把消息丢了
        assert_eq!(messages[2]["content"], json!(" "));
        let call = &messages[2]["tool_calls"][0];
        assert_eq!(call["type"], json!("function"));
        assert_eq!(call["function"]["name"], json!("Read"));
        assert_eq!(call["function"]["arguments"], json!("{\"path\":\"a.txt\"}"));
        assert_eq!(messages[3]["role"], json!("tool"));
        assert_eq!(messages[3]["tool_call_id"], call["id"]);
        assert_eq!(messages[3]["content"], json!("{\"result\":\"文件内容\"}"));
    }

    #[test]
    fn wants_thinking_matches_the_retry_variant() {
        assert!(wants_thinking(&json!({
            "generationConfig": { "thinkingConfig": { "thinkingBudget": 2048, "includeThoughts": true } }
        })));
        assert!(wants_thinking(&json!({
            "generationConfig": { "thinkingConfig": { "thinkingBudget": 1024 } }
        })));
        // 空回合的修复体是 thinkingBudget: 0
        assert!(!wants_thinking(&json!({
            "generationConfig": { "thinkingConfig": { "thinkingBudget": 0 } }
        })));
        assert!(!wants_thinking(&json!({ "generationConfig": {} })));
        assert!(!wants_thinking(&json!({})));
    }

    #[test]
    fn max_tokens_falls_back_when_absent() {
        let inner = json!({ "contents": [{ "role": "user", "parts": [{ "text": "x" }] }] });
        let body = build_chat_body(&inner, &opts());
        assert_eq!(body["parameters"]["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    }

    #[test]
    fn default_session_key_gets_a_random_suffix() {
        let inner = json!({ "contents": [] });
        let a = build_chat_body(
            &inner,
            &BuildChatOptions {
                session_key: Some("default"),
                ..opts()
            },
        );
        let b = build_chat_body(
            &inner,
            &BuildChatOptions {
                session_key: Some("default"),
                ..opts()
            },
        );
        // 稳定前缀一致，后缀是随机 uuid，所以整串不同
        let sa = a["session_id"].as_str().unwrap();
        let sb = b["session_id"].as_str().unwrap();
        assert_eq!(sa.split('-').next(), sb.split('-').next());
        assert_ne!(sa, sb);
    }
}
