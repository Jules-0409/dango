//! Qoder 凭据：读 `~/.qoder-bridge/auth.json`，必要时换成新的 job token。
//!
//! 凭据文件（0600）字段：`jobToken / jobRefreshToken / accessToken / expiresAt / machineID /
//! userID / name / email`。策略（刻意避免打扰用户桌面端凭据）：
//!
//! 1. `jobToken` 没过期（提前 5 分钟算过期）→ 直接用；
//! 2. 过期但有 `accessToken` → `POST {openapi}/api/v1/me/jobToken` 换新的，写回文件；
//!    **不调** `/api/v1/deviceToken/refresh`（那会轮换用户桌面端的凭据）；
//! 3. 都不可用 → 配了 `refresh_command` 就跑它（30s 超时）再重读；否则报「请刷新 Qoder 凭据」。
//!
//! 并发互斥照 `upstream.rs` 的 `minting`：双检 + 锁，别并发重复换 token。
//! 日志与错误里永远不出现 token / 签名，只出现「jobToken 过期 / accessToken 不可用」这种形态。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::server::LogFn;
use crate::types::UpstreamError;

use super::{CLIENT_ID, CLIENT_TYPE, OPENAPI_COSY_VERSION};

const USER_AGENT: &str = "antigravity-bridge-qoder";
/// job token 提前这么久算过期
const JOB_TOKEN_MARGIN_MS: i64 = 5 * 60 * 1000;
/// `refresh_command` 的超时
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// 假时钟（测试注入）。
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 凭据文件。未知字段用 `flatten` 兜住，写回时原样带上（别把用户的 `pat` 弄丢）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AuthFile {
    #[serde(rename = "jobToken", default)]
    pub job_token: String,
    #[serde(rename = "jobRefreshToken", default)]
    pub job_refresh_token: String,
    #[serde(rename = "accessToken", default)]
    pub access_token: String,
    #[serde(rename = "expiresAt", default)]
    pub expires_at: i64,
    #[serde(rename = "machineID", default)]
    pub machine_id: String,
    #[serde(rename = "userID", default)]
    pub user_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    /// 认不出的键（例如 `pat`）——保留，不参与判断
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl AuthFile {
    /// job token 现在还能用吗？`expires_at == 0` 视为「没有过期信息」——既然文件里写着 jobToken，
    /// 就先用它（真过期了请求会 401，上层会失效重取）。
    pub fn job_token_valid(&self, now_ms: i64) -> bool {
        !self.job_token.is_empty()
            && (self.expires_at == 0 || self.expires_at > now_ms + JOB_TOKEN_MARGIN_MS)
    }
}

/// 凭据仓库：负责读文件、换 token、写回、互斥。
pub struct AuthStore {
    path: PathBuf,
    refresh_command: Option<String>,
    log: LogFn,
    cached: std::sync::Mutex<Option<AuthFile>>,
    minting: tokio::sync::Mutex<()>,
    now: Clock,
}

impl AuthStore {
    pub fn new(
        path: PathBuf,
        refresh_command: Option<String>,
        log: LogFn,
        now: Option<Clock>,
    ) -> Self {
        Self {
            path,
            refresh_command,
            log,
            cached: std::sync::Mutex::new(None),
            minting: tokio::sync::Mutex::new(()),
            now: now.unwrap_or_else(|| Arc::new(crate::types::now_millis)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn now_ms(&self) -> i64 {
        (self.now)()
    }

    fn read_file(&self) -> Option<AuthFile> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        match serde_json::from_str::<AuthFile>(&text) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                (self.log)(format!(
                    "Qoder 凭据文件读不动（{}）：{err}",
                    self.path.display()
                ));
                None
            }
        }
    }

    fn cache_get(&self) -> Option<AuthFile> {
        self.cached.lock().ok().and_then(|guard| guard.clone())
    }

    fn cache_set(&self, creds: &AuthFile) {
        if let Ok(mut guard) = self.cached.lock() {
            *guard = Some(creds.clone());
        }
    }

    fn cache_clear(&self) {
        if let Ok(mut guard) = self.cached.lock() {
            *guard = None;
        }
    }

    /// 同步快照：内存缓存优先，其次读文件。给 `Session::identity`（同步签名）用。
    pub fn snapshot(&self) -> Option<AuthFile> {
        if let Some(cached) = self.cache_get() {
            return Some(cached);
        }
        let loaded = self.read_file();
        if let Some(creds) = &loaded {
            self.cache_set(creds);
        }
        loaded
    }

    /// 请求上游前拿一份可用凭据；必要时换/刷新。失败返回清晰错误（绝不回显 token）。
    pub async fn credentials(
        &self,
        client: &reqwest::Client,
        openapi: &str,
    ) -> Result<AuthFile, UpstreamError> {
        let now = self.now_ms();
        if let Some(creds) = self.snapshot() {
            if creds.job_token_valid(now) {
                return self.ensure_identity(client, openapi, creds).await;
            }
        }

        let _guard = self.minting.lock().await;
        // 双检：等锁期间别人可能已经换好了
        let now = self.now_ms();
        if let Some(creds) = self.snapshot() {
            if creds.job_token_valid(now) {
                return self.ensure_identity(client, openapi, creds).await;
            }
        }

        if let Some(creds) = self.snapshot() {
            if !creds.access_token.is_empty() {
                match self.exchange_job_token(client, openapi, &creds).await {
                    Ok(next) => {
                        self.persist(&next);
                        return self.ensure_identity(client, openapi, next).await;
                    }
                    Err(err) => (self.log)(format!(
                        "Qoder：用 accessToken 换 job token 失败（{}），改看 refresh_command",
                        err.reason
                    )),
                }
            }
        }

        if let Some(command) = self.refresh_command.clone() {
            self.run_refresh_command(&command).await;
            self.cache_clear();
            if let Some(creds) = self.snapshot() {
                if creds.job_token_valid(self.now_ms()) {
                    return self.ensure_identity(client, openapi, creds).await;
                }
            }
        }

        Err(UpstreamError::http(
            401,
            "qoder_credentials",
            format!(
                "Qoder 凭据不可用：请刷新 {}（jobToken 过期，且没有可用的 accessToken / refresh_command）",
                self.path.display()
            ),
        ))
    }

    /// 上游报 401/403（「文件说没过期、上游说过期」）时的强刷：跳过有效期判断，
    /// 直接走 accessToken → refresh_command。
    pub async fn credentials_forced(
        &self,
        client: &reqwest::Client,
        openapi: &str,
    ) -> Result<AuthFile, UpstreamError> {
        let _guard = self.minting.lock().await;
        self.cache_clear();
        if let Some(creds) = self.read_file() {
            if !creds.access_token.is_empty() {
                if let Ok(next) = self.exchange_job_token(client, openapi, &creds).await {
                    self.persist(&next);
                    return self.ensure_identity(client, openapi, next).await;
                }
            }
        }
        if let Some(command) = self.refresh_command.clone() {
            self.run_refresh_command(&command).await;
            self.cache_clear();
            if let Some(creds) = self.read_file() {
                if creds.job_token_valid(self.now_ms()) {
                    return self.ensure_identity(client, openapi, creds).await;
                }
            }
        }
        Err(UpstreamError::http(
            401,
            "qoder_credentials",
            format!("Qoder 凭据刷新失败：请手动刷新 {}", self.path.display()),
        ))
    }

    /// uid 必须真实，否则网关报 105 Login expired。文件里没有就现查 `/api/v1/userinfo` 并写回。
    async fn ensure_identity(
        &self,
        client: &reqwest::Client,
        openapi: &str,
        creds: AuthFile,
    ) -> Result<AuthFile, UpstreamError> {
        if !creds.user_id.is_empty() {
            return Ok(creds);
        }
        let url = format!("{openapi}/api/v1/userinfo");
        let response = client
            .get(&url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {}", creds.job_token))
            .header("Cosy-Version", OPENAPI_COSY_VERSION)
            .header("Cosy-ClientType", CLIENT_TYPE)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|err| UpstreamError::network("qoder_userinfo", err.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(UpstreamError::http(
                status.as_u16(),
                "qoder_userinfo",
                crate::server::truncate(&text, 200),
            ));
        }
        let info: Value = serde_json::from_str(&text)
            .map_err(|err| UpstreamError::network("qoder_userinfo_parse", err.to_string()))?;
        let uid = info
            .get("id")
            .or_else(|| info.get("uid"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if uid.is_empty() {
            return Err(UpstreamError::http(
                401,
                "qoder_userinfo",
                "userinfo 没给 uid（网关会报 105 Login expired）",
            ));
        }
        let mut next = creds;
        next.user_id = uid;
        if let Some(name) = info.get("name").and_then(Value::as_str) {
            next.name = name.to_string();
        }
        if let Some(email) = info.get("email").and_then(Value::as_str) {
            next.email = email.to_string();
        }
        self.persist(&next);
        Ok(next)
    }

    async fn exchange_job_token(
        &self,
        client: &reqwest::Client,
        openapi: &str,
        creds: &AuthFile,
    ) -> Result<AuthFile, UpstreamError> {
        let url = format!("{openapi}/api/v1/me/jobToken");
        let response = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {}", creds.access_token))
            .header("Cosy-Version", OPENAPI_COSY_VERSION)
            .header("Cosy-ClientType", CLIENT_TYPE)
            .header("User-Agent", USER_AGENT)
            .json(&json!({ "clientId": CLIENT_ID }))
            .send()
            .await
            .map_err(|err| UpstreamError::network("qoder_jobtoken", err.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(UpstreamError::http(
                status.as_u16(),
                "qoder_jobtoken",
                crate::server::truncate(&text, 200),
            ));
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|err| UpstreamError::network("qoder_jobtoken_parse", err.to_string()))?;
        let token = parsed
            .get("token")
            .or_else(|| parsed.get("device_token"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if token.is_empty() {
            return Err(UpstreamError::http(
                502,
                "qoder_jobtoken",
                "换 job token 的响应里没有 token 字段",
            ));
        }
        let mut next = creds.clone();
        next.job_token = token;
        if let Some(refresh) = parsed.get("refresh_token").and_then(Value::as_str) {
            next.job_refresh_token = refresh.to_string();
        }
        next.expires_at = resolve_expiry(&parsed, self.now_ms());
        Ok(next)
    }

    /// 原子写回（先写 .tmp 再 rename），并尽量把权限压到 0600。
    fn persist(&self, creds: &AuthFile) {
        self.cache_set(creds);
        if let Some(dir) = self.path.parent() {
            if let Err(err) = std::fs::create_dir_all(dir) {
                (self.log)(format!("Qoder 凭据目录建不了（{}）：{err}", dir.display()));
                return;
            }
        }
        let body = match serde_json::to_string_pretty(creds) {
            Ok(body) => body,
            Err(err) => {
                (self.log)(format!("Qoder 凭据序列化失败：{err}"));
                return;
            }
        };
        let tmp = self.path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, body) {
            (self.log)(format!("Qoder 凭据写不进去（{}）：{err}", tmp.display()));
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        if let Err(err) = std::fs::rename(&tmp, &self.path) {
            (self.log)(format!(
                "Qoder 凭据落位失败（{}）：{err}",
                self.path.display()
            ));
        }
    }

    async fn run_refresh_command(&self, command: &str) {
        (self.log)("Qoder：job token 不可用，执行 refresh_command 刷新凭据".to_string());
        #[cfg(windows)]
        let mut process = {
            let mut process = tokio::process::Command::new("cmd");
            process.arg("/C").arg(command);
            process
        };
        #[cfg(not(windows))]
        let mut process = {
            let mut process = tokio::process::Command::new("sh");
            process.arg("-c").arg(command);
            process
        };
        // 输出全部丢弃：刷新脚本可能打印 token，日志里绝不能带出来
        process.stdout(std::process::Stdio::null());
        process.stderr(std::process::Stdio::null());
        process.stdin(std::process::Stdio::null());
        match tokio::time::timeout(REFRESH_TIMEOUT, process.status()).await {
            Ok(Ok(status)) => (self.log)(format!("Qoder：refresh_command 退出码 {status}")),
            Ok(Err(err)) => (self.log)(format!("Qoder：refresh_command 起不来：{err}")),
            Err(_) => (self.log)("Qoder：refresh_command 超时（30s）".to_string()),
        }
    }
}

/// 从换 token 的响应里定过期时间（毫秒）。认不出来就托底 1 小时。
fn resolve_expiry(parsed: &Value, now_ms: i64) -> i64 {
    if let Some(seconds) = parsed.get("expires_in").and_then(Value::as_i64) {
        if seconds > 0 {
            return now_ms + seconds.saturating_mul(1000);
        }
    }
    if let Some(raw) = parsed
        .get("expires_at")
        .or_else(|| parsed.get("expire_time"))
    {
        if let Some(number) = raw.as_i64() {
            if number > 1_000_000_000_000 {
                return number; // 已经是毫秒
            }
            if number > 1_000_000_000 {
                return number.saturating_mul(1000); // 秒
            }
        }
    }
    now_ms + 3_600_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_auth(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("时钟")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "qoder-auth-test-{}-{tag}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir.join("auth.json")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn store(path: PathBuf, command: Option<String>, now: i64) -> AuthStore {
        AuthStore::new(
            path,
            command,
            Arc::new(|_msg: String| {}),
            Some(Arc::new(move || now)),
        )
    }

    #[test]
    fn auth_file_round_trips_and_keeps_unknown_fields() {
        let parsed: AuthFile = serde_json::from_str(
            r#"{"jobToken":"jt-1","accessToken":"dt-1","userID":"u","pat":"pt-secret","future":1}"#,
        )
        .expect("解析");
        assert_eq!(parsed.job_token, "jt-1");
        assert_eq!(parsed.access_token, "dt-1");
        assert_eq!(parsed.user_id, "u");
        // 未知键（pat）必须保留，写回别把它弄丢
        assert_eq!(parsed.extra.get("pat"), Some(&json!("pt-secret")));
        let text = serde_json::to_string(&parsed).expect("序列化");
        assert!(text.contains("pt-secret"));
        assert!(text.contains("future"));
    }

    #[test]
    fn job_token_validity_uses_the_five_minute_margin() {
        let mut creds = AuthFile {
            job_token: "jt".to_string(),
            expires_at: 1_000_000,
            ..Default::default()
        };
        // 正好在余量内 → 不算有效
        assert!(!creds.job_token_valid(1_000_000 - JOB_TOKEN_MARGIN_MS));
        assert!(creds.job_token_valid(1_000_000 - JOB_TOKEN_MARGIN_MS - 1));
        // 没有过期信息时，只要有 jobToken 就先用
        creds.expires_at = 0;
        assert!(creds.job_token_valid(i64::MAX / 2));
        creds.job_token = String::new();
        assert!(!creds.job_token_valid(0));
    }

    #[tokio::test]
    async fn valid_job_token_is_used_without_any_network() {
        let path = tmp_auth("valid");
        std::fs::write(
            &path,
            r#"{"jobToken":"jt-x","expiresAt":9007199254740991,"userID":"uid-1","name":"n","email":"a@b.c"}"#,
        )
        .unwrap();
        let store = store(path.clone(), None, 1_000);
        let client = reqwest::Client::new();
        let creds = store
            .credentials(&client, "http://127.0.0.1:1")
            .await
            .expect("有效 job token 不该走网络");
        assert_eq!(creds.user_id, "uid-1");
        assert_eq!(creds.job_token, "jt-x");
        cleanup(&path);
    }

    #[tokio::test]
    async fn missing_credentials_gives_a_clear_error() {
        let path = tmp_auth("missing");
        let store = store(path.clone(), None, 1_000);
        let client = reqwest::Client::new();
        let err = store
            .credentials(&client, "http://127.0.0.1:1")
            .await
            .expect_err("没有任何凭据必须报错");
        assert_eq!(err.status, 401);
        assert!(
            err.message.contains("请刷新"),
            "错误要说人话：{}",
            err.message
        );
        // 不许回显 token（这里本来也没有，但错误里不该出现路径以外的敏感串）
        assert!(!err.message.contains("jt-"));
        cleanup(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_command_is_run_then_the_file_is_reread() {
        let path = tmp_auth("refresh");
        // 没有 jobToken / accessToken，只能靠 refresh_command 写一份新的
        std::fs::write(&path, r#"{"userID":"uid-1"}"#).unwrap();
        let target = path.display().to_string();
        let command = format!(
            "printf '%s' '{{\"jobToken\":\"jt-new\",\"expiresAt\":9007199254740991,\"userID\":\"uid-1\"}}' > '{target}'"
        );
        let store = store(path.clone(), Some(command), 1_000);
        let client = reqwest::Client::new();
        let creds = store
            .credentials(&client, "http://127.0.0.1:1")
            .await
            .expect("refresh_command 之后必须能拿到凭据");
        assert_eq!(creds.job_token, "jt-new");
        cleanup(&path);
    }

    #[test]
    fn resolve_expiry_handles_in_seconds_millis_and_fallback() {
        let now = 1_700_000_000_000;
        assert_eq!(
            resolve_expiry(&json!({ "expires_in": 60 }), now),
            now + 60_000
        );
        assert_eq!(
            resolve_expiry(&json!({ "expires_at": 1_800_000_000_000i64 }), now),
            1_800_000_000_000
        );
        assert_eq!(
            resolve_expiry(&json!({ "expires_at": 1_800_000_000i64 }), now),
            1_800_000_000_000
        );
        assert_eq!(
            resolve_expiry(&json!({ "expires_at": "2026-01-01T00:00:00Z" }), now),
            now + 3_600_000
        );
    }
}
