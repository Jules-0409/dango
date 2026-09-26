use std::path::PathBuf;

use super::CursorAuth;

/// Locate state.vscdb for Cursor on macOS or Windows
fn find_cursor_vscdb_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            let p = PathBuf::from(home)
                .join("Library/Application Support/Cursor/User/globalStorage/state.vscdb");
            if p.exists() {
                return Some(p);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let p = PathBuf::from(appdata).join("Cursor\\User\\globalStorage\\state.vscdb");
            if p.exists() {
                return Some(p);
            }
        }
        if let Ok(userprofile) = std::env::var("USERPROFILE") {
            let p = PathBuf::from(userprofile)
                .join("AppData\\Roaming\\Cursor\\User\\globalStorage\\state.vscdb");
            if p.exists() {
                return Some(p);
            }
        }
    }

    None
}

/// Locate ~/.cursor/cli-config.json for Cursor user info
fn find_cursor_cli_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            let p = PathBuf::from(home).join(".cursor/cli-config.json");
            if p.exists() {
                return Some(p);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(userprofile) = std::env::var("USERPROFILE") {
            let p = PathBuf::from(userprofile).join(".cursor\\cli-config.json");
            if p.exists() {
                return Some(p);
            }
        }
    }

    None
}

pub(super) fn load_cursor_auth() -> Result<CursorAuth, String> {
    let mut token = String::new();
    let mut user_id = String::new();
    let mut auth_id = String::new();
    let mut email = None;
    let mut membership_type = None;
    let mut subscription_status = None;

    // 1. 从 cli-config.json 读取 userId 和 email
    if let Some(cfg_path) = find_cursor_cli_config_path() {
        if let Ok(content) = std::fs::read_to_string(cfg_path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(auth_info) = val.get("authInfo") {
                    if let Some(uid) = auth_info.get("userId") {
                        if let Some(uid_num) = uid.as_i64() {
                            user_id = uid_num.to_string();
                        } else if let Some(uid_str) = uid.as_str() {
                            user_id = uid_str.to_string();
                        }
                    }
                    if let Some(em) = auth_info.get("email").and_then(|v| v.as_str()) {
                        email = Some(em.to_string());
                    }
                }
            }
        }
    }

    // 2. 从 state.vscdb (SQLite) 读取 accessToken 与 会员状态
    //    统一用 bundled rusqlite 只读打开，macOS / Windows 行为一致：
    //    原来 macOS 走内联 python3、Windows 走 PowerShell + System.Data.SQLite，
    //    而普通 Windows 既没有那个程序集也不一定装了 python，失败后是静默的。
    let mut db_error: Option<String> = None;

    if let Some(db_path) = find_cursor_vscdb_path() {
        const CURSOR_KEYS: [&str; 7] = [
            "cursorAuth/accessToken",
            "cursorAuth/cachedEmail",
            "cursorAuth/stripeMembershipType",
            "cursorAuth/stripeSubscriptionStatus",
            "glass.lastSignedInAuthId",
            "cursorAuth/stripeMembershipAuthId",
            "adminSettings.cachedAuthId",
        ];

        match crate::probes::credentials::read_vscdb_items(&db_path, &CURSOR_KEYS) {
            Ok(parsed) => {
                if let Some(t) = parsed.get("cursorAuth/accessToken") {
                    token = t.clone();
                }
                if email.is_none() {
                    if let Some(e) = parsed.get("cursorAuth/cachedEmail") {
                        email = Some(e.clone());
                    }
                }
                membership_type = parsed.get("cursorAuth/stripeMembershipType").cloned();
                subscription_status = parsed.get("cursorAuth/stripeSubscriptionStatus").cloned();
                auth_id = [
                    "glass.lastSignedInAuthId",
                    "cursorAuth/stripeMembershipAuthId",
                    "adminSettings.cachedAuthId",
                ]
                .iter()
                .find_map(|k| parsed.get(*k).filter(|v| !v.is_empty()).cloned())
                .unwrap_or_default();
            }
            Err(e) => db_error = Some(e),
        }
    }

    // 3. Fallback: Keychain on macOS
    #[cfg(target_os = "macos")]
    {
        if token.is_empty() {
            if let Ok(output) = std::process::Command::new("security")
                .args(["find-generic-password", "-s", "cursor-access-token", "-w"])
                .output()
            {
                if output.status.success() {
                    token = String::from_utf8_lossy(&output.stdout).trim().to_string();
                }
            }
        }
    }

    if token.is_empty() {
        let mut msg = "cursor.no_token".to_string();
        if let Some(e) = db_error {
            msg = format!("cursor.no_token_db: {e}");
        }
        return Err(msg);
    }

    Ok(CursorAuth {
        token,
        user_id,
        auth_id,
        email,
        membership_type,
        subscription_status,
    })
}
