//! 两个方向上的报文解析：Anthropic Messages 与 OpenAI Chat Completions。
//!
//! 两者都先归一到 [`UnifiedRequest`]，再拼成 CLI 要的那一段 prompt。这样协议层的差异
//! 只在这个文件里，后面发请求、翻译事件流的代码就一套。

use serde::Deserialize;
use serde_json::Value;

use crate::error::{Error, Result};

/// 归一化之后的一次请求。
#[derive(Debug, Clone, Default)]
pub struct UnifiedRequest {
    pub model: Option<String>,
    pub system: Option<String>,
    pub turns: Vec<Turn>,
    pub stream: bool,
    /// 客户端有没有明确要思考内容（Anthropic 的 `thinking` 字段 / OpenAI 的
    /// `reasoning_effort`）。没要就别硬塞 thinking 块，有的客户端会不认。
    pub wants_thinking: bool,
}

/// 归一化之后的一条消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// `user` 或 `assistant`（其他角色统一当 user 处理）。
    pub role: String,
    pub text: String,
}

impl UnifiedRequest {
    /// 最后一条用户消息，空请求时返回 None。
    pub fn last_user_text(&self) -> Option<&str> {
        self.turns
            .iter()
            .rev()
            .find(|t| t.role == "user")
            .map(|t| t.text.as_str())
    }
}

/// 拼给 CLI 的那段 prompt。
///
/// 形状（尽量直白，让 CLI 那边的 agent 一看就懂是接续对话）：
///
/// ```text
/// <system>
/// …客户端给的系统提示…
/// </system>
///
/// User: 上一条
///
/// Assistant: 上一条回答
///
/// User: 这一条
/// ```
///
/// `budget` 是历史正文的字符上限，超了就从**最旧**的开始丢（保留系统提示和最后一条用户
/// 消息）。CLI 是「单次调用 + 一段 prompt」，没有服务端会话，历史只能这么带。
pub fn build_prompt(req: &UnifiedRequest, budget: usize) -> Result<String> {
    let mut head = String::new();
    if let Some(system) = req
        .system
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        head.push_str("<system>\n");
        head.push_str(system);
        head.push_str("\n</system>\n\n");
    }

    let turns: Vec<&Turn> = req
        .turns
        .iter()
        .filter(|t| !t.text.trim().is_empty())
        .collect();
    if turns.is_empty() {
        return Err(Error::EmptyPrompt);
    }

    // 最后一条用户消息永远保留
    let last_user_idx = turns
        .iter()
        .rposition(|t| t.role == "user")
        .unwrap_or(turns.len() - 1);

    // 从后往前攒到预算为止
    let mut kept: Vec<&Turn> = Vec::new();
    let mut used = head.len();
    let mut dropped = 0usize;
    for (idx, turn) in turns.iter().enumerate().rev() {
        let cost = turn.text.len() + 16;
        let mandatory = idx == last_user_idx;
        if !mandatory && used + cost > budget && !kept.is_empty() {
            dropped = idx + 1;
            break;
        }
        used += cost;
        kept.push(turn);
    }
    kept.reverse();

    let mut body = String::new();
    if dropped > 0 {
        body.push_str(&format!("（更早的 {dropped} 条消息已省略）\n\n"));
    }
    for (i, turn) in kept.iter().enumerate() {
        let label = if turn.role == "assistant" {
            "Assistant"
        } else {
            "User"
        };
        body.push_str(label);
        body.push_str(": ");
        body.push_str(turn.text.trim());
        // 最后一条后面也留一个空行，免得 CLI 把它和别的东西粘一起
        body.push_str("\n\n");
        let _ = i;
    }

    Ok(format!("{head}{body}").trim_end().to_string())
}

/// Anthropic `/v1/messages` 的请求体（只声明我们要用的字段）。
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system: Option<SystemField>,
    #[serde(default)]
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub thinking: Option<Value>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub tools: Option<Value>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
}

/// `system` 既可以是字符串，也可以是内容块数组。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SystemField {
    Text(String),
    Blocks(Vec<Value>),
}

impl SystemField {
    pub fn flatten(&self) -> String {
        match self {
            SystemField::Text(s) => s.clone(),
            SystemField::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    #[serde(default)]
    pub content: Value,
}

impl AnthropicRequest {
    pub fn from_value(value: Value) -> Result<Self> {
        Ok(serde_json::from_value(value)?)
    }

    /// 客户端有没有要思考内容。Anthropic 的写法是 `thinking: {type:"enabled", …}`。
    /// 另外只要请求里带了 thinking 块（多轮回放），也当成要 —— 否则多轮会突然断掉。
    pub fn wants_thinking(&self) -> bool {
        let enabled = self
            .thinking
            .as_ref()
            .and_then(|t| t.get("type"))
            .and_then(Value::as_str)
            .map(|t| t != "disabled")
            .unwrap_or(false);
        let replaying = self
            .messages
            .iter()
            .any(|m| has_block_kind(&m.content, "thinking"));
        enabled || replaying
    }

    pub fn into_unified(self) -> UnifiedRequest {
        let wants_thinking = self.wants_thinking();
        UnifiedRequest {
            model: self.model,
            system: self.system.as_ref().map(SystemField::flatten),
            turns: self
                .messages
                .iter()
                .map(|m| Turn {
                    role: normalize_role(&m.role),
                    text: flatten_content(&m.content),
                })
                .collect(),
            stream: self.stream.unwrap_or(false),
            wants_thinking,
        }
    }
}

/// OpenAI `/v1/chat/completions` 的请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub tools: Option<Value>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream_options: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default)]
    pub name: Option<String>,
}

impl OpenAiRequest {
    pub fn from_value(value: Value) -> Result<Self> {
        Ok(serde_json::from_value(value)?)
    }

    pub fn wants_thinking(&self) -> bool {
        self.reasoning_effort.is_some()
    }

    /// 客户端要不要在流末尾补一条用量块（`stream_options.include_usage`）。
    pub fn stream_options_wants_usage(&self) -> bool {
        self.stream_options
            .as_ref()
            .and_then(|o| o.get("include_usage"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn into_unified(self) -> UnifiedRequest {
        let wants_thinking = self.wants_thinking();
        let mut system_parts: Vec<String> = Vec::new();
        let mut turns: Vec<Turn> = Vec::new();
        for m in &self.messages {
            let text = flatten_content(&m.content);
            let role = normalize_role(&m.role);
            if role == "system" {
                if !text.trim().is_empty() {
                    system_parts.push(text);
                }
            } else if role == "tool" {
                // 客户端工具的结果：当上下文文本带上，别丢
                turns.push(Turn {
                    role: "user".into(),
                    text: format!(
                        "[工具结果{}] {}",
                        m.name
                            .as_deref()
                            .map(|n| format!(" {n}"))
                            .unwrap_or_default(),
                        text.trim()
                    ),
                });
            } else {
                turns.push(Turn { role, text });
            }
        }
        UnifiedRequest {
            model: self.model,
            system: if system_parts.is_empty() {
                None
            } else {
                Some(system_parts.join("\n\n"))
            },
            turns,
            stream: self.stream.unwrap_or(false),
            wants_thinking,
        }
    }
}

/// 角色归一：只有 `assistant` 算助手，`system` / `developer` 归系统提示，`tool` 单独认，
/// 其余都当用户。（Anthropic 那边系统提示在 `system` 字段里，不会走到这里。）
fn normalize_role(role: &str) -> String {
    match role {
        "assistant" => "assistant".to_string(),
        "system" | "developer" => "system".to_string(),
        "tool" => "tool".to_string(),
        _ => "user".to_string(),
    }
}

/// 内容可能是字符串、块数组、或者 null。统一压成文本。
pub fn flatten_content(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            parts.push(t.to_string());
                        }
                    }
                    Some("thinking") | Some("redacted_thinking") => {
                        // 客户端回放的思考：CLI 那边不需要，但别让它变成噪声
                        if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                            parts.push(format!("[思考] {t}"));
                        }
                    }
                    Some("tool_use") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        parts.push(format!(
                            "[工具调用 {name}({})]",
                            serde_json::to_string(&input).unwrap_or_default()
                        ));
                    }
                    Some("tool_result") => {
                        let inner = block.get("content").cloned().unwrap_or(Value::Null);
                        parts.push(format!("[工具结果] {}", flatten_content(&inner).trim()));
                    }
                    Some("image") => parts.push("[图片已省略]".into()),
                    Some(other) => parts.push(format!("[{other}]")),
                    None => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            parts.push(t.to_string());
                        }
                    }
                }
            }
            parts.join("")
        }
        other => other.to_string(),
    }
}

fn has_block_kind(content: &Value, kind: &str) -> bool {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some(kind))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_request_with_thinking_and_tools_becomes_one_prompt() {
        let raw = json!({
            "model": "gpt-5.3-codex",
            "system": [{"type": "text", "text": "你是助手"}],
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "max_tokens": 1000,
            "stream": true,
            "messages": [
                {"role": "user", "content": "帮我看看这个函数"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "先看看代码", "signature": "skip_thought_signature_validator"},
                    {"type": "text", "text": "我先读文件。"},
                    {"type": "tool_use", "name": "read_file", "input": {"path": "a.rs"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "content": "fn main() {}"},
                    {"type": "text", "text": "继续"}
                ]}
            ]
        });
        let req = AnthropicRequest::from_value(raw).unwrap();
        assert!(req.wants_thinking());
        let unified = req.into_unified();
        assert!(unified.stream);
        assert_eq!(unified.model.as_deref(), Some("gpt-5.3-codex"));

        let prompt = build_prompt(&unified, 10_000).unwrap();
        assert!(prompt.starts_with("<system>\n你是助手\n</system>\n\n"));
        assert!(prompt.contains("User: 帮我看看这个函数"));
        assert!(prompt.contains(
            "Assistant: [思考] 先看看代码我先读文件。[工具调用 read_file({\"path\":\"a.rs\"})]"
        ));
        assert!(prompt.contains("User: [工具结果] fn main() {}继续"));
        assert!(!prompt.contains("已省略"));
    }

    #[test]
    fn anthropic_request_tolerates_string_system_and_plain_content() {
        let raw = json!({
            "system": "简单系统提示",
            "messages": [{"role": "user", "content": "你好"}]
        });
        let req = AnthropicRequest::from_value(raw).unwrap();
        assert!(!req.wants_thinking());
        let unified = req.into_unified();
        assert_eq!(unified.system.as_deref(), Some("简单系统提示"));
        assert_eq!(
            build_prompt(&unified, 1000).unwrap(),
            "<system>\n简单系统提示\n</system>\n\nUser: 你好"
        );
    }

    #[test]
    fn empty_request_is_rejected_not_sent_to_the_cli() {
        let raw = json!({"messages": []});
        let req = AnthropicRequest::from_value(raw).unwrap();
        let err = build_prompt(&req.into_unified(), 1000).unwrap_err();
        assert!(matches!(err, Error::EmptyPrompt), "{err:?}");

        let raw = json!({"messages": [{"role": "user", "content": "   "}]});
        let req = AnthropicRequest::from_value(raw).unwrap();
        assert!(matches!(
            build_prompt(&req.into_unified(), 1000),
            Err(Error::EmptyPrompt)
        ));
    }

    #[test]
    fn openai_request_lifts_system_and_keeps_tool_results() {
        let raw = json!({
            "model": "auto",
            "stream": true,
            "reasoning_effort": "high",
            "messages": [
                {"role": "system", "content": "系统甲"},
                {"role": "system", "content": "系统乙"},
                {"role": "user", "content": "第一个问题"},
                {"role": "assistant", "content": "第一个回答"},
                {"role": "tool", "name": "shell", "content": "total 0"},
                {"role": "user", "content": [{"type": "text", "text": "接着来"}]}
            ]
        });
        let req = OpenAiRequest::from_value(raw).unwrap();
        assert!(req.wants_thinking());
        let unified = req.into_unified();
        assert_eq!(unified.system.as_deref(), Some("系统甲\n\n系统乙"));
        assert_eq!(unified.turns.len(), 4);
        assert_eq!(unified.turns[2].text, "[工具结果 shell] total 0");
        assert_eq!(unified.last_user_text(), Some("接着来"));

        let prompt = build_prompt(&unified, 10_000).unwrap();
        assert!(prompt.contains("Assistant: 第一个回答"));
        assert!(prompt.ends_with("User: 接着来"));
    }

    #[test]
    fn history_is_trimmed_from_the_oldest_but_keeps_system_and_last_question() {
        let mut req = UnifiedRequest {
            model: None,
            system: Some("S".into()),
            turns: Vec::new(),
            stream: false,
            wants_thinking: false,
        };
        for i in 0..40 {
            req.turns.push(Turn {
                role: "user".into(),
                text: format!("问题{i}"),
            });
            req.turns.push(Turn {
                role: "assistant".into(),
                text: "答".repeat(200),
            });
        }
        req.turns.push(Turn {
            role: "user".into(),
            text: "最后的问题".into(),
        });

        let prompt = build_prompt(&req, 2000).unwrap();
        assert!(prompt.starts_with("<system>\nS\n</system>"));
        assert!(prompt.contains("已省略"));
        assert!(prompt.ends_with("User: 最后的问题"));
        assert!(!prompt.contains("问题0"));
        assert!(prompt.len() < 4000, "预算没生效，长度 {}", prompt.len());
    }

    #[test]
    fn a_very_long_last_question_still_goes_through() {
        let req = UnifiedRequest {
            model: None,
            system: None,
            turns: vec![Turn {
                role: "user".into(),
                text: "长".repeat(5000),
            }],
            stream: false,
            wants_thinking: false,
        };
        let prompt = build_prompt(&req, 100).unwrap();
        assert!(prompt.len() > 5000, "最后一条不能被预算砍掉");
    }

    #[test]
    fn thinking_replay_alone_counts_as_wanting_thinking() {
        // 有些客户端第二轮不带 thinking 字段，只把上一轮的思考块贴回来；
        // 这时候不能再把思考块吞掉，否则多轮会突然断掉。
        let raw = json!({
            "messages": [
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "嗯", "signature": "x"}]},
                {"role": "user", "content": "继续"}
            ]
        });
        let req = AnthropicRequest::from_value(raw).unwrap();
        assert!(req.wants_thinking());
    }
}
