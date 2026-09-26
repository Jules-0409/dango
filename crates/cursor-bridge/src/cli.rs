//! 和 Cursor CLI 打交道的那一层：找可执行文件、拼参数、解析它的 `stream-json` 事件。
//!
//! 事件形状是拿本机 `agent 2026.09.08` 实测出来的（`--print --output-format stream-json
//! --stream-partial-output`）：
//!
//! ```text
//! {"type":"system","subtype":"init","apiKeySource":"login","model":"Auto","session_id":"…"}
//! {"type":"user","message":{…}}                       ← 回显我们给的 prompt，忽略
//! {"type":"thinking","subtype":"delta","text":"…","timestamp_ms":…}
//! {"type":"thinking","subtype":"completed"}
//! {"type":"assistant","message":{…"text":"1"},…,"timestamp_ms":…}   ← 增量碎片，有 timestamp_ms
//! …
//! {"type":"assistant","message":{…"text":"1+1 等于 2。"}}            ← 完整正文，**没有** timestamp_ms
//! {"type":"result","subtype":"success","result":"1+1 等于 2。","usage":{…}}
//! ```
//!
//! 也就是说正文会被喂两遍（先碎片、后整段），去重是必须的，见 [`crate::turn`]。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{Error, Result};

/// CLI 报回来的 token 用量。字段名就是 CLI 的 camelCase。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(rename = "inputTokens", default)]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens", default)]
    pub output_tokens: u64,
    #[serde(rename = "cacheReadTokens", default)]
    pub cache_read_tokens: u64,
    #[serde(rename = "cacheWriteTokens", default)]
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Vec<ContentPart>,
}

impl AssistantMessage {
    /// 把 text 段拼起来；非 text 段（工具调用等）在 v1 里只记不想转。
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.content {
            if part.kind == "text" {
                if let Some(t) = &part.text {
                    out.push_str(t);
                }
            }
        }
        out
    }

    /// 非 text 段的类型名，用来在日志里说明「这一轮还有别的东西」。
    pub fn non_text_kinds(&self) -> Vec<String> {
        self.content
            .iter()
            .filter(|p| p.kind != "text")
            .map(|p| p.kind.clone())
            .collect()
    }
}

/// CLI 的一行 stream-json。未知类型落到 [`Event::Unknown`]，不让它把整轮搞挂。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    System {
        #[serde(default)]
        subtype: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default, rename = "apiKeySource")]
        api_key_source: Option<String>,
    },
    User {
        #[serde(default)]
        message: Option<Value>,
    },
    Thinking {
        #[serde(default)]
        subtype: Option<String>,
        #[serde(default)]
        text: Option<String>,
        /// 增量事件才有；完整的（如果有）没有。
        #[serde(default)]
        timestamp_ms: Option<i64>,
    },
    Assistant {
        message: AssistantMessage,
        /// 增量碎片才有；整段正文没有。
        #[serde(default)]
        timestamp_ms: Option<i64>,
    },
    Result {
        #[serde(default)]
        subtype: Option<String>,
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        is_error: Option<bool>,
        #[serde(default)]
        usage: Option<Usage>,
        #[serde(default)]
        session_id: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::System { .. } => "system",
            Event::User { .. } => "user",
            Event::Thinking { .. } => "thinking",
            Event::Assistant { .. } => "assistant",
            Event::Result { .. } => "result",
            Event::Unknown => "unknown",
        }
    }
}

/// 解析一行。认识的形状出错时退化成 `Unknown` 而不是报错 —— CLI 换版本不该把桥打死。
pub fn parse_line(line: &str) -> Option<Event> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Some(Event::Unknown),
    };
    Some(serde_json::from_value(value).unwrap_or(Event::Unknown))
}

/// 一次调用的请求参数。
#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub workspace: PathBuf,
    pub trust: bool,
    /// 用 API key 认证（`--api-key`）；不填就用 CLI 当前登录态。
    pub api_key: Option<String>,
}

/// CLI 可执行文件的位置。
#[derive(Debug, Clone)]
pub struct AgentCmd {
    pub bin: PathBuf,
}

impl AgentCmd {
    /// 优先级：`CURSOR_BRIDGE_AGENT_BIN` → PATH 上的 `cursor-agent` / `agent` → `~/.local/bin/agent`。
    pub fn locate() -> Result<Self> {
        if let Some(p) = std::env::var_os("CURSOR_BRIDGE_AGENT_BIN") {
            let p = PathBuf::from(p);
            if is_executable(&p) {
                return Ok(Self { bin: p });
            }
        }
        let mut tried = Vec::new();
        for candidate in ["cursor-agent", "agent"] {
            if let Some(found) = which(candidate) {
                return Ok(Self { bin: found });
            }
            tried.push(PathBuf::from(candidate));
        }
        if let Some(home) = home_dir() {
            let p = home.join(".local/bin/agent");
            if is_executable(&p) {
                return Ok(Self { bin: p });
            }
            tried.push(p);
        }
        Err(Error::AgentNotFound { tried })
    }

    /// 拼出一次调用的参数（不含 argv[0]）。
    pub fn args(&self, req: &TurnRequest) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "--print".into(),
            "--output-format".into(),
            "stream-json".into(),
            // 不加这个就只有整段、没有碎片，流式就假了
            "--stream-partial-output".into(),
        ];
        if req.trust {
            args.push("--trust".into());
        }
        if let Some(mode) = &req.mode {
            args.push("--mode".into());
            args.push(mode.into());
        }
        args.push("--workspace".into());
        args.push(req.workspace.clone().into_os_string());
        if let Some(model) = &req.model {
            args.push("--model".into());
            args.push(model.into());
        }
        if let Some(key) = &req.api_key {
            args.push("--api-key".into());
            args.push(key.into());
        }
        args.push("--".into());
        args.push(req.prompt.clone().into());
        args
    }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// 极简 which：只按 PATH 找，不理会 PATHEXT 之类的 Windows 规矩。
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// 本机实测抓下来的事件行（`agent 2026.09.08`）。测试和文档共用一份，别再手抄。
#[cfg(test)]
pub mod probe {
    /// `agent --print --output-format stream-json --stream-partial-output --mode ask --trust`
    /// 跑一句「用一句话回答：1+1 等于几？」的完整输出。
    pub const LINES: &[&str] = &[
        r#"{"type":"system","subtype":"init","apiKeySource":"login","cwd":"/private/tmp/cursorprobe","session_id":"397eb3a8-9975-4426-8447-80879db995e0","model":"Auto","permissionMode":"default"}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"用一句话回答：1+1 等于几？"}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0"}"#,
        r#"{"type":"thinking","subtype":"delta","text":"用户在询问 1+1 的结果。","session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302801}"#,
        r#"{"type":"thinking","subtype":"delta","text":"\n\n1+1 等于 2。","session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"thinking","subtype":"completed","session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"1"}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"+"}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"1"}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":" "}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"等于"}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":" "}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"2"}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"。"}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0","timestamp_ms":1789708302806}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"1+1 等于 2。"}]},"session_id":"397eb3a8-9975-4426-8447-80879db995e0"}"#,
        r#"{"type":"result","subtype":"success","duration_ms":4501,"duration_api_ms":4501,"is_error":false,"result":"1+1 等于 2。","session_id":"397eb3a8-9975-4426-8447-80879db995e0","request_id":"db0ff094-a255-415a-a860-19461075fd15","usage":{"inputTokens":5851,"outputTokens":56,"cacheReadTokens":7808,"cacheWriteTokens":0}}"#,
    ];
}

#[cfg(test)]
mod tests {
    use super::probe::LINES as REAL_LINES;
    use super::*;

    #[test]
    fn parses_the_real_event_shape() {
        let events: Vec<Event> = REAL_LINES.iter().filter_map(|l| parse_line(l)).collect();
        assert_eq!(events.len(), REAL_LINES.len());

        match &events[0] {
            Event::System {
                model,
                api_key_source,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("Auto"));
                assert_eq!(api_key_source.as_deref(), Some("login"));
            }
            other => panic!("第 1 行应是 system，实际 {}", other.kind()),
        }
        match &events[2] {
            Event::Thinking { subtype, text, .. } => {
                assert_eq!(subtype.as_deref(), Some("delta"));
                assert_eq!(text.as_deref(), Some("用户在询问 1+1 的结果。"));
            }
            other => panic!("第 3 行应是 thinking，实际 {}", other.kind()),
        }
        // 碎片有 timestamp_ms，整段没有 —— 这就是去重的依据（倒数第 2 行是整段）
        let assistant: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e, Event::Assistant { .. }))
            .collect();
        let (frags, full): (Vec<_>, Vec<_>) = assistant.iter().copied().partition(|e| {
            matches!(
                e,
                Event::Assistant {
                    timestamp_ms: Some(_),
                    ..
                }
            )
        });
        assert_eq!(full.len(), 1, "整段正文应恰好一条");
        assert!(frags.len() >= 2, "碎片应有多条");
        match full[0] {
            Event::Assistant { message, .. } => assert_eq!(message.text(), "1+1 等于 2。"),
            _ => unreachable!(),
        }
        match events.last().unwrap() {
            Event::Result {
                subtype,
                is_error,
                usage,
                result,
                ..
            } => {
                assert_eq!(subtype.as_deref(), Some("success"));
                assert_eq!(*is_error, Some(false));
                assert_eq!(result.as_deref(), Some("1+1 等于 2。"));
                let u = usage.as_ref().expect("result 里有 usage");
                assert_eq!(
                    u,
                    &Usage {
                        input_tokens: 5851,
                        output_tokens: 56,
                        cache_read_tokens: 7808,
                        cache_write_tokens: 0
                    }
                );
            }
            other => panic!("最后一行应是 result，实际 {}", other.kind()),
        }
    }

    #[test]
    fn unknown_and_broken_lines_do_not_kill_the_turn() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   \n").is_none());
        assert!(matches!(
            parse_line("not json at all"),
            Some(Event::Unknown)
        ));
        // 新事件类型：形状不认识也要活着
        assert!(matches!(
            parse_line(r#"{"type":"tool_call","name":"read"}"#),
            Some(Event::Unknown)
        ));
        assert!(matches!(
            parse_line(r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#),
            Some(Event::Assistant { .. })
        ));
    }

    #[test]
    fn usage_defaults_to_zero_when_cli_omits_it() {
        let ev = parse_line(r#"{"type":"result","subtype":"error","is_error":true}"#).unwrap();
        match ev {
            Event::Result {
                usage, is_error, ..
            } => {
                assert!(usage.is_none());
                assert_eq!(is_error, Some(true));
            }
            other => panic!("应为 result，实际 {}", other.kind()),
        }
    }

    #[test]
    fn args_put_the_prompt_last_and_honour_optional_flags() {
        let cmd = AgentCmd {
            bin: PathBuf::from("/bin/true"),
        };
        let req = TurnRequest {
            prompt: "你好".into(),
            model: Some("gpt-5.3-codex".into()),
            mode: Some("ask".into()),
            workspace: PathBuf::from("/tmp/ws"),
            trust: true,
            api_key: None,
        };
        let args: Vec<String> = cmd
            .args(&req)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--print");
        assert!(args
            .windows(2)
            .any(|w| w == ["--output-format", "stream-json"]));
        assert!(args.windows(2).any(|w| w == ["--mode", "ask"]));
        assert!(args.windows(2).any(|w| w == ["--model", "gpt-5.3-codex"]));
        assert!(args.windows(2).any(|w| w == ["--workspace", "/tmp/ws"]));
        assert!(!args.iter().any(|a| a == "--api-key"));
        // prompt 是最后一个位置参数，前面用 -- 挡着，防止它以 - 开头被当成 flag
        assert_eq!(args[args.len() - 1], "你好");
        assert_eq!(args[args.len() - 2], "--");
    }
}
