//! 跑一个回合：把 CLI 的事件流归成 [`Step`]，顺带解决「正文喂两遍」的去重问题。
//!
//! 去重规则（见 [`crate::cli`] 里实测的事件形状）：带 `timestamp_ms` 的 assistant 事件是
//! 碎片，直接发；不带的是整段正文（CLI 最后会补一条完整的），如果它以上一次累计的正文
//! 开头，就只发多出来的尾巴。这样无论 CLI 以后只发碎片、只发整段、还是两个都发，客户端
//! 拿到的正文都恰好一份。

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::cli::{AgentCmd, Event, TurnRequest, Usage};
use crate::error::{Error, Result};

/// 一个回合里对外抛出的事件。
#[derive(Debug, Clone)]
pub enum Step {
    /// CLI 起来了，知道用哪个模型、哪个 session。
    Init {
        model: Option<String>,
        session_id: Option<String>,
    },
    /// 思考增量。
    Thinking(String),
    /// 思考结束（Anthropic 侧要在这里关掉 thinking 块）。
    ThinkingDone,
    /// 正文增量（已去重）。
    Text(String),
    /// 值得记一笔但不用发给客户端的事（比如遇到工具调用）。
    Note(String),
    /// 收尾，带上整轮的结果。
    Done(TurnOutcome),
}

#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    /// 这一轮发给客户端的正文总量。
    pub text: String,
    /// 这一轮发给客户端的思考总量。
    pub thinking: String,
    pub usage: Usage,
    pub is_error: bool,
    pub session_id: Option<String>,
    pub model: Option<String>,
    /// CLI 的 stderr 尾巴，出问题时用来解释原因。
    pub stderr_tail: String,
    pub saw_event: bool,
    pub notes: Vec<String>,
}

/// 把一件增量/整段文本并进累计值，返回**这次该发出去的部分**。
///
/// `is_delta` 为真表示它是碎片；否则是整段，需要按前缀去重。
fn absorb(acc: &mut String, text: &str, is_delta: bool) -> Absorbed {
    if text.is_empty() {
        return Absorbed::Nothing;
    }
    if is_delta {
        acc.push_str(text);
        return Absorbed::Emit(text.to_string());
    }
    if text == acc {
        // 整段和已发出的完全一样：CLI 又补了一遍，什么都不用发
        return Absorbed::Nothing;
    }
    if let Some(tail) = text.strip_prefix(acc.as_str()) {
        let tail = tail.to_string();
        *acc = text.to_string();
        return if tail.is_empty() {
            Absorbed::Nothing
        } else {
            Absorbed::Emit(tail)
        };
    }
    if acc.starts_with(text) {
        // 收到的整段比已发出的短：已经在流里发过了
        return Absorbed::Nothing;
    }
    Absorbed::Mismatch
}

#[derive(Debug, PartialEq, Eq)]
enum Absorbed {
    Emit(String),
    Nothing,
    /// 整段和累计对不上（既不是前缀也没包含）——不重复发，记一笔。
    Mismatch,
}

/// 一轮的状态机。纯逻辑，方便用真实抓包直接测。
#[derive(Debug, Default)]
pub struct TurnState {
    outcome: TurnOutcome,
    finished: bool,
}

impl TurnState {
    pub fn outcome(&self) -> &TurnOutcome {
        &self.outcome
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// 吃一个事件，吐出一串对外的 [`Step`]。
    pub fn apply(&mut self, ev: &Event) -> Vec<Step> {
        let mut steps = Vec::new();
        match ev {
            Event::System {
                model, session_id, ..
            } => {
                self.outcome.model = model.clone();
                self.outcome.session_id = session_id.clone();
                steps.push(Step::Init {
                    model: model.clone(),
                    session_id: session_id.clone(),
                });
            }
            Event::Thinking {
                subtype,
                text,
                timestamp_ms,
            } => {
                let is_delta = timestamp_ms.is_some();
                if subtype.as_deref() == Some("completed") {
                    steps.push(Step::ThinkingDone);
                } else if let Some(text) = text {
                    self.outcome.saw_event = true;
                    match absorb(&mut self.outcome.thinking, text, is_delta) {
                        Absorbed::Emit(part) => steps.push(Step::Thinking(part)),
                        Absorbed::Nothing => {}
                        Absorbed::Mismatch => {
                            let note = "思考整段与增量对不上，已跳过重复内容".to_string();
                            self.outcome.notes.push(note.clone());
                            steps.push(Step::Note(note));
                        }
                    }
                }
            }
            Event::Assistant {
                message,
                timestamp_ms,
            } => {
                self.outcome.saw_event = true;
                let kinds = message.non_text_kinds();
                if !kinds.is_empty() {
                    let note = format!("CLI 给了非文本内容：{}", kinds.join(","));
                    self.outcome.notes.push(note.clone());
                    steps.push(Step::Note(note));
                }
                let text = message.text();
                match absorb(&mut self.outcome.text, &text, timestamp_ms.is_some()) {
                    Absorbed::Emit(part) => steps.push(Step::Text(part)),
                    Absorbed::Nothing => {}
                    Absorbed::Mismatch => {
                        let note = "正文整段与增量对不上，已跳过重复内容".to_string();
                        self.outcome.notes.push(note.clone());
                        steps.push(Step::Note(note));
                    }
                }
            }
            Event::Result {
                subtype,
                result,
                is_error,
                usage,
                session_id,
            } => {
                self.outcome.saw_event = true;
                if let Some(u) = usage {
                    self.outcome.usage = u.clone();
                }
                if let Some(sid) = session_id {
                    self.outcome.session_id = Some(sid.clone());
                }
                self.outcome.is_error = is_error.unwrap_or(false)
                    || subtype.as_deref().is_some_and(|s| s.starts_with("error"));
                if let Some(result) = result {
                    match absorb(&mut self.outcome.text, result, false) {
                        Absorbed::Emit(part) => steps.push(Step::Text(part)),
                        Absorbed::Nothing => {}
                        Absorbed::Mismatch => {
                            let note = "result 正文与增量对不上，已跳过重复内容".to_string();
                            self.outcome.notes.push(note.clone());
                            steps.push(Step::Note(note));
                        }
                    }
                }
                self.finished = true;
                steps.push(Step::Done(self.outcome.clone()));
            }
            Event::User { .. } | Event::Unknown => {}
        }
        steps
    }
}

/// 跑一轮，边跑边把 [`Step`] 交给 `on_step`。
pub async fn run_turn<F>(
    cmd: &AgentCmd,
    req: &TurnRequest,
    timeout_secs: u64,
    mut on_step: F,
) -> Result<TurnOutcome>
where
    F: FnMut(Step) + Send,
{
    let mut command = Command::new(&cmd.bin);
    command
        .args(cmd.args(req))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 客户端断开时别把 CLI 留在后台空跑
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(Error::Spawn)?;
    let stdout = child.stdout.take().expect("stdout 已设为 piped");
    let stderr = child.stderr.take().expect("stderr 已设为 piped");

    // stderr 单独收：未登录、模型不可用这类原因都只在 stderr 上
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
            // 只留尾巴，别让报错把内存撑爆
            if buf.len() > 8192 {
                let cut = buf.len() - 4096;
                // 从 cut 往后找第一个合法字符边界（`ceil_char_boundary` 要 Rust 1.91+，这条线是 1.80）
                let cut = (cut..buf.len())
                    .find(|&i| buf.is_char_boundary(i))
                    .unwrap_or(buf.len());
                buf.replace_range(..cut, "");
            }
        }
        buf
    });

    let work = async {
        let mut state = TurnState::default();
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await.map_err(Error::Io)? {
            if let Some(ev) = crate::cli::parse_line(&line) {
                for step in state.apply(&ev) {
                    on_step(step);
                }
            }
        }
        Ok::<TurnState, Error>(state)
    };

    let pumped = tokio::select! {
        r = work => r,
        _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(Error::Timeout { secs: timeout_secs })
        }
    };

    let status = child.wait().await.ok();
    let stderr_tail = stderr_task.await.unwrap_or_default();

    let mut state = pumped?;
    state.outcome.stderr_tail = stderr_tail.clone();
    state.outcome.saw_event |= status.is_some();

    if !state.outcome.saw_event {
        return Err(Error::EmptyTurn {
            code: status.and_then(|s| s.code()),
            stderr: compact_stderr(&stderr_tail),
        });
    }
    if !state.outcome.is_error && state.outcome.text.is_empty() {
        // 有事件没正文：和 antigravity-bridge 那边一个判断口径，不算成功
        return Err(Error::EmptyTurn {
            code: status.and_then(|s| s.code()),
            stderr: compact_stderr(&stderr_tail),
        });
    }
    Ok(state.outcome)
}

/// 把 CLI 的 stderr 压成一行，用来解释「为什么这轮没成」。
pub fn compact_stderr(stderr: &str) -> String {
    let joined: String = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if joined.len() > 400 {
        let mut cut = 400;
        while !joined.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &joined[..cut])
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::probe::LINES;

    fn run_lines(lines: &[&str]) -> (TurnOutcome, Vec<Step>) {
        let mut state = TurnState::default();
        let mut steps = Vec::new();
        for line in lines {
            if let Some(ev) = crate::cli::parse_line(line) {
                steps.extend(state.apply(&ev));
            }
        }
        (state.outcome().clone(), steps)
    }

    fn emitted_text(steps: &[Step]) -> String {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn emitted_thinking(steps: &[Step]) -> String {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Thinking(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 真实抓包：碎片 + 整段，正文必须恰好出现一次。
    #[test]
    fn real_probe_lines_yield_text_exactly_once() {
        let (outcome, steps) = run_lines(LINES);
        assert_eq!(emitted_text(&steps), "1+1 等于 2。");
        assert_eq!(outcome.text, "1+1 等于 2。");
        assert_eq!(
            emitted_thinking(&steps),
            "用户在询问 1+1 的结果。\n\n1+1 等于 2。"
        );
        assert_eq!(outcome.usage.input_tokens, 5851);
        assert_eq!(outcome.usage.output_tokens, 56);
        assert_eq!(outcome.usage.cache_read_tokens, 7808);
        assert!(!outcome.is_error);
        assert!(
            outcome.notes.is_empty(),
            "不该有异常记录：{:?}",
            outcome.notes
        );
        assert!(matches!(steps.last(), Some(Step::Done(_))));
        // 思考结束标记要在正文之前出现
        let think_done = steps
            .iter()
            .position(|s| matches!(s, Step::ThinkingDone))
            .unwrap();
        let first_text = steps
            .iter()
            .position(|s| matches!(s, Step::Text(_)))
            .unwrap();
        assert!(think_done <= first_text);
    }

    /// 只发碎片不补整段（CLI 换了行为）：正文一样不多不少。
    #[test]
    fn fragments_alone_are_enough() {
        let lines: Vec<&str> = LINES
            .iter()
            .copied()
            .filter(|l| !(l.contains("\"type\":\"assistant\"") && !l.contains("timestamp_ms")))
            .collect();
        let (outcome, steps) = run_lines(&lines);
        assert_eq!(emitted_text(&steps), "1+1 等于 2。");
        assert_eq!(outcome.text, "1+1 等于 2。");
    }

    /// 只发整段不发碎片（比如没开 --stream-partial-output）：靠 result 补齐。
    #[test]
    fn full_message_only_still_emits_text_once() {
        let lines: Vec<&str> = LINES
            .iter()
            .copied()
            .filter(|l| !(l.contains("\"type\":\"assistant\"") && l.contains("timestamp_ms")))
            .collect();
        let (outcome, steps) = run_lines(&lines);
        assert_eq!(emitted_text(&steps), "1+1 等于 2。");
        assert_eq!(outcome.text, "1+1 等于 2。");
    }

    /// 两段内容对不上时宁可不发，也不重复。
    #[test]
    fn mismatched_full_message_is_not_re_emitted() {
        let lines = [
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"前半段"}]},"timestamp_ms":1}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"完全不同的内容"}]}}"#,
        ];
        let (outcome, steps) = run_lines(&lines);
        assert_eq!(emitted_text(&steps), "前半段");
        assert_eq!(outcome.notes.len(), 1);
        assert!(outcome.notes[0].contains("对不上"));
    }

    #[test]
    fn error_result_is_marked_and_keeps_partial_text() {
        let lines = [
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"写到一半"}]},"timestamp_ms":1}"#,
            r#"{"type":"result","subtype":"error","is_error":true,"result":"写到一半"}"#,
        ];
        let (outcome, steps) = run_lines(&lines);
        assert!(outcome.is_error);
        assert_eq!(outcome.text, "写到一半");
        assert!(matches!(steps.last(), Some(Step::Done(o)) if o.is_error));
    }

    #[test]
    fn usage_defaults_when_result_has_none() {
        let lines = [r#"{"type":"result","subtype":"success","result":"hi"}"#];
        let (outcome, steps) = run_lines(&lines);
        assert_eq!(outcome.usage, Usage::default());
        assert_eq!(emitted_text(&steps), "hi");
    }

    #[test]
    fn unknown_events_are_ignored() {
        let lines = [
            r#"{"type":"whatever","x":1}"#,
            "not json",
            r#"{"type":"result","subtype":"success","result":"ok"}"#,
        ];
        let (outcome, _steps) = run_lines(&lines);
        assert_eq!(outcome.text, "ok");
        assert!(!outcome.notes.iter().any(|n| n.contains("whatever")));
    }
}
