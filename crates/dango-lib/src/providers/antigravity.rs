// Antigravity Bridge（Gemini）：本机 127.0.0.1:8050 的 /quota 提供额度；
// /healthz 带账号池、breakers 和可能存在的额度缓存。
// /quota 不带 ?all=1 时桥只返回 current 账号；带 ?all=1 才返回全部账号，
// 单个账号查询失败时 groups 为空并带 error 字段——不影响其他账号。
use crate::models::{mask_email, timestamp_to_millis, Bucket, PlanQuota, ProxyStatus};
use crate::proxies::{antigravity_from_healthz, antigravity_unavailable};

const QUOTA_URL: &str = "http://127.0.0.1:8050/quota?all=1";
const HEALTHZ_URL: &str = "http://127.0.0.1:8050/healthz";

/// 一组桶的小写窗口名（桥会带 window: "weekly"/"5h"，缺省时从 label 猜）规范化为
/// 统一的 window 口径："5h" | "day" | "week" | "month" | "total" | "other"。
pub(crate) fn normalize_window(group: &str, bucket: &serde_json::Value) -> Option<String> {
    let raw = bucket
        .get("window")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            bucket
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .to_lowercase();
    let group = group.to_lowercase();
    let text = format!("{group} {raw}");
    if text.contains("5h") || text.contains("five hour") || text.contains("5 hour") {
        Some("5h".into())
    } else if text.contains("week") {
        Some("week".into())
    } else if text.contains("daily") || text.contains("day") {
        Some("day".into())
    } else if text.contains("month") {
        Some("month".into())
    } else if text.contains("total") || text.contains("总额") {
        Some("total".into())
    } else {
        Some("other".into())
    }
}

/// group 名（英文、口语化）映射成短 pool 名。
fn pool_short(group: &str) -> String {
    let g = group.to_lowercase();
    if g.contains("gemini") {
        "Gemini".into()
    } else if g.contains("claude") || g.contains("gpt") {
        "Claude & GPT".into()
    } else {
        group.to_string()
    }
}

fn bucket_label(window: Option<&str>) -> String {
    match window {
        Some("5h") => "5 小时".into(),
        Some("week") => "周".into(),
        Some("day") => "日".into(),
        Some("month") => "月".into(),
        _ => "?".into(),
    }
}

fn empty_plan() -> PlanQuota {
    PlanQuota {
        id: "antigravity".into(),
        name: "Gemini".into(),
        ok: false,
        error: None,
        remaining_percent: None,
        buckets: vec![],
        note: None,
        proxy: None,
        headline_label: None,
        resets_at: None,
    }
}

/// One account's parse: its rows (no account prefix) and its Gemini-pool
/// headline `min(周, 5h)` with the matching reset time.
struct AccountQuota {
    masked: String,
    current: bool,
    rows: Vec<Bucket>,
    gemini: Option<(f64, Option<u64>)>,
}

fn parse_account(account: &serde_json::Value, index: usize) -> Option<AccountQuota> {
    let has_error = account
        .get("error")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|message| !message.is_empty());
    let groups: &[serde_json::Value] = account["groups"].as_array().map_or(&[], Vec::as_slice);
    if has_error || groups.is_empty() {
        return None;
    }
    let masked = account
        .get("email")
        .and_then(serde_json::Value::as_str)
        .map(mask_email)
        .filter(|masked| masked != "账号")
        .unwrap_or_else(|| format!("账号{}", index + 1));
    let mut rows = vec![];
    let mut weekly: Option<(f64, Option<u64>)> = None;
    let mut five_hour: Option<(f64, Option<u64>)> = None;
    for group in groups {
        let group_name = group["name"].as_str().unwrap_or("");
        let pool = pool_short(group_name);
        let Some(buckets) = group["buckets"].as_array() else {
            continue;
        };
        for bucket in buckets {
            let remaining_percent = bucket["remainingPercent"].as_f64();
            let resets_at = bucket.get("resetTime").and_then(timestamp_to_millis);
            let window = normalize_window(group_name, bucket);
            if pool == "Gemini" {
                match (window.as_deref(), remaining_percent) {
                    (Some("week"), Some(p)) => weekly = Some((p, resets_at)),
                    (Some("5h"), Some(p)) => five_hour = Some((p, resets_at)),
                    _ => {}
                }
            }
            rows.push(Bucket {
                label: format!("{} · {}", pool, bucket_label(window.as_deref())),
                remaining_percent,
                detail: bucket["description"].as_str().map(str::to_string),
                resets_at,
                pool: Some(pool.clone()),
                window,
            });
        }
    }
    let gemini = [weekly, five_hour]
        .into_iter()
        .flatten()
        .min_by(|(a, _), (b, _)| a.total_cmp(b));
    Some(AccountQuota {
        masked,
        current: account["current"].as_bool() == Some(true),
        rows,
        gemini,
    })
}

/// The bridge rotates accounts by itself, so the card shows only the one
/// in use (`current`). If that one failed to report, fall back to the
/// healthiest account and say so in the note — never a stale number.
fn parse_quota(value: &serde_json::Value) -> PlanQuota {
    let mut plan = empty_plan();
    let accounts = match value["accounts"].as_array() {
        Some(accounts) if !accounts.is_empty() => accounts,
        _ => {
            plan.error = Some("bridge 没有账号".into());
            return plan;
        }
    };
    let parsed: Vec<AccountQuota> = accounts
        .iter()
        .enumerate()
        .filter_map(|(index, account)| parse_account(account, index))
        .collect();
    let failed = accounts.len() - parsed.len();
    let current_failed = accounts
        .iter()
        .any(|a| a["current"].as_bool() == Some(true))
        && !parsed.iter().any(|a| a.current);

    let shown = parsed.iter().find(|a| a.current).or_else(|| {
        parsed.iter().max_by(|a, b| {
            let pa = a.gemini.map_or(-1.0, |g| g.0);
            let pb = b.gemini.map_or(-1.0, |g| g.0);
            pa.total_cmp(&pb)
        })
    });

    let mut note = format!("{} 个账号", accounts.len());
    if let Some(shown) = shown {
        note.push_str(if shown.current {
            " · 在用 "
        } else {
            " · 显示 "
        });
        note.push_str(&shown.masked);
    }
    if current_failed {
        note.push_str(" · 当前账号查询失败");
    } else if failed > 0 {
        note.push_str(&format!(" · {failed} 个查询失败"));
    }
    plan.note = Some(note);

    let Some(shown) = shown.filter(|a| !a.rows.is_empty()) else {
        plan.error = Some("bridge 没返回 buckets".into());
        return plan;
    };
    match shown.gemini {
        Some((percent, resets)) => {
            plan.remaining_percent = Some(percent);
            plan.resets_at = resets;
            plan.headline_label = Some("Gemini 池".into());
        }
        None => {
            // No Gemini numbers on this account: tightest window, honestly named.
            plan.remaining_percent = shown
                .rows
                .iter()
                .filter_map(|bucket| bucket.remaining_percent)
                .reduce(f64::min);
            plan.headline_label = Some("最小窗口".into());
        }
    }
    plan.buckets = shown.rows.clone();
    plan.ok = true;
    plan
}

fn failed_plan(error: String) -> PlanQuota {
    let mut plan = empty_plan();
    plan.error = Some(error);
    plan
}

/// Fetch Gemini quota and Bridge proxy status together.
///
/// `/healthz` contains cached per-account quota rows, but those may be absent or stale;
/// `/quota` remains the authoritative live quota response. Both requests run in parallel.
pub async fn fetch_with_proxy(client: &reqwest::Client) -> (PlanQuota, ProxyStatus) {
    let quota_request = client
        .get(QUOTA_URL)
        .timeout(std::time::Duration::from_secs(8))
        .send();
    let health_request = client
        .get(HEALTHZ_URL)
        .timeout(std::time::Duration::from_secs(3))
        .send();
    let (quota_result, health_result) = tokio::join!(quota_request, health_request);

    let plan = match quota_result {
        Ok(response) if !response.status().is_success() => {
            failed_plan(format!("bridge http {}", response.status().as_u16()))
        }
        Ok(response) => match response.json::<serde_json::Value>().await {
            Ok(value) => parse_quota(&value),
            Err(_) => failed_plan("bridge 返回无效 JSON".into()),
        },
        Err(error) => {
            let message = if error.is_timeout() {
                "bridge 请求超时"
            } else {
                "bridge 连接失败"
            };
            failed_plan(message.into())
        }
    };

    let proxy = match health_result {
        Ok(response) if !response.status().is_success() => {
            antigravity_unavailable(format!("HTTP {}", response.status().as_u16()))
        }
        Ok(response) => match response.json::<serde_json::Value>().await {
            Ok(value) => antigravity_from_healthz(&value),
            Err(_) => antigravity_unavailable("无效 JSON"),
        },
        Err(error) => antigravity_unavailable(if error.is_timeout() {
            "连接超时"
        } else {
            "连接失败"
        }),
    };

    (plan, proxy)
}

/// Compatibility API used by the existing app; callers can migrate to
/// [`fetch_with_proxy`] when ready to attach the Bridge status in the same refresh.
pub async fn fetch(client: &reqwest::Client) -> PlanQuota {
    fetch_with_proxy(client).await.0
}

#[cfg(test)]
mod tests {
    use super::parse_quota;
    use serde_json::json;

    fn account(
        email: &str,
        current: bool,
        weekly: f64,
        five_hour: f64,
        reset: &str,
    ) -> serde_json::Value {
        json!({
            "current": current,
            "email": email,
            "groups": [
                {
                    "name": "Gemini Models",
                    "buckets": [
                        {"label": "Weekly Limit Remaining", "remainingPercent": weekly, "resetTime": reset, "window": "weekly"},
                        {"label": "Five Hour Limit Remaining", "remainingPercent": five_hour, "resetTime": "2026-09-26T06:52:53Z", "window": "5h"}
                    ]
                },
                {
                    "name": "Claude and GPT models",
                    "buckets": [
                        {"label": "Weekly Limit Remaining", "remainingPercent": 100.0, "resetTime": "2026-10-03T01:53:01Z", "window": "weekly"}
                    ]
                }
            ]
        })
    }

    #[test]
    fn quota_bucket_reset_time_maps_to_milliseconds() {
        let plan = parse_quota(&json!({
            "accounts": [{
                "current": true,
                "groups": [{
                    "name": "Gemini Models",
                    "buckets": [{
                        "label": "5h",
                        "remainingPercent": 75.0,
                        "description": "available",
                        "resetTime": "2026-09-24T00:00:00Z"
                    }]
                }]
            }]
        }));
        assert_eq!(plan.buckets.len(), 1);
        assert_eq!(plan.buckets[0].resets_at, Some(1_790_208_000_000));
        assert_eq!(plan.buckets[0].window.as_deref(), Some("5h"));
        assert_eq!(plan.buckets[0].pool.as_deref(), Some("Gemini"));
    }

    #[test]
    fn only_the_account_in_use_is_shown() {
        let plan = parse_quota(&json!({
            "accounts": [
                account("env@gmail.com", false, 69.7, 100.0, "2026-09-30T02:55:19Z"),
                account("omni@gmail.com", true, 40.0, 90.0, "2026-09-30T02:54:08Z")
            ]
        }));
        assert!(plan.ok);
        // 只有在用账号的 3 个桶，标签不带账号前缀
        assert_eq!(plan.buckets.len(), 3);
        assert_eq!(plan.note.as_deref(), Some("2 个账号 · 在用 o***@g***"));
        assert_eq!(plan.remaining_percent, Some(40.0));
        assert_eq!(plan.headline_label.as_deref(), Some("Gemini 池"));
        assert_eq!(plan.resets_at, Some(1_790_736_848_000));
        let labels: Vec<&str> = plan.buckets.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Gemini · 周", "Gemini · 5 小时", "Claude & GPT · 周"]
        );
        assert_eq!(plan.buckets[0].pool.as_deref(), Some("Gemini"));
        assert_eq!(plan.buckets[0].window.as_deref(), Some("week"));
    }

    #[test]
    fn failed_current_account_falls_back_to_healthiest_and_says_so() {
        let plan = parse_quota(&json!({
            "accounts": [
                {"current": true, "email": "dead@gmail.com", "error": "quota fetch failed"},
                account("omni@gmail.com", false, 55.0, 80.0, "2026-09-30T02:54:08Z"),
                account("zed@gmail.com", false, 20.0, 80.0, "2026-09-30T02:54:08Z")
            ]
        }));
        assert!(plan.ok);
        assert_eq!(
            plan.note.as_deref(),
            Some("3 个账号 · 显示 o***@g*** · 当前账号查询失败")
        );
        assert_eq!(plan.remaining_percent, Some(55.0));
        assert_eq!(plan.buckets.len(), 3);
    }

    #[test]
    fn other_account_failure_is_counted_in_the_note() {
        let plan = parse_quota(&json!({
            "accounts": [
                {"current": false, "email": "dead@gmail.com", "error": "quota fetch failed"},
                account("omni@gmail.com", true, 55.0, 80.0, "2026-09-30T02:54:08Z")
            ]
        }));
        assert_eq!(
            plan.note.as_deref(),
            Some("2 个账号 · 在用 o***@g*** · 1 个查询失败")
        );
        assert_eq!(plan.remaining_percent, Some(55.0));
    }

    #[test]
    fn all_accounts_failing_is_plan_failure() {
        let plan = parse_quota(&json!({
            "accounts": [
                {"current": false, "error": "boom"},
                {"current": true, "groups": []}
            ]
        }));
        assert!(!plan.ok);
        assert_eq!(plan.error.as_deref(), Some("bridge 没返回 buckets"));
        assert_eq!(plan.note.as_deref(), Some("2 个账号 · 当前账号查询失败"));
        assert!(plan.remaining_percent.is_none());
    }

    #[test]
    fn headline_prefers_weekly_reset_when_weekly_is_tighter() {
        // 5 小时更紧时 resets_at 用 5h 的；周更紧时用周的。
        let plan = parse_quota(&json!({
            "accounts": [account("omni@gmail.com", true, 90.0, 10.0, "2026-09-30T02:54:08Z")]
        }));
        // min(90, 10) = 10 → 5 小时窗口，resets_at 取 5h 的 resetTime
        assert_eq!(plan.remaining_percent, Some(10.0));
        assert_eq!(plan.resets_at, Some(1_790_405_573_000));
    }
}
