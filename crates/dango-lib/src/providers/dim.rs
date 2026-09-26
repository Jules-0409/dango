// DimAgent（dimagent.cn）：App 自己的 OAuth 登录在 ~/.dimcode/v2/auth.json，
// GET {issuer}/api/me/usage 拿订阅点数、滚动窗口和功能次数（联网搜索）。
// 规矩同别家：只读，永不 refresh（refresh token 轮换会把 App 挤下线）；
// access 过期就如实报，打开一次 DimAgent 它自己会续上。
use std::path::{Path, PathBuf};

use crate::models::{timestamp_to_millis, Bucket, PlanQuota};

const DEFAULT_ISSUER: &str = "https://dimagent.cn";

fn pct(remaining: f64, total: f64) -> Option<f64> {
    (total > 0.0).then(|| (remaining / total * 100.0).clamp(0.0, 100.0))
}

fn num(v: &serde_json::Value, key: &str) -> Option<f64> {
    let x = v.get(key)?;
    x.as_f64().or_else(|| x.as_str()?.trim().parse().ok())
}

fn fmt_count(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 100_000.0 {
        format!("{:.1}K", n / 1_000.0)
    } else {
        format!("{n:.0}")
    }
}

pub fn auth_path(home: &Path) -> PathBuf {
    home.join(".dimcode/v2/auth.json")
}

/// (access token, issuer)。过期不 refresh，直接报 auth 错。
pub fn read_auth(path: &Path, now_ms: u64) -> Result<(String, String), String> {
    let text =
        std::fs::read_to_string(path).map_err(|_| "dim.auth: 没找到 DimAgent 登录".to_string())?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("dim.auth: auth.json 读不懂（{e}）"))?;
    let oauth = json
        .get("nextApiOauth")
        .ok_or("dim.auth: auth.json 里没有登录信息")?;
    let access = oauth
        .get("access")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("dim.auth: auth.json 里没有 access token")?;
    if let Some(expires) = oauth.get("expires").and_then(serde_json::Value::as_u64) {
        if expires <= now_ms {
            return Err("dim.auth: 登录过期，打开一次 DimAgent 让它自己续上".into());
        }
    }
    let issuer = oauth
        .get("issuer")
        .and_then(serde_json::Value::as_str)
        .filter(|s| s.starts_with("https://"))
        .unwrap_or(DEFAULT_ISSUER)
        .trim_end_matches('/')
        .to_string();
    Ok((access.to_string(), issuer))
}

/// 解析 `/api/me/usage` 的 `data`。返回 (buckets, 头条百分比, 头条口径, 头条重置时间)。
pub fn parse_usage(
    data: &serde_json::Value,
) -> Result<(Vec<Bucket>, Option<f64>, Option<String>, Option<u64>), String> {
    let credits = data.get("credits").ok_or("parse: missing credits")?;
    let sub = data.get("subscription").filter(|v| !v.is_null());
    let product = sub
        .and_then(|s| s.get("product"))
        .and_then(|p| p.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cancels = sub
        .and_then(|s| s.get("subscription"))
        .and_then(|s| s.get("cancel_at_period_end"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let term_end = sub
        .and_then(|s| s.get("current_term"))
        .and_then(|t| t.get("end_at"))
        .and_then(timestamp_to_millis);

    let mut rows = Vec::new();
    let mut headline: Vec<(f64, String, Option<u64>)> = Vec::new();

    // 订阅点数：credits 顶层是订阅 + 加油包的合计
    let total = num(credits, "total_units").or_else(|| num(credits, "total_credits"));
    let remaining = num(credits, "remaining_units").or_else(|| num(credits, "remaining_credits"));
    let unlimited = credits
        .get("unlimited")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if unlimited {
        rows.push(Bucket {
            label: "点数".into(),
            remaining_percent: None,
            detail: Some("不限量".into()),
            resets_at: None,
            pool: None,
            window: Some("month".into()),
        });
    } else if let (Some(total), Some(remaining)) = (total, remaining) {
        let p = pct(remaining, total);
        // 不续订时那一刻是到期，不是重置：倒计时不挂，写进明细。
        let (resets_at, detail) = match (cancels, term_end) {
            (true, Some(end)) => {
                let day = chrono::DateTime::from_timestamp_millis(end as i64)
                    .map(|t| t.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap()))
                    .map(|t| t.format(" · %-m/%-d 到期").to_string())
                    .unwrap_or_default();
                (
                    None,
                    format!("{} / {}{day}", fmt_count(remaining), fmt_count(total)),
                )
            }
            _ => (
                term_end,
                format!("{} / {}", fmt_count(remaining), fmt_count(total)),
            ),
        };
        rows.push(Bucket {
            label: "点数".into(),
            remaining_percent: p,
            detail: Some(detail),
            resets_at,
            pool: None,
            window: Some("month".into()),
        });
        if let Some(p) = p {
            let label = match (product, cancels) {
                (Some(name), true) => format!("{name} · 到期不续"),
                (Some(name), false) => format!("{name} · 点数"),
                (None, _) => "点数".into(),
            };
            headline.push((p, label, resets_at));
        }
    }

    // 滚动窗口（有的套餐按 N 小时限 token）：新版在 window_states，老版平铺在 bucket 上
    let buckets = credits
        .get("subscription_buckets")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .or_else(|| credits.get("subscription_bucket").map(|b| vec![b.clone()]))
        .unwrap_or_default();
    for bucket in buckets.iter().filter(|b| !b.is_null()) {
        let states = match bucket
            .get("window_states")
            .and_then(serde_json::Value::as_array)
        {
            Some(states) => states.clone(),
            None => vec![bucket.clone()],
        };
        for w in states {
            let hours = num(&w, "window_duration_hours").unwrap_or(0.0);
            let cap = num(&w, "window_token_cap").unwrap_or(0.0);
            if hours <= 0.0 || cap <= 0.0 {
                continue;
            }
            let used = num(&w, "window_token_used").unwrap_or(0.0);
            let p = pct(cap - used, cap);
            let resets_at = w.get("window_expires_at").and_then(timestamp_to_millis);
            let label = format!("{hours:.0} 小时");
            rows.push(Bucket {
                label: label.clone(),
                remaining_percent: p,
                detail: Some(format!("{} / {}", fmt_count(cap - used), fmt_count(cap))),
                resets_at,
                pool: None,
                window: Some(if hours == 5.0 {
                    "5h".into()
                } else {
                    "other".into()
                }),
            });
            if let Some(p) = p {
                headline.push((p, format!("{label}窗口"), resets_at));
            }
        }
    }

    // 功能次数（联网搜索等）：单独一行，不进头条
    for meter in data
        .get("feature_meters")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let key = meter
            .get("feature_key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let label = match key {
            "web_search" => "联网搜索".to_string(),
            "" => continue,
            other => other.to_string(),
        };
        let unit = if meter.get("unit").and_then(serde_json::Value::as_str) == Some("call") {
            " 次"
        } else {
            ""
        };
        let (p, detail) =
            if meter.get("unlimited").and_then(serde_json::Value::as_bool) == Some(true) {
                (None, "不限".to_string())
            } else {
                let allowance = num(meter, "total_allowance").unwrap_or(0.0);
                let left = num(meter, "total_remaining").unwrap_or(0.0);
                (
                    pct(left, allowance),
                    format!("{} / {}{unit}", fmt_count(left), fmt_count(allowance)),
                )
            };
        rows.push(Bucket {
            label,
            remaining_percent: p,
            detail: Some(detail),
            resets_at: if cancels {
                None
            } else {
                meter.get("period_end").and_then(timestamp_to_millis)
            },
            pool: None,
            window: None,
        });
    }

    if rows.is_empty() {
        return Err("parse: 没有订阅也没有点数".into());
    }
    let best = headline.into_iter().min_by(|a, b| a.0.total_cmp(&b.0));
    Ok(match best {
        Some((p, label, at)) => (rows, Some(p), Some(label), at),
        None => (rows, None, product.map(str::to_string), None),
    })
}

pub async fn fetch(client: &reqwest::Client) -> PlanQuota {
    let mut plan = PlanQuota {
        id: "dim".into(),
        name: "DimAgent".into(),
        ok: false,
        error: None,
        remaining_percent: None,
        buckets: vec![],
        note: None,
        proxy: None,
        headline_label: None,
        resets_at: None,
    };
    // 手动凭据槽优先，App 自己的 auth.json 兜底。
    let auth = tokio::task::spawn_blocking(|| {
        if let Some(token) = crate::manual_creds::get("dim") {
            return Ok((token, DEFAULT_ISSUER.to_string()));
        }
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("dim.auth: HOME 不可用")?);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        read_auth(&auth_path(&home), now)
    })
    .await
    .unwrap_or_else(|e| Err(format!("task: {e}")));
    let (token, issuer) = match auth {
        Ok(v) => v,
        Err(e) => {
            plan.error = Some(e);
            return plan;
        }
    };
    let resp = match client
        .get(format!("{issuer}/api/me/usage"))
        .bearer_auth(&token)
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
    if !resp.status().is_success() {
        plan.error = Some(format!("http {}", resp.status().as_u16()));
        return plan;
    }
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            plan.error = Some(format!("json: {e}"));
            return plan;
        }
    };
    if body.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
        let msg = body
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("success=false");
        plan.error = Some(format!("dim: {msg}"));
        return plan;
    }
    let Some(data) = body.get("data") else {
        plan.error = Some("parse: missing data".into());
        return plan;
    };
    match parse_usage(data) {
        Ok((rows, p, label, at)) => {
            plan.buckets = rows;
            plan.remaining_percent = p;
            plan.headline_label = label;
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

    fn sample(cancel: bool) -> serde_json::Value {
        json!({
            "account_id": 1,
            "subscription": {
                "subscription": { "id": 9, "status": "active", "cancel_at_period_end": cancel },
                "current_term": { "end_at": "2026-09-27T01:04:27.373Z" },
                "product": { "name": "Lite套餐" }
            },
            "credits": {
                "subscription_bucket": {
                    "id": 1, "total_units": 11000, "used_units": 6951, "remaining_units": 4049,
                    "window_token_cap": 0, "window_token_used": 0, "window_duration_hours": 0,
                    "window_started_at": null, "window_expires_at": null
                },
                "subscription_buckets": [{
                    "id": 1, "total_units": 11000, "used_units": 6951, "remaining_units": 4049,
                    "window_token_cap": 0, "window_token_used": 0, "window_duration_hours": 0,
                    "window_started_at": null, "window_expires_at": null
                }],
                "addon_buckets": [],
                "total_units": 11000, "used_units": 6951, "remaining_units": 4049, "unlimited": false
            },
            "resets": { "window": { "available_count": 0 }, "monthly_full": { "available_count": 0 } },
            "feature_meters": [{
                "feature_key": "web_search", "unit": "call", "unlimited": false,
                "total_allowance": 500, "total_used": 80, "total_remaining": 420,
                "period_end": "2026-09-27T01:04:27.373Z"
            }]
        })
    }

    #[test]
    fn credits_are_the_headline_and_search_is_its_own_row() {
        let (rows, p, label, at) = parse_usage(&sample(false)).unwrap();
        assert!((p.unwrap() - 36.809).abs() < 0.01);
        assert_eq!(label.as_deref(), Some("Lite套餐 · 点数"));
        assert!(at.is_some());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].detail.as_deref(), Some("4049 / 11000"));
        assert_eq!(rows[1].label, "联网搜索");
        assert_eq!(rows[1].detail.as_deref(), Some("420 / 500 次"));
        assert_eq!(rows[1].remaining_percent, Some(84.0));
    }

    #[test]
    fn a_plan_that_will_not_renew_says_expires_instead_of_counting_down_a_reset() {
        let (rows, _, label, at) = parse_usage(&sample(true)).unwrap();
        assert_eq!(label.as_deref(), Some("Lite套餐 · 到期不续"));
        assert_eq!(at, None);
        assert_eq!(rows[0].resets_at, None);
        assert_eq!(rows[0].detail.as_deref(), Some("4049 / 11000 · 9/27 到期"));
    }

    #[test]
    fn a_rolling_window_counts_toward_the_headline() {
        let mut d = sample(false);
        d["credits"]["subscription_buckets"][0]["window_states"] = json!([{
            "window_duration_hours": 5, "window_token_cap": 1000, "window_token_used": 900,
            "window_started_at": null, "window_expires_at": "2026-09-26T20:00:00Z"
        }]);
        let (rows, p, label, _) = parse_usage(&d).unwrap();
        assert!(rows.iter().any(|r| r.window.as_deref() == Some("5h")));
        assert!((p.unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(label.as_deref(), Some("5 小时窗口"));
    }

    #[test]
    fn an_expired_login_is_reported_not_refreshed() {
        let dir = std::env::temp_dir().join(format!("dango-dim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"nextApiOauth":{"access":"x","expires":1000,"issuer":"https://dimagent.cn"}}"#,
        )
        .unwrap();
        let err = read_auth(&path, 2000).unwrap_err();
        assert!(err.contains("过期"));
        let (_, issuer) = read_auth(&path, 500).unwrap();
        assert_eq!(issuer, "https://dimagent.cn");
        std::fs::remove_dir_all(&dir).ok();
    }
}
