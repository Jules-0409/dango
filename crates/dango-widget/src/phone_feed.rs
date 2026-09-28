//! `GET /feed`: the snapshot trimmed down for the iPhone widget.
//!
//! The phone never talks to vendors: something on this Mac (a launchd job)
//! copies this JSON somewhere the phone can read. So it carries only what
//! a glance needs — name, colour, shape, face, percentages, reset times —
//! and a human hint instead of the raw error. No tokens, accounts, emails,
//! URLs or proxy details.

use dango_lib::{PlanQuota, Settings, Snapshot};
use serde::Serialize;

use crate::theme;

pub const FEED_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Feed {
    pub v: u32,
    /// When the Mac last fetched the quotas (unix seconds; reset times are ms).
    pub fetched_at: u64,
    pub balls: Vec<FeedBall>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedBall {
    pub id: String,
    pub name: String,
    pub color: String,
    pub shape: String,
    /// grok-ball emotion id (10 / 19 / 12 / 34 / 02), same rule as the capsule.
    pub emotion: &'static str,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    pub buckets: Vec<FeedBucket>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedBucket {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

/// On the phone there is no "raw error below"; point back at the Mac.
fn phone_hint(error: Option<&str>) -> &'static str {
    match theme::error_hint(error) {
        "查询失败，详情见下方原始错误" => "查询失败，到电脑上看详情",
        hint => hint,
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn ball(plan: &PlanQuota, settings: &Settings) -> FeedBall {
    let shape = serde_json::to_value(theme::shape_for(&plan.id, settings))
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "blob".into());
    FeedBall {
        id: plan.id.clone(),
        name: plan.name.clone(),
        color: theme::color_for_plan(&plan.id, settings),
        shape,
        emotion: theme::emotion_for(plan),
        ok: plan.ok,
        percent: plan
            .ok
            .then_some(plan.remaining_percent)
            .flatten()
            .map(round1),
        label: plan.ok.then(|| plan.headline_label.clone()).flatten(),
        hint: (!plan.ok).then(|| phone_hint(plan.error.as_deref())),
        resets_at: plan.resets_at,
        buckets: if plan.ok {
            plan.buckets
                .iter()
                .map(|bucket| FeedBucket {
                    label: match &bucket.pool {
                        Some(pool) => format!("{pool} · {}", bucket.label),
                        None => bucket.label.clone(),
                    },
                    percent: bucket.remaining_percent.map(round1),
                    resets_at: bucket.resets_at,
                })
                .collect()
        } else {
            Vec::new()
        },
    }
}

pub fn build(snapshot: &Snapshot, settings: &Settings) -> Feed {
    Feed {
        v: FEED_VERSION,
        fetched_at: snapshot.fetched_at,
        balls: theme::ordered_plans(&snapshot.plans, settings)
            .iter()
            .map(|plan| ball(plan, settings))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dango_lib::Bucket;

    fn plan(id: &str) -> PlanQuota {
        PlanQuota {
            id: id.into(),
            name: "Claude".into(),
            ok: true,
            error: None,
            remaining_percent: Some(47.04),
            buckets: vec![Bucket {
                label: "5 小时".into(),
                remaining_percent: Some(93.0),
                detail: Some("account foo@example.com".into()),
                resets_at: Some(1_790_000_000_000),
                pool: None,
                window: Some("5h".into()),
            }],
            note: Some("桌面端 10 分钟前采样".into()),
            proxy: None,
            headline_label: Some("7 天窗口".into()),
            resets_at: None,
        }
    }

    #[test]
    fn a_healthy_ball_keeps_numbers_and_drops_everything_else() {
        let snapshot = Snapshot {
            plans: vec![plan("claude")],
            fetched_at: 42,
        };
        let feed = build(&snapshot, &Settings::default());
        let text = serde_json::to_string(&feed).unwrap();
        assert!(!text.contains("example.com"), "bucket detail leaked");
        assert!(!text.contains("采样"), "note leaked");
        let ball = &feed.balls[0];
        assert_eq!(ball.percent, Some(47.0));
        assert_eq!(ball.emotion, "19");
        assert_eq!(ball.shape, "star");
        assert!(ball.color.starts_with('#'));
        assert_eq!(ball.buckets[0].resets_at, Some(1_790_000_000_000));
    }

    #[test]
    fn a_failed_ball_says_what_to_do_not_the_raw_error() {
        let mut failed = plan("haze");
        failed.ok = false;
        failed.error = Some("http 401 https://usehaze.ai/api/usage token=abc".into());
        let feed = build(
            &Snapshot {
                plans: vec![failed],
                fetched_at: 1,
            },
            &Settings::default(),
        );
        let text = serde_json::to_string(&feed).unwrap();
        assert!(!text.contains("usehaze") && !text.contains("token="));
        let ball = &feed.balls[0];
        assert_eq!(ball.emotion, "34");
        assert_eq!(ball.percent, None);
        assert!(ball.buckets.is_empty());
        assert_eq!(ball.hint, Some("登录态过期，打开对应 App 重新登录即恢复"));
    }
}
