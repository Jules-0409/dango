//! Shared, platform-neutral palette / shape / emotion mapping.
//!
//! Mirrors `ui/common.js` (PALETTE / DEFAULT_SHAPE / emotionFor) so the native
//! widget and the settings page render the same colours, shapes and emotions.

use dango_lib::settings::{RingMode, Theme};
use dango_lib::{PlanQuota, Settings};
use grok_ball::ShapeKind;

/// Re-exported so callers can use the same type without naming `dango_lib` twice.
pub use dango_lib::settings::BallSettings;

/// Ball colours per plan id, from `ui/common.js`.
pub const PALETTE: &[(&str, &str)] = &[
    ("claude", "#E7A97C"),
    ("antigravity", "#8FB5D9"),
    ("devin", "#A8C6A2"),
    ("cursor", "#C9BCA6"),
    ("factory", "#B8A9C9"),
];

/// Default ball shape per plan id, from `ui/common.js`.
pub const DEFAULT_SHAPE: &[(&str, &str)] = &[
    ("claude", "star"),
    ("antigravity", "gem"),
    ("devin", "wedge"),
    ("cursor", "blob"),
    ("factory", "wedge"),
];

const EYE_COLOR: &str = "#F5F2ED";
const FALLBACK_COLOR: &str = "#B9B0A4";

pub fn palette_color(plan_id: &str) -> &'static str {
    PALETTE
        .iter()
        .find(|(id, _)| *id == plan_id)
        .map(|(_, color)| *color)
        .unwrap_or(FALLBACK_COLOR)
}

pub fn color_for_plan(plan_id: &str, settings: &Settings) -> String {
    settings
        .balls
        .get(plan_id)
        .and_then(|ball| ball.color.as_deref())
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string())
        .unwrap_or_else(|| palette_color(plan_id).to_string())
}

pub fn shape_for(plan_id: &str, settings: &Settings) -> ShapeKind {
    let configured = settings
        .balls
        .get(plan_id)
        .and_then(|ball| ball.shape.clone())
        .or_else(|| {
            DEFAULT_SHAPE
                .iter()
                .find(|(id, _)| *id == plan_id)
                .map(|(_, shape)| (*shape).to_string())
        })
        .unwrap_or_else(|| "blob".to_string());
    match configured.as_str() {
        "gem" => ShapeKind::Gem,
        "wedge" => ShapeKind::Wedge,
        "star" => ShapeKind::Star,
        "cloud" => ShapeKind::Cloud,
        "square" => ShapeKind::Square,
        "drop" => ShapeKind::Drop,
        _ => ShapeKind::Blob,
    }
}

/// Emotion id for a plan, matching `ui/common.js::emotionFor`.
///
/// Three glanceable tiers plus the error face: at 44 pt the differences
/// between "19" (满意) and "18" (无奈) were noise, so the middle band is merged.
/// 10 = 开心 / 19 = 满意 / 12 = 失落 / 34 = 出错 / 02 = 待机.
/// grok-ball's "出错" face; reactions never paper over it.
pub const ERROR_EMOTION: &str = "34";

pub fn emotion_for(plan: &PlanQuota) -> &'static str {
    if !plan.ok {
        return ERROR_EMOTION;
    }
    match plan.remaining_percent {
        None => "02",
        Some(percent) if percent >= 60.0 => "10",
        Some(percent) if percent >= 15.0 => "19",
        _ => "12",
    }
}

/// A one-line next-step hint for a failed plan, matching
/// `ui/common.js::errorHint`. The raw `plan.error` stays technical; this is
/// the sentence a human can act on.
pub fn error_hint(error: Option<&str>) -> &'static str {
    let err = error.unwrap_or_default();
    // 自己加的小球：没填 key / key 不对，指到设置页而不是「去 App 登录」。
    if err.starts_with("custom.no_key") {
        return "还没填 API Key，去设置页的小球里填上";
    }
    if err.starts_with("custom.auth") {
        return "API Key 无效，去设置页的小球里换一个";
    }
    // keychain 先于 auth：凭据串是 "keychain:<service>.auth.<kind>"，两类都命中。
    if err.contains("keychain") {
        "钥匙串里没找到凭据，先登录一次对应 App"
    } else if err.contains("401")
        || err.contains("403")
        || err.contains("auth")
        || err.contains("Unauthorized")
    {
        "登录态过期，打开对应 App 重新登录即恢复"
    } else if err.contains("超时") || err.contains("timeout") {
        "网络超时，检查一下代理或网络"
    } else if err.contains("net:") {
        "网络异常，检查一下代理或网络"
    } else if err.contains("parse") {
        "接口变了，解析失败，等探针更新"
    } else {
        "查询失败，详情见下方原始错误"
    }
}

/// Parse `#RRGGBB` into sRGB floats (0–1) for `card.rs::Rgba`.
pub fn hex_to_rgb(hex: &str) -> Option<(f64, f64, f64)> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some((
        ((value >> 16) & 0xff) as f64 / 255.0,
        ((value >> 8) & 0xff) as f64 / 255.0,
        (value & 0xff) as f64 / 255.0,
    ))
}

pub fn eye_color() -> &'static str {
    EYE_COLOR
}

pub(crate) const WARN_COLOR: &str = "#C0791A";
pub(crate) const DANGER_COLOR: &str = "#C74A3F";

/// Progress ring around a ball, matching the retired web capsule's `updateSlotVisual`.
#[derive(Debug, Clone, PartialEq)]
pub enum RingStyle {
    /// Solid arc from 12 o'clock, clockwise, covering `fraction` (0..=1).
    Arc {
        fraction: f64,
        color: String,
        alpha: f32,
        /// Below 15 %: the web UI breathes the arc's opacity.
        low: bool,
        /// Which of the six ring looks to draw (settings `ringMode`).
        mode: RingMode,
        /// Short-window (5 h) remaining fraction for the double ring's thin
        /// outer arc; `None` draws just its track.
        secondary: Option<f64>,
        /// The plan's own colour, for layers that must not inherit the main
        /// arc's warn/danger tint (the double ring's short-window arc).
        plan_color: String,
    },
    /// Failed plan: full dashed danger ring.
    Error { color: String },
    /// Healthy plan whose provider reports no percentage (pay-as-you-go or
    /// unmetered): a quiet static ring in the plan's own colour. Reporting
    /// this as `Error` would be a false alarm — nothing is wrong, there is
    /// just no number to show.
    Unknown { color: String },
}

/// Ring alpha for a plan with plenty left.
pub const QUIET_RING_ALPHA: f32 = 0.45;

pub fn ring_style(plan: &PlanQuota, settings: &Settings) -> RingStyle {
    let percent = match (plan.ok, plan.remaining_percent) {
        (true, Some(percent)) => percent,
        // Provider healthy, just no percentage: quiet ring, not an alarm.
        (true, None) => {
            return RingStyle::Unknown {
                color: color_for_plan(&plan.id, settings),
            }
        }
        (false, _) => {
            return RingStyle::Error {
                color: DANGER_COLOR.to_string(),
            }
        }
    };
    // Healthy is quiet, trouble is loud: plenty left fades the ring back so
    // the eye only lands on the balls that need attention.
    let (color, alpha) = if percent >= 50.0 {
        (color_for_plan(&plan.id, settings), QUIET_RING_ALPHA)
    } else if percent >= 20.0 {
        (color_for_plan(&plan.id, settings), 1.0)
    } else if percent > 0.0 {
        (WARN_COLOR.to_string(), 1.0)
    } else {
        (DANGER_COLOR.to_string(), 1.0)
    };
    RingStyle::Arc {
        fraction: (percent / 100.0).clamp(0.0, 1.0),
        color,
        alpha,
        low: percent < 20.0,
        mode: settings.ring_mode(),
        secondary: short_window_fraction(plan),
        plan_color: color_for_plan(&plan.id, settings),
    }
}

/// Tightest short (5 h) window among the plan's buckets, as a 0..=1 fraction.
/// Prefers the structured `window` field; falls back to label sniffing for
/// providers that don't fill it ("5 小时", "Five Hour", "fiveHour", "5h").
fn short_window_fraction(plan: &PlanQuota) -> Option<f64> {
    plan.buckets
        .iter()
        .filter(|bucket| {
            if let Some(window) = bucket.window.as_deref() {
                return window == "5h";
            }
            let label = bucket.label.to_ascii_lowercase();
            label.contains("5 小时")
                || label.contains("five hour")
                || label.contains("fivehour")
                || label.contains("5h")
        })
        .filter_map(|bucket| bucket.remaining_percent)
        .reduce(f64::min)
        .map(|percent| (percent / 100.0).clamp(0.0, 1.0))
}

/// Resolve the persisted `Theme` into a concrete dark flag. `System` defers
/// to whatever macOS reports (`window_ext::system_prefers_dark`).
pub fn resolved_dark(theme: Theme, system_dark: bool) -> bool {
    match theme {
        Theme::Dark => true,
        Theme::Light => false,
        Theme::System => system_dark,
    }
}

/// Capsule glass recipe per mode: the tint over the vibrancy blur and the
/// 0.5 pt hairline, as sRGB (r, g, b, a) 0-1 — shared with
/// `window_ext::add_glass_tint_overlay`.
pub fn capsule_tint(dark: bool) -> (f64, f64, f64, f64) {
    if dark {
        (0.08, 0.08, 0.09, 0.62)
    } else {
        // Warm paper-white glass: enough opacity that saturated wallpaper
        // can't turn the balls' backdrop into noise, still readable as glass.
        (0.98, 0.97, 0.95, 0.60)
    }
}

pub fn capsule_hairline(dark: bool) -> (f64, f64, f64, f64) {
    if dark {
        (1.0, 1.0, 1.0, 0.08)
    } else {
        (0.0, 0.0, 0.0, 0.10)
    }
}

/// The quiet ring track behind every ball's progress arc.
pub fn ring_track(dark: bool) -> (&'static str, f32) {
    if dark {
        ("#FFFFFF", 0.09)
    } else {
        ("#1B1917", 0.12)
    }
}

/// The collapse handle: a small pill bar riding the capsule's bottom curve.
pub fn handle_color(dark: bool) -> (f64, f64, f64, f64) {
    if dark {
        (1.0, 1.0, 1.0, 0.38)
    } else {
        (0.0, 0.0, 0.0, 0.32)
    }
}

/// Plans in the order they should be rendered.
pub fn ordered_plans(plans: &[PlanQuota], settings: &Settings) -> Vec<PlanQuota> {
    let mut ordered = plans.to_vec();
    dango_lib::settings::apply_order(&mut ordered, settings);
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(id: &str, ok: bool, percent: Option<f64>) -> PlanQuota {
        PlanQuota {
            id: id.into(),
            name: id.into(),
            ok,
            error: None,
            remaining_percent: percent,
            buckets: vec![],
            note: None,
            proxy: None,
            headline_label: None,
            resets_at: None,
        }
    }

    #[test]
    fn emotion_matches_the_web_ui_thresholds() {
        assert_eq!(emotion_for(&plan("nova", false, None)), "34");
        assert_eq!(emotion_for(&plan("nova", true, None)), "02");
        assert_eq!(emotion_for(&plan("nova", true, Some(60.0))), "10");
        assert_eq!(emotion_for(&plan("nova", true, Some(59.9))), "19");
        assert_eq!(emotion_for(&plan("nova", true, Some(30.0))), "19");
        assert_eq!(emotion_for(&plan("nova", true, Some(29.9))), "19");
        assert_eq!(emotion_for(&plan("nova", true, Some(15.0))), "19");
        assert_eq!(emotion_for(&plan("nova", true, Some(14.9))), "12");
    }

    #[test]
    fn error_hint_covers_auth_network_keychain_and_parse() {
        assert_eq!(
            error_hint(Some("http 401")),
            "登录态过期，打开对应 App 重新登录即恢复"
        );
        assert_eq!(
            error_hint(Some("factory.auth_expired")),
            "登录态过期，打开对应 App 重新登录即恢复"
        );
        assert_eq!(
            error_hint(Some("keychain:vendor.auth.session")),
            "钥匙串里没找到凭据，先登录一次对应 App"
        );
        assert_eq!(
            error_hint(Some("net: 请求超时")),
            "网络超时，检查一下代理或网络"
        );
        assert_eq!(
            error_hint(Some("parse: missing weeklyTokensUsed")),
            "接口变了，解析失败，等探针更新"
        );
        assert_eq!(error_hint(None), "查询失败，详情见下方原始错误");
    }

    #[test]
    fn hex_to_rgb_parses_palette_values() {
        assert_eq!(
            hex_to_rgb(PLAN_TEST_COLOR),
            Some((216.0 / 255.0, 167.0 / 255.0, 160.0 / 255.0))
        );
        assert_eq!(
            hex_to_rgb("8FB5D9"),
            Some((143.0 / 255.0, 181.0 / 255.0, 217.0 / 255.0))
        );
        assert_eq!(hex_to_rgb("#fff"), None);
        assert_eq!(hex_to_rgb("garbage"), None);
    }

    const PLAN_TEST_COLOR: &str = "#D8A7A0"; // 任意颜色，与 PALETTE 无关

    #[test]
    fn ring_matches_the_web_ui_thresholds() {
        let mut settings = Settings::default();
        settings.balls.insert(
            "nova".into(),
            BallSettings {
                shape: None,
                color: Some(PLAN_TEST_COLOR.into()),
            },
        );
        let settings = settings;
        assert_eq!(
            ring_style(&plan("nova", true, Some(76.0)), &settings),
            RingStyle::Arc {
                fraction: 0.76,
                color: "#D8A7A0".into(),
                alpha: QUIET_RING_ALPHA,
                low: false,
                mode: RingMode::Plain,
                secondary: None,
                plan_color: PLAN_TEST_COLOR.into()
            }
        );
        assert_eq!(
            ring_style(&plan("nova", true, Some(30.0)), &settings),
            RingStyle::Arc {
                fraction: 0.3,
                color: "#D8A7A0".into(),
                alpha: 1.0,
                low: false,
                mode: RingMode::Plain,
                secondary: None,
                plan_color: PLAN_TEST_COLOR.into()
            }
        );
        assert_eq!(
            ring_style(&plan("cursor", true, Some(0.0)), &settings),
            RingStyle::Arc {
                fraction: 0.0,
                color: DANGER_COLOR.into(),
                alpha: 1.0,
                low: true,
                mode: RingMode::Plain,
                secondary: None,
                plan_color: "#C9BCA6".into()
            }
        );
        // Healthy but no percentage is not an error: quiet ring in the plan
        // colour, never the danger alarm.
        assert_eq!(
            ring_style(&plan("nova", true, None), &settings),
            RingStyle::Unknown {
                color: "#D8A7A0".into()
            }
        );
        assert_eq!(
            ring_style(&plan("nova", false, Some(80.0)), &settings),
            RingStyle::Error {
                color: DANGER_COLOR.into()
            }
        );
    }

    #[test]
    fn ring_carries_mode_and_short_window() {
        let settings = Settings {
            ring_mode: Some(RingMode::Double),
            ..Settings::default()
        };
        let mut p = plan("antigravity", true, Some(70.0));
        p.buckets = vec![
            dango_lib::Bucket {
                label: "Gemini · Weekly Limit Remaining".into(),
                remaining_percent: Some(70.0),
                detail: None,
                resets_at: None,
                pool: None,
                window: None,
            },
            dango_lib::Bucket {
                label: "Gemini · Five Hour Limit Remaining".into(),
                remaining_percent: Some(90.0),
                detail: None,
                resets_at: None,
                pool: None,
                window: None,
            },
            dango_lib::Bucket {
                label: "5 小时".into(),
                remaining_percent: Some(40.0),
                detail: None,
                resets_at: None,
                pool: None,
                window: None,
            },
        ];
        match ring_style(&p, &settings) {
            RingStyle::Arc {
                mode, secondary, ..
            } => {
                assert_eq!(mode, RingMode::Double);
                assert_eq!(secondary, Some(0.4));
            }
            other => panic!("unexpected {other:?}"),
        }
        p.buckets.clear();
        assert!(matches!(
            ring_style(&p, &settings),
            RingStyle::Arc {
                secondary: None,
                ..
            }
        ));
    }

    #[test]
    fn shape_defaults_follow_settings_then_palette_table() {
        let settings = dango_lib::Settings::default();
        assert_eq!(shape_for("nova", &settings), ShapeKind::Blob);
        assert_eq!(shape_for("antigravity", &settings), ShapeKind::Gem);
        assert_eq!(shape_for("devin", &settings), ShapeKind::Wedge);
        assert_eq!(shape_for("cursor", &settings), ShapeKind::Blob);
        assert_eq!(shape_for("factory", &settings), ShapeKind::Wedge);
        assert_eq!(shape_for("unknown-plan", &settings), ShapeKind::Blob);
    }

    #[test]
    fn explicit_shape_override_wins() {
        let mut settings = dango_lib::Settings::default();
        settings.balls.insert(
            "nova".into(),
            BallSettings {
                shape: Some("gem".into()),
                color: None,
            },
        );
        assert_eq!(shape_for("nova", &settings), ShapeKind::Gem);
    }

    #[test]
    fn color_override_wins_over_palette() {
        let mut settings = dango_lib::Settings::default();
        assert_eq!(color_for_plan("claude", &settings), "#E7A97C");
        settings.balls.insert(
            "claude".into(),
            BallSettings {
                shape: None,
                color: Some("#8FB5D9".into()),
            },
        );
        assert_eq!(color_for_plan("claude", &settings), "#8FB5D9");
    }
}
