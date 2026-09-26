use crate::probes::credentials::load_factory_credentials;
use crate::probes::models::{FactoryQuota, FactoryWindows, QuotaWindow};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn parse_window_group(group: Option<&Value>) -> Option<HashMap<String, QuotaWindow>> {
    let obj = group?.as_object()?;
    let mut map = HashMap::new();

    for (k, v) in obj {
        if let Some(used_val) = v.get("usedPercent") {
            let used_percent = used_val.as_f64().unwrap_or(0.0);
            let seconds_remaining = v.get("secondsRemaining").and_then(|s| s.as_i64());
            let window_end = v.get("windowEnd").and_then(|w| w.as_i64());

            map.insert(
                k.clone(),
                QuotaWindow {
                    used_percent,
                    seconds_remaining,
                    window_end,
                },
            );
        }
    }

    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

pub async fn fetch_factory(client: &reqwest::Client) -> FactoryQuota {
    // load_factory_credentials 是同步阻塞的（spawn `security` / PowerShell 读系统凭据库、
    // 读磁盘、做 AES-GCM 解密），放到阻塞线程池里跑。
    // 手动凭据槽优先：贴进来的 access_token 直接当 FactoryAuth 用。
    let auth = match tokio::task::spawn_blocking(|| {
        crate::manual_creds::get("factory").map_or_else(load_factory_credentials, |access_token| {
            Ok(crate::probes::credentials::FactoryAuth {
                access_token,
                active_organization_id: None,
            })
        })
    })
    .await
    {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            return FactoryQuota {
                ok: false,
                id: "factory".into(),
                name: "Factory (Droid)".into(),
                error: Some(e),
                fetched_at: now_millis(),
                ..Default::default()
            };
        }
        Err(join_err) => {
            return FactoryQuota {
                ok: false,
                id: "factory".into(),
                name: "Factory (Droid)".into(),
                error: Some(format!("factory.credential_task_failed: {}", join_err)),
                fetched_at: now_millis(),
                ..Default::default()
            };
        }
    };

    // 网络错误 / 5xx / 429 自动重试（最多 3 次尝试，300ms、900ms 指数退避）
    let send_result = crate::probes::http::send_with_retry(|| {
        client
            .get("https://api.factory.ai/api/billing/limits")
            .header(AUTHORIZATION, format!("Bearer {}", auth.access_token))
            .header(ACCEPT, "application/json")
    })
    .await;

    let resp = match send_result {
        Ok(r) => r,
        Err(e) => {
            return FactoryQuota {
                ok: false,
                id: "factory".into(),
                name: "Factory (Droid)".into(),
                org_id: auth.active_organization_id,
                error: Some(format!("Network error: {}", e)),
                fetched_at: now_millis(),
                ..Default::default()
            };
        }
    };

    let status = resp.status();
    if status.as_u16() == 401 {
        return FactoryQuota {
            ok: false,
            id: "factory".into(),
            name: "Factory (Droid)".into(),
            org_id: auth.active_organization_id,
            error: Some("factory.auth_expired".into()),
            fetched_at: now_millis(),
            ..Default::default()
        };
    }

    if !status.is_success() {
        return FactoryQuota {
            ok: false,
            id: "factory".into(),
            name: "Factory (Droid)".into(),
            org_id: auth.active_organization_id,
            error: Some(format!("Factory API error {}", status.as_u16())),
            fetched_at: now_millis(),
            ..Default::default()
        };
    }

    let val: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return FactoryQuota {
                ok: false,
                id: "factory".into(),
                name: "Factory (Droid)".into(),
                org_id: auth.active_organization_id,
                error: Some(format!("Failed to parse JSON response: {}", e)),
                fetched_at: now_millis(),
                ..Default::default()
            };
        }
    };

    let plan_name = val
        .get("planName")
        .and_then(|p| p.as_str())
        .map(String::from);
    let overage_preference = val
        .get("overagePreference")
        .and_then(|p| p.as_str())
        .map(String::from);
    let extra_usage_balance_cents = val.get("extraUsageBalanceCents").and_then(|c| c.as_i64());
    let extra_usage_allowed = val.get("extraUsageAllowed").and_then(|a| a.as_bool());

    let limits_obj = val.get("limits");
    let standard = limits_obj.and_then(|l| parse_window_group(l.get("standard")));
    let core = limits_obj.and_then(|l| parse_window_group(l.get("core")));

    FactoryQuota {
        ok: true,
        id: "factory".into(),
        name: "Factory (Droid)".into(),
        plan_name,
        org_id: auth.active_organization_id,
        windows: FactoryWindows { standard, core },
        overage_preference,
        extra_usage_balance_cents,
        extra_usage_allowed,
        fetched_at: now_millis(),
        error: None,
    }
}
