// Haze 的 session JWT 存过两个地方：
//   新版（~1.3.x+）写 WebKit LocalStorage 的 `haze.auth.session`（登录/刷新都落这里），
//   老版写登录钥匙串 `haze.auth.session`(+.1 截段)。
// 2026-09-26 实测：app 重新登录只更新了 LocalStorage，钥匙串那份停在 9-24 已过期。
// 读法：LocalStorage 优先（活的那份），钥匙串兜底。
// haze provider 查额度用。
// 规矩：只读，永不自己 refresh（refresh 轮换整对、把 app 挤下线）。
use std::path::PathBuf;
use std::process::Command;

/// Extract the bearer token from the keychain payload. Split out of
/// [`haze_token`] so the two-piece JSON拼接 logic has a pure unit test
/// without needing a real Keychain.
fn session_token(part0: &str, part1: &str) -> Result<String, String> {
    for candidate in [format!("{part0}{part1}"), part0.to_string()] {
        if let Ok(sess) = serde_json::from_str::<serde_json::Value>(&candidate) {
            if let Some(t) = sess["accessToken"].as_str() {
                return Ok(t.to_string());
            }
        }
    }
    Err("session 里没有 accessToken".into())
}

pub fn haze_token() -> Result<String, String> {
    match haze_localstorage_token() {
        Ok(token) => Ok(token),
        Err(ls_err) => haze_keychain_token()
            .map_err(|kc_err| format!("localstorage: {ls_err}; keychain: {kc_err}")),
    }
}

fn haze_keychain_token() -> Result<String, String> {
    let grab = |acct: &str| -> Result<String, String> {
        let out = Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                "ai.legionedge.haze",
                "-a",
                acct,
                "-w",
            ])
            .output()
            .map_err(|e| format!("security exec: {e}"))?;
        if !out.status.success() {
            return Err(format!("keychain:{acct}"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let part0 = grab("haze.auth.session")?;
    let part1 = grab("haze.auth.session.1").unwrap_or_default();
    session_token(&part0, &part1)
}

/// 新版 Haze 把 session 写进 WKWebView 的 LocalStorage：
/// `~/Library/WebKit/ai.legionedge.haze/WebsiteData/Default/<hash>/<hash>/LocalStorage/localstorage.sqlite3`
/// origin 目录名是哈希（不固定），浅层遍历按 mtime 取最新的一份。
/// 值是 UTF-16LE 编码的 session JSON（个别版本可能是 UTF-8，两条路都试）。
fn haze_localstorage_token() -> Result<String, String> {
    let home = std::env::var_os("HOME").ok_or("no HOME")?;
    let base = PathBuf::from(home).join("Library/WebKit/ai.legionedge.haze/WebsiteData/Default");
    let mut candidates = Vec::new();
    collect_localstorage(&base, 0, &mut candidates);
    if candidates.is_empty() {
        return Err("WebKit localstorage.sqlite3 不存在".into());
    }
    // 最新写入的优先——搬家过 origin 的话老副本可能还在
    candidates.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    let mut errors = Vec::new();
    for db in candidates.into_iter().rev() {
        match read_session_from_sqlite(&db) {
            Ok(token) => return Ok(token),
            Err(e) => errors.push(format!("{}: {e}", db.display())),
        }
    }
    Err(errors.join(" | "))
}

fn collect_localstorage(dir: &std::path::Path, depth: u8, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n == "localstorage.sqlite3")
        {
            out.push(path);
        } else if path.is_dir() {
            collect_localstorage(&path, depth + 1, out);
        }
    }
}

fn read_session_from_sqlite(db: &std::path::Path) -> Result<String, String> {
    // `sqlite3` CLI is part of macOS；`hex(value)` 把 BLOB 原样十六进制吐出，
    // 避免 blob 里有非文本字节污染输出。
    let out = Command::new("sqlite3")
        .arg(db)
        .arg("SELECT hex(value) FROM ItemTable WHERE key='haze.auth.session'")
        .output()
        .map_err(|e| format!("sqlite3 exec: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "sqlite3: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let hex_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if hex_str.is_empty() {
        return Err("没有 haze.auth.session 键".into());
    }
    let bytes = hex_decode(&hex_str).ok_or("hex 解码失败")?;
    // WebKit localStorage 值是 UTF-16LE（实测 `7B 00` = '{'）。
    // 个别版本可能直接存 UTF-8，解不出 JSON 时退回按 UTF-8 读。
    let utf16: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let utf16_text = String::from_utf16_lossy(&utf16);
    if let Ok(token) = session_token(&utf16_text, "") {
        return Ok(token);
    }
    let utf8_text = String::from_utf8_lossy(&bytes).into_owned();
    session_token(&utf8_text, "").map_err(|e| format!("session 解码失败: {e}"))
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let digit = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    bytes
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? * 16 + digit(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::session_token;
    use serde_json::json;

    #[test]
    fn parses_a_split_session_json() {
        let full = json!({ "accessToken": "abc.def.ghi" }).to_string();
        let mid = full.len() / 2;
        let (part0, part1) = full.split_at(mid);
        assert_eq!(session_token(part0, part1).unwrap(), "abc.def.ghi");
    }

    #[test]
    fn parses_a_single_piece_session() {
        let full = json!({ "accessToken": "tok123" }).to_string();
        assert_eq!(session_token(&full, "").unwrap(), "tok123");
    }

    #[test]
    fn no_access_token_field_is_an_error() {
        let body = json!({ "refreshToken": "r" }).to_string();
        assert!(session_token(&body, "").is_err());
    }
}
