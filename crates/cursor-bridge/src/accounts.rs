//! 日抛号的账号清单。
//!
//! 每个账号就是一个 Cursor API key（`agent --api-key` / `CURSOR_API_KEY` 那条路子）。
//! 为什么不用「每账号一个 `CURSOR_CONFIG_DIR`」那套：macOS 上 Cursor CLI 的登录凭据在
//! **登录钥匙串**里（固定条目 `cursor-access-token` / `cursor-refresh-token`，account 恒为
//! `cursor-user`），换配置目录根本隔离不了它 —— 实测 `CURSOR_CONFIG_DIR=/tmp/x agent status`
//! 照样显示原账号。也就是说第二种登录方式会直接覆盖掉用户自己的登录态。
//!
//! API key 就没这个问题：按请求传进去，CLI 的钥匙串一个字都不动。清单里没有账号时，
//! 桥回退到「CLI 当前登录态」，也就是用户自己 `agent login` 的那个号。
//!
//! 清单形状（`~/.cursor-bridge/accounts.json`，建议 0600）：
//!
//! ```json
//! {
//!   "accounts": [
//!     { "name": "day-1", "apiKey": "…" },
//!     { "name": "day-2", "apiKey": "…", "disabled": true }
//!   ]
//! }
//! ```

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
struct AccountsFile {
    #[serde(default)]
    accounts: Vec<AccountEntry>,
}

#[derive(Debug, Deserialize)]
struct AccountEntry {
    name: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    #[serde(default)]
    disabled: bool,
}

/// 读清单。文件不在 = 没有账号（不是错误，回退登录态）；文件坏了 = 报错，别静默降级。
pub fn load(path: &Path) -> Result<Vec<Account>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(Error::Io(err)),
    };
    let parsed: AccountsFile = serde_json::from_str(&text)?;
    let mut out = Vec::new();
    for entry in parsed.accounts {
        if entry.disabled {
            continue;
        }
        if entry.api_key.trim().is_empty() {
            continue;
        }
        out.push(Account {
            name: entry.name,
            api_key: entry.api_key.trim().to_string(),
        });
    }
    Ok(out)
}

/// 日志里只留指纹：前 4 位 + 后 4 位，中间打码。
pub fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "…".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// 轮换器：严格轮询。一个回合只用一个 key，不用排队等负载。
#[derive(Debug, Default)]
pub struct Pool {
    accounts: Vec<Account>,
    cursor: AtomicUsize,
}

impl Pool {
    pub fn new(accounts: Vec<Account>) -> Self {
        Self {
            accounts,
            cursor: AtomicUsize::new(0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn names(&self) -> Vec<&str> {
        self.accounts.iter().map(|a| a.name.as_str()).collect()
    }

    /// 下一个该用的账号；空池返回 None（调用方回退 CLI 登录态）。
    pub fn next(&self) -> Option<Account> {
        if self.accounts.is_empty() {
            return None;
        }
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % self.accounts.len();
        Some(self.accounts[idx].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cursor-bridge-test-{}-{}",
            std::process::id(),
            name
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn missing_file_means_no_accounts() {
        let path = std::env::temp_dir().join("cursor-bridge-test-does-not-exist.json");
        let _ = std::fs::remove_file(&path);
        assert!(load(&path).unwrap().is_empty());
    }

    #[test]
    fn disabled_and_blank_entries_are_skipped() {
        let path = write_temp(
            "accounts.json",
            r#"{"accounts":[
                {"name":"a","apiKey":"key-a"},
                {"name":"b","apiKey":"key-b","disabled":true},
                {"name":"c","apiKey":"   "},
                {"name":"d","apiKey":"key-d"}
            ]}"#,
        );
        let accounts = load(&path).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].name, "a");
        assert_eq!(accounts[1].name, "d");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn broken_file_is_an_error_not_a_silent_fallback() {
        let path = write_temp("broken.json", "{ 这不是 JSON");
        let err = load(&path).unwrap_err();
        assert!(matches!(err, Error::Json(_)), "{err:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pool_rotates_in_order_and_wraps() {
        let pool = Pool::new(vec![
            Account {
                name: "a".into(),
                api_key: "ka".into(),
            },
            Account {
                name: "b".into(),
                api_key: "kb".into(),
            },
        ]);
        let picked: Vec<String> = (0..5).map(|_| pool.next().unwrap().name).collect();
        assert_eq!(picked, ["a", "b", "a", "b", "a"]);
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.names(), ["a", "b"]);
    }

    #[test]
    fn empty_pool_yields_nothing_so_the_cli_login_is_used() {
        let pool = Pool::new(Vec::new());
        assert!(pool.is_empty());
        assert!(pool.next().is_none());
    }

    #[test]
    fn mask_keeps_only_the_edges() {
        assert_eq!(mask("sk-1234567890abcdef"), "sk-1…cdef");
        assert_eq!(mask("short"), "…");
    }
}
