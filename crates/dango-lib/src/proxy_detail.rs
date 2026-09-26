//! Read-only detail data for the two local OpenAI-compatible proxies.
use crate::models::{timestamp_to_millis, RecentRequest};
use serde::Serialize;
use serde_json::Value;

const ANTIGRAVITY_BASE_URL: &str = "http://127.0.0.1:8050/v1";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyDetail {
    pub plan_id: String,
    /// Display name and one-line description — the settings page renders any
    /// proxy from these instead of hard-coding each one.
    pub name: String,
    pub description: String,
    pub base_url: String,
    /// Every client-facing protocol this proxy speaks (OpenAI / Anthropic).
    pub endpoints: Vec<EndpointInfo>,
    /// Where requests end up, for the "上游" line.
    pub upstream: Option<String>,
    pub models: Vec<ModelInfo>,
    pub models_error: Option<String>,
    pub accounts: Vec<AccountInfo>,
    pub accounts_error: Option<String>,
    pub recent: Vec<RecentRequest>,
    pub recent_error: Option<String>,
    pub config_snippet: String,
    /// Live counters for the status strip; sources differ per proxy
    /// (tap stats vs bridge healthz), so every field is optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<ProxyLiveStats>,
}

/// One client-facing protocol endpoint.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointInfo {
    /// "openai" | "anthropic"
    pub kind: String,
    pub label: String,
    /// What a client puts in its base-URL field for this protocol.
    pub base_url: String,
}

fn openai_endpoint(base_url: &str) -> EndpointInfo {
    EndpointInfo {
        kind: "openai".into(),
        label: "OpenAI 兼容".into(),
        base_url: base_url.into(),
    }
}

/// Counters that show in the proxy pane's status strip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyLiveStats {
    pub listening: bool,
    pub requests_total: Option<u64>,
    pub requests_today: Option<u64>,
    pub errors_total: Option<u64>,
    pub in_flight: Option<u64>,
    /// Last upstream HTTP status (401/200/…), for the 上游 chip.
    pub upstream_status: Option<u16>,
    /// Unix ms of the latest upstream response.
    pub last_upstream_at: Option<u64>,
    /// Resident memory of the proxy process, when the backend reports it.
    pub memory_mb: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub owned_by: Option<String>,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    /// Full email when the bridge reports one (private single-user tool, the
    /// account label is how you tell pool entries apart), otherwise an
    /// eight-character id prefix or a redacted email.
    pub label: String,
    /// One of `available`, `cooling`, `disabled`, or `error`.
    pub status: String,
    pub detail: Option<String>,
}

/// Fetch Antigravity models, pool health, and recent logs concurrently.
/// `/healthz` may schedule the bridge's normal background quota refresh.
pub async fn antigravity_detail(client: &reqwest::Client) -> ProxyDetail {
    let (models_result, health_result, logs_result) = tokio::join!(
        fetch_json(client, "http://127.0.0.1:8050/v1/models"),
        fetch_json(client, "http://127.0.0.1:8050/healthz"),
        fetch_json(client, "http://127.0.0.1:8050/logs/recent?n=50"),
    );
    let (models, models_error) = match models_result {
        Ok(value) => parse_models(&value),
        Err(error) => (Vec::new(), Some(error)),
    };
    let (accounts, accounts_error, live) = match health_result {
        Ok(value) => {
            let (accounts, accounts_error) = parse_accounts(&value);
            (accounts, accounts_error, Some(live_from_healthz(&value)))
        }
        Err(error) => (Vec::new(), Some(error), None),
    };
    let (recent, recent_error) = match logs_result {
        Ok(value) => parse_recent(&value),
        Err(error) => (Vec::new(), Some(error)),
    };

    ProxyDetail {
        plan_id: "antigravity".into(),
        name: "Gemini 反代".into(),
        description: "本机账号池桥，按额度自动换号；同时提供 OpenAI 与 Anthropic 两种接口。".into(),
        base_url: ANTIGRAVITY_BASE_URL.into(),
        endpoints: vec![
            openai_endpoint(ANTIGRAVITY_BASE_URL),
            EndpointInfo {
                kind: "anthropic".into(),
                label: "Anthropic 兼容".into(),
                base_url: ANTIGRAVITY_BASE_URL.trim_end_matches("/v1").into(),
            },
        ],
        upstream: Some("Google Antigravity（账号池）".into()),
        config_snippet: config_snippet(ANTIGRAVITY_BASE_URL, &models),
        models,
        models_error,
        accounts,
        accounts_error,
        recent,
        recent_error,
        stats: live,
    }
}

/// Bridge `/healthz` carries a `counters` object and a `memory` block — the
/// same live-strip semantics the tap exposes.
fn live_from_healthz(value: &serde_json::Value) -> ProxyLiveStats {
    let counter = |key: &str| value["counters"][key].as_u64();
    ProxyLiveStats {
        listening: true,
        requests_total: counter("requests"),
        requests_today: None,
        errors_total: counter("errors"),
        in_flight: counter("buffered"),
        upstream_status: None,
        last_upstream_at: None,
        memory_mb: value["memory"]["rssMb"].as_f64(),
    }
}

/// Local OpenAI base URL for a plan's proxy, if it has one.
pub fn proxy_base_url(plan_id: &str) -> Option<&'static str> {
    match plan_id {
        "antigravity" => Some(ANTIGRAVITY_BASE_URL),
        _ => None,
    }
}

/// One step of the connectivity test.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestStep {
    pub name: String,
    pub ok: bool,
    pub status: Option<u16>,
    pub ms: u64,
    /// Human summary: model count, reply snippet, or the error.
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestResult {
    pub ok: bool,
    pub model: Option<String>,
    pub steps: Vec<TestStep>,
}

/// Click-to-test: list models, then send the smallest possible chat
/// completion through the proxy (so it proves the whole path: local proxy →
/// credentials → upstream). Spends a handful of tokens, only on demand.
pub async fn proxy_test(
    client: &reqwest::Client,
    base_url: &str,
    model: Option<String>,
) -> ProxyTestResult {
    let mut steps = Vec::new();
    let started = std::time::Instant::now();
    let listed = client
        .get(format!("{base_url}/models"))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    let ms = started.elapsed().as_millis() as u64;
    let mut first_model = None;
    match listed {
        Ok(response) => {
            let status = response.status().as_u16();
            let body: Value = response.json().await.unwrap_or(Value::Null);
            let (models, error) = parse_models(&body);
            first_model = models.first().map(|m| m.id.clone());
            let ok = (200..300).contains(&status) && error.is_none();
            steps.push(TestStep {
                name: "模型列表".into(),
                ok,
                status: Some(status),
                ms,
                detail: if ok {
                    format!("{} 个模型", models.len())
                } else {
                    error.unwrap_or_else(|| format!("HTTP {status}"))
                },
            });
        }
        Err(error) => steps.push(TestStep {
            name: "模型列表".into(),
            ok: false,
            status: None,
            ms,
            detail: request_error(&error),
        }),
    }

    let model = model.filter(|m| !m.trim().is_empty()).or(first_model);
    if let Some(model_id) = model.clone() {
        let started = std::time::Instant::now();
        let sent = client
            .post(format!("{base_url}/chat/completions"))
            .timeout(std::time::Duration::from_secs(45))
            .json(&serde_json::json!({
                "model": model_id,
                "messages": [{"role": "user", "content": "Reply with the single word: pong"}],
                "max_tokens": 16,
                "stream": false
            }))
            .send()
            .await;
        let ms = started.elapsed().as_millis() as u64;
        match sent {
            Ok(response) => {
                let status = response.status().as_u16();
                let text = response.text().await.unwrap_or_default();
                let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let ok = (200..300).contains(&status);
                let detail = if ok {
                    let reply = body["choices"][0]["message"]["content"]
                        .as_str()
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .take(60)
                        .collect::<String>();
                    if reply.is_empty() {
                        "上游有响应（空回复）".into()
                    } else {
                        format!("回复：{reply}")
                    }
                } else {
                    let reason: String = body["error"]["message"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| text.trim().to_string())
                        .chars()
                        .take(120)
                        .collect();
                    let reason = if reason.is_empty() {
                        format!("HTTP {status}")
                    } else {
                        reason
                    };
                    if status >= 500 {
                        format!("{reason}（上游出错，可能只是这个模型暂时不可用，换个模型再试）")
                    } else {
                        reason
                    }
                };
                steps.push(TestStep {
                    name: "对话请求".into(),
                    ok,
                    status: Some(status),
                    ms,
                    detail,
                });
            }
            Err(error) => steps.push(TestStep {
                name: "对话请求".into(),
                ok: false,
                status: None,
                ms,
                detail: request_error(&error),
            }),
        }
    }
    ProxyTestResult {
        ok: !steps.is_empty() && steps.iter().all(|step| step.ok),
        model,
        steps,
    }
}

fn request_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "请求超时".into()
    } else if error.is_connect() {
        "连不上本机反代（没在监听？）".into()
    } else {
        "请求失败".into()
    }
}

/// Fetch detail for a supported plan. Other plans return `Ok(None)`.
pub async fn proxy_detail(
    client: &reqwest::Client,
    plan_id: &str,
) -> Result<Option<ProxyDetail>, String> {
    match plan_id {
        "antigravity" => Ok(Some(antigravity_detail(client).await)),
        _ => Ok(None),
    }
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Value, String> {
    let response = client
        .get(url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                "请求超时".to_string()
            } else {
                "本机服务连接失败".to_string()
            }
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| "响应 JSON 无效".to_string())
}

fn parse_models(value: &Value) -> (Vec<ModelInfo>, Option<String>) {
    let Some(data) = value.get("data").and_then(Value::as_array) else {
        return (Vec::new(), Some("模型响应缺少 data 列表".into()));
    };
    let models = data
        .iter()
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?.to_string();
            if id.contains('@') {
                return None;
            }
            Some(ModelInfo {
                id,
                owned_by: model
                    .get("owned_by")
                    .or_else(|| model.get("ownedBy"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.contains('@'))
                    .map(str::to_string),
                context_window: model
                    .get("context_window")
                    .or_else(|| model.get("contextWindow"))
                    .and_then(Value::as_u64),
            })
        })
        .collect();
    (models, None)
}

fn parse_accounts(value: &Value) -> (Vec<AccountInfo>, Option<String>) {
    let pool = value.get("pool").and_then(Value::as_array);
    let accounts = value.get("accounts").and_then(Value::as_array);
    if pool.is_none() && accounts.is_none() {
        return (Vec::new(), Some("健康响应缺少账号列表".into()));
    }
    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut email_by_id = std::collections::HashMap::new();
    if let Some(accounts) = accounts {
        for account in accounts {
            if let (Some(id), Some(email)) = (
                account.get("id").and_then(Value::as_str),
                account.get("email").and_then(Value::as_str),
            ) {
                let trimmed = email.trim();
                if !trimmed.is_empty() && trimmed != "***@***" {
                    email_by_id.insert(id.to_string(), trimmed.to_string());
                }
            }
        }
    }

    if let Some(pool) = pool {
        for (index, account) in pool.iter().enumerate() {
            let id = account.get("id").and_then(Value::as_str);
            if let Some(id) = id {
                seen.insert(id.to_string());
            }
            let email_hint = id.and_then(|id| email_by_id.get(id).map(|s| s.as_str()));
            output.push(account_info(account, email_hint, index));
        }
    }
    if let Some(accounts) = accounts {
        for (index, account) in accounts.iter().enumerate() {
            if let Some(id) = account.get("id").and_then(Value::as_str) {
                if seen.contains(id) {
                    continue;
                }
            }
            output.push(account_info(account, None, output.len().max(index)));
        }
    }
    (output, None)
}

fn account_info(account: &Value, email_hint: Option<&str>, index: usize) -> AccountInfo {
    let disabled = account.get("disabled").and_then(Value::as_bool) == Some(true)
        || account.get("validationBlocked").and_then(Value::as_bool) == Some(true);
    let cooling = account
        .get("breakers")
        .and_then(Value::as_array)
        .is_some_and(|breakers| {
            breakers
                .iter()
                .any(|breaker| breaker.get("cooling").and_then(Value::as_bool) == Some(true))
        });
    // lastError 是历史记录不是状态：账号恢复后它还在（留着排查用），
    // 拿它判 error 会把痊愈的账号永远标成「不可用」。状态只看活信号。
    let unavailable = account.get("sessionReady").and_then(Value::as_bool) == Some(false)
        || account
            .get("error")
            .is_some_and(|error| !error.is_null() && error.as_str() != Some(""));
    let status = if disabled {
        "disabled"
    } else if cooling {
        "cooling"
    } else if unavailable {
        "error"
    } else {
        "available"
    };
    let last_error_note = account
        .get("lastError")
        .filter(|error| !error.is_null())
        .and_then(|error| {
            let status_code = error.get("status").and_then(Value::as_u64);
            let ago = error.get("at").and_then(Value::as_str).and_then(|iso| {
                chrono::DateTime::parse_from_rfc3339(iso).ok().map(|t| {
                    let then_ms = t.timestamp_millis();
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or_default();
                    let secs = (now_ms - then_ms).div_euclid(1000).max(0);
                    match secs {
                        0..=89 => "刚刚".to_string(),
                        90..=3599 => format!("{} 分钟前", secs / 60),
                        3600..=86399 => format!("{} 小时前", secs / 3600),
                        _ => format!("{} 天前", secs / 86400),
                    }
                })
            });
            match (status_code, ago) {
                (Some(code), Some(when)) => Some(format!("上次错误 {code} · {when}")),
                (Some(code), None) => Some(format!("上次错误 {code}")),
                (None, Some(when)) => Some(format!("上次错误 · {when}")),
                (None, None) => None,
            }
        });
    let detail = match status {
        "disabled" => Some("账号已停用".into()),
        "cooling" => Some("账号正在冷却".into()),
        "error" => Some(match last_error_note {
            Some(note) => format!("账号会话不可用（{note}）"),
            None => "账号会话不可用".into(),
        }),
        _ => last_error_note,
    };
    AccountInfo {
        label: safe_account_label(account, email_hint, index),
        status: status.into(),
        detail,
    }
}

fn safe_account_label(account: &Value, email_hint: Option<&str>, _index: usize) -> String {
    if let Some(email) = email_hint {
        let trimmed = email.trim();
        if !trimmed.is_empty() && trimmed != "***@***" {
            return trimmed.to_string();
        }
    }
    if let Some(email) = account.get("email").and_then(Value::as_str) {
        let trimmed = email.trim();
        if !trimmed.is_empty() && trimmed != "***@***" {
            return trimmed.to_string();
        }
    }
    if let Some(id) = account.get("id").and_then(Value::as_str) {
        let prefix: String = id.chars().take(8).collect();
        if prefix.chars().count() == 8 && !prefix.contains('@') {
            return prefix;
        }
    }
    if let Some(email) = account.get("email").and_then(Value::as_str) {
        return mask_email(email);
    }
    "***@***".into()
}

fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "***@***".into();
    };
    if local.is_empty() || domain.is_empty() {
        return "***@***".into();
    }
    format!(
        "{}***@{}***",
        local.chars().next().unwrap_or('*'),
        domain.chars().next().unwrap_or('*')
    )
}

fn parse_recent(value: &Value) -> (Vec<RecentRequest>, Option<String>) {
    let Some(lines) = value.get("lines").and_then(Value::as_array) else {
        return (Vec::new(), Some("日志响应缺少 lines 列表".into()));
    };
    let recent = lines.iter().filter_map(map_bridge_log).collect();
    (newest_first(recent), None)
}

fn map_bridge_log(line: &Value) -> Option<RecentRequest> {
    let at = timestamp_to_millis(line.get("at")?)?;
    let kind = line.get("kind").and_then(Value::as_str)?;
    let (method, path) = match kind {
        "chat.completions" => ("POST", "/v1/chat/completions"),
        "messages" => ("POST", "/v1/messages"),
        _ => return None,
    };
    let status = line
        .get("error")
        .and_then(|error| error.get("http"))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .or_else(|| (line.get("ok").and_then(Value::as_bool) == Some(true)).then_some(200));
    Some(RecentRequest {
        at,
        method: method.into(),
        path: path.into(),
        model: line
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.contains('@'))
            .map(str::to_string),
        status,
        ms: line.get("elapsedMs").and_then(Value::as_u64),
    })
}

fn newest_first(mut recent: Vec<RecentRequest>) -> Vec<RecentRequest> {
    recent.sort_by(|left, right| right.at.cmp(&left.at));
    recent.truncate(50);
    recent
}

fn config_snippet(base_url: &str, models: &[ModelInfo]) -> String {
    let model = models
        .first()
        .map(|model| model.id.as_str())
        .unwrap_or("<MODEL_ID_FROM_LIST>");
    format!("OPENAI_BASE_URL={base_url}\nOPENAI_MODEL={model}\n")
}

#[cfg(test)]
mod tests {
    use super::{map_bridge_log, parse_accounts, parse_models, parse_recent};
    use serde_json::json;

    #[test]
    fn model_mapping_uses_only_returned_fields() {
        let (models, error) = parse_models(&json!({
            "object": "list",
            "data": [{
                "id": "gemini-3-flash",
                "owned_by": "antigravity",
                "context_window": 65536
            }]
        }));
        assert_eq!(error, None);
        assert_eq!(models[0].id, "gemini-3-flash");
        assert_eq!(models[0].owned_by.as_deref(), Some("antigravity"));
        assert_eq!(models[0].context_window, Some(65536));
    }

    #[test]
    fn account_mapping_uses_email_labels_and_maps_health_states() {
        let (accounts, error) = parse_accounts(&json!({
            "pool": [
                { "id": "a1b2c3d4", "disabled": false, "sessionReady": true,
                  "breakers": [{ "cooling": true }] },
                { "id": "e5f6g7h8", "disabled": true, "breakers": [] }
            ],
            "accounts": [
                { "id": "a1b2c3d4", "email": "masked@example.invalid" },
                { "id": "e5f6g7h8", "email": "other@example.invalid" }
            ]
        }));
        assert_eq!(error, None);
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].label, "masked@example.invalid");
        assert_eq!(accounts[0].status, "cooling");
        assert_eq!(accounts[1].label, "other@example.invalid");
        assert_eq!(accounts[1].status, "disabled");
        let encoded = serde_json::to_string(&accounts).unwrap();
        assert!(encoded.contains("masked@example.invalid"));
        assert!(encoded.contains("other@example.invalid"));
    }

    #[test]
    fn a_recovered_account_is_available_with_the_error_kept_as_a_note() {
        // lastError 是历史：sessionReady=true + 熔断没冷却 = 已恢复，
        // 状态给 available，错误降级成 detail 备注。
        let (accounts, error) = parse_accounts(&json!({
            "pool": [
                { "id": "a1b2c3d4", "disabled": false, "sessionReady": true,
                  "breakers": [],
                  "lastError": { "at": "2026-09-26T03:51:37Z", "status": 429,
                                 "reason": "RESOURCE_EXHAUSTED" } }
            ],
            "accounts": []
        }));
        assert_eq!(error, None);
        assert_eq!(accounts[0].status, "available");
        assert!(
            accounts[0]
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("429")),
            "detail 应带上历史错误，实际：{:?}",
            accounts[0].detail
        );
    }

    #[test]
    fn missing_account_arrays_are_reported() {
        let (accounts, error) = parse_accounts(&json!({ "ok": true }));
        assert!(accounts.is_empty());
        assert_eq!(error.as_deref(), Some("健康响应缺少账号列表"));
    }

    #[test]
    fn bridge_log_mapping_normalizes_times_and_route_fields() {
        let request = map_bridge_log(&json!({
            "at": "2023-11-14T22:13:20Z",
            "kind": "chat.completions",
            "model": "gemini-3-flash",
            "ok": false,
            "elapsedMs": 450,
            "error": { "http": 503 }
        }))
        .unwrap();
        assert_eq!(request.at, 1_700_000_000_000);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/chat/completions");
        assert_eq!(request.status, Some(503));
        assert_eq!(request.ms, Some(450));
    }

    #[test]
    fn recent_log_response_maps_lines_newest_first_and_caps_count() {
        let lines = (0..60)
            .map(|offset| {
                json!({
                    "at": 1_700_000_000_i64 + offset,
                    "kind": "messages",
                    "model": "model",
                    "ok": true,
                    "elapsedMs": offset
                })
            })
            .collect::<Vec<_>>();
        let (recent, error) = parse_recent(&json!({ "lines": lines }));
        assert_eq!(error, None);
        assert_eq!(recent.len(), 50);
        assert_eq!(recent[0].at, 1_700_000_059_000);
        assert_eq!(recent[49].at, 1_700_000_010_000);
    }
}
