// Grok Build（xAI 的命令行 agent）：登录态在 ~/.grok/auth.json（OIDC，access 只活 6 小时），
// GET cli-chat-proxy.grok.com/v1/billing?format=credits 拿 SuperGrok 这一周的额度百分比
// 和各产品（Chat / Voice / Build…）各占多少。
// 规矩同别家：只读，永不 refresh；过期就如实报，跑一次 `grok` 它自己会续上。
use std::path::{Path, PathBuf};

use crate::models::{Bucket, PlanQuota};

const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";

pub fn auth_path(home: &Path) -> PathBuf {
    home.join(".grok/auth.json")
}

/// auth.json 是 `{"<issuer>::<id>": {key, expires_at, ...}}`；取第一份带 key 的。过期不 refresh。
pub fn read_auth(path: &Path, now_ms: i64) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| "grok.auth: 没找到 Grok Build 登录".to_string())?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("grok.auth: auth.json 读不懂（{e}）"))?;
    let entry = json
        .as_object()
        .and_then(|m| {
            m.values().find(|v| {
                v.get("key")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|k| !k.is_empty())
            })
        })
        .ok_or("grok.auth: auth.json 里没有登录信息")?;
    if let Some(exp) = entry
        .get("expires_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    {
        if exp.timestamp_millis() <= now_ms {
            return Err("grok.auth: 登录过期，终端里跑一次 grok 让它自己续上".into());
        }
    }
    Ok(entry["key"].as_str().unwrap_or_default().to_string())
}

fn product_label(product: &str) -> String {
    match product {
        "GrokChat" => "Chat".into(),
        "GrokVoice" => "Voice".into(),
        "GrokBuild" | "GrokCode" | "GrokCli" => "Build".into(),
        other => other.strip_prefix("Grok").unwrap_or(other).to_string(),
    }
}

fn window_of(period_type: Option<&str>) -> (&'static str, &'static str) {
    match period_type {
        Some("USAGE_PERIOD_TYPE_WEEKLY") => ("本周", "week"),
        Some("USAGE_PERIOD_TYPE_DAILY") => ("今天", "day"),
        Some("USAGE_PERIOD_TYPE_MONTHLY") => ("本月", "month"),
        _ => ("额度", "other"),
    }
}

/// 解析 `config`。返回 (buckets, 头条剩余 %, 头条口径, 重置时间)。
pub fn parse_billing(
    config: &serde_json::Value,
) -> Result<(Vec<Bucket>, f64, String, Option<u64>), String> {
    let used = config
        .get("creditUsagePercent")
        .and_then(serde_json::Value::as_f64)
        .ok_or("parse: missing creditUsagePercent")?;
    let period = config.get("currentPeriod");
    let (label, window) = window_of(
        period
            .and_then(|p| p.get("type"))
            .and_then(serde_json::Value::as_str),
    );
    let resets_at = period
        .and_then(|p| p.get("end"))
        .or_else(|| config.get("billingPeriodEnd"))
        .and_then(serde_json::Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis() as u64);
    let remaining = (100.0 - used).clamp(0.0, 100.0);
    let mut rows = vec![Bucket {
        label: label.into(),
        remaining_percent: Some(remaining),
        detail: Some(format!("已用 {used:.0}%")),
        resets_at,
        pool: None,
        window: Some(window.into()),
    }];
    // 各产品吃掉了多少（占总额度的百分比，不是各自的配额）
    if let Some(products) = config
        .get("productUsage")
        .and_then(serde_json::Value::as_array)
    {
        for p in products {
            let (Some(name), Some(pct)) = (
                p.get("product").and_then(serde_json::Value::as_str),
                p.get("usagePercent").and_then(serde_json::Value::as_f64),
            ) else {
                continue;
            };
            rows.push(Bucket {
                label: product_label(name),
                remaining_percent: None,
                detail: Some(format!("用掉 {pct:.0}%")),
                resets_at: None,
                pool: Some("SuperGrok".into()),
                window: Some(window.into()),
            });
        }
    }
    Ok((rows, remaining, format!("SuperGrok · {label}"), resets_at))
}

pub async fn fetch(client: &reqwest::Client) -> PlanQuota {
    let mut plan = PlanQuota {
        id: "grok".into(),
        name: "Grok Build".into(),
        ok: false,
        error: None,
        remaining_percent: None,
        buckets: vec![],
        note: None,
        proxy: None,
        headline_label: None,
        resets_at: None,
    };
    let auth = tokio::task::spawn_blocking(|| {
        if let Some(token) = crate::manual_creds::get("grok") {
            return Ok(token);
        }
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("grok.auth: HOME 不可用")?);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        read_auth(&auth_path(&home), now)
    })
    .await
    .unwrap_or_else(|e| Err(format!("task: {e}")));
    let token = match auth {
        Ok(v) => v,
        Err(e) => {
            plan.error = Some(e);
            return plan;
        }
    };
    let resp = match client
        .get(BILLING_URL)
        .bearer_auth(&token)
        .header("User-Agent", "grok/1.0.25")
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            plan.error = Some(if e.is_timeout() {
                "net: 请求超时".into()
            } else {
                format!("net: {e}")
            });
            return plan;
        }
    };
    let status = resp.status().as_u16();
    if status == 401 {
        plan.error = Some("grok.auth: 登录失效，终端里跑一次 grok 让它自己续上".into());
        return plan;
    }
    if !resp.status().is_success() {
        plan.error = Some(format!("http {status}"));
        return plan;
    }
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            plan.error = Some(format!("json: {e}"));
            return plan;
        }
    };
    let Some(config) = body.get("config") else {
        plan.error = Some("parse: missing config".into());
        return plan;
    };
    match parse_billing(config) {
        Ok((rows, p, label, at)) => {
            plan.buckets = rows;
            plan.remaining_percent = Some(p);
            plan.headline_label = Some(label);
            plan.resets_at = at;
            plan.ok = true;
        }
        Err(e) => plan.error = Some(e),
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn weekly_credits_become_remaining_with_product_rows() {
        let config = json!({
            "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-09-23T09:07:02+00:00", "end": "2026-09-30T09:07:02+00:00"},
            "creditUsagePercent": 8.0,
            "productUsage": [{"product": "GrokVoice", "usagePercent": 5.0},
                             {"product": "GrokChat", "usagePercent": 3.0}]
        });
        let (rows, p, label, at) = parse_billing(&config).unwrap();
        assert_eq!(p, 92.0);
        assert_eq!(label, "SuperGrok · 本周");
        assert_eq!(at, Some(1_790_759_222_000));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].detail.as_deref(), Some("已用 8%"));
        assert_eq!(rows[1].label, "Voice");
        assert_eq!(rows[2].remaining_percent, None);
    }

    #[test]
    fn missing_usage_is_an_error_not_a_number() {
        assert!(parse_billing(&json!({"currentPeriod": {}})).is_err());
    }

    #[test]
    fn expired_login_is_reported_not_refreshed() {
        let dir = std::env::temp_dir().join(format!("dango-grok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"https://auth.x.ai::x":{"key":"k","expires_at":"2026-09-24T11:41:52Z"}}"#,
        )
        .unwrap();
        let before = chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(read_auth(&path, before).unwrap(), "k");
        assert!(read_auth(&path, before + 3 * 3_600_000)
            .unwrap_err()
            .contains("过期"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
