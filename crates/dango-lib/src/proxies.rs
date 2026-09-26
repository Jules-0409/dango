// 反代端点健康检查：挂在套餐上，不是独立列表。
// 目前唯一的内置反代是 dango-bridge(8050)（Gemini 账号池）。
use crate::models::{PlanQuota, ProxyStatus};
use futures_util::future::join_all;
use serde_json::Value;
use std::time::Instant;

pub const PLAN_PROXIES: &[(&str, &str)] = &[("antigravity", "http://127.0.0.1:8050/healthz")];

const ANTIGRAVITY_PROXY_URL: &str = "http://127.0.0.1:8050/healthz";

/// Parse Antigravity's healthz payload into account counts and a safe status.
pub fn antigravity_from_healthz(value: &Value) -> ProxyStatus {
    let pool = value.get("pool").and_then(Value::as_array);
    let accounts = value.get("accounts").and_then(Value::as_array);
    let account_states = pool.filter(|states| !states.is_empty()).or(accounts);

    let (accounts_available, accounts_cooling) = account_states
        .map(|states| {
            states
                .iter()
                .fold((0_u64, 0_u64), |(available, cooling), account| {
                    let cooling_here = account
                        .get("breakers")
                        .and_then(Value::as_array)
                        .is_some_and(|breakers| {
                            breakers.iter().any(|breaker| {
                                breaker.get("cooling").and_then(Value::as_bool) == Some(true)
                            })
                        });
                    let disabled = account.get("disabled").and_then(Value::as_bool) == Some(true)
                        || account.get("validationBlocked").and_then(Value::as_bool) == Some(true);
                    if disabled {
                        (available, cooling)
                    } else if cooling_here {
                        (available, cooling + 1)
                    } else {
                        (available + 1, cooling)
                    }
                })
        })
        .map(|(available, cooling)| (Some(available), Some(cooling)))
        .unwrap_or((None, None));

    let upstream_status = value
        .get("upstream")
        .and_then(Value::as_object)
        .and_then(|upstream| {
            upstream
                .get("status")
                .or_else(|| upstream.get("httpStatus"))
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok())
        });

    let summary = match (accounts_available, accounts_cooling) {
        (Some(available), Some(cooling)) => {
            Some(format!("{available} 个账号可用 · {cooling} 个冷却中"))
        }
        _ => Some("账号状态未知".into()),
    };
    let bridge_ok = value.get("ok").and_then(Value::as_bool) == Some(true);
    let ok = bridge_ok && accounts_available.is_some_and(|count| count > 0);

    ProxyStatus {
        name: "Gemini 反代".into(),
        url: ANTIGRAVITY_PROXY_URL.into(),
        ok,
        latency_ms: None,
        detail: upstream_object_detail(value),
        summary,
        upstream_status,
        requests_today: None,
        requests_total: None,
        in_flight: None,
        last_upstream_at: None,
        token_available: None,
        accounts_available,
        accounts_cooling,
    }
}

/// A safe status when the health endpoint could not be read.
pub fn antigravity_unavailable(detail: impl Into<String>) -> ProxyStatus {
    ProxyStatus {
        name: "Gemini 反代".into(),
        url: ANTIGRAVITY_PROXY_URL.into(),
        ok: false,
        latency_ms: None,
        detail: Some(detail.into()),
        summary: Some("Bridge 健康检查失败".into()),
        upstream_status: None,
        requests_today: None,
        requests_total: None,
        in_flight: None,
        last_upstream_at: None,
        token_available: None,
        accounts_available: None,
        accounts_cooling: None,
    }
}

fn upstream_object_detail(value: &Value) -> Option<String> {
    let upstream = value.get("upstream")?;
    if let Some(text) = upstream.as_str() {
        return Some(text.to_string());
    }
    let object = upstream.as_object()?;
    let tier = object.get("tier").and_then(Value::as_str);
    Some(match tier {
        Some(tier) => format!("Bridge · {tier}"),
        None => "Bridge 已响应".into(),
    })
}

/// Old general-purpose HTTP probe remains available for existing callers.
pub async fn check_one(client: &reqwest::Client, url: &str) -> ProxyStatus {
    let started = Instant::now();
    let (ok, detail, status, parsed) = match client
        .get(url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(response) => {
            let code = response.status().as_u16();
            let ok = response.status().is_success();
            let body = response.text().await.unwrap_or_default();
            let parsed = serde_json::from_str::<Value>(&body).ok();
            let detail = parsed
                .as_ref()
                .and_then(|value| {
                    value
                        .get("upstream")
                        .and_then(Value::as_str)
                        .or_else(|| value.get("version").and_then(Value::as_str))
                        .or_else(|| value.get("build")?.get("git")?.as_str())
                        .map(str::to_string)
                })
                .or_else(|| parsed.as_ref().and_then(upstream_object_detail))
                .or_else(|| Some(format!("HTTP {code}")));
            (ok, detail, Some(code), parsed)
        }
        Err(error) => (false, Some(safe_network_error(&error)), None, None),
    };
    let antigravity = parsed
        .as_ref()
        .filter(|_| url.contains(":8050/"))
        .map(antigravity_from_healthz);
    ProxyStatus {
        name: antigravity
            .as_ref()
            .map(|status| status.name.clone())
            .unwrap_or_else(|| url.to_string()),
        url: url.to_string(),
        ok: antigravity.as_ref().map_or(ok, |status| status.ok),
        latency_ms: Some(started.elapsed().as_millis() as u64),
        detail: antigravity
            .as_ref()
            .and_then(|status| status.detail.clone())
            .or(detail),
        summary: antigravity.and_then(|status| status.summary),
        upstream_status: status,
        requests_today: None,
        requests_total: None,
        in_flight: None,
        last_upstream_at: None,
        token_available: None,
        accounts_available: None,
        accounts_cooling: None,
    }
}

fn safe_network_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "请求超时".into()
    } else if error.is_connect() {
        "连接失败".into()
    } else {
        "健康检查失败".into()
    }
}

/// Attach HTTP-checked proxies in parallel while preserving existing proxy fields.
pub async fn attach_proxies(client: &reqwest::Client, plans: &mut [PlanQuota]) {
    let tasks = plans
        .iter()
        .filter_map(|plan| {
            PLAN_PROXIES
                .iter()
                .find(|(id, _)| *id == plan.id)
                .map(|(_, url)| (plan.id.clone(), *url))
        })
        .map(|(id, url)| async move { (id, check_one(client, url).await) });
    let results = join_all(tasks).await;
    for plan in plans.iter_mut() {
        if let Some((_, status)) = results.iter().find(|(id, _)| id == &plan.id) {
            plan.proxy = Some(status.clone());
        }
    }
}

/// Attach proxies, reusing an optional already-fetched Antigravity status
/// (for example the result of `fetch_with_proxy`).
pub async fn attach_proxies_with_statuses(
    client: &reqwest::Client,
    plans: &mut [PlanQuota],
    antigravity_status: Option<ProxyStatus>,
) {
    let tasks = plans
        .iter()
        .filter(|plan| !(plan.id == "antigravity" && antigravity_status.is_some()))
        .filter_map(|plan| {
            PLAN_PROXIES
                .iter()
                .find(|(id, _)| *id == plan.id)
                .map(|(_, url)| (plan.id.clone(), *url))
        })
        .map(|(id, url)| async move { (id, check_one(client, url).await) });
    let results = join_all(tasks).await;

    for plan in plans {
        if plan.id == "antigravity" {
            if let Some(status) = &antigravity_status {
                plan.proxy = Some(status.clone());
            } else if let Some((_, status)) = results.iter().find(|(id, _)| id == &plan.id) {
                plan.proxy = Some(status.clone());
            }
        } else if let Some((_, status)) = results.iter().find(|(id, _)| id == &plan.id) {
            plan.proxy = Some(status.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::antigravity_from_healthz;
    use serde_json::json;

    #[test]
    fn antigravity_summary_counts_available_and_cooling_accounts() {
        let status = antigravity_from_healthz(&json!({
            "ok": true,
            "upstream": {
                "tier": "standard",
                "project": "project-test",
                "allowedTiers": ["standard"]
            },
            "pool": [
                { "id": "account-a", "disabled": false, "breakers": [] },
                { "id": "account-b", "disabled": false,
                  "breakers": [{ "key": "family:gemini", "cooling": true }] },
                { "id": "account-c", "disabled": true, "breakers": [] }
            ],
            "accounts": []
        }));

        assert_eq!(status.summary.as_deref(), Some("1 个账号可用 · 1 个冷却中"));
        assert_eq!(status.accounts_available, Some(1));
        assert_eq!(status.accounts_cooling, Some(1));
        assert_eq!(status.detail.as_deref(), Some("Bridge · standard"));
        assert!(status.ok);
    }
}
