//! Cursor's own per-request usage ledger (`/api/dashboard/get-filtered-usage-events`,
//! what the dashboard's usage table shows): every Cursor App / CLI Agent /
//! Grok Bot call with its model and token counts. Read-only, same login as
//! `usage-summary`.

use super::auth::load_cursor_auth;
use crate::tokens::Counts;

const PAGE_SIZE: usize = 100;
/// Hard stop per sync: 100 pages = 10 000 events (a heavy month is ~7 000;
/// later syncs only walk back to the previous one, usually a single page).
const MAX_PAGES: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub struct CursorUsageEvent {
    pub at_ms: i64,
    pub model: String,
    pub counts: Counts,
    /// Stable dedupe key (the API has no event id).
    pub key: String,
}

/// Events in `[since_ms, until_ms]`, newest first. Stops at the first page
/// that reaches past `since_ms`, an empty page, or [`MAX_PAGES`].
pub async fn fetch_usage_events(
    client: &reqwest::Client,
    since_ms: i64,
    until_ms: i64,
) -> Result<Vec<CursorUsageEvent>, String> {
    let auth = tokio::task::spawn_blocking(load_cursor_auth)
        .await
        .map_err(|error| format!("cursor.auth_task_failed: {error}"))??;
    let cookie_user = if auth.auth_id.is_empty() {
        auth.user_id.clone()
    } else {
        auth.auth_id.clone()
    };
    let mut events = Vec::new();
    for page in 1..=MAX_PAGES {
        let body = serde_json::json!({
            "teamId": 0,
            "startDate": since_ms.to_string(),
            "endDate": until_ms.to_string(),
            "page": page,
            "pageSize": PAGE_SIZE,
        });
        let resp = crate::probes::http::send_with_retry(|| {
            client
                .post("https://cursor.com/api/dashboard/get-filtered-usage-events")
                .header("Authorization", format!("Bearer {}", auth.token))
                .header("Origin", "https://cursor.com")
                .header(
                    "Cookie",
                    format!("WorkosCursorSessionToken={cookie_user}%3A%3A{}", auth.token),
                )
                .json(&body)
                .timeout(std::time::Duration::from_secs(15))
        })
        .await
        .map_err(|error| format!("cursor.net_failed: {error}"))?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err("cursor.auth_expired".into());
        }
        if !status.is_success() {
            return Err(format!("cursor usage events HTTP {status}"));
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|error| format!("cursor.json_parse_failed: {error}"))?;
        let (page_events, raw_len) = parse_events(&value)?;
        // Count the raw page: events without tokens are dropped by the parser
        // but still fill the page.
        let short = raw_len < PAGE_SIZE;
        events.extend(page_events);
        if short {
            break;
        }
    }
    Ok(events)
}

/// One page of the response. A body without the list is a parse error, not
/// an empty page (never report "no usage" when the shape changed).
/// Returns the events with tokens and the page's raw length.
pub fn parse_events(value: &serde_json::Value) -> Result<(Vec<CursorUsageEvent>, usize), String> {
    // A window with no events comes back as `{}` (or with a zero count).
    let empty = value.as_object().is_some_and(|object| object.is_empty())
        || value.get("totalUsageEventsCount").and_then(|n| n.as_u64()) == Some(0);
    if empty && value.get("usageEventsDisplay").is_none() {
        return Ok((Vec::new(), 0));
    }
    let list = value
        .get("usageEventsDisplay")
        .and_then(|list| list.as_array())
        .ok_or("cursor usage events: 响应里没有 usageEventsDisplay")?;
    Ok((list.iter().filter_map(parse_event).collect(), list.len()))
}

fn parse_event(event: &serde_json::Value) -> Option<CursorUsageEvent> {
    let at_ms = match event.get("timestamp")? {
        serde_json::Value::String(text) => text.parse().ok()?,
        other => other.as_i64()?,
    };
    let usage = event.get("tokenUsage")?;
    let num = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let counts = Counts {
        input: num("inputTokens"),
        output: num("outputTokens"),
        cache_read: num("cacheReadTokens"),
        cache_write: num("cacheWriteTokens"),
    };
    if counts.total() == 0 {
        return None;
    }
    let model = event
        .get("model")
        .and_then(|model| model.as_str())
        .unwrap_or("unknown")
        .to_string();
    let conversation = event
        .get("conversationId")
        .and_then(|id| id.as_str())
        .unwrap_or("");
    let key = format!(
        "{at_ms}:{conversation}:{model}:{}:{}:{}:{}",
        counts.input, counts.output, counts.cache_read, counts.cache_write
    );
    Some(CursorUsageEvent {
        at_ms,
        model,
        counts,
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_parse_and_calls_without_tokens_are_skipped() {
        let body = serde_json::json!({
            "totalUsageEventsCount": 2,
            "usageEventsDisplay": [
                {"timestamp": "1790386964971", "model": "grok-bot-automation",
                 "conversationId": "c1",
                 "tokenUsage": {"inputTokens": 888, "outputTokens": 181, "cacheReadTokens": 34752, "totalCents": 2.02}},
                {"timestamp": "1790386900000", "model": "auto", "kind": "USAGE_EVENT_KIND_ERRORED_NOT_CHARGED"}
            ]
        });
        let (events, raw_len) = parse_events(&body).unwrap();
        assert_eq!((events.len(), raw_len), (1, 2));
        assert_eq!(events[0].at_ms, 1_790_386_964_971);
        assert_eq!(events[0].counts.cache_read, 34_752);
        assert_eq!(events[0].model, "grok-bot-automation");
    }

    #[test]
    fn a_changed_shape_is_an_error_not_zero_usage() {
        assert!(parse_events(&serde_json::json!({"events": []})).is_err());
    }

    #[test]
    fn an_empty_window_is_no_events() {
        assert_eq!(parse_events(&serde_json::json!({})).unwrap().1, 0);
        assert_eq!(
            parse_events(&serde_json::json!({"totalUsageEventsCount": 0}))
                .unwrap()
                .1,
            0
        );
    }
}
