// Haze Pro：session JWT 在登录钥匙串，/api/usage 查额度。
// 同 haze-tap 的规矩：只读 keychain，永不自己 refresh。
use crate::keychain::haze_token;
use crate::models::{Bucket, PlanQuota};

fn pct(used: f64, limit: f64) -> Option<f64> {
    if limit <= 0.0 {
        None
    } else {
        Some(((limit - used) / limit * 100.0).clamp(0.0, 100.0))
    }
}

fn fmt_tokens(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}K", n / 1_000.0)
    } else {
        format!("{n:.0}")
    }
}

/// 头条规则：min(周额度, 5 小时)；Cloud agents、Credits 不参与。
/// 返回 (头条百分比, 口径名)；两个窗口都没有百分比就 None，不编数。
pub fn headline(rows: &[Bucket]) -> (Option<f64>, Option<String>) {
    let weekly = rows
        .iter()
        .find(|b| b.window.as_deref() == Some("week"))
        .and_then(|b| b.remaining_percent);
    let five_hour = rows
        .iter()
        .find(|b| b.window.as_deref() == Some("5h"))
        .and_then(|b| b.remaining_percent);
    [("周额度", weekly), ("5 小时窗口", five_hour)]
        .into_iter()
        .filter_map(|(label, p)| p.map(|p| (p, label)))
        .min_by(|(a, _), (b, _)| a.total_cmp(b))
        .map(|(p, label)| (Some(p), Some(label.to_string())))
        .unwrap_or((None, None))
}

/// Parse the documented quota fields without manufacturing values for missing data.
pub fn parse_usage(d: &serde_json::Value) -> Result<Vec<Bucket>, String> {
    fn number(d: &serde_json::Value, key: &str) -> Result<f64, String> {
        d.get(key)
            .ok_or_else(|| format!("parse: missing {key}"))?
            .as_f64()
            .ok_or_else(|| format!("parse: invalid {key}"))
    }

    let w_used = number(d, "weeklyTokensUsed")?;
    let w_lim = number(d, "weeklyTokenLimit")?;
    let f_used = number(d, "fiveHTokensUsed")?;
    let f_lim = number(d, "fiveHTokenLimit")?;

    let mut rows = vec![
        Bucket {
            label: "周额度".into(),
            remaining_percent: pct(w_used, w_lim),
            detail: Some(format!("{} / {}", fmt_tokens(w_used), fmt_tokens(w_lim))),
            // Existing /api/usage fields and fixtures expose no reset timestamp.
            resets_at: None,
            pool: None,
            window: Some("week".into()),
        },
        Bucket {
            label: "5 小时".into(),
            remaining_percent: pct(f_used, f_lim),
            detail: Some(format!("{} / {}", fmt_tokens(f_used), fmt_tokens(f_lim))),
            resets_at: None,
            pool: None,
            window: Some("5h".into()),
        },
    ];

    let cloud_used = d.get("cloudAgentsUsed");
    let cloud_limit = d.get("cloudAgentLimit");
    match (cloud_used, cloud_limit) {
        (None, None) => {}
        (Some(_), Some(_)) => {
            let used = number(d, "cloudAgentsUsed")?;
            let limit = number(d, "cloudAgentLimit")?;
            rows.push(Bucket {
                label: "Cloud agents".into(),
                remaining_percent: pct(used, limit),
                detail: Some(format!("{} / {}", fmt_tokens(used), fmt_tokens(limit))),
                resets_at: None,
                pool: None,
                window: None,
            });
        }
        _ => return Err("parse: incomplete cloudAgents".into()),
    }

    if let Some(credits) = d.get("credits") {
        if let Some(remaining) = credits.get("remaining") {
            let remaining = remaining
                .as_f64()
                .ok_or_else(|| "parse: invalid credits.remaining".to_string())?;
            rows.push(Bucket {
                label: "Credits".into(),
                remaining_percent: None,
                detail: Some(format!("{remaining} credits")),
                resets_at: None,
                pool: None,
                window: None,
            });
        }
    }

    Ok(rows)
}

pub async fn fetch(client: &reqwest::Client) -> PlanQuota {
    let mut plan = PlanQuota {
        id: "haze".into(),
        name: "Haze Pro".into(),
        ok: false,
        error: None,
        remaining_percent: None,
        buckets: vec![],
        note: None,
        proxy: None,
        headline_label: None,
        resets_at: None,
    };
    // `security` spawns a subprocess synchronously; keep it off the tokio
    // worker (only 2 exist) just like every other credential read.
    // 手动凭据槽优先：用户在设置页粘的 token 是明确意图，vendor 源兜底。
    let tok = match tokio::task::spawn_blocking(|| {
        crate::manual_creds::get("haze").map_or_else(haze_token, Ok)
    })
    .await
    .unwrap_or_else(|e| Err(format!("task: {e}")))
    {
        Ok(t) => t,
        Err(e) => {
            plan.error = Some(format!("keychain: {e}"));
            return plan;
        }
    };
    // CF 拦默认 UA，要用 app 同款 UA 才放行进 API（实测结论）。这是唯一一个
    // 必须伪装 UA 的端点：诚实 UA 重构时误删过一次，dango UA 直接被拦。
    // A timeout is mandatory: the 60 s quota loop joins on this request, and a
    // hung upstream would otherwise freeze every plan at its last known value
    // instead of reporting `ok = false`.
    let resp = match client
        .get("https://usehaze.ai/api/usage")
        .header("authorization", format!("Bearer {tok}"))
        .header("user-agent", "Haze/1.3.670")
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            plan.error = Some(if e.is_timeout() {
                "net: 请求超时".to_string()
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
    let d: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            plan.error = Some(format!("json: {e}"));
            return plan;
        }
    };
    let rows = match parse_usage(&d) {
        Ok(rows) => rows,
        Err(error) => {
            plan.error = Some(error);
            return plan;
        }
    };

    let (headline_percent, headline_label) = headline(&rows);
    plan.remaining_percent = headline_percent;
    plan.headline_label = headline_label;
    plan.buckets = rows;
    plan.ok = true;
    plan
}

#[cfg(test)]
mod tests {
    use super::{headline, parse_usage};
    use serde_json::json;

    #[test]
    fn parses_complete_usage_without_fixed_credit_limit() {
        let rows = parse_usage(&json!({
            "weeklyTokensUsed": 25_000,
            "weeklyTokenLimit": 100_000,
            "fiveHTokensUsed": 20,
            "fiveHTokenLimit": 100,
            "cloudAgentsUsed": 2,
            "cloudAgentLimit": 8,
            "credits": { "remaining": 125.5 }
        }))
        .unwrap();

        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].remaining_percent, Some(75.0));
        assert_eq!(rows[0].window.as_deref(), Some("week"));
        assert_eq!(rows[1].window.as_deref(), Some("5h"));
        assert_eq!(rows[2].remaining_percent, Some(75.0));
        assert_eq!(rows[3].remaining_percent, None);
        assert_eq!(rows[3].detail.as_deref(), Some("125.5 credits"));
        assert!(rows.iter().all(|bucket| bucket.resets_at.is_none()));
    }

    #[test]
    fn headline_ignores_cloud_agents_and_credits() {
        // Cloud agents 见底（0%）也不能拉低头条：头条只看周额度和 5 小时。
        let rows = parse_usage(&json!({
            "weeklyTokensUsed": 25_000,
            "weeklyTokenLimit": 100_000,
            "fiveHTokensUsed": 50,
            "fiveHTokenLimit": 100,
            "cloudAgentsUsed": 8,
            "cloudAgentLimit": 8,
            "credits": { "remaining": 10.0 }
        }))
        .unwrap();
        let (percent, label) = headline(&rows);
        assert_eq!(percent, Some(50.0));
        assert_eq!(label.as_deref(), Some("5 小时窗口"));
    }

    #[test]
    fn headline_prefers_the_tighter_window() {
        let rows = parse_usage(&json!({
            "weeklyTokensUsed": 95_000,
            "weeklyTokenLimit": 100_000,
            "fiveHTokensUsed": 0,
            "fiveHTokenLimit": 100
        }))
        .unwrap();
        let (percent, label) = headline(&rows);
        assert_eq!(percent, Some(5.0));
        assert_eq!(label.as_deref(), Some("周额度"));
    }

    #[test]
    fn missing_required_usage_field_is_an_error_and_optional_groups_are_omitted() {
        let error = parse_usage(&json!({
            "weeklyTokensUsed": 1,
            "weeklyTokenLimit": 10,
            "fiveHTokenLimit": 20
        }))
        .err()
        .unwrap();
        assert_eq!(error, "parse: missing fiveHTokensUsed");

        let rows = parse_usage(&json!({
            "weeklyTokensUsed": 1,
            "weeklyTokenLimit": 10,
            "fiveHTokensUsed": 2,
            "fiveHTokenLimit": 20,
            "credits": {}
        }))
        .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn wrong_required_field_type_is_an_error() {
        let error = parse_usage(&json!({
            "weeklyTokensUsed": "1",
            "weeklyTokenLimit": 10,
            "fiveHTokensUsed": 2,
            "fiveHTokenLimit": 20
        }))
        .err()
        .unwrap();
        assert_eq!(error, "parse: invalid weeklyTokensUsed");
    }
}
