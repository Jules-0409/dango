use super::auth::load_cursor_auth;
use super::time::parse_iso_to_unix;
use super::usage::{fetch_grok_usage, fetch_usage_summary};
use super::CursorQuota;

pub async fn query_cursor_quota(client: &reqwest::Client) -> CursorQuota {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // load_cursor_auth 是同步阻塞的（读磁盘、打开 SQLite、spawn `security`），
    // 丢到阻塞线程池，别占住 tokio 的 worker 线程把并发拉成串行
    let auth = match tokio::task::spawn_blocking(load_cursor_auth).await {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            return CursorQuota {
                ok: false,
                id: "cursor".into(),
                name: "Cursor".into(),
                fetched_at: now,
                error: Some(e),
                ..Default::default()
            };
        }
        Err(join_err) => {
            return CursorQuota {
                ok: false,
                id: "cursor".into(),
                name: "Cursor".into(),
                fetched_at: now,
                error: Some(format!("cursor.auth_task_failed: {join_err}")),
                ..Default::default()
            };
        }
    };

    // 两个接口并发拉：任一方失败都不该把另一方的数据一起带走，
    // 所以错误分别记在 error（Cursor）和 grok_error（Grok Bot）上
    let (summary, grok) = tokio::join!(
        fetch_usage_summary(client, &auth),
        fetch_grok_usage(client, &auth.token),
    );

    let summary_err = summary.as_ref().err().cloned();
    // 失败时用 Null 兜底，下面整条 val.get(..) 链会自然全部得到 None，不必每处判空
    let val = summary.unwrap_or(serde_json::Value::Null);

    let membership_type = val
        .get("membershipType")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| auth.membership_type.clone());

    let cycle_start = val
        .get("billingCycleStart")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let cycle_end = val
        .get("billingCycleEnd")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());

    let cycle_reset_unix = cycle_end.as_deref().and_then(parse_iso_to_unix);

    let auto_message = val
        .get("autoModelSelectedDisplayMessage")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let api_message = val
        .get("namedModelSelectedDisplayMessage")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());

    let plan_obj = val.get("individualUsage").and_then(|u| u.get("plan"));

    let auto_percent_used = plan_obj
        .and_then(|p| p.get("autoPercentUsed"))
        .and_then(|v| v.as_f64());
    let api_percent_used = plan_obj
        .and_then(|p| p.get("apiPercentUsed"))
        .and_then(|v| v.as_f64());
    let total_percent_used = plan_obj
        .and_then(|p| p.get("totalPercentUsed"))
        .and_then(|v| v.as_f64());

    let fast_requests_used = plan_obj
        .and_then(|p| p.get("used"))
        .and_then(|v| v.as_i64());
    let fast_requests_limit = plan_obj
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_i64());

    let remaining_pct = if let Some(auto_pct) = auto_percent_used {
        Some((100.0 - auto_pct).clamp(0.0, 100.0))
    } else if let (Some(used), Some(limit)) = (fast_requests_used, fast_requests_limit) {
        if limit > 0 {
            let used_pct = (used as f64 / limit as f64) * 100.0;
            Some((100.0 - used_pct).clamp(0.0, 100.0))
        } else {
            None
        }
    } else {
        None
    };

    CursorQuota {
        // usage-summary 和 Grok 任一取到数就算这张卡可用，UI 会逐格判断各自有没有值
        ok: !val.is_null() || grok.percent_used.is_some(),
        id: "cursor".into(),
        name: "Cursor".into(),
        plan_name: membership_type.clone().or_else(|| Some("Pro".into())),
        email: auth.email.as_deref().map(crate::models::mask_email),
        user_id: if auth.user_id.is_empty() {
            None
        } else {
            Some(auth.user_id)
        },
        membership_type,
        subscription_status: auth.subscription_status,
        auto_percent_used,
        api_percent_used,
        total_percent_used,
        fast_requests_used,
        fast_requests_limit,
        fast_requests_remaining_percent: remaining_pct,
        cycle_start,
        cycle_end,
        cycle_reset_unix,
        auto_message,
        api_message,
        grok_percent_used: grok.percent_used,
        grok_has_available_usage: grok.has_available_usage,
        grok_reset_unix: grok.reset_unix,
        grok_error: grok.error,
        fetched_at: now,
        error: summary_err,
    }
}
