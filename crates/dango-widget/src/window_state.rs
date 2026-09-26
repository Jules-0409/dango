//! Capsule window position persistence.
//!
//! The position lives in its own `window.json` next to `settings.json` because
//! it is window state, not a user setting: `dango_lib::settings::Settings` has no
//! field for it, so saving it into `settings.json` would be dropped by the next
//! settings write (API, tray, settings window).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Where the capsule sits when nothing has been saved yet: top-left origin,
/// logical points, matching winit's `set_outer_position`.
pub const DEFAULT_POSITION: (f64, f64) = (80.0, 120.0);

/// A dragged window emits `Moved` continuously; wait for the window to be still
/// for this long before touching the disk.
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// `~/Library/Application Support/dango/window.json`.
pub fn path() -> Result<PathBuf, String> {
    Ok(dango_lib::settings::settings_dir()?.join("window.json"))
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    /// The mini-pill fold is window state like the position is — it survives
    /// restarts and stays out of `settings.json`.
    #[serde(default)]
    pub collapsed: bool,
}

/// Load the capsule position: `window.json` first, then the legacy
/// `windowX`/`windowY` keys of `settings.json`, then [`DEFAULT_POSITION`].
pub fn load() -> (f64, f64) {
    if let Ok(path) = path() {
        if let Some(position) = read_position(&path) {
            return position;
        }
    }
    legacy_settings_position().unwrap_or(DEFAULT_POSITION)
}

/// Read `window.json`; anything unreadable or non-finite is ignored so a broken
/// file degrades to the legacy/default position instead of an error state.
pub fn read_position(path: &Path) -> Option<(f64, f64)> {
    read_state(path).map(|state| (state.x, state.y))
}

/// Full state read: position plus the collapsed flag.
pub fn read_state(path: &Path) -> Option<Position> {
    let text = std::fs::read_to_string(path).ok()?;
    let position: Position = serde_json::from_str(&text).ok()?;
    (position.x.is_finite() && position.y.is_finite()).then_some(position)
}

/// Load position + collapsed in one shot.
pub fn load_full() -> (f64, f64, bool) {
    if let Ok(path) = path() {
        if let Some(state) = read_state(&path) {
            return (state.x, state.y, state.collapsed);
        }
    }
    let (x, y) = legacy_settings_position().unwrap_or(DEFAULT_POSITION);
    (x, y, false)
}

/// Read the pre-`window.json` position out of a settings file's text.
pub fn legacy_position(settings_text: &str) -> Option<(f64, f64)> {
    let value: serde_json::Value = serde_json::from_str(settings_text).ok()?;
    let x = value.get("windowX").and_then(|v| v.as_f64())?;
    let y = value.get("windowY").and_then(|v| v.as_f64())?;
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

fn legacy_settings_position() -> Option<(f64, f64)> {
    let path = dango_lib::settings::settings_path().ok()?;
    legacy_position(&std::fs::read_to_string(path).ok()?)
}

/// Save to the standard `window.json` path.
pub fn save_path(x: f64, y: f64, collapsed: bool) -> Result<(), String> {
    save(&path()?, x, y, collapsed)
}

/// Atomically write `window.json` (temp file + rename), so a crash mid-write
/// cannot leave a truncated position behind.
pub fn save(path: &Path, x: f64, y: f64, collapsed: bool) -> Result<(), String> {
    if !x.is_finite() || !y.is_finite() {
        return Err("refusing to save a non-finite window position".into());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| format!("create window directory: {error}"))?;

    let text = serde_json::to_vec(&Position { x, y, collapsed })
        .map_err(|error| format!("serialize window position: {error}"))?;
    let temporary = temporary_path(path);
    let result = (|| {
        std::fs::write(&temporary, text)
            .map_err(|error| format!("write temporary window position: {error}"))?;
        std::fs::rename(&temporary, path).map_err(|error| format!("replace window.json: {error}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(&name)
}

/// Keep a `size`-sized window at `(x, y)` inside `screen`, leaving `margin`
/// points visible on every side. A window larger than the screen is pinned to
/// the top-left corner instead of being pushed out of view.
pub fn clamp_to_screen(
    x: f64,
    y: f64,
    size: (f64, f64),
    screen: (f64, f64, f64, f64),
    margin: f64,
) -> (f64, f64) {
    let (screen_left, screen_top, screen_right, screen_bottom) = screen;
    let (width, height) = size;
    let max_x = (screen_right - margin - width).max(screen_left + margin);
    let max_y = (screen_bottom - margin - height).max(screen_top + margin);
    (
        x.clamp(screen_left + margin, max_x),
        y.clamp(screen_top + margin, max_y),
    )
}

/// When the pending position should be flushed, given the last `Moved` event.
pub fn flush_deadline(last_moved_at: Instant) -> Instant {
    last_moved_at + SAVE_DEBOUNCE
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "dango-window-{}-{}-{}",
                std::process::id(),
                name,
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

    #[test]
    fn window_json_is_preferred_over_settings_and_default() {
        let dir = TempDir::new("prefer");
        let path = dir.0.join("window.json");
        std::fs::write(&path, r#"{"x":321,"y":654}"#).unwrap();
        assert_eq!(read_position(&path), Some((321.0, 654.0)));

        let settings = r#"{"version":1,"windowX":10,"windowY":20}"#;
        assert_eq!(legacy_position(settings), Some((10.0, 20.0)));
        assert_eq!(DEFAULT_POSITION, (80.0, 120.0));
    }

    #[test]
    fn missing_or_broken_window_json_is_ignored() {
        let dir = TempDir::new("broken");
        let path = dir.0.join("window.json");
        assert_eq!(read_position(&path), None);

        std::fs::write(&path, "{ not-json").unwrap();
        assert_eq!(read_position(&path), None);

        std::fs::write(&path, r#"{"x":"left","y":20}"#).unwrap();
        assert_eq!(read_position(&path), None);

        // Legacy settings are only used when window.json has nothing usable.
        assert_eq!(
            legacy_position(r#"{"windowX":1,"windowY":2}"#),
            Some((1.0, 2.0))
        );
        assert_eq!(legacy_position(r#"{"version":1}"#), None);
        assert_eq!(legacy_position("not json"), None);
        assert_eq!(legacy_position(r#"{"windowX":1}"#), None);
    }

    #[test]
    fn save_writes_json_atomically_and_round_trips() {
        let dir = TempDir::new("save");
        let path = dir.0.join("window.json");
        save(&path, 1234.5, 678.0, true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, r#"{"x":1234.5,"y":678.0,"collapsed":true}"#);
        assert_eq!(read_position(&path), Some((1234.5, 678.0)));
        assert_eq!(read_state(&path).unwrap().collapsed, true);
        // The temporary file is renamed away, not left behind.
        assert!(!dir.0.join("window.json.tmp").exists());
    }

    #[test]
    fn save_rejects_non_finite_positions_without_writing() {
        let dir = TempDir::new("nonfinite");
        let path = dir.0.join("window.json");
        assert!(save(&path, f64::NAN, 10.0, false).is_err());
        assert!(save(&path, 10.0, f64::INFINITY, false).is_err());
        assert!(!path.exists());
        assert!(!dir.0.join("window.json.tmp").exists());
    }

    #[test]
    fn clamp_keeps_the_window_fully_visible() {
        let screen = (0.0, 0.0, 1440.0, 900.0);
        let size = (62.0, 300.0);
        assert_eq!(
            clamp_to_screen(100.0, 200.0, size, screen, 8.0),
            (100.0, 200.0)
        );
        // Past the right / bottom edge.
        assert_eq!(
            clamp_to_screen(1500.0, 950.0, size, screen, 8.0),
            (1440.0 - 8.0 - 62.0, 900.0 - 8.0 - 300.0)
        );
        // Off-screen to the top-left (e.g. a display was unplugged).
        assert_eq!(
            clamp_to_screen(-400.0, -400.0, size, screen, 8.0),
            (8.0, 8.0)
        );
    }

    #[test]
    fn clamp_handles_a_screen_smaller_than_the_window_and_negative_origins() {
        let screen = (-1920.0, -1080.0, 0.0, 0.0);
        let size = (62.0, 300.0);
        // Already fully inside a screen left of the primary one.
        assert_eq!(
            clamp_to_screen(-1900.0, -700.0, size, screen, 8.0),
            (-1900.0, -700.0)
        );
        // Off the top of that screen: clamped to its top margin.
        assert_eq!(
            clamp_to_screen(-1900.0, -5000.0, size, screen, 8.0),
            (-1900.0, -1072.0)
        );
        // Still fully visible: untouched.
        assert_eq!(
            clamp_to_screen(-1900.0, -1050.0, size, screen, 8.0),
            (-1900.0, -1050.0)
        );
        assert_eq!(
            clamp_to_screen(50.0, 50.0, size, screen, 8.0),
            (-70.0, -308.0)
        );
        // Larger than the screen: pinned to the margin corner, never pushed out.
        let tiny = (0.0, 0.0, 40.0, 40.0);
        assert_eq!(clamp_to_screen(-500.0, 900.0, size, tiny, 8.0), (8.0, 8.0));
    }

    #[test]
    fn debounce_deadline_is_300ms_after_the_last_move() {
        let start = Instant::now();
        assert_eq!(flush_deadline(start) - start, SAVE_DEBOUNCE);
    }
}
