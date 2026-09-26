//! `antigravity-bridge login` —— 一趟完整的 Google OAuth 回环登录：
//! 浏览器开授权页 → 回环收 code → 换 token → 账号落进 `~/.antigravity-bridge/accounts/`。
//!
//! client_id / secret 照旧不硬编码：先扫本机二进制（见 `oauth.rs`），
//! 没有官方客户端的机器上由 `ANTIGRAVITY_OAUTH_CLIENTS` 环境变量补上：
//! `key|client_id|client_secret|可选标签`（多条用 `;` 分隔）。
//!
//! redirect_uri 用回环地址 `http://127.0.0.1:<动态端口>/oauth2callback` ——
//! Google 对「已安装的桌面应用」客户端允许任意端口的 loopback 回调。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::oauth::{self, mask_email};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// 官方客户端申请的那套 scope（和 PROTOCOL.md 实测值一致）。
const SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/// 等回调的超时：人要在浏览器里点完登录，120 秒够用了。
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(120);

/// 登录跑完的结果：账号文件路径 + 邮箱（调用方拿去 rescan 池子 / 展示）。
pub struct LoginOutcome {
    pub email: String,
    pub account_path: std::path::PathBuf,
}

pub async fn run(client: &reqwest::Client) -> Result<String, String> {
    run_full(client).await.map(|outcome| {
        format!(
            "账号已添加：{} → {}",
            mask_email(&outcome.email),
            outcome.account_path.display()
        )
    })
}

/// `run` 的返回体版本：桥内 `/control/login` 拿到邮箱后要顺手 rescan 池子。
pub async fn run_full(client: &reqwest::Client) -> Result<LoginOutcome, String> {
    let candidates = login_candidates().await?;
    let client_id = candidates.client_ids.first().cloned().ok_or(
        "没拿到任何 OAuth client_id（没扫到官方二进制，也没配 ANTIGRAVITY_OAUTH_CLIENTS）",
    )?;

    // 起回环监听 → 拼授权链接 → 开浏览器 → 等回调，一气呵成。
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("回环监听起不来: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("读监听端口失败: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth2callback");
    let state = uuid::Uuid::new_v4().to_string();
    let auth_url = format!(
        "{AUTH_URL}?client_id={client_id}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={state}",
        url_encode(&redirect_uri),
        url_encode(&SCOPES.join(" ")),
    );

    println!("正在打开浏览器完成 Google 授权…");
    println!("如果没弹出来，手动访问：\n{auth_url}\n");
    open_browser(&auth_url);

    let code = wait_for_code(listener, &state).await?;
    println!("拿到授权码，正在换 token…");

    let grant = exchange_code_with_candidates(client, &code, &redirect_uri, &candidates).await?;
    let claims = decode_id_token(&grant.id_token)?;
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .ok_or("id_token 里没有 email 字段")?
        .to_string();
    let name = claims
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let path = write_account(&email, &name, &grant)?;
    Ok(LoginOutcome {
        email,
        account_path: path,
    })
}

/// 登录用的候选凭据：本机二进制扫描 + `ANTIGRAVITY_OAUTH_CLIENTS` 环境变量并集。
async fn login_candidates() -> Result<oauth::OauthCandidates, String> {
    let mut candidates = oauth::load_oauth_candidates().await.unwrap_or_default();
    if let Some(spec) = std::env::var_os("ANTIGRAVITY_OAUTH_CLIENTS") {
        for entry in spec.to_string_lossy().split(';') {
            let parts: Vec<&str> = entry.split('|').collect();
            if parts.len() >= 3 {
                let id = parts[1].to_string();
                let secret = parts[2].to_string();
                if !candidates.client_ids.contains(&id) {
                    candidates.client_ids.push(id);
                }
                if !secret.is_empty() && !candidates.secrets.contains(&secret) {
                    candidates.secrets.push(secret);
                }
            }
        }
    }
    if candidates.client_ids.is_empty() {
        return Err(
            "没拿到 OAuth client 凭据：请安装官方 Antigravity 客户端，或用 \
             ANTIGRAVITY_OAUTH_CLIENTS=\"key|client_id|client_secret\" 提供一对"
                .into(),
        );
    }
    Ok(candidates)
}

/// 授权码交换：按「client_id × secret」候选逐个试，和 refresh 交换同一套重试哲学。
struct Grant {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_in: u64,
}

async fn exchange_code_with_candidates(
    client: &reqwest::Client,
    code: &str,
    redirect_uri: &str,
    candidates: &oauth::OauthCandidates,
) -> Result<Grant, String> {
    let mut secrets = candidates.secrets.clone();
    secrets.push(String::new());
    let mut last = String::new();
    for client_id in &candidates.client_ids {
        for secret in &secrets {
            let mut form = vec![
                ("grant_type", "authorization_code".to_string()),
                ("code", code.to_string()),
                ("redirect_uri", redirect_uri.to_string()),
                ("client_id", client_id.clone()),
            ];
            if !secret.is_empty() {
                form.push(("client_secret", secret.clone()));
            }
            let Ok(res) = client
                .post(oauth::TOKEN_URL)
                .form(&form)
                .timeout(Duration::from_secs(20))
                .send()
                .await
            else {
                continue;
            };
            let text = res.text().await.unwrap_or_default();
            let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if let (Some(access), Some(refresh)) = (
                json.get("access_token").and_then(|v| v.as_str()),
                json.get("refresh_token").and_then(|v| v.as_str()),
            ) {
                return Ok(Grant {
                    access_token: access.to_string(),
                    refresh_token: refresh.to_string(),
                    id_token: json
                        .get("id_token")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    expires_in: json
                        .get("expires_in")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(3600),
                });
            }
            last = json
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| "unknown".into());
        }
    }
    Err(format!(
        "所有 client 凭据组合都换不到 token（最后一次错误：{last}）"
    ))
}

/// 浏览器里收那一发 GET /oauth2callback?code=…&state=… —— 监听只认一个连接。
async fn wait_for_code(listener: TcpListener, want_state: &str) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("nonblocking: {e}"))?;
    let deadline = std::time::Instant::now() + CALLBACK_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .map_err(|e| format!("读回调请求失败: {e}"))?;
                // 浏览器还会发 favicon 请求 —— 不是我们的路径就礼貌地回个 404 继续等
                let ok_page = "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\n\r\n<!doctype html><meta charset=utf-8><body style=\"font-family:system-ui;display:grid;place-items:center;height:100vh;margin:0;background:#faf7f2\"><div style=\"text-align:center\"><h2>授权完成 ✅</h2><p>可以关掉这个页面，回到终端看结果。</p></div>";
                let not_found = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n";
                let Some(query) = request_line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|path| path.split_once('?').map(|(_, q)| q))
                else {
                    let _ = stream.write_all(not_found.as_bytes());
                    continue;
                };
                if !request_line.contains("/oauth2callback") {
                    let _ = stream.write_all(not_found.as_bytes());
                    continue;
                }
                let params: std::collections::HashMap<String, String> = query
                    .split('&')
                    .filter_map(|pair| {
                        let (k, v) = pair.split_once('=')?;
                        Some((k.to_string(), url_decode(v)))
                    })
                    .collect();
                let _ = stream.write_all(ok_page.as_bytes());
                let _ = stream.flush();
                if let Some(err) = params.get("error") {
                    return Err(format!("授权被拒：{err}"));
                }
                if params.get("state").map(String::as_str) != Some(want_state) {
                    return Err("state 不匹配，可能回调被串了 —— 重新 login".into());
                }
                return params
                    .get("code")
                    .cloned()
                    .ok_or_else(|| "回调里没有 code".to_string());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    return Err("等回调超时（120 秒）——重新 login".into());
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            Err(e) => return Err(format!("accept: {e}")),
        }
    }
}

/// 账号落盘：`accounts/<uuid>.json` + 更新 `accounts.json` 索引（0600，和既有读写同一套）。
fn write_account(email: &str, name: &str, grant: &Grant) -> Result<std::path::PathBuf, String> {
    let root = oauth::accounts_root().ok_or("找不到用户目录")?;
    let dir = root.join("accounts");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建账号目录失败: {e}"))?;
    let id = uuid::Uuid::new_v4().to_string();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let file = json!({
        "id": id,
        "email": email,
        "name": name,
        "token": {
            "access_token": grant.access_token,
            "refresh_token": grant.refresh_token,
            "expires_in": grant.expires_in,
            "expiry_timestamp": now_secs + grant.expires_in,
            "token_type": "Bearer",
            "email": email,
            "id_token": grant.id_token,
        },
        "disabled": false,
        "created_at": now_secs,
    });
    let path = dir.join(format!("{id}.json"));
    write_private(&path, serde_json::to_vec_pretty(&file).unwrap())?;

    // 索引：往 accounts.json 的数组里登记一份（缺失就新建），current 没指过就指向新来的。
    let index_path = root.join("accounts.json");
    let mut index: Value = std::fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({ "version": "2.0", "accounts": [] }));
    let entry = json!({
        "id": id,
        "email": email,
        "name": name,
        "disabled": false,
        "proxy_disabled": false,
        "created_at": now_secs,
    });
    if let Some(list) = index.get_mut("accounts").and_then(|v| v.as_array_mut()) {
        list.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
        list.push(entry);
    } else {
        index["accounts"] = json!([entry]);
    }
    if index.get("current_account_id").is_none() {
        index["current_account_id"] = json!(id);
    }
    write_private(&index_path, serde_json::to_vec_pretty(&index).unwrap())?;
    Ok(path)
}

fn write_private(path: &Path, bytes: Vec<u8>) -> Result<(), String> {
    std::fs::write(path, &bytes).map_err(|e| format!("写 {} 失败：{e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// id_token 是 JWT —— 解中间段 payload 拿 claims（email / name / sub）。
fn decode_id_token(id_token: &str) -> Result<Value, String> {
    let payload = id_token.split('.').nth(1).ok_or("id_token 不是 JWT 形状")?;
    let mut b64 = payload.replace('-', "+").replace('_', "/");
    while b64.len() % 4 != 0 {
        b64.push('=');
    }
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .map_err(|e| format!("id_token base64: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("id_token json: {e}"))
}

fn open_browser(url: &str) {
    let result = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
        .args(if cfg!(windows) {
            vec!["/c", "start", "", url]
        } else if cfg!(target_os = "macos") {
            vec!["-c", "open \"$1\"", "sh", url]
        } else {
            vec!["-c", "xdg-open \"$1\" 2>/dev/null || true", "sh", url]
        })
        .spawn();
    if let Err(e) = result {
        eprintln!("（开浏览器失败：{e}，请手动访问上面的链接）");
    }
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(hex);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_codec_round_trips() {
        let s = "http://127.0.0.1:8050/oauth2callback?x=a b&y=中文";
        assert_eq!(url_decode(&url_encode(s)), s);
    }

    #[test]
    fn decodes_a_jwt_payload() {
        // header.{email: x@y.z}.sig —— 只要 payload 能解出来就行
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"email":"x@y.z","name":"X"}"#);
        let token = format!("e30.{payload}.sig");
        let claims = decode_id_token(&token).unwrap();
        assert_eq!(claims["email"], "x@y.z");
    }
}
