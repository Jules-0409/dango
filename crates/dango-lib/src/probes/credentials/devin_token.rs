use std::fs;

use super::get_home_dir;
use super::vscdb::read_vscdb_items;

/// Retrieve Devin token:
/// 1. From Devin CLI `credentials.toml`
/// 2. From Devin Desktop `state.vscdb` (read in-process with bundled rusqlite)
pub fn load_devin_token() -> Result<(String, String), String> {
    let home = get_home_dir().ok_or("Cannot locate user home directory")?;

    // Check Devin CLI credentials.toml candidates
    let cli_candidates = [
        home.join(".config").join("devin").join("credentials.toml"),
        home.join(".codeium")
            .join("windsurf")
            .join("credentials.toml"),
        home.join("AppData")
            .join("Roaming")
            .join("devin")
            .join("credentials.toml"),
        home.join("Library")
            .join("Application Support")
            .join("devin")
            .join("credentials.toml"),
    ];

    for path in &cli_candidates {
        if path.exists() {
            if let Ok(content) = fs::read_to_string(path) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("windsurf_api_key") || trimmed.starts_with("api_key") {
                        if let Some((_, v)) = trimmed.split_once('=') {
                            let clean = v.trim().trim_matches('"').trim_matches('\'');
                            if !clean.is_empty() {
                                return Ok((
                                    clean.to_string(),
                                    "Devin CLI credentials".to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // Check Devin Desktop state.vscdb candidates
    let db_candidates = [
        home.join("Library")
            .join("Application Support")
            .join("Devin")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"),
        home.join("Library")
            .join("Application Support")
            .join("Windsurf")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"),
        home.join("AppData")
            .join("Roaming")
            .join("Devin")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"),
        home.join(".config")
            .join("Devin")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"),
    ];

    let mut db_errors: Vec<String> = Vec::new();

    for db_path in &db_candidates {
        if !db_path.exists() {
            continue;
        }

        let items = match read_vscdb_items(db_path, &["windsurfAuthStatus"]) {
            Ok(items) => items,
            Err(e) => {
                db_errors.push(e);
                continue;
            }
        };

        let json_str = match items.get("windsurfAuthStatus") {
            Some(s) => s,
            // 这个库里没有 Devin 登录态，继续看下一个候选路径
            None => continue,
        };

        match serde_json::from_str::<serde_json::Value>(json_str) {
            Ok(v) => {
                if let Some(key) = v.get("apiKey").and_then(|k| k.as_str()) {
                    if !key.is_empty() {
                        return Ok((key.to_string(), "Devin Desktop session".to_string()));
                    }
                }
            }
            Err(e) => db_errors.push(format!(
                "cred.devin_windsurf_parse: path={} err={}",
                db_path.display(),
                e
            )),
        }
    }

    let mut err = "devin.no_session".to_string();
    if !db_errors.is_empty() {
        err = format!(
            "devin.no_session: state.vscdb errors: {}",
            db_errors.join("; ")
        );
    }
    Err(err)
}
