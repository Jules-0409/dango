//! 模型清单：直接问 CLI 要（`agent --list-models`），不自己维护一份会过期的表。

use std::process::Stdio;

use tokio::process::Command;

use crate::cli::AgentCmd;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
}

/// 解析 `--list-models` 的文本输出：
///
/// ```text
/// Available models
///
/// auto - Auto (current, default)
/// gpt-5.3-codex-low - Codex 5.3 Low
/// ```
pub fn parse(text: &str) -> Vec<ModelInfo> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("Available models") {
                return None;
            }
            let (id, name) = line.split_once(" - ")?;
            let id = id.trim();
            if id.is_empty() || id.contains(' ') {
                return None;
            }
            Some(ModelInfo {
                id: id.to_string(),
                name: name.trim().to_string(),
            })
        })
        .collect()
}

/// 问一次 CLI。失败就返回错误，让调用方决定是空清单还是 500。
pub async fn list(cmd: &AgentCmd) -> Result<Vec<ModelInfo>> {
    let output = Command::new(&cmd.bin)
        .arg("--list-models")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(Error::Spawn)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let models = parse(&text);
    if models.is_empty() && !output.status.success() {
        return Err(Error::Upstream {
            message: format!(
                "`agent --list-models` 退出码 {:?}：{}",
                output.status.code(),
                crate::turn::compact_stderr(&String::from_utf8_lossy(&output.stderr))
            ),
        });
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机实测输出（截取前 10 个模型）。
    const REAL: &str = "Available models\n\nauto - Auto (current, default)\ngpt-5.3-codex-low - Codex 5.3 Low\ngpt-5.3-codex-low-fast - Codex 5.3 Low Fast\ngpt-5.3-codex - Codex 5.3\ngpt-5.3-codex-fast - Codex 5.3 Fast\ngpt-5.3-codex-high - Codex 5.3 High\ngpt-5.3-codex-high-fast - Codex 5.3 High Fast\ngpt-5.3-codex-xhigh - Codex 5.3 Extra High\ngpt-5.3-codex-xhigh-fast - Codex 5.3 Extra High Fast\ngpt-5.2 - GPT-5.2\n";

    #[test]
    fn parses_the_real_listing() {
        let models = parse(REAL);
        assert_eq!(models.len(), 10);
        assert_eq!(
            models[0],
            ModelInfo {
                id: "auto".into(),
                name: "Auto (current, default)".into()
            }
        );
        assert_eq!(models[1].id, "gpt-5.3-codex-low");
        assert_eq!(models[9].id, "gpt-5.2");
    }

    #[test]
    fn ignores_noise_lines() {
        let models = parse(
            "Available models\n\n某个没有分隔符的行\n\nweird - line - with - dashes\nok - Fine\n",
        );
        assert_eq!(
            models,
            vec![
                ModelInfo {
                    id: "weird".into(),
                    name: "line - with - dashes".into()
                },
                ModelInfo {
                    id: "ok".into(),
                    name: "Fine".into()
                },
            ]
        );
    }
}
