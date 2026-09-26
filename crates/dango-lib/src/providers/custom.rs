//! User-added balls: API providers whose account balance has a documented
//! endpoint. The API key comes from the `dango` keychain slot of the
//! ball's id (`manual_creds`), is sent only to that provider, and never
//! appears in errors or logs.
//!
//! | kind | endpoint | balance field |
//! |---|---|---|
//! | deepseek | `GET api.deepseek.com/user/balance` | `balance_infos[].total_balance` |
//! | moonshot | `GET api.moonshot.cn/v1/users/me/balance` | `data.available_balance` |
//! | stepfun | `GET api.stepfun.com/v1/accounts` | `balance` |
//! | openrouter | `GET openrouter.ai/api/v1/credits` | `data.total_credits - data.total_usage` |
//! | siliconflow | `GET api.siliconflow.cn/v1/user/info` | `data.totalBalance` |
//!
//! Step Plan (StepFun's coding subscription) exposes no quota API, so a
//! StepFun ball shows the pay-as-you-go wallet only.

use serde_json::Value;

use crate::models::{Bucket, PlanQuota};
use crate::settings::CustomPlan;

/// What one template knows how to do.
pub struct Template {
    pub kind: &'static str,
    pub url: &'static str,
}

pub const TEMPLATES: &[Template] = &[
    Template {
        kind: "deepseek",
        url: "https://api.deepseek.com/user/balance",
    },
    Template {
        kind: "moonshot",
        url: "https://api.moonshot.cn/v1/users/me/balance",
    },
    Template {
        kind: "stepfun",
        url: "https://api.stepfun.com/v1/accounts",
    },
    Template {
        kind: "openrouter",
        url: "https://openrouter.ai/api/v1/credits",
    },
    Template {
        kind: "siliconflow",
        url: "https://api.siliconflow.cn/v1/user/info",
    },
];

/// A balance in some currency.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    pub amount: f64,
    pub currency: &'static str,
    /// OpenRouter reports what was bought: then the % needs no budget.
    pub total: Option<f64>,
}

/// Numbers arrive as JSON numbers or strings depending on the vendor.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn currency_symbol(code: &str) -> &'static str {
    match code.to_ascii_uppercase().as_str() {
        "CNY" | "RMB" => "¥",
        _ => "$",
    }
}

pub fn parse_balance(kind: &str, body: &Value) -> Result<Balance, String> {
    let missing = || format!("{kind}: 响应里没有余额字段（接口可能改了）");
    match kind {
        "deepseek" => {
            let infos = body
                .get("balance_infos")
                .and_then(|infos| infos.as_array())
                .ok_or_else(missing)?;
            // Prefer CNY when the account holds several currencies.
            let info = infos
                .iter()
                .find(|info| info.get("currency").and_then(Value::as_str) == Some("CNY"))
                .or_else(|| infos.first())
                .ok_or_else(missing)?;
            Ok(Balance {
                amount: number(info.get("total_balance")).ok_or_else(missing)?,
                currency: currency_symbol(
                    info.get("currency")
                        .and_then(Value::as_str)
                        .unwrap_or("CNY"),
                ),
                total: None,
            })
        }
        "moonshot" => Ok(Balance {
            amount: number(body.pointer("/data/available_balance")).ok_or_else(missing)?,
            currency: "¥",
            total: None,
        }),
        "stepfun" => Ok(Balance {
            amount: number(body.get("balance")).ok_or_else(missing)?,
            currency: "¥",
            total: None,
        }),
        "openrouter" => {
            let total = number(body.pointer("/data/total_credits")).ok_or_else(missing)?;
            let used = number(body.pointer("/data/total_usage")).ok_or_else(missing)?;
            Ok(Balance {
                amount: total - used,
                currency: "$",
                total: Some(total),
            })
        }
        "siliconflow" => Ok(Balance {
            amount: number(body.pointer("/data/totalBalance"))
                .or_else(|| number(body.pointer("/data/balance")))
                .ok_or_else(missing)?,
            currency: "¥",
            total: None,
        }),
        other => Err(format!("未知的小球类型 {other}")),
    }
}

fn format_amount(balance: &Balance) -> String {
    format!("{}{:.2}", balance.currency, balance.amount)
}

/// Balance → ball. Without a budget (and not OpenRouter) there is no % —
/// the ring stays quiet and the balance is the subtitle; never a made-up %.
pub fn plan_from_balance(plan: &CustomPlan, balance: &Balance) -> PlanQuota {
    let full = balance.total.or(plan.budget.map(|budget| budget as f64));
    let percent = full
        .filter(|full| *full > 0.0)
        .map(|full| (balance.amount / full * 100.0).clamp(0.0, 100.0));
    let amount = format_amount(balance);
    PlanQuota {
        id: plan.id.clone(),
        name: plan.name.clone(),
        ok: true,
        error: None,
        remaining_percent: percent,
        buckets: vec![Bucket {
            label: "余额".into(),
            remaining_percent: percent,
            detail: Some(match full {
                Some(full) => format!("{amount} / {}{full:.0}", balance.currency),
                None => amount.clone(),
            }),
            resets_at: None,
            pool: None,
            window: Some("total".into()),
        }],
        note: None,
        proxy: None,
        headline_label: Some(format!("余额 {amount}")),
        resets_at: None,
    }
}

fn failed(plan: &CustomPlan, error: String) -> PlanQuota {
    PlanQuota {
        id: plan.id.clone(),
        name: plan.name.clone(),
        ok: false,
        error: Some(error),
        remaining_percent: None,
        buckets: vec![],
        note: None,
        proxy: None,
        headline_label: None,
        resets_at: None,
    }
}

async fn fetch_one(client: &reqwest::Client, plan: &CustomPlan) -> PlanQuota {
    let Some(template) = TEMPLATES.iter().find(|t| t.kind == plan.kind) else {
        return failed(plan, format!("未知的小球类型 {}", plan.kind));
    };
    let id = plan.id.clone();
    let key = match tokio::task::spawn_blocking(move || crate::manual_creds::get(&id)).await {
        Ok(Some(key)) => key,
        Ok(None) => return failed(plan, "custom.no_key: 还没填 API Key".into()),
        Err(error) => return failed(plan, format!("custom.keychain: {error}")),
    };
    let sent = crate::probes::http::send_with_retry(|| {
        client
            .get(template.url)
            .bearer_auth(&key)
            .timeout(std::time::Duration::from_secs(10))
    })
    .await;
    let resp = match sent {
        Ok(resp) => resp,
        // reqwest errors carry the URL, never headers: safe to show.
        Err(error) => return failed(plan, format!("custom.net_failed: {error}")),
    };
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return failed(plan, "custom.auth: API Key 无效或没有查余额的权限".into());
    }
    if !status.is_success() {
        return failed(plan, format!("HTTP {status}"));
    }
    match resp.json::<Value>().await {
        Ok(body) => match parse_balance(&plan.kind, &body) {
            Ok(balance) => plan_from_balance(plan, &balance),
            Err(error) => failed(plan, error),
        },
        Err(error) => failed(plan, format!("custom.json_parse_failed: {error}")),
    }
}

/// All user-added balls, fetched concurrently.
pub async fn fetch_all(client: &reqwest::Client, plans: &[CustomPlan]) -> Vec<PlanQuota> {
    futures_util::future::join_all(plans.iter().map(|plan| fetch_one(client, plan))).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(kind: &str, budget: Option<u64>) -> CustomPlan {
        CustomPlan {
            id: format!("custom-{kind}"),
            kind: kind.into(),
            name: kind.into(),
            budget,
        }
    }

    #[test]
    fn every_template_parses_its_documented_shape() {
        let cases = [
            (
                "deepseek",
                json!({"is_available": true, "balance_infos": [
                    {"currency": "USD", "total_balance": "1.00"},
                    {"currency": "CNY", "total_balance": "110.53", "granted_balance": "0.00"}]}),
                110.53,
                "¥",
            ),
            (
                "moonshot",
                json!({"code": 0, "data": {"available_balance": 49.58894, "voucher_balance": 46.58893}}),
                49.58894,
                "¥",
            ),
            (
                "stepfun",
                json!({"type": "account", "balance": 23.5, "total_cash_balance": 20.0}),
                23.5,
                "¥",
            ),
            (
                "openrouter",
                json!({"data": {"total_credits": 50.0, "total_usage": 12.5}}),
                37.5,
                "$",
            ),
            (
                "siliconflow",
                json!({"code": 20000, "data": {"balance": "0.88", "totalBalance": "14.88"}}),
                14.88,
                "¥",
            ),
        ];
        for (kind, body, amount, currency) in cases {
            let balance = parse_balance(kind, &body).unwrap();
            assert!((balance.amount - amount).abs() < 1e-9, "{kind}");
            assert_eq!(balance.currency, currency, "{kind}");
        }
        for template in TEMPLATES {
            assert!(crate::settings::CUSTOM_KINDS.contains(&template.kind));
        }
    }

    #[test]
    fn a_changed_shape_is_an_error_not_zero() {
        assert!(parse_balance("deepseek", &json!({"balance": 3})).is_err());
        assert!(parse_balance("moonshot", &json!({"data": {}})).is_err());
    }

    #[test]
    fn percent_only_with_a_budget_or_a_known_total() {
        let balance = Balance {
            amount: 25.0,
            currency: "¥",
            total: None,
        };
        let no_budget = plan_from_balance(&plan("deepseek", None), &balance);
        assert_eq!(no_budget.remaining_percent, None);
        assert_eq!(no_budget.headline_label.as_deref(), Some("余额 ¥25.00"));
        let with_budget = plan_from_balance(&plan("deepseek", Some(100)), &balance);
        assert_eq!(with_budget.remaining_percent, Some(25.0));
        let openrouter = plan_from_balance(
            &plan("openrouter", None),
            &Balance {
                amount: 37.5,
                currency: "$",
                total: Some(50.0),
            },
        );
        assert_eq!(openrouter.remaining_percent, Some(75.0));
    }
}
