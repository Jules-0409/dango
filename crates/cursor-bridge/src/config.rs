//! 配置：全部来自环境变量，带 `CURSOR_BRIDGE_` 前缀。默认值面向「本机自己用」。
//!
//! | 变量 | 默认 | 说明 |
//! |---|---|---|
//! | `CURSOR_BRIDGE_BIND` | `127.0.0.1` | 监听地址；只有明确要对外才改 |
//! | `CURSOR_BRIDGE_PORT` | `8052` | 监听端口 |
//! | `CURSOR_BRIDGE_STATE_DIR` | `~/.cursor-bridge` | 桥自己的状态目录（日志、账号清单、工作区） |
//! | `CURSOR_BRIDGE_WORKSPACE` | `<state_dir>/workspace` | 给 CLI 的空工作区，别让 agent 乱翻你的仓库 |
//! | `CURSOR_BRIDGE_MODE` | `ask` | 透传给 `--mode`；`ask` = 只读问答 |
//! | `CURSOR_BRIDGE_TRUST` | `1` | 是否传 `--trust` |
//! | `CURSOR_BRIDGE_TIMEOUT_SECS` | `600` | 单回合上限，超时掐掉 CLI |
//! | `CURSOR_BRIDGE_KEEPALIVE_SECS` | `15` | SSE 心跳间隔 |
//! | `CURSOR_BRIDGE_AGENT_BIN` | 自动找 | Cursor CLI 可执行文件 |
//! | `CURSOR_BRIDGE_API_KEY` | 无 | 单个 API key；设了就优先于 CLI 登录态 |
//! | `CURSOR_BRIDGE_ACCOUNTS` | `<state_dir>/accounts.json` | 多账号清单（日抛号） |
//! | `CURSOR_BRIDGE_BACKEND` | `cli` | `cli` = 拉 CLI；`native` = 直接和 agent 服务说话 |
//! | `CURSOR_BRIDGE_API_ENDPOINT` | `https://api2.cursor.sh` | 换 access token 的端点 |
//! | `CURSOR_BRIDGE_AGENT_ENDPOINT` | `https://agentn.global.api5.cursor.sh` | agent 服务端点 |
//! | `CURSOR_BRIDGE_CLIENT_VERSION` | 照抄本机 CLI | 请求头里的 `x-cursor-client-version` |

use std::path::PathBuf;
use std::str::FromStr;

use crate::cli::home_dir;

/// 后端实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// 拉 `cursor-agent` 子进程，解析它的 stream-json（默认，见 [`crate::turn`]）。
    Cli,
    /// 直接和 `agent.v1.AgentService/Run` 说话（见 [`crate::native`]）。
    Native,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Cli => "cli",
            Backend::Native => "native",
        }
    }
}

/// 原生后端的默认上游。
pub const DEFAULT_API_ENDPOINT: &str = "https://api2.cursor.sh";
pub const DEFAULT_AGENT_ENDPOINT: &str = "https://agentn.global.api5.cursor.sh";

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub state_dir: PathBuf,
    pub workspace: PathBuf,
    pub mode: Option<String>,
    pub trust: bool,
    pub timeout_secs: u64,
    pub keepalive_secs: u64,
    pub agent_bin: Option<PathBuf>,
    pub single_api_key: Option<String>,
    pub accounts_file: PathBuf,
    /// 走哪条后端。
    pub backend: Backend,
    /// 换 access token 的端点（原生后端用）。
    pub api_endpoint: String,
    /// agent 服务端点（原生后端用）。
    pub agent_endpoint: String,
    /// 环境变量取值有问题时的说明（不静默吞掉）。
    pub notes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let state_dir = home_dir()
            .map(|h| h.join(".cursor-bridge"))
            .unwrap_or_else(|| PathBuf::from(".cursor-bridge"));
        Self {
            bind: "127.0.0.1".to_string(),
            port: 8052,
            workspace: state_dir.join("workspace"),
            accounts_file: state_dir.join("accounts.json"),
            state_dir,
            mode: Some("ask".to_string()),
            trust: true,
            timeout_secs: 600,
            keepalive_secs: 15,
            agent_bin: None,
            single_api_key: None,
            backend: Backend::Cli,
            api_endpoint: DEFAULT_API_ENDPOINT.to_string(),
            agent_endpoint: DEFAULT_AGENT_ENDPOINT.to_string(),
            notes: Vec::new(),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        let mut notes = Vec::new();

        if let Ok(v) = std::env::var("CURSOR_BRIDGE_BIND") {
            if v.trim().is_empty() {
                notes.push("CURSOR_BRIDGE_BIND 是空的，按默认 127.0.0.1 处理".into());
            } else {
                cfg.bind = v.trim().to_string();
            }
        }
        if let Some(port) = parsed::<u16>("CURSOR_BRIDGE_PORT", &mut notes) {
            cfg.port = port;
        }
        if let Some(dir) = std::env::var_os("CURSOR_BRIDGE_STATE_DIR") {
            cfg.state_dir = PathBuf::from(dir);
            cfg.workspace = cfg.state_dir.join("workspace");
            cfg.accounts_file = cfg.state_dir.join("accounts.json");
        }
        if let Some(dir) = std::env::var_os("CURSOR_BRIDGE_WORKSPACE") {
            cfg.workspace = PathBuf::from(dir);
        }
        if let Some(dir) = std::env::var_os("CURSOR_BRIDGE_ACCOUNTS") {
            cfg.accounts_file = PathBuf::from(dir);
        }
        if let Ok(v) = std::env::var("CURSOR_BRIDGE_MODE") {
            let v = v.trim().to_string();
            cfg.mode = if v.is_empty() || v == "none" || v == "off" {
                None
            } else {
                Some(v)
            };
        }
        if let Some(trust) = parsed_bool("CURSOR_BRIDGE_TRUST", &mut notes) {
            cfg.trust = trust;
        }
        if let Some(secs) = parsed::<u64>("CURSOR_BRIDGE_TIMEOUT_SECS", &mut notes) {
            if secs == 0 {
                notes.push("CURSOR_BRIDGE_TIMEOUT_SECS=0 没意义，按 600 处理".into());
            } else {
                cfg.timeout_secs = secs;
            }
        }
        if let Some(secs) = parsed::<u64>("CURSOR_BRIDGE_KEEPALIVE_SECS", &mut notes) {
            cfg.keepalive_secs = secs;
        }
        if let Some(bin) = std::env::var_os("CURSOR_BRIDGE_AGENT_BIN") {
            cfg.agent_bin = Some(PathBuf::from(bin));
        }
        if let Ok(key) = std::env::var("CURSOR_BRIDGE_API_KEY") {
            let key = key.trim().to_string();
            if !key.is_empty() {
                cfg.single_api_key = Some(key);
            }
        }
        if let Ok(v) = std::env::var("CURSOR_BRIDGE_BACKEND") {
            match v.trim().to_ascii_lowercase().as_str() {
                "cli" | "agent" | "" => cfg.backend = Backend::Cli,
                "native" | "direct" => cfg.backend = Backend::Native,
                other => notes.push(format!(
                    "CURSOR_BRIDGE_BACKEND={other} 不认识（只有 cli / native），按 cli 处理"
                )),
            }
        }
        if let Some(v) = non_empty_env("CURSOR_BRIDGE_API_ENDPOINT") {
            cfg.api_endpoint = v;
        }
        if let Some(v) = non_empty_env("CURSOR_BRIDGE_AGENT_ENDPOINT") {
            cfg.agent_endpoint = v;
        }
        cfg.notes = notes;
        cfg
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }
}

fn parsed<T: FromStr>(key: &str, notes: &mut Vec<String>) -> Option<T> {
    match std::env::var(key) {
        Ok(v) => match v.trim().parse::<T>() {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                notes.push(format!("{key}={v} 读不出来，按默认处理"));
                None
            }
        },
        Err(_) => None,
    }
}

fn parsed_bool(key: &str, notes: &mut Vec<String>) -> Option<bool> {
    let v = std::env::var(key).ok()?;
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        other => {
            notes.push(format!("{key}={other} 不是布尔值，按默认处理"));
            None
        }
    }
}

/// 取一个非空环境变量；空串当没设。
fn non_empty_env(key: &str) -> Option<String> {
    let v = std::env::var(key).ok()?;
    let v = v.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 环境变量是进程级的，涉及它的测试必须串起来跑。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const KEYS: &[&str] = &[
        "CURSOR_BRIDGE_BIND",
        "CURSOR_BRIDGE_PORT",
        "CURSOR_BRIDGE_STATE_DIR",
        "CURSOR_BRIDGE_WORKSPACE",
        "CURSOR_BRIDGE_ACCOUNTS",
        "CURSOR_BRIDGE_MODE",
        "CURSOR_BRIDGE_TRUST",
        "CURSOR_BRIDGE_TIMEOUT_SECS",
        "CURSOR_BRIDGE_KEEPALIVE_SECS",
        "CURSOR_BRIDGE_AGENT_BIN",
        "CURSOR_BRIDGE_API_KEY",
        "CURSOR_BRIDGE_BACKEND",
        "CURSOR_BRIDGE_API_ENDPOINT",
        "CURSOR_BRIDGE_AGENT_ENDPOINT",
        "CURSOR_BRIDGE_CLIENT_VERSION",
    ];

    fn with_env<F: FnOnce()>(pairs: &[(&str, &str)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for key in KEYS {
            std::env::remove_var(key);
        }
        for (k, v) in pairs {
            std::env::set_var(k, v);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        for key in KEYS {
            std::env::remove_var(key);
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn defaults_are_local_and_read_only() {
        with_env(&[], || {
            let cfg = Config::from_env();
            assert_eq!(cfg.bind, "127.0.0.1");
            assert_eq!(cfg.port, 8052);
            assert_eq!(cfg.mode.as_deref(), Some("ask"));
            assert!(cfg.trust);
            assert_eq!(cfg.timeout_secs, 600);
            assert_eq!(cfg.keepalive_secs, 15);
            assert!(cfg.notes.is_empty(), "默认值不该产生说明：{:?}", cfg.notes);
            assert!(cfg.workspace.ends_with("workspace"));
            assert_eq!(cfg.accounts_file.file_name().unwrap(), "accounts.json");
        });
    }

    #[test]
    fn state_dir_moves_workspace_and_accounts_with_it() {
        with_env(&[("CURSOR_BRIDGE_STATE_DIR", "/tmp/cb-test")], || {
            let cfg = Config::from_env();
            assert_eq!(cfg.workspace, PathBuf::from("/tmp/cb-test/workspace"));
            assert_eq!(
                cfg.accounts_file,
                PathBuf::from("/tmp/cb-test/accounts.json")
            );
        });
    }

    #[test]
    fn explicit_workspace_and_accounts_win_over_state_dir() {
        with_env(
            &[
                ("CURSOR_BRIDGE_STATE_DIR", "/tmp/cb-test"),
                ("CURSOR_BRIDGE_WORKSPACE", "/tmp/ws"),
                ("CURSOR_BRIDGE_ACCOUNTS", "/tmp/acc.json"),
            ],
            || {
                let cfg = Config::from_env();
                assert_eq!(cfg.workspace, PathBuf::from("/tmp/ws"));
                assert_eq!(cfg.accounts_file, PathBuf::from("/tmp/acc.json"));
            },
        );
    }

    #[test]
    fn bad_values_fall_back_with_a_note_instead_of_panicking() {
        with_env(
            &[
                ("CURSOR_BRIDGE_PORT", "不是数字"),
                ("CURSOR_BRIDGE_TRUST", "maybe"),
                ("CURSOR_BRIDGE_TIMEOUT_SECS", "0"),
            ],
            || {
                let cfg = Config::from_env();
                assert_eq!(cfg.port, 8052);
                assert!(cfg.trust);
                assert_eq!(cfg.timeout_secs, 600);
                assert_eq!(cfg.notes.len(), 3, "{:?}", cfg.notes);
            },
        );
    }

    #[test]
    fn mode_off_means_no_mode_flag() {
        with_env(&[("CURSOR_BRIDGE_MODE", "none")], || {
            assert_eq!(Config::from_env().mode, None);
        });
        with_env(&[("CURSOR_BRIDGE_MODE", "default")], || {
            assert_eq!(Config::from_env().mode.as_deref(), Some("default"));
        });
    }

    #[test]
    fn empty_api_key_is_treated_as_absent() {
        with_env(&[("CURSOR_BRIDGE_API_KEY", "   ")], || {
            assert_eq!(Config::from_env().single_api_key, None);
        });
        with_env(&[("CURSOR_BRIDGE_API_KEY", "key-123")], || {
            assert_eq!(
                Config::from_env().single_api_key.as_deref(),
                Some("key-123")
            );
        });
    }

    #[test]
    fn backend_defaults_to_cli_and_switches_on_request() {
        with_env(&[], || {
            let cfg = Config::from_env();
            assert_eq!(cfg.backend, Backend::Cli);
            assert_eq!(cfg.api_endpoint, DEFAULT_API_ENDPOINT);
            assert_eq!(cfg.agent_endpoint, DEFAULT_AGENT_ENDPOINT);
        });
        with_env(&[("CURSOR_BRIDGE_BACKEND", "native")], || {
            assert_eq!(Config::from_env().backend, Backend::Native);
        });
        // 大小写和空串都不该把人绊倒
        with_env(&[("CURSOR_BRIDGE_BACKEND", " NATIVE ")], || {
            assert_eq!(Config::from_env().backend, Backend::Native);
        });
        with_env(&[("CURSOR_BRIDGE_BACKEND", "")], || {
            assert_eq!(Config::from_env().backend, Backend::Cli);
        });
        // 不认识的值退默认，并留一句说明
        with_env(&[("CURSOR_BRIDGE_BACKEND", "gpu")], || {
            let cfg = Config::from_env();
            assert_eq!(cfg.backend, Backend::Cli);
            assert_eq!(cfg.notes.len(), 1);
            assert!(cfg.notes[0].contains("gpu"), "{:?}", cfg.notes);
        });
    }

    #[test]
    fn endpoints_can_be_overridden() {
        with_env(
            &[
                ("CURSOR_BRIDGE_API_ENDPOINT", "http://127.0.0.1:9999"),
                ("CURSOR_BRIDGE_AGENT_ENDPOINT", "http://127.0.0.1:9998"),
            ],
            || {
                let cfg = Config::from_env();
                assert_eq!(cfg.api_endpoint, "http://127.0.0.1:9999");
                assert_eq!(cfg.agent_endpoint, "http://127.0.0.1:9998");
            },
        );
    }
}
