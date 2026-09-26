//! macOS platform layer.
//!
//! Everything that touches AppKit / Core Animation lives here. The rest of the
//! widget is platform-neutral, so a future Windows implementation can be added
//! as `platform/windows/` without touching the model or data layers.

pub mod ball_view;
pub mod card;
pub mod card_font;
pub mod card_layer;
pub mod display_link;
pub mod dock;
pub mod settings_window;
pub mod tray;
pub mod window_ext;
