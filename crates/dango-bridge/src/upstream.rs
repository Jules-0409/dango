//! 上游会话：账号 → access_token → v1internal，外加端点回退和一次性的 token 重签。
//!
//! 移植自 `src/bridge/upstream.mjs`。这里所有凭据都只在内存里：refresh_token 只读自老桥的
//! 账号文件，access_token 每次现换。日志里只出现掩码邮箱和指纹，不出现任何 token 原文。

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc::Sender, Mutex, OnceCell};

use crate::accounts::LogFn;
use crate::models::resolve_model;
use crate::oauth::{self, OauthCandidates};
use crate::quota::{self, ModelLimit};
use crate::types::{
    now_millis, Account, GenerateRequest, QuotaSnapshot, ResolvedModel, Session, StreamEvent,
    UpstreamError,
};
use crate::v1internal::{self, Utf8Stream, ENDPOINTS};

/// token 还剩不到这个秒数就重签
const TOKEN_REFRESH_MARGIN_S: i64 = 120;
/// 模型表缓存多久（上游退役模型时是回一句 200 的纯文本，别缓存太久）
const MODEL_TTL_MS: i64 = 10 * 60 * 1000;
const POST_TIMEOUT_MS: u64 = 30_000;
const STREAM_TIMEOUT_MS: u64 = 600_000;

/// 凭据扫描是"扫一次就够"的重活（一百多 MB），整个进程只做一次，账号池里每个账号共用。
static CANDIDATES: OnceCell<OauthCandidates> = OnceCell::const_new();

pub async fn oauth_candidates_cached() -> Result<&'static OauthCandidates, String> {
    CANDIDATES
        .get_or_try_init(oauth::load_oauth_candidates)
        .await
}

/// 老桥账号库里有哪些账号（只读）。
pub async fn list_accounts() -> Result<Vec<Account>, String> {
    let (accounts, _) = tokio::task::spawn_blocking(oauth::load_bridge_accounts)
        .await
        .map_err(|e| format!("读账号库的任务失败：{e}"))??;
    Ok(accounts)
}

/// 池外单独要用的一次 HTTP 客户端（连接池共用，别每个请求新建）。
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Debug, Default)]
struct Inner {
    access_token: Option<String>,
    expires_at: i64,
    /// 换 token 成功的那一对凭据（下次直接用它，不再从头试）
    creds: Option<(String, String)>,
    model_cache: Option<(i64, Value)>,
    /// 上次成功的端点下标：优先复用，少一次跨洋打脸
    endpoint_hint: Option<usize>,
}

pub struct Upstream {
    client: reqwest::Client,
    log: LogFn,
    endpoints: Vec<String>,
    /// 只用于选中账号（对外一律掩码），以及自己签不动时退回它自带的 access_token
    account: Account,
    /// loadCodeAssist 可能补一个 project 回来，所以是可变的。
    /// 用 std 锁（不是 tokio 的）：这两处只在返回值拼装时读一眼，持锁期间不 await，
    /// 而且 `Session::identity` 是同步签名。
    project: std::sync::Mutex<Option<String>>,
    tier: std::sync::Mutex<Value>,
    inner: Mutex<Inner>,
    /// 签 token 的互斥：并发请求共享同一次签发（JS 里是共享同一个 promise）
    minting: Mutex<()>,
}

pub struct UpstreamOptions {
    pub email: Option<String>,
    pub endpoints: Option<Vec<String>>,
    pub log: LogFn,
    pub client: Option<reqwest::Client>,
}

impl Upstream {
    /// 建会话：读账号库选账号 + 拿一份 OAuth 候选（候选整进程只扫一次）。
    pub async fn init(opts: UpstreamOptions) -> Result<Self, String> {
        let accounts = list_accounts().await?;
        let account = oauth::pick_account(&accounts, opts.email.as_deref())?;
        let candidates = oauth_candidates_cached().await?;
        (opts.log)(format!(
            "账号 {}，project={}，client_id 候选 {} 个 / secret 候选 {} 个",
            account.email_masked,
            account
                .project
                .clone()
                .unwrap_or_else(|| "<无>".to_string()),
            candidates.client_ids.len(),
            candidates.secrets.len()
        ));
        if account.project.is_none() {
            (opts.log)("警告：账号里没有 project_id，等 loadCodeAssist 给一个".to_string());
        }
        Ok(Self {
            client: opts.client.unwrap_or_else(http_client),
            log: opts.log,
            endpoints: opts
                .endpoints
                .unwrap_or_else(|| ENDPOINTS.iter().map(|s| s.to_string()).collect()),
            project: std::sync::Mutex::new(account.project.clone()),
            account,
            tier: std::sync::Mutex::new(Value::Null),
            inner: Mutex::new(Inner::default()),
            minting: Mutex::new(()),
        })
    }

    pub fn identity(&self) -> Value {
        let tier = lock(&self.tier).clone();
        let current: String = tier
            .get("currentTier")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let allowed: Vec<String> = tier
            .get("allowedTiers")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "email": self.account.email_masked,
            "project": lock(&self.project).clone(),
            "tier": if current.is_empty() { Value::Null } else { Value::String(current) },
            "allowedTiers": allowed,
        })
    }

    /// 拿一个可用的 access_token。有效期不足 2 分钟就重签；并发调用共享同一次签发。
    pub async fn token(&self, force: bool) -> Result<String, UpstreamError> {
        let now = now_millis() / 1000;
        if !force {
            let inner = self.inner.lock().await;
            if let Some(token) = inner.fresh_token(now) {
                return Ok(token);
            }
        }

        // 双检 + 互斥：第二个等锁的请求在拿到锁后会看到别人刚签好的 token
        let _guard = self.minting.lock().await;
        if !force {
            let inner = self.inner.lock().await;
            if let Some(token) = inner.fresh_token(now) {
                return Ok(token);
            }
        }

        let candidates = oauth_candidates_cached()
            .await
            .map_err(|e| UpstreamError::network("candidates_failed", e))?;

        let existing = { self.inner.lock().await.creds.clone() };
        let narrowed = OauthCandidates {
            extracted_at: candidates.extracted_at.clone(),
            sources: candidates.sources.clone(),
            client_ids: match &existing {
                Some((id, _)) => vec![id.clone()],
                None => candidates.client_ids.clone(),
            },
            secrets: match &existing {
                Some((_, secret)) => vec![secret.clone()],
                None => candidates.secrets.clone(),
            },
        };

        let refresh_token = self.account.refresh_token.clone().ok_or_else(|| {
            UpstreamError::network("no_refresh_token", "账号文件里没有 refresh_token")
        })?;

        match oauth::exchange_with_candidates(&self.client, &refresh_token, &narrowed).await {
            Ok(exchange) => {
                let mut inner = self.inner.lock().await;
                inner.creds = Some((exchange.client_id.clone(), exchange.client_secret.clone()));
                inner.access_token = Some(exchange.access_token.clone());
                inner.expires_at = now + exchange.expires_in as i64;
                (self.log)(format!(
                    "token 已签：client {}，{}s 后过期",
                    oauth::fingerprint(&exchange.client_id),
                    exchange.expires_in
                ));
                Ok(exchange.access_token)
            }
            Err(err) => {
                // 自己签不动时（比如 Google 换了客户端凭据），退回老桥账号文件里现成的 token，
                // 别让服务停摆 —— 但 force（重签）时不吃兜底，免得拿着坏 token 转圈。
                if !force {
                    if let Some((token, expires_at)) = self.stored_token() {
                        if expires_at > now + 60 {
                            let mut inner = self.inner.lock().await;
                            inner.access_token = Some(token.clone());
                            inner.expires_at = expires_at;
                            (self.log)(format!(
                                "自己签 token 失败（{}），改用账号文件里现成的 token（剩 {}s）",
                                err.chars().take(120).collect::<String>(),
                                expires_at - now
                            ));
                            return Ok(token);
                        }
                    }
                }
                Err(UpstreamError::network("token_exchange_failed", err))
            }
        }
    }

    /// 账号文件里带的 token（老桥刷过的那一份）；只在兜底时用。
    /// 这条路极少走，直接读盘：注意读的是**我们自己的那份账号文件**，不会去写它。
    fn stored_token(&self) -> Option<(String, i64)> {
        let path = stored_account_path(&self.account.id)?;
        let raw = std::fs::read_to_string(path).ok()?;
        let json: Value = serde_json::from_str(&raw).ok()?;
        let token = json
            .get("token")?
            .get("access_token")?
            .as_str()?
            .to_string();
        let expiry = json
            .get("token")
            .and_then(|t| t.get("expiry_timestamp"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        Some((token, expiry))
    }

    /// 端点顺序：上次成功的优先。
    fn ordered_endpoints(&self, hint: Option<usize>) -> Vec<String> {
        match hint {
            Some(i) if i < self.endpoints.len() => {
                let mut out = vec![self.endpoints[i].clone()];
                out.extend(
                    self.endpoints
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .map(|(_, e)| e.clone()),
                );
                out
            }
            _ => self.endpoints.clone(),
        }
    }

    pub async fn load_code_assist(&self) -> Result<Value, UpstreamError> {
        let token = self.token(false).await?;
        let metadata =
            json!({"ideType":"ANTIGRAVITY","platform":"DARWIN_ARM64","pluginType":"GEMINI"});
        let bodies: [(&str, Value); 2] = [
            ("默认", json!({ "metadata": metadata })),
            (
                "mode=HEALTH_CHECK",
                json!({ "metadata": metadata, "mode": "HEALTH_CHECK" }),
            ),
        ];
        let hint = { self.inner.lock().await.endpoint_hint };

        for (tag, body) in bodies {
            for endpoint in self.ordered_endpoints(hint) {
                let res = v1internal::post_method(
                    &self.client,
                    &endpoint,
                    "loadCodeAssist",
                    &token,
                    &body,
                    POST_TIMEOUT_MS,
                )
                .await;
                if !res.ok {
                    continue;
                }
                let j = res.json.clone().unwrap_or(Value::Null);
                let project = j.get("cloudaicompanionProject").and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(_) => v.get("id").and_then(Value::as_str).map(str::to_string),
                    _ => None,
                });
                let tier_id = j
                    .get("currentTier")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("-")
                    .to_string();
                (self.log)(format!(
                    "loadCodeAssist({tag}) → tier={tier_id} project={}",
                    project.clone().unwrap_or_else(|| "<无>".to_string())
                ));

                if let Some(p) = project.clone() {
                    *lock(&self.project) = Some(p);
                }
                *lock(&self.tier) = j.clone();
                // 和 JS 一致：默认模式拿到响应就返回；HEALTH_CHECK 只有拿到 project 才算数，
                // 否则继续换端点问（它本来就是个备胎）。
                if project.is_some() || tag == "默认" {
                    return Ok(j);
                }
            }
        }

        (self.log)("loadCodeAssist 两个模式都没成功，继续用账号里带的 project".to_string());
        Ok(Value::Null)
    }

    /// 可用模型表（带 10 分钟缓存）。
    pub async fn models(&self) -> Result<Value, UpstreamError> {
        let now = now_millis();
        let (hint, cached) = {
            let inner = self.inner.lock().await;
            (inner.endpoint_hint, inner.model_cache.clone())
        };
        if let Some((at, data)) = &cached {
            if now - *at < MODEL_TTL_MS {
                return Ok(data.clone());
            }
        }

        let token = self.token(false).await?;
        let body = json!({ "project": self.project_value() });
        for endpoint in self.ordered_endpoints(hint) {
            let res = v1internal::post_method(
                &self.client,
                &endpoint,
                "fetchAvailableModels",
                &token,
                &body,
                POST_TIMEOUT_MS,
            )
            .await;
            if res.ok {
                let data = res.json.unwrap_or(Value::Null);
                let mut inner = self.inner.lock().await;
                inner.model_cache = Some((now, data.clone()));
                return Ok(data);
            }
        }
        Err(UpstreamError::network(
            "no_endpoint",
            "所有端点都拿不到模型表",
        ))
    }

    /// 查这个账号的额度：分组窗口（weekly / 5h）+ 模型级剩余。
    /// 两个接口都是只读的；查不到就把错误抛出去，由调用方决定怎么呈现（不静默给旧数字）。
    pub async fn quota_snapshot(&self) -> Result<QuotaSnapshot, UpstreamError> {
        let token = self.token(false).await?;
        let hint = { self.inner.lock().await.endpoint_hint };
        let body = json!({ "project": self.project_value() });
        let mut summary: Option<(String, Value)> = None;
        let mut last_error: Option<UpstreamError> = None;

        for endpoint in self.ordered_endpoints(hint) {
            let res = v1internal::post_method(
                &self.client,
                &endpoint,
                "retrieveUserQuotaSummary",
                &token,
                &body,
                POST_TIMEOUT_MS,
            )
            .await;
            if res.ok {
                summary = Some((endpoint, res.json.unwrap_or(Value::Null)));
                break;
            }
            last_error = res.error;
        }
        let Some((endpoint, summary_json)) = summary else {
            let err =
                last_error.unwrap_or_else(|| UpstreamError::network("no_endpoint", "查不到额度"));
            return Err(UpstreamError::network(
                "quota_failed",
                format!(
                    "查不到额度：{} {} {}",
                    err.status,
                    err.reason,
                    err.message.chars().take(120).collect::<String>()
                ),
            ));
        };

        let models = self.models().await.unwrap_or(Value::Null);
        let groups = quota::normalize_groups(&summary_json);
        let infos = quota::normalize_models(&models);
        let mut limits = std::collections::HashMap::new();
        for info in &infos {
            if info.max_output_tokens.is_some() {
                if let Some(limit) = quota::model_limits(&models, &info.id) {
                    limits.insert(info.id.clone(), limit);
                }
            }
        }

        Ok(QuotaSnapshot {
            endpoint: Some(endpoint),
            summary: quota::summarize(&groups),
            groups,
            models: infos,
            model_default: models
                .get("defaultAgentModelId")
                .and_then(Value::as_str)
                .map(str::to_string),
            models_deprecated: models
                .get("deprecatedModelIds")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0),
            model_limits: limits,
        })
    }

    /// 客户端要的模型名 → 上游的模型 id（纯逻辑在 `models::resolve_model`）。
    pub async fn resolve_model_name(&self, requested: &str) -> ResolvedModel {
        let models = self.models().await.unwrap_or(Value::Null);
        let ids: Vec<String> = models
            .get("models")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let default = models
            .get("defaultAgentModelId")
            .and_then(Value::as_str)
            .map(str::to_string);
        resolve_model(&ids, requested, default.as_deref())
    }

    /// 模型表里这个模型的硬上限（预算钳制用）。
    pub async fn limit_for(&self, model: &str) -> Option<ModelLimit> {
        let models = self.models().await.ok()?;
        quota::model_limits(&models, model)
    }

    /// 发给上游的 project 字段：没有就是 null（上游不接受空字符串，和 JS 一致）。
    fn project_value(&self) -> Value {
        match lock(&self.project).clone() {
            Some(p) => Value::String(p),
            None => Value::Null,
        }
    }
}

/// 取锁：只在短小的同步读写上用，中毒（别的线程 panic 过）时直接拿回数据继续用 ——
/// 这里的数据是「上次知道的 project / tier」，没有一致性负担。
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[async_trait::async_trait]
impl Session for Upstream {
    fn identity(&self) -> Value {
        Upstream::identity(self)
    }

    async fn load_code_assist(&self) -> Result<Value, UpstreamError> {
        Upstream::load_code_assist(self).await
    }

    async fn models(&self) -> Result<Value, UpstreamError> {
        Upstream::models(self).await
    }

    async fn quota(&self) -> Result<QuotaSnapshot, UpstreamError> {
        self.quota_snapshot().await
    }

    async fn resolve_model(&self, requested: &str) -> ResolvedModel {
        self.resolve_model_name(requested).await
    }

    /// 流式生成：把事件推进 channel（`Open` → `Chunk`* → `NoData` | `Error`）。
    /// 端点回退只发生在「还没吐出第一个 chunk」时；一旦开流就不换端点（换了客户端会看到两段正文）。
    async fn stream_generate(
        &self,
        req: GenerateRequest,
        tx: Sender<StreamEvent>,
    ) -> Result<(), UpstreamError> {
        let mut retried_token = false;
        loop {
            let token = self.token(retried_token).await?;
            let request_obj = match &req.session_key {
                Some(key) if !key.is_empty() => {
                    let mut obj = req.request.clone();
                    if let Value::Object(map) = &mut obj {
                        map.insert("session_id".to_string(), Value::String(key.clone()));
                    }
                    obj
                }
                _ => req.request.clone(),
            };
            let body = json!({
                "project": self.project_value(),
                "model": req.model,
                "request": request_obj,
                "userAgent": "antigravity",
                "requestId": req.request_id,
            });

            let hint = { self.inner.lock().await.endpoint_hint };
            let mut last_error: Option<(String, UpstreamError)> = None;
            let mut retry_after_token = false;

            for endpoint in self.ordered_endpoints(hint) {
                let opened = v1internal::open_stream(
                    &self.client,
                    &endpoint,
                    "streamGenerateContent",
                    &token,
                    &body,
                    STREAM_TIMEOUT_MS,
                )
                .await;

                if !opened.ok {
                    let err = opened.error.clone().unwrap_or_else(|| {
                        UpstreamError::network("no_error", "端点失败但没错误信息")
                    });
                    if err.status == 401 && !retried_token {
                        // token 可能刚失效：重签一次再走一遍（只重试一次，避免死循环）
                        retry_after_token = true;
                        break;
                    }
                    (self.log)(format!(
                        "端点 {} 失败：{} {} {}",
                        host_of(&endpoint),
                        err.status,
                        err.reason,
                        err.message.chars().take(120).collect::<String>()
                    ));
                    last_error = Some((endpoint.clone(), err));
                    continue;
                }

                // 记住这个端点好用，下次先试它
                if let Some(idx) = self.endpoints.iter().position(|e| e == &endpoint) {
                    self.inner.lock().await.endpoint_hint = Some(idx);
                }
                let _ = tx
                    .send(StreamEvent::Open {
                        endpoint: endpoint.clone(),
                    })
                    .await;

                let mut response = opened.body.expect("ok 的时候一定有 body");
                let mut utf8 = Utf8Stream::default();
                let mut decoder = v1internal::SseDecoder::default();
                let mut stream_error: Option<UpstreamError> = None;

                while let Some(piece) = response.chunk().await.transpose() {
                    match piece {
                        Ok(bytes) => {
                            let text = utf8.push(&bytes);
                            for event in decoder.feed(&text) {
                                if tx.send(StreamEvent::Chunk(event)).await.is_err() {
                                    // 客户端走了（接收端被丢），别再把上游读下去
                                    return Ok(());
                                }
                            }
                        }
                        Err(err) => {
                            stream_error =
                                Some(UpstreamError::network("stream_broken", err.to_string()));
                            break;
                        }
                    }
                }

                if let Some(err) = stream_error {
                    let _ = tx.send(StreamEvent::Error(err)).await;
                    return Ok(());
                }

                if !decoder.saw_data() {
                    // 200 但一个 data: 行都没有 —— 上游对已下线的模型就是这么回的纯文本
                    let _ = tx
                        .send(StreamEvent::NoData {
                            endpoint: endpoint.clone(),
                            raw_text: decoder.raw_text().to_string(),
                        })
                        .await;
                }
                return Ok(());
            }

            if retry_after_token {
                retried_token = true;
                continue;
            }

            let err = last_error
                .map(|(_, e)| e)
                .unwrap_or_else(|| UpstreamError::network("no_endpoint", "所有端点都失败了"));
            let _ = tx.send(StreamEvent::Error(err)).await;
            return Ok(());
        }
    }
}

fn host_of(url: &str) -> String {
    url.split("//")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

/// 老桥账号文件里某个账号的路径（兜底读 access_token 用）。
fn stored_account_path(id: &str) -> Option<std::path::PathBuf> {
    let root = oauth::accounts_root()?;
    let dir = root.join("accounts");
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if json.get("id").and_then(Value::as_str) == Some(id) {
            return Some(path);
        }
    }
    None
}

impl Inner {
    fn fresh_token(&self, now_s: i64) -> Option<String> {
        let token = self.access_token.clone()?;
        if self.expires_at - now_s > TOKEN_REFRESH_MARGIN_S {
            Some(token)
        } else {
            None
        }
    }
}

/// 单账号模式/测试用的便捷构造：把 `Arc<dyn Session>` 包成池子要的形状。
pub fn as_session(upstream: Arc<Upstream>) -> Arc<dyn Session> {
    upstream
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_token_respects_margin() {
        let inner = Inner {
            access_token: Some("t".into()),
            expires_at: 1000,
            ..Default::default()
        };
        // 剩余 500s：还能用
        assert_eq!(inner.fresh_token(500), Some("t".to_string()));
        // 剩余 100s：小于 120s 的余量，要重签
        assert_eq!(inner.fresh_token(900), None);
        assert_eq!(Inner::default().fresh_token(0), None);
    }

    #[test]
    fn ordered_endpoints_puts_hint_first() {
        let upstream = Upstream {
            client: http_client(),
            log: Arc::new(|_: String| {}),
            endpoints: ENDPOINTS.iter().map(|s| s.to_string()).collect(),
            account: Account::default(),
            project: std::sync::Mutex::new(None),
            tier: std::sync::Mutex::new(Value::Null),
            inner: Mutex::new(Inner::default()),
            minting: Mutex::new(()),
        };
        let order = upstream.ordered_endpoints(Some(2));
        assert!(order[0].contains("cloudcode-pa.googleapis.com"));
        assert_eq!(order.len(), 3);
        // hint 越界也不能丢端点
        let all = upstream.ordered_endpoints(Some(9));
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], ENDPOINTS[0]);
    }

    #[test]
    fn host_of_extracts_hostname() {
        assert_eq!(
            host_of("https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal"),
            "daily-cloudcode-pa.sandbox.googleapis.com"
        );
        assert_eq!(host_of("不是URL"), "不是URL");
    }
}
