use crate::models::PlanQuota;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

const DEFAULT_ORDER: &[&str] = &[
    "claude",
    "haze",
    "antigravity",
    "devin",
    "cursor",
    "factory",
    "dim",
    "grok",
];

/// Animation pacing mode for the widget.
///
/// Serialized as `perfMode` and stored in `settings.json`; the field is optional so
/// settings files written before it existed still load (they get [`PerfMode::Balanced`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum PerfMode {
    /// 60 fps with the full (non-lite) ball renderer.
    Smooth,
    /// 60 fps with the lite ball renderer.
    #[default]
    Balanced,
    /// Animate only while hovered or while a state change is in flight.
    Saver,
}

impl PerfMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PerfMode::Smooth => "smooth",
            PerfMode::Balanced => "balanced",
            PerfMode::Saver => "saver",
        }
    }

    /// Parse a user-supplied value; unknown strings fall back to the default.
    pub fn parse(value: &str) -> Self {
        match value {
            "smooth" => PerfMode::Smooth,
            "saver" => PerfMode::Saver,
            _ => PerfMode::Balanced,
        }
    }
}

fn deserialize_lenient_perf_mode<'de, D>(deserializer: D) -> Result<Option<PerfMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt.as_deref() {
        Some("smooth") => Ok(Some(PerfMode::Smooth)),
        Some("balanced") => Ok(Some(PerfMode::Balanced)),
        Some("saver") => Ok(Some(PerfMode::Saver)),
        _ => Ok(None),
    }
}

/// Appearance theme for the widget and settings UI.
///
/// Serialized as `theme` and stored in `settings.json`; the field is optional so
/// settings files written before it existed still load (defaulting to [`Theme::System`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum Theme {
    /// Follow system appearance (dark or light mode).
    #[default]
    System,
    /// Force dark mode.
    Dark,
    /// Force light mode.
    Light,
}

impl Theme {
    pub fn as_str(&self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    /// Parse a user-supplied value; unknown strings fall back to the default.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "dark" => Theme::Dark,
            "light" => Theme::Light,
            _ => Theme::System,
        }
    }
}

impl std::fmt::Display for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn deserialize_lenient_theme<'de, D>(deserializer: D) -> Result<Option<Theme>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt.as_deref() {
        Some(s) => match s.trim().to_ascii_lowercase().as_str() {
            "system" => Ok(Some(Theme::System)),
            "dark" => Ok(Some(Theme::Dark)),
            "light" => Ok(Some(Theme::Light)),
            _ => Ok(None),
        },
        None => Ok(None),
    }
}

/// How each ball's quota ring is drawn. Global (all balls share one look).
///
/// Serialized as `ringMode`; optional so older settings files load as [`RingMode::Plain`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RingMode {
    /// One thin arc — the original ring.
    #[default]
    Plain,
    /// 20 beads, one per 5 %; spent beads shrink to dots.
    Beads,
    /// Main arc plus a thin outer arc for the short (5 h) window.
    Double,
    /// Thin arc with a highlight orbiting inside it; faster when tighter.
    Flow,
    /// 8 rounded segments with gaps.
    Segments,
    /// Arc fading from a bright head to a transparent tail.
    Trail,
}

impl RingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RingMode::Plain => "plain",
            RingMode::Beads => "beads",
            RingMode::Double => "double",
            RingMode::Flow => "flow",
            RingMode::Segments => "segments",
            RingMode::Trail => "trail",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "plain" => RingMode::Plain,
            "beads" => RingMode::Beads,
            "double" => RingMode::Double,
            "flow" => RingMode::Flow,
            "segments" => RingMode::Segments,
            "trail" => RingMode::Trail,
            _ => return None,
        })
    }
}

fn deserialize_lenient_ring_mode<'de, D>(deserializer: D) -> Result<Option<RingMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.as_deref().and_then(RingMode::parse))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub version: u32,
    pub order: Vec<String>,
    pub balls: BTreeMap<String, BallSettings>,
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_perf_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub perf_mode: Option<PerfMode>,
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_theme",
        skip_serializing_if = "Option::is_none"
    )]
    pub theme: Option<Theme>,
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_ring_mode",
        skip_serializing_if = "Option::is_none"
    )]
    pub ring_mode: Option<RingMode>,
    /// User-added balls (API balance providers). Their API keys live in the
    /// `dango` keychain service under the plan id, never in this file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<CustomPlan>,
    /// Built-in balls the user removed (or that a first run found nothing
    /// for). Hidden plans are dropped from the snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<String>,
}

/// The built-in balls, in their default order.
pub const BUILTIN_PLANS: &[&str] = DEFAULT_ORDER;

/// Built-ins that stay hidden until the user adds them from 添加小球 (and
/// signs in there): newer providers most people don't have.
pub const OPT_IN_PLANS: &[&str] = &["grok"];

/// An opt-in ball the user never added (absent from both `order` and
/// `hidden`, e.g. a settings file from before it existed) starts hidden.
fn hide_unadded_opt_ins(settings: &mut Settings) {
    for id in OPT_IN_PLANS {
        if !settings.order.iter().any(|x| x == id) && !settings.hidden.iter().any(|x| x == id) {
            settings.hidden.push((*id).to_string());
        }
    }
}

/// A ball the user added from a template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CustomPlan {
    /// `custom-<slug>`; see [`is_custom_plan_id`].
    pub id: String,
    /// Template: `deepseek` | `moonshot` | `stepfun` | `openrouter` | `siliconflow`.
    pub kind: String,
    pub name: String,
    /// Balance (whole currency units) that counts as "full"; with it the
    /// ball gets a ring and a %.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u64>,
}

pub const CUSTOM_KINDS: &[&str] = &[
    "deepseek",
    "moonshot",
    "stepfun",
    "openrouter",
    "siliconflow",
];

/// `custom-` + 1–32 of `[a-z0-9-]`. The id is a keychain account name passed
/// to `security`, so nothing else gets through.
pub fn is_custom_plan_id(id: &str) -> bool {
    id.strip_prefix("custom-").is_some_and(|slug| {
        (1..=32).contains(&slug.len())
            && slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BallSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            order: DEFAULT_ORDER.iter().map(|id| (*id).to_string()).collect(),
            balls: DEFAULT_ORDER
                .iter()
                .map(|id| ((*id).to_string(), BallSettings::default()))
                .collect(),
            perf_mode: Some(PerfMode::Balanced),
            theme: Some(Theme::System),
            ring_mode: Some(RingMode::Plain),
            custom: Vec::new(),
            hidden: OPT_IN_PLANS.iter().map(|id| (*id).to_string()).collect(),
        }
    }
}

impl Settings {
    /// Effective performance mode, falling back to the default for legacy files.
    pub fn perf_mode(&self) -> PerfMode {
        self.perf_mode.unwrap_or_default()
    }

    /// Effective ring mode, falling back to the default for legacy files.
    pub fn ring_mode(&self) -> RingMode {
        self.ring_mode.unwrap_or_default()
    }

    /// Effective theme, falling back to the default for legacy files.
    pub fn theme(&self) -> Theme {
        self.theme.unwrap_or_default()
    }
}

/// The standard macOS settings path: `~/Library/Application Support/dango/settings.json`.
pub fn settings_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is unavailable".to_string())?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("dango")
        .join("settings.json"))
}

/// `~/Library/Application Support/dango`, the directory holding the app's
/// own files (`settings.json`, `window.json`, …).
pub fn settings_dir() -> Result<PathBuf, String> {
    settings_path().map(|path| {
        path.parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    })
}

/// Load settings from the standard user path, falling back to defaults on failure.
pub fn load() -> Settings {
    settings_path().map(load_from).unwrap_or_default()
}

/// Load settings from an explicit path, useful for tests and alternate app profiles.
///
/// Invalid JSON is preserved by renaming it to `<filename>.bak` before returning defaults.
pub fn load_from(path: impl AsRef<Path>) -> Settings {
    let path = path.as_ref();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
        Err(_) => {
            let _ = std::fs::rename(path, backup_path(path));
            return Settings::default();
        }
    };
    match serde_json::from_str(&text) {
        Ok(mut settings) if validate(&settings).is_ok() => {
            hide_unadded_opt_ins(&mut settings);
            settings
        }
        Ok(_) | Err(_) => {
            let _ = std::fs::rename(path, backup_path(path));
            Settings::default()
        }
    }
}

/// Validate and atomically save settings to the standard user path.
pub fn save(settings: &Settings) -> Result<(), String> {
    save_to(settings_path()?, settings)
}

/// Validate and atomically save settings to an explicit path.
pub fn save_to(path: impl AsRef<Path>, settings: &Settings) -> Result<(), String> {
    validate(settings)?;
    let path = path.as_ref();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create settings directory: {error}"))?;

    let mut normalized = settings.clone();
    deduplicate_order(&mut normalized.order);
    let text = serde_json::to_vec_pretty(&normalized)
        .map_err(|error| format!("serialize settings: {error}"))?;
    let temporary = temporary_path(path);
    let result = (|| {
        std::fs::write(&temporary, text)
            .map_err(|error| format!("write temporary settings: {error}"))?;
        std::fs::rename(&temporary, path).map_err(|error| format!("replace settings: {error}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Sort plans by configured id order; unlisted plans retain their original relative order.
pub fn apply_order(plans: &mut Vec<PlanQuota>, settings: &Settings) {
    let mut positions: BTreeMap<&str, usize> = BTreeMap::new();
    for (position, id) in settings.order.iter().enumerate() {
        positions.entry(id.as_str()).or_insert(position);
    }
    plans.sort_by_key(|plan| {
        positions
            .get(plan.id.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
}

fn validate(settings: &Settings) -> Result<(), String> {
    if let Some(id) = settings
        .hidden
        .iter()
        .find(|id| !BUILTIN_PLANS.contains(&id.as_str()))
    {
        return Err(format!("only built-in balls can be hidden, not {id}"));
    }
    let mut ids = HashSet::new();
    for plan in &settings.custom {
        if !is_custom_plan_id(&plan.id) {
            return Err(format!("invalid custom ball id {}", plan.id));
        }
        if !ids.insert(plan.id.as_str()) {
            return Err(format!("duplicate custom ball {}", plan.id));
        }
        if !CUSTOM_KINDS.contains(&plan.kind.as_str()) {
            return Err(format!("unknown custom ball kind {}", plan.kind));
        }
        if plan.name.trim().is_empty() || plan.name.chars().count() > 24 {
            return Err("custom ball name must be 1-24 characters".into());
        }
        if plan.budget == Some(0) {
            return Err("custom ball budget must be a positive number".into());
        }
    }
    for (plan_id, ball) in &settings.balls {
        if let Some(shape) = &ball.shape {
            if !matches!(
                shape.as_str(),
                "blob"
                    | "gem"
                    | "wedge"
                    | "star"
                    | "cloud"
                    | "heart"
                    | "square"
                    | "drop"
                    | "whale"
                    | "cat"
            ) {
                return Err(format!("invalid shape for {plan_id}"));
            }
        }
        if let Some(color) = &ball.color {
            if !is_valid_hex_color(color) {
                return Err(format!("invalid color for {plan_id}"));
            }
        }
    }
    Ok(())
}

fn is_valid_hex_color(color: &str) -> bool {
    let bytes = color.as_bytes();
    if bytes.first() != Some(&b'#') {
        return false;
    }
    let hex = &bytes[1..];
    (hex.len() == 3 || hex.len() == 6 || hex.len() == 8)
        && hex.iter().all(|b| b.is_ascii_hexdigit())
}

fn deduplicate_order(order: &mut Vec<String>) {
    let mut seen = HashSet::new();
    order.retain(|id| seen.insert(id.clone()));
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    let backup = path.with_file_name(&name);
    if !backup.exists() {
        return backup;
    }
    for index in 1_u32.. {
        let mut alternate = name.clone();
        alternate.push(format!(".{index}"));
        let alternate = path.with_file_name(alternate);
        if !alternate.exists() {
            return alternate;
        }
    }
    unreachable!("u32 backup suffix space exhausted")
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_order, load_from, save_to, BallSettings, PerfMode, RingMode, Settings, Theme,
    };
    use crate::models::PlanQuota;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "dango-settings-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn settings(order: &[&str]) -> Settings {
        Settings {
            version: 1,
            order: order.iter().map(|id| (*id).to_string()).collect(),
            balls: BTreeMap::from([(
                "nova".into(),
                BallSettings {
                    shape: Some("gem".into()),
                    color: None,
                },
            )]),
            perf_mode: None,
            theme: None,
            ring_mode: None,
            custom: Vec::new(),
            hidden: Vec::new(),
        }
    }

    fn plan(id: &str) -> PlanQuota {
        PlanQuota {
            id: id.into(),
            name: id.into(),
            ok: true,
            error: None,
            remaining_percent: None,
            buckets: vec![],
            note: None,
            proxy: None,
            headline_label: None,
            resets_at: None,
        }
    }

    #[test]
    fn missing_settings_returns_defaults() {
        let dir = TempDir::new();
        assert_eq!(load_from(dir.0.join("settings.json")), Settings::default());
    }

    #[test]
    fn malformed_settings_are_backed_up_and_defaults_returned() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(&path, "{ not-json").unwrap();
        assert_eq!(load_from(&path), Settings::default());
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.json.bak")).unwrap(),
            "{ not-json"
        );
    }

    #[test]
    fn malformed_settings_do_not_overwrite_an_existing_backup() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(&path, "{ newer-invalid").unwrap();
        std::fs::write(dir.0.join("settings.json.bak"), "older-backup").unwrap();
        assert_eq!(load_from(&path), Settings::default());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.json.bak")).unwrap(),
            "older-backup"
        );
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.json.bak.1")).unwrap(),
            "{ newer-invalid"
        );
    }

    #[test]
    fn invalid_saved_shape_is_backed_up_and_defaults_returned() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(
            &path,
            r#"{"version":1,"order":["nova"],"balls":{"nova":{"shape":"octagon"}}}"#,
        )
        .unwrap();
        assert_eq!(load_from(&path), Settings::default());
        assert!(!path.exists());
        assert!(dir.0.join("settings.json.bak").exists());
    }

    #[test]
    fn save_roundtrips_camel_case_and_deduplicates_order() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        save_to(&path, &settings(&["cursor", "nova", "cursor"])).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["order"], serde_json::json!(["cursor", "nova"]));
        assert_eq!(value["balls"]["nova"]["shape"], "gem");
        assert_eq!(load_from(path).order, vec!["cursor", "nova"]);
    }

    #[test]
    fn invalid_shape_is_rejected_without_writing_file() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        save_to(&path, &settings(&[])).unwrap();
        let mut invalid = settings(&[]);
        invalid.balls.get_mut("nova").unwrap().shape = Some("octagon".into());
        assert!(save_to(&path, &invalid).is_err());
        assert_eq!(load_from(path).balls["nova"].shape.as_deref(), Some("gem"));
    }

    #[test]
    fn apply_order_keeps_unlisted_plans_stable() {
        let mut plans = vec![plan("devin"), plan("cursor"), plan("factory"), plan("nova")];
        apply_order(&mut plans, &settings(&["nova", "cursor"]));
        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.id.as_str())
                .collect::<Vec<_>>(),
            vec!["nova", "cursor", "devin", "factory"]
        );
    }

    #[test]
    fn ring_mode_round_trips_and_tolerates_junk() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(
            &path,
            r#"{"version":1,"order":[],"balls":{},"ringMode":"beads"}"#,
        )
        .unwrap();
        assert_eq!(load_from(&path).ring_mode(), RingMode::Beads);
        std::fs::write(
            &path,
            r#"{"version":1,"order":[],"balls":{},"ringMode":"spiral"}"#,
        )
        .unwrap();
        assert_eq!(load_from(&path).ring_mode(), RingMode::Plain);
        std::fs::write(&path, r#"{"version":1,"order":[],"balls":{}}"#).unwrap();
        assert_eq!(load_from(&path).ring_mode(), RingMode::Plain);
        let json = serde_json::to_value(Settings {
            ring_mode: Some(RingMode::Trail),
            ..Settings::default()
        })
        .unwrap();
        assert_eq!(json["ringMode"], "trail");
    }

    #[test]
    fn legacy_settings_without_perf_mode_load_with_default() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(
            &path,
            r#"{"version":1,"order":["nova"],"balls":{"nova":{"shape":"gem"}}}"#,
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.perf_mode, None);
        assert_eq!(loaded.perf_mode(), PerfMode::Balanced);
        assert_eq!(PerfMode::Balanced.as_str(), "balanced");
    }

    #[test]
    fn perf_mode_roundtrips_as_camel_case_string() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        let mut value = settings(&["nova"]);
        value.perf_mode = Some(PerfMode::Saver);
        save_to(&path, &value).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["perfMode"], serde_json::json!("saver"));
        assert_eq!(load_from(&path).perf_mode(), PerfMode::Saver);
    }

    #[test]
    fn unknown_perf_mode_is_rejected_and_backed_up() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(
            &path,
            r#"{"version":1,"order":["nova"],"balls":{},"perfMode":"turbo"}"#,
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.order, vec!["nova"]);
        assert_eq!(loaded.perf_mode, None);
        assert_eq!(loaded.perf_mode(), PerfMode::Balanced);
        assert!(!dir.0.join("settings.json.bak").exists());
    }

    #[test]
    fn perf_mode_strings_round_trip() {
        for (text, expected) in [
            ("smooth", PerfMode::Smooth),
            ("balanced", PerfMode::Balanced),
            ("saver", PerfMode::Saver),
            ("nonsense", PerfMode::Balanced),
        ] {
            assert_eq!(PerfMode::parse(text), expected);
        }
    }

    #[test]
    fn ball_color_round_trips_and_validates() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        let mut value = settings(&["nova"]);
        value.balls.insert(
            "nova".into(),
            BallSettings {
                shape: Some("gem".into()),
                color: Some("#8FB5D9".into()),
            },
        );
        save_to(&path, &value).unwrap();
        let loaded = load_from(&path);
        assert_eq!(
            loaded.balls.get("nova").and_then(|b| b.color.as_deref()),
            Some("#8FB5D9")
        );

        let mut invalid = value.clone();
        invalid.balls.get_mut("nova").unwrap().color = Some("not-a-color".into());
        assert!(save_to(&path, &invalid).is_err());
    }

    #[test]
    fn legacy_settings_without_theme_load_with_default() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(
            &path,
            r#"{"version":1,"order":["nova"],"balls":{"nova":{"shape":"gem"}}}"#,
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.theme, None);
        assert_eq!(loaded.theme(), Theme::System);
        assert_eq!(Theme::System.as_str(), "system");
    }

    #[test]
    fn theme_roundtrips_as_camel_case_string() {
        for (theme_variant, expected_str) in [
            (Theme::System, "system"),
            (Theme::Dark, "dark"),
            (Theme::Light, "light"),
        ] {
            let dir = TempDir::new();
            let path = dir.0.join("settings.json");
            let mut value = settings(&["nova"]);
            value.theme = Some(theme_variant);
            save_to(&path, &value).unwrap();
            let raw: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(raw["theme"], serde_json::json!(expected_str));
            let loaded = load_from(&path);
            assert_eq!(loaded.theme, Some(theme_variant));
            assert_eq!(loaded.theme(), theme_variant);
        }
    }

    #[test]
    fn unknown_theme_is_handled_leniently() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(
            &path,
            r#"{"version":1,"order":["nova"],"balls":{},"theme":"neon"}"#,
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.order, vec!["nova"]);
        assert_eq!(loaded.theme, None);
        assert_eq!(loaded.theme(), Theme::System);
        assert!(!dir.0.join("settings.json.bak").exists());
    }

    #[test]
    fn theme_strings_round_trip() {
        for (text, expected) in [
            ("system", Theme::System),
            ("dark", Theme::Dark),
            ("light", Theme::Light),
            ("  Dark  ", Theme::Dark),
            ("LIGHT", Theme::Light),
            ("nonsense", Theme::System),
        ] {
            assert_eq!(Theme::parse(text), expected);
        }
    }

    #[test]
    fn custom_ball_ids_are_strict_because_they_reach_the_keychain_command() {
        use super::is_custom_plan_id;
        assert!(is_custom_plan_id("custom-deepseek"));
        assert!(is_custom_plan_id("custom-kimi-2"));
        assert!(!is_custom_plan_id("deepseek"));
        assert!(!is_custom_plan_id("custom-"));
        assert!(!is_custom_plan_id("custom-a b"));
        assert!(!is_custom_plan_id("custom-x\" -w evil"));
        assert!(!is_custom_plan_id("custom-UPPER"));
    }

    #[test]
    fn custom_balls_validate() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        let mut good = Settings::default();
        good.custom.push(super::CustomPlan {
            id: "custom-deepseek".into(),
            kind: "deepseek".into(),
            name: "DeepSeek".into(),
            budget: Some(100),
        });
        save_to(&path, &good).unwrap();
        assert_eq!(load_from(&path).custom, good.custom);
        let mut bad = good.clone();
        bad.custom[0].kind = "made-up".into();
        assert!(save_to(&path, &bad).is_err());
        let mut dup = good.clone();
        dup.custom.push(good.custom[0].clone());
        assert!(save_to(&path, &dup).is_err());
    }

    #[test]
    fn only_builtin_balls_can_be_hidden() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.json");
        let mut settings = Settings::default();
        settings.hidden = vec!["devin".into()];
        save_to(&path, &settings).unwrap();
        assert_eq!(load_from(&path).hidden, vec!["devin".to_string()]);
        settings.hidden = vec!["custom-x".into()];
        assert!(save_to(&path, &settings).is_err());
    }

    #[test]
    fn opt_in_balls_start_hidden_until_added() {
        assert!(Settings::default().hidden.contains(&"grok".to_string()));
        let dir = std::env::temp_dir().join(format!("dango-optin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        // 旧文件：没见过 grok → 藏着
        std::fs::write(&path, r#"{"version":1,"order":["claude"],"balls":{}}"#).unwrap();
        assert_eq!(load_from(&path).hidden, vec!["grok".to_string()]);
        // 从「添加小球」加过（进了 order、出了 hidden）→ 不再藏
        std::fs::write(
            &path,
            r#"{"version":1,"order":["claude","grok"],"balls":{}}"#,
        )
        .unwrap();
        assert!(load_from(&path).hidden.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whale_and_cat_are_valid_shapes_and_unknown_ones_are_not() {
        let mut value = settings(&["nova"]);
        for shape in ["whale", "cat"] {
            value.balls.insert(
                "nova".into(),
                BallSettings {
                    shape: Some(shape.into()),
                    color: None,
                },
            );
            assert!(super::validate(&value).is_ok(), "{shape}");
        }
        value.balls.get_mut("nova").unwrap().shape = Some("dolphin".into());
        assert!(super::validate(&value).is_err());
    }
}
