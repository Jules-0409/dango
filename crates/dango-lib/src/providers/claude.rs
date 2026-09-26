//! Claude（小克自己）的额度：Claude 桌面端把 plan usage 采样写在
//! `~/Library/Application Support/Claude/plan-usage-history.json`，
//! `u.fh` = 5 小时窗口已用 %，`u.sd` = 7 天窗口已用 %。
//! 纯读本地文件，不碰任何 token；采样太旧就如实报错，不拿旧数冒充。
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::models::{Bucket, PlanQuota};

/// 桌面端不开的时候不会写采样；超过这个年龄就不当现值展示。
const STALE_AFTER_MS: u64 = 6 * 60 * 60 * 1000;

fn history_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Claude/plan-usage-history.json"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

pub async fn fetch(_client: &reqwest::Client) -> PlanQuota {
    match history_path() {
        Some(path) => from_file(&path, now_ms()),
        None => failed("claude.no_home".into()),
    }
}

fn failed(error: String) -> PlanQuota {
    PlanQuota {
        id: "claude".into(),
        name: "Claude".into(),
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

fn from_file(path: &Path, now: u64) -> PlanQuota {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return failed("claude.no_samples: 桌面端还没写过额度采样".into())
        }
        Err(error) => return failed(format!("claude.read: {error}")),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(json) => from_json(&json, now),
        Err(error) => failed(format!("parse: {error}")),
    }
}

fn used_to_remaining(used: Option<f64>) -> Option<f64> {
    used.map(|used| (100.0 - used).clamp(0.0, 100.0))
}

fn from_json(json: &Value, now: u64) -> PlanQuota {
    let Some(sample) = json
        .get("samples")
        .and_then(Value::as_array)
        .and_then(|samples| {
            samples
                .iter()
                .max_by_key(|s| s.get("t").and_then(Value::as_u64))
        })
    else {
        return failed("parse: missing samples".into());
    };
    let Some(at) = sample.get("t").and_then(Value::as_u64) else {
        return failed("parse: missing sample time".into());
    };
    let age = now.saturating_sub(at);
    if age > STALE_AFTER_MS {
        return failed(format!(
            "claude.stale: 桌面端 {} 小时没更新采样，打开 Claude 即恢复",
            age / 3_600_000
        ));
    }
    let usage = sample.get("u");
    let five_hour = used_to_remaining(usage.and_then(|u| u.get("fh")).and_then(Value::as_f64));
    let seven_day = used_to_remaining(usage.and_then(|u| u.get("sd")).and_then(Value::as_f64));
    if five_hour.is_none() && seven_day.is_none() {
        return failed("parse: missing fh/sd".into());
    }

    let buckets = vec![
        Bucket {
            label: "5 小时".into(),
            remaining_percent: five_hour,
            detail: None,
            resets_at: None,
            pool: None,
            window: Some("5h".into()),
        },
        Bucket {
            label: "7 天".into(),
            remaining_percent: seven_day,
            detail: None,
            resets_at: None,
            pool: None,
            window: Some("week".into()),
        },
    ];
    // 头条 = min(5 小时, 7 天)，口径名取更紧的那个窗口；拿不到就读不到，不编数。
    let (remaining_percent, headline_label) = [("5 小时窗口", five_hour), ("7 天窗口", seven_day)]
        .into_iter()
        .filter_map(|(label, p)| p.map(|p| (p, label)))
        .min_by(|(a, _), (b, _)| a.total_cmp(b))
        .map(|(p, label)| (Some(p), Some(label.to_string())))
        .unwrap_or((None, None));
    let minutes = age / 60_000;
    PlanQuota {
        id: "claude".into(),
        name: "Claude".into(),
        ok: true,
        error: None,
        remaining_percent,
        buckets,
        note: Some(if minutes == 0 {
            "桌面端刚采样".into()
        } else {
            format!("桌面端 {minutes} 分钟前采样")
        }),
        proxy: None,
        headline_label,
        resets_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_790_385_814_943;

    #[test]
    fn latest_sample_becomes_remaining_percent() {
        let json = json!({"version":2,"samples":[
            {"t": NOW - 600_000, "org":"o", "u":{"fh":0,"sd":0}},
            {"t": NOW - 60_000, "org":"o", "u":{"fh":2,"sd":35}}
        ]});
        let plan = from_json(&json, NOW);
        assert!(plan.ok);
        assert_eq!(plan.buckets[0].remaining_percent, Some(98.0));
        assert_eq!(plan.buckets[0].window.as_deref(), Some("5h"));
        assert_eq!(plan.buckets[1].remaining_percent, Some(65.0));
        assert_eq!(plan.buckets[1].window.as_deref(), Some("week"));
        assert_eq!(plan.remaining_percent, Some(65.0));
        assert_eq!(plan.headline_label.as_deref(), Some("7 天窗口"));
        assert_eq!(plan.resets_at, None);
        assert_eq!(plan.note.as_deref(), Some("桌面端 1 分钟前采样"));
    }

    #[test]
    fn headline_names_the_tighter_window() {
        // 5 小时窗口更紧时，口径名就是 "5 小时窗口"
        let json = json!({"samples":[{"t": NOW, "u":{"fh":80,"sd":10}}]});
        let plan = from_json(&json, NOW);
        assert_eq!(plan.remaining_percent, Some(20.0));
        assert_eq!(plan.headline_label.as_deref(), Some("5 小时窗口"));

        // 只有一个窗口拿得到时用剩下的那个
        let json = json!({"samples":[{"t": NOW, "u":{"fh":30}}]});
        let plan = from_json(&json, NOW);
        assert!(plan.ok);
        assert_eq!(plan.remaining_percent, Some(70.0));
        assert_eq!(plan.headline_label.as_deref(), Some("5 小时窗口"));
    }

    #[test]
    fn stale_sample_is_an_error_not_a_number() {
        let json = json!({"samples":[{"t": NOW - STALE_AFTER_MS - 1, "u":{"fh":10,"sd":10}}]});
        let plan = from_json(&json, NOW);
        assert!(!plan.ok);
        assert!(plan.remaining_percent.is_none());
        assert!(plan.error.unwrap().starts_with("claude.stale"));
    }

    #[test]
    fn over_100_used_clamps_to_zero() {
        let json = json!({"samples":[{"t": NOW, "u":{"fh":120,"sd":50}}]});
        assert_eq!(
            from_json(&json, NOW).buckets[0].remaining_percent,
            Some(0.0)
        );
    }

    #[test]
    fn missing_file_and_garbage_fail_honestly() {
        let plan = from_file(Path::new("/nonexistent/plan-usage-history.json"), NOW);
        assert!(plan.error.unwrap().starts_with("claude.no_samples"));
        assert!(!from_json(&json!({"samples":[]}), NOW).ok);
        assert!(!from_json(&json!({"samples":[{"t": NOW, "u":{}}]}), NOW).ok);
    }
}
