// 把 vendor 进来的探针结果（CursorQuota/DevinQuota/FactoryQuota）映射成
// Dango 的统一 PlanQuota。
use crate::models::{mask_email, timestamp_to_millis, Bucket, PlanQuota};
use crate::probes::{self, CursorQuota, DevinQuota, FactoryQuota};

fn b(
    label: &str,
    rp: Option<f64>,
    detail: Option<String>,
    resets_at: Option<u64>,
    pool: Option<String>,
    window: Option<String>,
) -> Bucket {
    Bucket {
        label: label.into(),
        remaining_percent: rp,
        detail,
        resets_at,
        pool,
        window,
    }
}
fn used2rem(u: Option<f64>) -> Option<f64> {
    u.map(|x| (100.0 - x).clamp(0.0, 100.0))
}
fn plan_name(base: &str, plan: &Option<String>) -> String {
    match plan {
        Some(pn) => format!("{base} {pn}"),
        None => base.into(),
    }
}

fn from_devin(d: DevinQuota) -> PlanQuota {
    let mut p = PlanQuota {
        id: "devin".into(),
        name: plan_name(&d.name, &d.plan_name),
        ok: d.ok,
        error: d.error.clone(),
        remaining_percent: None,
        buckets: vec![],
        note: d.source.clone(),
        proxy: None,
        headline_label: None,
        resets_at: None,
    };
    if p.ok {
        let daily_reset = d
            .daily_reset_at_unix
            .and_then(|value| timestamp_to_millis(&serde_json::Value::from(value)));
        p.buckets.push(b(
            "每日",
            d.daily_remaining_percent,
            None,
            daily_reset,
            None,
            Some("day".into()),
        ));
        p.buckets.push(b(
            "每周",
            d.weekly_remaining_percent,
            None,
            d.weekly_reset_at_unix
                .and_then(|value| timestamp_to_millis(&serde_json::Value::from(value))),
            None,
            Some("week".into()),
        ));
        if let (Some(c), Some(l)) = (d.acu_consumed, d.acu_limit) {
            p.buckets.push(b(
                "ACU",
                if l > 0 {
                    Some((l - c) as f64 / l as f64 * 100.0)
                } else {
                    None
                },
                Some(format!("{c} / {l}")),
                None,
                None,
                Some("total".into()),
            ));
        }
        // 头条 = 每日（devin 按日重置计费）；拿不到就读不到，不编数。
        if let Some(daily) = d.daily_remaining_percent {
            p.remaining_percent = Some(daily);
            p.headline_label = Some("每日额度".into());
            p.resets_at = daily_reset;
        }
    }
    p
}

fn from_cursor(c: CursorQuota) -> PlanQuota {
    let cycle_reset = c
        .cycle_end
        .as_deref()
        .and_then(|value| timestamp_to_millis(&serde_json::Value::from(value)));
    let mut p = PlanQuota {
        id: "cursor".into(),
        name: plan_name(&c.name, &c.plan_name),
        ok: c.ok,
        error: c.error.clone(),
        remaining_percent: None,
        buckets: vec![],
        note: c.email.as_deref().map(mask_email),
        proxy: None,
        headline_label: None,
        resets_at: None,
    };
    if p.ok {
        // 头条只看"总额度"：API/Fast/Grok 见底不代表主额度没了。
        let total_remaining = used2rem(c.total_percent_used);
        p.buckets.push(b(
            "总额度",
            total_remaining,
            c.auto_message.clone(),
            cycle_reset,
            None,
            Some("total".into()),
        ));
        p.buckets.push(b(
            "Fast requests",
            c.fast_requests_remaining_percent,
            match (c.fast_requests_used, c.fast_requests_limit) {
                (Some(u), Some(l)) => Some(format!("{u} / {l}")),
                _ => None,
            },
            cycle_reset,
            None,
            Some("month".into()),
        ));
        p.buckets.push(b(
            "API",
            used2rem(c.api_percent_used),
            None,
            cycle_reset,
            None,
            Some("other".into()),
        ));
        if let Some(g) = c.grok_percent_used {
            p.buckets.push(b(
                "Grok Bot",
                Some((100.0 - g).clamp(0.0, 100.0)),
                None,
                c.grok_reset_unix
                    .and_then(|value| timestamp_to_millis(&serde_json::Value::from(value))),
                None,
                Some("week".into()),
            ));
        }
        if total_remaining.is_some() {
            p.remaining_percent = total_remaining;
            p.headline_label = Some("总额度".into());
            p.resets_at = cycle_reset;
        }
    }
    p
}

fn from_factory(f: FactoryQuota) -> PlanQuota {
    let mut p = PlanQuota {
        id: "factory".into(),
        name: plan_name(&f.name, &f.plan_name),
        ok: f.ok,
        error: f.error.clone(),
        remaining_percent: None,
        buckets: vec![],
        note: f.org_id.as_ref().map(|o| format!("org {o}")),
        proxy: None,
        headline_label: None,
        resets_at: None,
    };
    if p.ok {
        // 每个 pool：min(各窗口)；头条 = 各 pool 里最大的那个（standard 用完了
        // core 还能用，谁是谁备注清楚）。
        let mut pools: Vec<(String, Option<f64>, Option<u64>)> = vec![];
        for (wname, wmap) in [("standard", &f.windows.standard), ("core", &f.windows.core)] {
            let Some(m) = wmap else { continue };
            // 固定窗口顺序，桶显示稳定（HashMap 迭代无序）。
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by_key(|k| match k.as_str() {
                "fiveHour" | "five_hour" | "5h" => 0,
                "daily" => 1,
                "weekly" => 2,
                "monthly" => 3,
                _ => 9,
            });
            let mut pool_min: Option<(f64, Option<u64>)> = None;
            for k in keys {
                let w = &m[k];
                let remaining = (100.0 - w.used_percent).clamp(0.0, 100.0);
                let resets_at = w
                    .window_end
                    .and_then(|value| timestamp_to_millis(&serde_json::Value::from(value)));
                p.buckets.push(b(
                    &format!("{wname} · {}", factory_window_label(k)),
                    Some(remaining),
                    None,
                    resets_at,
                    Some(wname.to_string()),
                    Some(factory_window_name(k)),
                ));
                let tighter = pool_min.is_none_or(|(current, _)| remaining < current);
                if tighter {
                    pool_min = Some((remaining, resets_at));
                }
            }
            pools.push((
                wname.to_string(),
                pool_min.map(|(v, _)| v),
                pool_min.and_then(|(_, r)| r),
            ));
        }
        // 见底的 pool（<=5%）写进说明，免得用户以为数字错了。
        let exhausted: Vec<&str> = pools
            .iter()
            .filter(|(_, min, _)| min.is_some_and(|v| v <= 5.0))
            .map(|(name, _, _)| name.as_str())
            .collect();
        if let Some((best_name, best, resets)) = pools
            .iter()
            .filter_map(|(name, min, resets)| min.map(|v| (name, v, resets)))
            .max_by(|(_, a, _), (_, b, _)| a.total_cmp(b))
        {
            let dry = exhausted
                .iter()
                .filter(|name| **name != best_name.as_str())
                .map(|name| format!("{name} 见底"))
                .collect::<Vec<_>>()
                .join(" · ");
            p.remaining_percent = Some(best);
            p.headline_label = Some(if dry.is_empty() {
                format!("{best_name} 池")
            } else {
                format!("{best_name} 池（{dry}）")
            });
            p.resets_at = *resets;
        }
    }
    p
}

/// factory 的窗口 key（"fiveHour"/"daily"/"weekly"/"monthly"）→ label 里的人话。
fn factory_window_label(key: &str) -> &str {
    match key {
        "fiveHour" | "five_hour" | "5h" => "5 小时",
        "daily" => "日",
        "weekly" => "周",
        "monthly" => "月",
        other => other,
    }
}

/// factory 的窗口 key → 规范化的 window 口径。
fn factory_window_name(key: &str) -> String {
    match key {
        "fiveHour" | "five_hour" | "5h" => "5h",
        "daily" => "day",
        "weekly" => "week",
        "monthly" => "month",
        _ => "other",
    }
    .to_string()
}

/// 三路探针并发跑完，返回统一 PlanQuota 列表。
pub async fn probe_plans(client: &reqwest::Client) -> Vec<PlanQuota> {
    let (f, d, c) = probes::query_all(client).await;
    vec![from_devin(d), from_cursor(c), from_factory(f)]
}

#[cfg(test)]
mod tests {
    use super::{from_cursor, from_devin, from_factory};
    use crate::probes::models::{FactoryWindows, QuotaWindow};
    use crate::probes::{CursorQuota, DevinQuota, FactoryQuota};
    use std::collections::HashMap;

    #[test]
    fn cursor_reset_fields_map_to_milliseconds() {
        let plan = from_cursor(CursorQuota {
            ok: true,
            cycle_end: Some("2023-11-14T22:13:20Z".into()),
            cycle_reset_unix: Some(1_800_000_000),
            grok_percent_used: Some(20.0),
            grok_reset_unix: Some(1_700_000_100),
            ..Default::default()
        });
        assert_eq!(plan.buckets[0].resets_at, Some(1_700_000_000_000));
        assert_eq!(
            plan.buckets
                .iter()
                .find(|bucket| bucket.label == "Grok Bot")
                .unwrap()
                .resets_at,
            Some(1_700_000_100_000)
        );
        let no_cycle_end = from_cursor(CursorQuota {
            ok: true,
            cycle_reset_unix: Some(1_800_000_000),
            ..Default::default()
        });
        assert_eq!(no_cycle_end.buckets[0].resets_at, None);
    }

    #[test]
    fn devin_reset_fields_map_to_milliseconds() {
        let plan = from_devin(DevinQuota {
            ok: true,
            daily_remaining_percent: Some(50.0),
            weekly_remaining_percent: Some(80.0),
            daily_reset_at_unix: Some(1_700_000_000),
            weekly_reset_at_unix: Some(1_700_500_000),
            ..Default::default()
        });
        assert_eq!(plan.buckets[0].resets_at, Some(1_700_000_000_000));
        assert_eq!(plan.buckets[1].resets_at, Some(1_700_500_000_000));
    }

    #[test]
    fn factory_window_end_maps_to_milliseconds() {
        let plan = from_factory(FactoryQuota {
            ok: true,
            windows: FactoryWindows {
                standard: Some(HashMap::from([(
                    "daily".into(),
                    QuotaWindow {
                        used_percent: 25.0,
                        seconds_remaining: Some(900),
                        window_end: Some(1_700_000_000),
                    },
                )])),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(plan.buckets[0].resets_at, Some(1_700_000_000_000));
    }

    #[test]
    fn cursor_plan_note_masks_full_email() {
        let plan = from_cursor(CursorQuota {
            ok: true,
            email: Some("person@example.invalid".into()),
            ..Default::default()
        });
        let note = plan.note.unwrap();
        assert_eq!(note, "p***@e***");
        assert!(!note.contains("person@example.invalid"));
    }

    #[test]
    fn cursor_headline_only_looks_at_total_not_api() {
        // API 桶 0% 也不能把头条拉成 0：头条只看总额度（还剩 11%）。
        let plan = from_cursor(CursorQuota {
            ok: true,
            total_percent_used: Some(89.0),
            api_percent_used: Some(100.0),
            fast_requests_remaining_percent: Some(20.0),
            cycle_end: Some("2023-11-14T22:13:20Z".into()),
            ..Default::default()
        });
        assert_eq!(plan.remaining_percent, Some(11.0));
        assert_eq!(plan.headline_label.as_deref(), Some("总额度"));
        assert_eq!(plan.resets_at, Some(1_700_000_000_000));
        assert_eq!(
            plan.buckets
                .iter()
                .find(|b| b.label == "总额度")
                .unwrap()
                .window
                .as_deref(),
            Some("total")
        );
    }

    #[test]
    fn devin_headline_is_daily_with_daily_reset() {
        // 周额度用完（0%）头条还是每日：devin 按日重置。
        let plan = from_devin(DevinQuota {
            ok: true,
            daily_remaining_percent: Some(42.0),
            weekly_remaining_percent: Some(0.0),
            daily_reset_at_unix: Some(1_700_000_000),
            ..Default::default()
        });
        assert_eq!(plan.remaining_percent, Some(42.0));
        assert_eq!(plan.headline_label.as_deref(), Some("每日额度"));
        assert_eq!(plan.resets_at, Some(1_700_000_000_000));
    }

    fn factory_quota(standard_weekly_used: f64, core_daily_used: f64) -> FactoryQuota {
        FactoryQuota {
            ok: true,
            windows: FactoryWindows {
                standard: Some(HashMap::from([(
                    "weekly".into(),
                    QuotaWindow {
                        used_percent: standard_weekly_used,
                        seconds_remaining: Some(3600),
                        window_end: Some(1_700_000_000),
                    },
                )])),
                core: Some(HashMap::from([
                    (
                        "daily".into(),
                        QuotaWindow {
                            used_percent: core_daily_used,
                            seconds_remaining: Some(900),
                            window_end: Some(1_700_500_000),
                        },
                    ),
                    (
                        "weekly".into(),
                        QuotaWindow {
                            used_percent: 10.0,
                            seconds_remaining: None,
                            window_end: Some(1_700_900_000),
                        },
                    ),
                ])),
            },
            ..Default::default()
        }
    }

    #[test]
    fn factory_headline_is_best_pool_not_global_min() {
        // standard 周 0% 见底、core 日 61%：头条取 core，口径带上 standard 见底说明。
        let plan = from_factory(factory_quota(100.0, 39.0));
        assert_eq!(plan.remaining_percent, Some(61.0));
        assert_eq!(
            plan.headline_label.as_deref(),
            Some("core 池（standard 见底）")
        );
        // core 池 min(日=61, 周=90) → 日窗口，resets_at 用 daily 的 windowEnd
        assert_eq!(plan.resets_at, Some(1_700_500_000_000));

        let labels: Vec<&str> = plan.buckets.iter().map(|b| b.label.as_str()).collect();
        assert!(labels.contains(&"standard · 周"));
        assert!(labels.contains(&"core · 日"));
        assert!(labels.contains(&"core · 周"));
        let core_daily = plan
            .buckets
            .iter()
            .find(|b| b.label == "core · 日")
            .unwrap();
        assert_eq!(core_daily.pool.as_deref(), Some("core"));
        assert_eq!(core_daily.window.as_deref(), Some("day"));
    }

    #[test]
    fn factory_headline_signs_standard_when_it_is_best() {
        // standard 池更宽松时头条是 standard；没 pool 见底就不加说明。
        let plan = from_factory(factory_quota(50.0, 60.0));
        assert_eq!(plan.remaining_percent, Some(50.0));
        assert_eq!(plan.headline_label.as_deref(), Some("standard 池"));
        // standard 池只有一个窗口，resets_at 就是它的
        assert_eq!(plan.resets_at, Some(1_700_000_000_000));
    }
}

#[cfg(test)]
mod factory_window_tests {
    use super::{factory_window_label, factory_window_name};

    #[test]
    fn five_hour_window_is_normalized() {
        assert_eq!(factory_window_label("fiveHour"), "5 小时");
        assert_eq!(factory_window_name("fiveHour"), "5h");
        assert_eq!(factory_window_name("weekly"), "week");
        assert_eq!(factory_window_name("mystery"), "other");
    }
}
