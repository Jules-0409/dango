// 用户手动粘贴的凭据，存在 dango 自己的钥匙串 service 下。
// 红线不变：vendor 的钥匙串项永远只读，这个槽是我们自己的东西——
// 读顺序是「手动槽优先，vendor 源兜底」，删掉手动槽就回到纯自动。
// token 只进 Keychain，不落盘、不进日志、不进响应体。

use std::io::Write as _;
use std::process::{Command, Stdio};

/// 我们自己的 service 名，跟 vendor 的钥匙串项完全隔离。
const SERVICE: &str = "dango";

/// 支持手动凭据的套餐。cursor 的认证是 token+auth_id 双段、
/// antigravity 是账号池——都不是一条 token 能接的，走引导不接槽。
/// 用户自己加的小球（`custom-*`）的 API Key 也放这里。
pub fn supports(plan_id: &str) -> bool {
    matches!(plan_id, "haze" | "devin" | "factory" | "dim" | "grok")
        || crate::settings::is_custom_plan_id(plan_id)
}

/// 粘贴进来的东西必须长得像 token（JWT / base64 / hex 家族）。
/// 含空白或引号的输入多半是连换行一起复制了——直接拒掉，
/// 也顺带保证它拼进 `security -i` 的命令行里没有注入面。
/// 例外：整段 session JSON（用户最可能连花括号一起拷）先提出
/// accessToken 再走同一套校验。
fn extract_token(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.starts_with('{') {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|_| "JSON 没解析出来".to_string())?;
        let token = value["accessToken"]
            .as_str()
            .ok_or_else(|| "JSON 里没有 accessToken".to_string())?;
        return Ok(token.to_string());
    }
    Ok(raw.to_string())
}

fn valid_token(token: &str) -> bool {
    (8..=4096).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~+/=-:".contains(&b))
}

fn find_item(plan_id: &str) -> Result<String, String> {
    find_item_in(SERVICE, plan_id)
}

fn find_item_in(service: &str, plan_id: &str) -> Result<String, String> {
    let out = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", plan_id, "-w"])
        .output()
        .map_err(|e| format!("security exec: {e}"))?;
    if !out.status.success() {
        return Err("unset".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 手动槽里有没有这个套餐的凭据（不读值，只看存在）。
pub fn has(plan_id: &str) -> bool {
    if !supports(plan_id) {
        return false;
    }
    [SERVICE].iter().any(|service| {
        Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", plan_id])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    })
}

/// 读手动槽的 token，供 provider 优先于 vendor 源使用。
pub fn get(plan_id: &str) -> Option<String> {
    if !supports(plan_id) {
        return None;
    }
    find_item(plan_id).ok().filter(|token| !token.is_empty())
}

/// 写入/更新手动凭据。
/// `security -i` 从 stdin 吃命令：token 走管道不进 argv，
/// 进程表和 `ps` 都看不到它（`add-generic-password -w` 直调会短暂暴露）。
pub fn set(plan_id: &str, token: &str) -> Result<(), String> {
    if !supports(plan_id) {
        return Err(format!("{plan_id} 不支持手动凭据"));
    }
    let token = extract_token(token)?;
    if !valid_token(&token) {
        return Err("凭据格式不对：只能包含字母数字和 ._~+/=-: ，长度 8-4096".into());
    }
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("security exec: {e}"))?;
    let cmd = format!(
        "add-generic-password -s {SERVICE} -a {plan_id} -l \"dango ({plan_id})\" -w {token} -U\n"
    );
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(cmd.as_bytes())
        .map_err(|e| format!("security stdin: {e}"))?;
    let status = child.wait().map_err(|e| format!("security wait: {e}"))?;
    if !status.success() {
        return Err("钥匙串写入失败".into());
    }
    Ok(())
}

/// 删除手动凭据，回到纯 vendor 读取。
pub fn delete(plan_id: &str) -> Result<(), String> {
    if !supports(plan_id) {
        return Err(format!("{plan_id} 不支持手动凭据"));
    }
    if !delete_in(SERVICE, plan_id)? {
        return Err("钥匙串删除失败".into());
    }
    Ok(())
}

/// 删掉某个 service 下的项；`Ok(false)` 表示本来就没有。
fn delete_in(service: &str, plan_id: &str) -> Result<bool, String> {
    Command::new("security")
        .args(["delete-generic-password", "-s", service, "-a", plan_id])
        .output()
        .map(|out| out.status.success())
        .map_err(|e| format!("security exec: {e}"))
}

#[cfg(test)]
mod tests {
    use super::{extract_token, valid_token};

    #[test]
    fn session_json_extracts_access_token() {
        let json = r#"{"accessToken":"abc.def-ghi","refreshToken":"x"}"#;
        assert_eq!(extract_token(json).unwrap(), "abc.def-ghi");
        assert!(extract_token(r#"{"refreshToken":"x"}"#).is_err());
        assert!(extract_token("{oops").is_err());
        assert_eq!(extract_token("plain_tok").unwrap(), "plain_tok");
    }

    #[test]
    fn token_charset_accepts_jwt_and_base64() {
        assert!(valid_token(
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc_-def="
        ));
        assert!(valid_token("da54abcd0123/=+:"));
    }

    #[test]
    fn token_charset_rejects_whitespace_and_quotes() {
        assert!(!valid_token("tok en"));
        assert!(!valid_token("tok\"en"));
        assert!(!valid_token("tok\nen"));
        assert!(!valid_token("短"));
        assert!(!valid_token("abc"));
    }
}
