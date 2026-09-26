//! `QoderSession`：一个 Qoder CN 账号伪装成 [`Session`]，塞进 `Pool::Single` 就能被 pump 用。
//!
//! 翻译契约（最大风险点）：pump 交给 `stream_generate` 的是 **Gemini inner**（`build_gemini_request`
//! 的产物），这里转成 Qoder body；回给 `tx` 的 `StreamEvent::Chunk` 必须是 **Gemini 形状**
//! （`{response:{candidates:[{content:{parts},finishReason}],usageMetadata}}`），否则客户端空回合。
//!
//! 未验证的地方都标了 `TODO(未验证)`：工具调用映射、usage 字段映射、`enable_thinking` 口径。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc::Sender;

use crate::server::LogFn;
use crate::types::{
    iso_from_millis, now_millis, GenerateRequest, QuotaRow, QuotaSnapshot, ResolvedModel, Session,
    StreamEvent, UpstreamError,
};

use super::auth::{AuthFile, AuthStore, Clock};
use super::body::{self, BuildChatOptions, DEFAULT_MAX_TOKENS};
use super::cosy::{self, CosyCredentials};
use super::stream::{QoderStream, SseDecoder, SseEvent};
use super::{
    add_prefix, strip_prefix, CLIENT_TYPE, DEFAULT_GATEWAY, DEFAULT_OPENAPI, OPENAPI_COSY_VERSION,
};

const USER_AGENT: &str = "antigravity-bridge-qoder";
const MODEL_TTL_MS: i64 = 10 * 60 * 1000;
/// 图片上传缓存的上限（条）。客户端每轮重发整段历史，没缓存同一张图会被反复上传。
const IMAGE_CACHE_MAX: usize = 64;

/// 图片上传缓存：`sha256(base64)` → 图床 URL，先进先出淘汰。
#[derive(Default)]
struct ImageCache {
    urls: HashMap<String, String>,
    order: VecDeque<String>,
}

impl ImageCache {
    fn get(&self, key: &str) -> Option<String> {
        self.urls.get(key).cloned()
    }

    fn put(&mut self, key: String, url: String) {
        if self.urls.insert(key.clone(), url).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > IMAGE_CACHE_MAX {
            if let Some(oldest) = self.order.pop_front() {
                self.urls.remove(&oldest);
            }
        }
    }
}

fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            let first = local.chars().next().map(String::from).unwrap_or_default();
            format!("{first}***@{domain}")
        }
        None => String::new(),
    }
}

fn cosy_creds<'a>(creds: &'a AuthFile, machine_id: &'a str) -> CosyCredentials<'a> {
    CosyCredentials {
        user_id: &creds.user_id,
        auth_token: &creds.job_token,
        name: &creds.name,
        email: &creds.email,
        machine_id,
    }
}

/// 把模型清单归一化成桥认识的形状（`{models:{key:entry}, defaultAgentModelId}`），
/// 与 Antigravity 的 `fetchAvailableModels` 形状对齐，`handle_models` 才能直接合并。
fn normalize_models(raw: &Value) -> Value {
    let entries = raw
        .get("chat")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut models = Map::new();
    let mut default: Option<String> = None;
    let mut first_enabled: Option<String> = None;
    for entry in &entries {
        if entry.get("enable").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(key) = entry.get("key").and_then(Value::as_str) else {
            continue;
        };
        let display = entry
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or(key);
        let is_reasoning = entry
            .get("is_reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || entry.get("thinking_config").is_some();
        if first_enabled.is_none() {
            first_enabled = Some(key.to_string());
        }
        if default.is_none() && entry.get("is_default").and_then(Value::as_bool) == Some(true) {
            default = Some(key.to_string());
        }
        models.insert(
            key.to_string(),
            json!({
                "displayName": display,
                "supportsThinking": is_reasoning,
                "isReasoning": is_reasoning,
                "isVision": entry.get("is_vl").and_then(Value::as_bool).unwrap_or(false),
            }),
        );
    }
    let default = default.or(first_enabled);
    json!({ "models": models, "defaultAgentModelId": default })
}

/// 一个额度桶（`userQuota` / `addOnQuota` / `dedicatedResourcePackages[n]`）的剩余百分比。
fn remaining_percent_of(bucket: &Value) -> Option<f64> {
    let total = bucket.get("total").and_then(Value::as_f64)?;
    if total <= 0.0 {
        return None;
    }
    let remaining = bucket.get("remaining").and_then(Value::as_f64)?;
    Some((remaining / total * 1000.0).round() / 10.0)
}

/// `expiresAt` 是 **epoch 毫秒整数**（顶层和资源包里都是这个口径），不是字符串。
fn reset_time_of(value: &Value) -> Option<String> {
    value
        .get("expiresAt")
        .and_then(Value::as_i64)
        .filter(|ms| *ms > 0)
        .map(iso_from_millis)
}

/// 上游额度 JSON → 面板用的摘要行。
///
/// 真实响应（实测）是**顶层扁平**的：`userQuota{total,used,remaining,percentage,unit}`、
/// `addOnQuota` 同形、`dedicatedResourcePackages[]` 每项自带 `name/total/remaining/expiresAt`。
/// 上游的 `percentage` 口径可疑（1347/2000 给的 0.68），所以按 `remaining/total` 自己算。
pub fn summary_rows(raw: &Value) -> Vec<QuotaRow> {
    let mut summary = Vec::new();
    if let Some(user) = raw.get("userQuota") {
        summary.push(QuotaRow {
            group: "Qoder".to_string(),
            window: "plan".to_string(),
            remaining_percent: remaining_percent_of(user),
            reset_time: reset_time_of(raw),
        });
    }
    // 加量包：没买（total 为 0）就不占一行
    if let Some(addon) = raw
        .get("addOnQuota")
        .filter(|value| value.get("total").and_then(Value::as_f64).unwrap_or(0.0) > 0.0)
    {
        summary.push(QuotaRow {
            group: "Qoder".to_string(),
            window: "add-on".to_string(),
            remaining_percent: remaining_percent_of(addon),
            reset_time: reset_time_of(addon),
        });
    }
    // 专用资源包：每个可用包一行，窗口名用上游给的 name
    for pack in raw
        .get("dedicatedResourcePackages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        // `available:false` 是「当前用不了」的包，别报成还有余量
        if pack.get("available").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        summary.push(QuotaRow {
            group: "Qoder".to_string(),
            window: pack
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("resource-pack")
                .to_string(),
            remaining_percent: remaining_percent_of(pack),
            reset_time: reset_time_of(pack),
        });
    }
    summary
}

pub struct QoderSession {
    client: reqwest::Client,
    log: LogFn,
    gateway: String,
    openapi: String,
    auth: AuthStore,
    machine_id: String,
    /// 中和上游自带的产品人设（配置 `qoder.neutralize`，默认关 = 原样透传）
    neutralize: bool,
    /// 图片上传缓存（见 [`ImageCache`]）
    images: std::sync::Mutex<ImageCache>,
    models: tokio::sync::Mutex<Option<(i64, Value)>>,
    now: Clock,
}

impl QoderSession {
    /// 建会话：只读配置与凭据文件，不联网（凭据坏了也不拦启动，请求时再报清晰错误）。
    pub fn new(cfg: &crate::config::QoderConfig, log: LogFn) -> Result<Self, String> {
        Self::with_clock(cfg, log, None)
    }

    pub fn with_clock(
        cfg: &crate::config::QoderConfig,
        log: LogFn,
        now: Option<Clock>,
    ) -> Result<Self, String> {
        let home =
            crate::oauth::home_dir().ok_or_else(|| "拿不到 HOME / USERPROFILE".to_string())?;
        let auth_file = cfg
            .auth_file
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".qoder-bridge").join("auth.json"));
        let machine_id_file = cfg
            .machine_id_file
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".qoder-cn").join(".auth").join("machine_id"));
        let gateway = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_GATEWAY.to_string())
            .trim_end_matches('/')
            .to_string();

        let auth = AuthStore::new(
            auth_file,
            cfg.refresh_command.clone(),
            Arc::clone(&log),
            now.clone(),
        );
        let machine_id = read_machine_id(&machine_id_file)
            .or_else(|| {
                auth.snapshot()
                    .map(|creds| creds.machine_id)
                    .filter(|id| !id.is_empty())
            })
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            log,
            gateway,
            openapi: DEFAULT_OPENAPI.to_string(),
            auth,
            machine_id,
            neutralize: cfg.neutralize.unwrap_or(false),
            images: std::sync::Mutex::new(ImageCache::default()),
            models: tokio::sync::Mutex::new(None),
            now: now.unwrap_or_else(|| Arc::new(now_millis)),
        })
    }

    fn now_ms(&self) -> i64 {
        (self.now)()
    }

    fn chat_url(&self) -> String {
        format!(
            "{}/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1",
            self.gateway
        )
    }

    fn model_list_url(&self) -> String {
        format!("{}/algo/api/v2/model/list?Encode=1", self.gateway)
    }

    fn quota_url(&self) -> String {
        format!("{}/api/v2/quota/usage", self.openapi)
    }

    /// 模型表现在还需要吗（TTL 内就直接用缓存）。
    async fn cached_models(&self) -> Option<Value> {
        let cache = self.models.lock().await;
        match cache.as_ref() {
            Some((at, value)) if self.now_ms() - at < MODEL_TTL_MS => Some(value.clone()),
            _ => None,
        }
    }

    async fn load_models(&self) -> Result<Value, UpstreamError> {
        if let Some(value) = self.cached_models().await {
            return Ok(value);
        }
        let creds = self.auth.credentials(&self.client, &self.openapi).await?;
        let url = self.model_list_url();
        let headers = cosy::build_auth_headers(&[], &url, &cosy_creds(&creds, &self.machine_id))
            .map_err(|err| UpstreamError::network("qoder_sign", err))?;
        let mut request = self.client.get(&url).header("Accept", "application/json");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|err| UpstreamError::network("qoder_model_list", err.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(UpstreamError::http(
                status.as_u16(),
                "qoder_model_list",
                crate::server::truncate(&text, 200),
            ));
        }
        let raw: Value = serde_json::from_str(&text)
            .map_err(|err| UpstreamError::network("qoder_model_list_parse", err.to_string()))?;
        let normalized = normalize_models(&raw);
        *self.models.lock().await = Some((self.now_ms(), normalized.clone()));
        Ok(normalized)
    }

    async fn model_is_reasoning(&self, key: &str) -> bool {
        match self.load_models().await {
            Ok(models) => models
                .get("models")
                .and_then(|models| models.get(key))
                .and_then(|entry| entry.get("isReasoning"))
                .and_then(Value::as_bool)
                // 拿不到模型表时按「可能是推理模型」处理，与 PoC 的默认（isReasoning 默认真）一致
                .unwrap_or(true),
            Err(_) => true,
        }
    }

    /// 发一次带 COSY 签名的 POST，返回响应（网络层失败转成 `UpstreamError`）。
    async fn post_chat(
        &self,
        creds: &AuthFile,
        model_key: &str,
        encoded: &[u8],
    ) -> Result<reqwest::Response, UpstreamError> {
        let url = self.chat_url();
        let headers = cosy::build_auth_headers(encoded, &url, &cosy_creds(creds, &self.machine_id))
            .map_err(|err| UpstreamError::network("qoder_sign", err))?;
        let mut request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Accept-Encoding", "identity")
            .header("X-Model-Key", model_key)
            .header("X-Model-Source", "system")
            .body(encoded.to_vec());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request
            .send()
            .await
            .map_err(|err| UpstreamError::network("qoder_chat", err.to_string()))
    }

    /// `GET {openapi}/api/v2/quota/usage`，返回上游原始 JSON。
    async fn fetch_quota(&self) -> Result<Value, UpstreamError> {
        let creds = self.auth.credentials(&self.client, &self.openapi).await?;
        let url = self.quota_url();
        let response = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {}", creds.job_token))
            .header("Cosy-Version", OPENAPI_COSY_VERSION)
            .header("Cosy-ClientType", CLIENT_TYPE)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|err| UpstreamError::network("qoder_quota", err.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(UpstreamError::http(
                status.as_u16(),
                "qoder_quota",
                crate::server::truncate(&text, 200),
            ));
        }
        serde_json::from_str(&text)
            .map_err(|err| UpstreamError::network("qoder_quota_parse", err.to_string()))
    }

    /// 把这一轮请求里带的图先传进 Qoder 图床，返回 `sha256(base64) → url`。
    ///
    /// 上传失败只记日志、不拦请求：那张图会被丢掉，并在 `body` 的警告里留一条
    /// `image_not_uploaded:…`（不闷声）。同一张图按 base64 的 sha256 缓存，不重复上传。
    async fn upload_images(&self, creds: &AuthFile, request: &Value) -> HashMap<String, String> {
        let mut found: Vec<(String, String, String)> = Vec::new(); // (key, mime, base64)
        if let Some(contents) = request.get("contents").and_then(Value::as_array) {
            for content in contents {
                let parts = content
                    .get("parts")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for part in parts {
                    let Some(inline) = part.get("inlineData") else {
                        continue;
                    };
                    let data = inline.get("data").and_then(Value::as_str).unwrap_or("");
                    if data.is_empty() {
                        continue;
                    }
                    let key = body::image_key_of(data);
                    if !found.iter().any(|(seen, _, _)| *seen == key) {
                        let mime = inline
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png")
                            .to_string();
                        found.push((key, mime, data.to_string()));
                    }
                }
            }
        }

        let mut urls: HashMap<String, String> = HashMap::new();
        if found.is_empty() {
            return urls;
        }

        // 先翻缓存：命中就不上传
        let mut pending: Vec<(String, String, String)> = Vec::new();
        {
            let cache = self.images.lock().unwrap_or_else(|err| err.into_inner());
            for (key, mime, data) in found {
                match cache.get(&key) {
                    Some(url) => {
                        urls.insert(key, url);
                    }
                    None => pending.push((key, mime, data)),
                }
            }
        }
        if pending.is_empty() {
            return urls;
        }

        let creds_cosy = cosy_creds(creds, &self.machine_id);
        let mut uploaded = 0usize;
        let mut bytes_total = 0usize;
        for (key, mime, data) in pending {
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data.as_bytes()) {
                Ok(bytes) => bytes,
                Err(err) => {
                    (self.log)(format!("Qoder：图片 base64 解不开（{mime}）：{err}"));
                    continue;
                }
            };
            match super::upload::upload_image(
                &self.client,
                &self.gateway,
                &creds_cosy,
                &bytes,
                &mime,
            )
            .await
            {
                Ok(url) => {
                    uploaded += 1;
                    bytes_total += bytes.len();
                    {
                        let mut cache = self.images.lock().unwrap_or_else(|err| err.into_inner());
                        cache.put(key.clone(), url.clone());
                    }
                    urls.insert(key, url);
                }
                Err(err) => {
                    (self.log)(format!(
                        "Qoder：图片上传失败（{mime}，{:.1} KB）：{err}",
                        bytes.len() as f64 / 1024.0
                    ));
                }
            }
        }
        if uploaded > 0 {
            (self.log)(format!(
                "Qoder：上传了 {uploaded} 张图（共 {:.1} KB）",
                bytes_total as f64 / 1024.0
            ));
        }
        urls
    }
}

fn read_machine_id(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value = text.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[async_trait::async_trait]
impl Session for QoderSession {
    fn identity(&self) -> Value {
        let now = self.now_ms();
        let snapshot = self.auth.snapshot();
        let (email, uid, valid, expires) = match snapshot {
            Some(creds) => {
                let uid = creds.user_id.clone();
                let valid = creds.job_token_valid(now);
                let expires = if creds.expires_at > 0 {
                    Value::String(iso_from_millis(creds.expires_at))
                } else {
                    Value::Null
                };
                (mask_email(&creds.email), uid, valid, expires)
            }
            None => (String::new(), String::new(), false, Value::Null),
        };
        // 绝不回显 job token / 签名
        json!({
            "provider": "qoder",
            "email": email,
            "uid": uid,
            "jobTokenValid": valid,
            "expiresAt": expires,
        })
    }

    async fn load_code_assist(&self) -> Result<Value, UpstreamError> {
        // Qoder 没有这一跳；返回身份让预热日志也说得清
        Ok(self.identity())
    }

    async fn models(&self) -> Result<Value, UpstreamError> {
        self.load_models().await
    }

    /// Qoder 的额度：`GET {openapi}/api/v2/quota/usage`，原始响应交给 [`QoderSession::quota_raw`]。
    async fn quota(&self) -> Result<QuotaSnapshot, UpstreamError> {
        let raw = self.fetch_quota().await?;
        Ok(QuotaSnapshot {
            endpoint: Some(self.quota_url()),
            summary: summary_rows(&raw),
            ..Default::default()
        })
    }

    async fn quota_raw(&self) -> Result<Value, UpstreamError> {
        self.fetch_quota().await
    }

    async fn resolve_model(&self, requested: &str) -> ResolvedModel {
        let key = strip_prefix(requested).to_string();
        match self.load_models().await {
            Ok(models) => {
                let known = models
                    .get("models")
                    .and_then(|models| models.get(&key))
                    .is_some();
                if known {
                    return ResolvedModel {
                        model: add_prefix(&key),
                        substituted_from: None,
                        reason: "exact".to_string(),
                    };
                }
                if let Some(default) = models
                    .get("defaultAgentModelId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    return ResolvedModel {
                        model: add_prefix(default),
                        substituted_from: Some(requested.to_string()),
                        reason: "default_fallback".to_string(),
                    };
                }
                ResolvedModel {
                    model: requested.to_string(),
                    substituted_from: None,
                    reason: "no_model_list".to_string(),
                }
            }
            // 模型表拿不到（多半是没凭据）：照请求继续，真正的错误在开流时如实报
            Err(_) => ResolvedModel {
                model: add_prefix(&key),
                substituted_from: None,
                reason: "no_model_list".to_string(),
            },
        }
    }

    async fn stream_generate(
        &self,
        req: GenerateRequest,
        tx: Sender<StreamEvent>,
    ) -> Result<(), UpstreamError> {
        let key = strip_prefix(&req.model).to_string();
        let is_reasoning = match body::thinking_requested(&req.request) {
            Some(requested) => requested,
            None => self.model_is_reasoning(&key).await,
        };

        let mut creds = match self.auth.credentials(&self.client, &self.openapi).await {
            Ok(creds) => creds,
            Err(err) => {
                let _ = tx.send(StreamEvent::Error(err)).await;
                return Ok(());
            }
        };

        // 带图的请求：先把图传进图床拿 URL（缓存里有的不重复传），再组装请求体
        let image_urls = self.upload_images(&creds, &req.request).await;

        // 中和提示里带上当天日期：上游自己注入的那个 CurrentDate 是旧的，用它反而误导模型
        let today = iso_from_millis(self.now_ms());
        let today = today.get(..10).unwrap_or("").to_string();
        let (body_value, warnings) = body::build_chat_body_with_warnings(
            &req.request,
            &BuildChatOptions {
                model_key: &key,
                is_reasoning,
                user_id: &creds.user_id,
                session_key: req.session_key.as_deref(),
                default_max_tokens: DEFAULT_MAX_TOKENS,
                neutralize: self.neutralize,
                today: &today,
                image_urls: if image_urls.is_empty() {
                    None
                } else {
                    Some(&image_urls)
                },
            },
        );
        for warning in &warnings {
            (self.log)(format!("Qoder：{warning}"));
        }
        let plaintext = serde_json::to_vec(&body_value).unwrap_or_else(|_| b"{}".to_vec());
        let encoded = super::encoding::encode_body(&plaintext);

        let mut response = match self.post_chat(&creds, &key, &encoded).await {
            Ok(response) => response,
            Err(err) => {
                let _ = tx.send(StreamEvent::Error(err)).await;
                return Ok(());
            }
        };
        // job token 可能「文件说没过期、上游说过期」：401/403 强刷一次再试一发
        if matches!(response.status().as_u16(), 401 | 403) {
            if let Ok(fresh) = self
                .auth
                .credentials_forced(&self.client, &self.openapi)
                .await
            {
                (self.log)("Qoder：上游回 401/403，强制刷新凭据后重试一次".to_string());
                creds = fresh;
                match self.post_chat(&creds, &key, &encoded).await {
                    Ok(retry) => response = retry,
                    Err(err) => {
                        let _ = tx.send(StreamEvent::Error(err)).await;
                        return Ok(());
                    }
                }
            }
        }

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let _ = tx
                .send(StreamEvent::Error(UpstreamError::http(
                    status.as_u16(),
                    "qoder_chat",
                    crate::server::truncate(&text, 300),
                )))
                .await;
            return Ok(());
        }

        let endpoint = self.chat_url();
        let _ = tx
            .send(StreamEvent::Open {
                endpoint: endpoint.clone(),
            })
            .await;

        let mut decoder = SseDecoder::new();
        let mut stream = QoderStream::new();
        let mut bytes = response.bytes_stream();
        let mut saw_chunk = false;
        let mut done = false;
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    let _ = tx
                        .send(StreamEvent::Error(UpstreamError::network(
                            "qoder_stream_read",
                            err.to_string(),
                        )))
                        .await;
                    return Ok(());
                }
            };
            let events = match decoder.push(&chunk) {
                Ok(events) => events,
                Err(err) => {
                    let _ = tx.send(StreamEvent::Error(err)).await;
                    return Ok(());
                }
            };
            let mut out: Vec<Value> = Vec::new();
            for event in events {
                match event {
                    SseEvent::Done => {
                        done = true;
                        break;
                    }
                    SseEvent::Chunk(inner) => stream.ingest(&inner, &mut out),
                }
            }
            for chunk in out {
                saw_chunk = true;
                if tx.send(StreamEvent::Chunk(chunk)).await.is_err() {
                    return Ok(());
                }
            }
            if done {
                break;
            }
        }
        // 读懂 [DONE] 就主动断开：网关到 DONE 后不关连接，不 break 会一直挂
        drop(bytes);

        let mut tail: Vec<Value> = Vec::new();
        stream.flush(&mut tail);
        for chunk in tail {
            saw_chunk = true;
            if tx.send(StreamEvent::Chunk(chunk)).await.is_err() {
                return Ok(());
            }
        }
        if !saw_chunk {
            let _ = tx
                .send(StreamEvent::NoData {
                    endpoint,
                    raw_text: String::new(),
                })
                .await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_models_keeps_enabled_and_picks_default() {
        let raw = json!({ "chat": [
            { "key": "auto", "enable": true, "display_name": "Auto", "is_reasoning": true, "is_default": true, "is_vl": true },
            { "key": "qfmodel", "enable": true, "display_name": "Qwen3.8-Flash", "is_reasoning": true },
            { "key": "off", "enable": false, "display_name": "下线的" },
        ]});
        let models = normalize_models(&raw);
        let map = models["models"].as_object().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["auto"]["displayName"], json!("Auto"));
        assert_eq!(map["auto"]["supportsThinking"], json!(true));
        assert_eq!(map["auto"]["isVision"], json!(true));
        assert_eq!(models["defaultAgentModelId"], json!("auto"));
    }

    #[test]
    fn normalize_models_falls_back_to_first_enabled() {
        let raw = json!({ "chat": [
            { "key": "a", "enable": true, "display_name": "A" },
            { "key": "b", "enable": true, "display_name": "B", "is_default": true },
        ]});
        let models = normalize_models(&raw);
        assert_eq!(models["defaultAgentModelId"], json!("b"));
        // 没有 is_default 时退回第一个 enable
        let models = normalize_models(&json!({ "chat": [{ "key": "a", "enable": true }] }));
        assert_eq!(models["defaultAgentModelId"], json!("a"));
        // 一个都没有
        assert_eq!(
            normalize_models(&json!({ "chat": [] }))["defaultAgentModelId"],
            Value::Null
        );
    }

    #[test]
    fn mask_email_only_shows_the_first_letter() {
        assert_eq!(mask_email("alice@example.com"), "a***@example.com");
        assert_eq!(mask_email(""), "");
        assert_eq!(mask_email("no-at-sign"), "");
    }

    #[test]
    fn summary_rows_matches_the_real_shape() {
        // 结构照实测响应抄（数值是假的）：额度在顶层，expiresAt 是数字毫秒
        let raw = json!({
            "userId": "u-1",
            "usageType": "plan",
            "totalUsagePercentage": 0.68,
            "isQuotaExceeded": false,
            "expiresAt": 1_789_000_000_000i64,
            "userQuota": { "total": 2000.0, "used": 1347.0, "remaining": 653.0, "percentage": 0.68, "unit": "credits" },
            "addOnQuota": { "total": 0.0, "used": 0.0, "remaining": 0.0, "unit": "credits" },
            "dedicatedResourcePackages": [
                { "name": "包月资源", "total": 100.0, "remaining": 25.0, "expiresAt": 1_789_000_000_000i64, "available": true },
                { "name": "用不了了的", "total": 100.0, "remaining": 100.0, "available": false },
            ],
        });
        let rows = summary_rows(&raw);
        // plan + 一个可用资源包；total 为 0 的加量包不出现，available:false 的包跳过
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].window, "plan");
        let percent = rows[0].remaining_percent.expect("plan 有余量百分比");
        assert!((percent - 32.65).abs() < 0.1, "plan 剩余百分比 {percent}");
        // expiresAt 是数字也要出时间（以前读字符串，永远为空）
        let reset = rows[0].reset_time.clone().expect("plan 有重置时间");
        assert!(reset.starts_with("2026-"), "重置时间 {reset}");
        assert_eq!(rows[1].window, "包月资源");
        assert_eq!(rows[1].remaining_percent, Some(25.0));
    }

    #[test]
    fn summary_rows_is_empty_without_quota() {
        assert!(summary_rows(&json!({ "isQuotaExceeded": false })).is_empty());
    }
}
