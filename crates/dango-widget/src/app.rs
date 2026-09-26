//! The macOS app shell: windows, event loop, hover, pacing and menu bar.
//!
//! Two borderless non-activating panels (capsule + detail card), a menu-bar
//! status item, a `CVDisplayLink` paced render loop and the in-process tokio
//! runtime that owns the data loop and the localhost control API.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_quartz_core::{CALayer, CAShapeLayer};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
use winit::window::{Window, WindowAttributes, WindowId, WindowLevel};

use crate::app_model::{HoverChange, PerfModeAction, WidgetModel, CAPSULE_WIDTH};
use crate::data::{self, DataEvent, DataState};
use crate::platform::macos::ball_view::{self, BallView};
use crate::platform::macos::card::{self, CardSide, CardView};
use crate::platform::macos::display_link::DisplayLink;
use crate::platform::macos::settings_window::SettingsWindow;
use crate::platform::macos::tray::Tray;
use crate::platform::macos::window_ext;
use crate::window_state;

const CONTROL_API_PORT: u16 = dango_lib::ports::CONTROL_API;

use crate::app_model::HANDLE_ZONE;

/// The fold handle: a short arc concentric with the capsule's top curve.
/// Radius from the top curve's centre and half-width of the arc.
const HANDLE_ARC_R: f64 = 24.0;
const HANDLE_HALF_W: f64 = 9.0;
const HANDLE_STROKE: f64 = 3.0;
const HANDLE_POINTS: usize = 13;
/// Collapsed: the window is a slim pill that is itself the handle, and the
/// arc straightens into a flat bar in place.
const PILL_HEIGHT: f64 = 18.0;
const PILL_BAR_HALF_W: f64 = 11.0;
/// Fold/unfold timing: per-ball stagger, and the window clip's morph.
const FOLD_STAGGER_S: f64 = 0.03;
const FOLD_OUTLINE_S: f64 = 0.34;
const FOLD_OUTLINE_DELAY_S: f64 = 0.08;
/// Rapid pokes within this window stack into a combo.
const POKE_COMBO_MS: f64 = 1_500.0;
/// "Noticed you" / poke reactions: brief grok-ball faces.
const NOTICE_EMOTION: &str = "03";
const POKE_EMOTIONS: [&str; 4] = ["14", "13", "10", "03"];
fn card_layer_tail() -> f64 {
    crate::platform::macos::card_layer::TAIL_W
}

/// Remaining % jump between snapshots that reads as a window reset.
const REFILL_JUMP: f64 = 30.0;
/// Press-release distance below which a click counts as a click, not a drag.
const CLICK_SLOP: f64 = 4.0;

/// Paint the fold handle: quiet while the capsule is open, a touch brighter
/// when it is the mini pill's only affordance.
fn style_collapse_bar(bar: &CAShapeLayer, dark: bool, collapsed: bool) {
    let (r, g, b, a) = crate::theme::handle_color(dark);
    let a = if collapsed { (a + 0.25).min(0.85) } else { a };
    ball_view::set_stroke_rgba(bar, r, g, b, a);
}

/// Handle polyline in capsule coordinates (y-down). Open: an arc hugging the
/// top curve (centre `(W/2, W/2)`). Folded: a flat bar centred in the pill.
/// Both have the same point count so Core Animation can morph between them.
fn handle_points(collapsed: bool) -> Vec<(f64, f64)> {
    let cx = CAPSULE_WIDTH / 2.0;
    let half_angle = (HANDLE_HALF_W / HANDLE_ARC_R).asin();
    (0..HANDLE_POINTS)
        .map(|i| {
            let t = i as f64 / (HANDLE_POINTS - 1) as f64;
            if collapsed {
                (
                    cx - PILL_BAR_HALF_W + 2.0 * PILL_BAR_HALF_W * t,
                    PILL_HEIGHT / 2.0,
                )
            } else {
                let angle = -half_angle + 2.0 * half_angle * t;
                (
                    cx + HANDLE_ARC_R * angle.sin(),
                    cx - HANDLE_ARC_R * angle.cos(),
                )
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Config {
    pub control_port: u16,
    pub open_settings: Option<String>,
    /// `--debug-card[=plan]` — pin the detail card open at startup so it can be
    /// screenshotted without a real hover (synthetic cursor events never reach
    /// this non-activating panel).
    pub debug_card: Option<Option<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            control_port: CONTROL_API_PORT,
            open_settings: None,
            debug_card: None,
        }
    }
}

impl Config {
    /// Parse the supported CLI flags:
    /// `--control-port <n>`, `--open-settings[=<tab>]`, `--debug-card[=<plan>]`.
    pub fn from_args() -> Self {
        let mut config = Config::default();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--control-port" => {
                    if let Some(value) = args.next() {
                        config.control_port = value.parse().unwrap_or(CONTROL_API_PORT);
                    }
                }
                "--open-settings" => config.open_settings = Some("balls".into()),
                _ if arg.starts_with("--open-settings=") => {
                    config.open_settings = Some(arg["--open-settings=".len()..].to_string());
                }
                "--debug-card" => config.debug_card = Some(None),
                _ if arg.starts_with("--debug-card=") => {
                    config.debug_card = Some(Some(arg["--debug-card=".len()..].to_string()));
                }
                _ => {}
            }
        }
        config
    }
}

#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    DisplayTick,
    Data(DataPayload),
    MenuRefresh,
    OpenSettings(&'static str),
    ClipboardPending,
}

/// A `Copy` summary of a [`DataEvent`], because `DataEvent` owns values that are
/// not `Copy` and winit's proxy needs `Send + 'static`.
#[derive(Debug, Clone, Copy)]
pub enum DataPayload {
    Snapshot,
    Settings,
    ProxyDetail,
}

struct Windows {
    capsule: Arc<Window>,
    card: Arc<Window>,
}

pub struct WidgetApp {
    config: Config,
    proxy: Option<EventLoopProxy<UserEvent>>,
    windows: Option<Windows>,
    capsule_root: Option<Retained<CALayer>>,
    card_root: Option<Retained<CALayer>>,
    ball_views: Vec<BallView>,
    card_view: Option<CardView>,
    model: WidgetModel,
    data: Option<Arc<DataState>>,
    events: Option<tokio::sync::mpsc::UnboundedSender<DataEvent>>,
    runtime: Option<tokio::runtime::Handle>,
    display_link: Option<DisplayLink>,
    perf: Arc<Mutex<PerfModeAction>>,
    tray: Option<Tray>,
    capsule_origin: (f64, f64),
    scale: f64,
    card_side: CardSide,
    last_countdown: Instant,
    needs_redraw: bool,
    /// When the fading-out card window should be ordered off screen.
    card_order_out_at: Option<Instant>,
    card_hovered: bool,
    card_cursor: (f64, f64),
    /// Last cursor position inside the capsule, in points — lets a click hit
    /// the ball even if the pointer jumped there without a CursorMoved first.
    capsule_cursor: (f64, f64),
    settings_window: Option<SettingsWindow>,
    clipboard_requests: Option<tokio::sync::mpsc::UnboundedReceiver<crate::api::ClipboardRequest>>,
    /// Last time the capsule moved; the position is flushed once the drag stops.
    last_moved_at: Option<Instant>,
    /// Position pending in memory but not yet on disk.
    pending_position: Option<(f64, f64)>,
    /// Resolved dark/light glass for capsule + card + rings.
    dark: bool,
    /// Mini-pill fold state — every ball hidden, only the handle shows.
    collapsed: bool,
    /// A fold/unfold is animating; [`Self::finish_fold`] runs at this time.
    fold_done_at: Option<Instant>,
    /// The capsule's glass tint overlay (re-coloured on theme switch and
    /// re-laid out when the window resizes into/out of the pill).
    capsule_tint: Option<Retained<objc2_quartz_core::CAShapeLayer>>,
    /// The little bar riding the capsule's bottom curve — the fold handle.
    collapse_bar: Option<Retained<CAShapeLayer>>,
    /// Ball currently lifted by hover (index into `ball_views`).
    lifted: Option<usize>,
    handle_ready: bool,
    /// Card body offset inside its window (tail on the left edge).
    card_tail_offset: f64,
    /// Card window rect (top-left logical), for the pointer watchdog.
    card_rect: (f64, f64, f64, f64),
    /// Last time anyone touched or approached the capsule (sleep timer).
    last_interaction: Instant,
    /// Drag fling: last capsule x while dragging, for the eyes' inertia.
    drag_last_x: Option<f64>,
    /// Rapid-poke combo: plan, count, last poke time (ms).
    poke_combo: Option<(String, u32, f64)>,
    /// Where the last left-button press landed, for click-vs-drag tells:
    /// a press that releases >4 pt away was a drag, not a click.
    mouse_down: Option<(f64, f64)>,
}

impl WidgetApp {
    pub fn new(config: Config) -> Self {
        let settings = dango_lib::settings::load();
        let perf = PerfModeAction::from(settings.perf_mode());
        let dark = crate::theme::resolved_dark(settings.theme(), window_ext::system_prefers_dark());
        let (_, _, collapsed) = window_state::load_full();
        Self {
            config,
            proxy: None,
            windows: None,
            capsule_root: None,
            card_root: None,
            ball_views: Vec::new(),
            card_view: None,
            model: WidgetModel::new(settings),
            data: None,
            events: None,
            runtime: None,
            display_link: None,
            perf: Arc::new(Mutex::new(perf)),
            tray: None,
            capsule_origin: (80.0, 120.0),
            scale: 2.0,
            card_side: CardSide::Right,
            last_countdown: Instant::now(),
            needs_redraw: true,
            card_order_out_at: None,
            card_hovered: false,
            card_cursor: (0.0, 0.0),
            capsule_cursor: (0.0, 0.0),
            settings_window: None,
            clipboard_requests: None,
            last_moved_at: None,
            pending_position: None,
            dark,
            collapsed,
            fold_done_at: None,
            capsule_tint: None,
            collapse_bar: None,
            lifted: None,
            handle_ready: false,
            card_tail_offset: 0.0,
            card_rect: (0.0, 0.0, 0.0, 0.0),
            last_interaction: Instant::now(),
            drag_last_x: None,
            poke_combo: None,
            mouse_down: None,
        }
    }

    /// Expanded: handle strip on top + slots (the model accounts for both).
    /// Collapsed: a slim pill that is itself the handle.
    fn capsule_height(&self) -> f64 {
        if self.collapsed {
            PILL_HEIGHT
        } else {
            self.model.capsule_height()
        }
    }

    /// Resolved theme → capsule glass, ring tracks, collapse bar, card body.
    /// Everything that owns persistent colour gets repainted here.
    fn apply_appearance(&mut self) {
        let dark = self.dark;
        let Some(windows) = self.windows.as_ref() else {
            return;
        };
        unsafe {
            window_ext::set_window_appearance(&windows.capsule, dark);
        }
        let bounds = objc2_foundation::NSRect::new(
            objc2_foundation::NSPoint::new(0.0, 0.0),
            objc2_foundation::NSSize::new(CAPSULE_WIDTH, self.capsule_height()),
        );
        if let Some(overlay) = self.capsule_tint.as_ref() {
            let radius = if self.collapsed {
                PILL_HEIGHT / 2.0
            } else {
                CAPSULE_WIDTH / 2.0
            };
            window_ext::relayout_overlay(overlay, bounds, radius, dark);
        }
        self.sync_capsule_corners();
        for view in &mut self.ball_views {
            view.set_track_color(dark);
        }
        if let Some(bar) = self.collapse_bar.as_ref() {
            style_collapse_bar(bar, dark, self.collapsed);
        }
        if let Some(card) = self.card_view.as_mut() {
            card.set_dark(dark);
        }
        self.needs_redraw = true;
    }

    /// Re-resolve `theme` + system appearance; call whenever either could
    /// have moved (settings write, or any snapshot tick for follow-system).
    fn refresh_theme(&mut self) {
        let dark = crate::theme::resolved_dark(
            self.model.settings.theme(),
            window_ext::system_prefers_dark(),
        );
        if dark != self.dark {
            self.dark = dark;
            self.apply_appearance();
            // Repaint the open card with the new ink set.
            if let Some(plan_id) = self
                .card_view
                .as_ref()
                .and_then(|card| card.plan_id().map(str::to_string))
            {
                if self
                    .card_view
                    .as_ref()
                    .is_some_and(|card| card.is_visible())
                {
                    self.show_card_for(&plan_id);
                }
            }
        }
    }

    /// Is the capsule-local point on the fold handle's top strip?
    fn handle_hit(&self, x: f64, y: f64) -> bool {
        (0.0..=CAPSULE_WIDTH).contains(&x) && (0.0..=HANDLE_ZONE + 2.0).contains(&y)
    }

    /// Fold the capsule into the mini pill (or back out).
    ///
    /// The window snaps between sizes; what moves is a clip on the whole
    /// window (glass + tint + balls) plus the balls popping in/out, all on
    /// Core Animation's clock. (AppKit's animated resize grew the glass and
    /// the content on different clocks: square-cut balls, a bare tinted
    /// strip, one ball left hanging in an empty slab.) The state lands in
    /// window.json right away; [`Self::finish_fold`] tidies up at the end.
    fn set_collapsed(&mut self, collapsed: bool) {
        if self.collapsed == collapsed || self.fold_done_at.is_some() {
            return;
        }
        self.collapsed = collapsed;
        let full = self.model.capsule_height();
        let count = self.ball_views.len();
        let duration = if collapsed {
            self.model.set_hovered(None);
            self.drop_lift();
            self.hide_card();
            // Bottom ball goes first, so the balls fold up toward the handle.
            for (index, view) in self.ball_views.iter().enumerate() {
                view.set_dimmed(false);
                view.animate_fold_out((count - 1 - index) as f64 * FOLD_STAGGER_S);
            }
            if let Some(windows) = self.windows.as_ref() {
                window_ext::animate_window_outline(
                    &windows.capsule,
                    CAPSULE_WIDTH,
                    full,
                    PILL_HEIGHT,
                    FOLD_OUTLINE_S,
                    FOLD_OUTLINE_DELAY_S,
                );
            }
            FOLD_OUTLINE_DELAY_S + FOLD_OUTLINE_S
        } else {
            // Grow the window, then clip it back to the pill in the same
            // main-thread turn (nothing is committed in between); AppKit
            // drops a frame-view mask installed before the resize.
            self.apply_capsule_frame(full);
            if let Some(windows) = self.windows.as_ref() {
                window_ext::animate_window_outline(
                    &windows.capsule,
                    CAPSULE_WIDTH,
                    PILL_HEIGHT,
                    full,
                    FOLD_OUTLINE_S + 0.06,
                    0.0,
                );
            }
            for (index, view) in self.ball_views.iter().enumerate() {
                view.animate_fold_in(0.05 + index as f64 * FOLD_STAGGER_S * 1.3);
            }
            (0.05 + count as f64 * FOLD_STAGGER_S * 1.3 + 0.36).max(FOLD_OUTLINE_S + 0.06)
        };
        // While the clip animates it alone owns the outline: the glass and
        // tint go square, or their 31 pt corners dome the shrinking pill.
        if let Some(windows) = self.windows.as_ref() {
            window_ext::set_window_corner_radius(&windows.capsule, 0.0);
        }
        if let Some(overlay) = self.capsule_tint.as_ref() {
            window_ext::relayout_overlay(
                overlay,
                objc2_foundation::NSRect::new(
                    objc2_foundation::NSPoint::new(0.0, 0.0),
                    objc2_foundation::NSSize::new(CAPSULE_WIDTH, full),
                ),
                0.0,
                self.dark,
            );
        }
        self.layout_collapse_bar();
        self.fold_done_at = Some(Instant::now() + Duration::from_secs_f64(duration + 0.02));
        let (x, y) = self.capsule_origin;
        if let Err(error) = window_state::save_path(x, y, collapsed) {
            eprintln!("[window] save collapsed failed: {error}");
        }
        self.update_pacing();
        self.needs_redraw = true;
    }

    /// End of a fold/unfold: a folded capsule finally drops its balls and
    /// shrinks the window to the pill; either way the clip comes off.
    fn finish_fold(&mut self) {
        self.fold_done_at = None;
        if self.collapsed {
            for view in &self.ball_views {
                view.set_slot_hidden(true);
            }
        }
        // Unfolded: content may have changed height mid-animation.
        self.apply_capsule_frame(self.capsule_height());
        if let Some(windows) = self.windows.as_ref() {
            window_ext::clear_window_outline(&windows.capsule);
        }
        self.update_pacing();
        self.needs_redraw = true;
    }

    /// Snap the capsule window, its layer root, the glass tint and the
    /// window outline radius to `height` (no AppKit animation).
    fn apply_capsule_frame(&mut self, height: f64) {
        let radius = if height <= PILL_HEIGHT {
            PILL_HEIGHT / 2.0
        } else {
            CAPSULE_WIDTH / 2.0
        };
        let Some(windows) = self.windows.as_ref() else {
            return;
        };
        if let Some(root) = self.capsule_root.as_ref() {
            let scale = self.scale;
            objc2_quartz_core::CATransaction::begin();
            objc2_quartz_core::CATransaction::setDisableActions(true);
            root.setFrame(objc2_foundation::CGRect::new(
                objc2_foundation::CGPoint::new(0.0, 0.0),
                objc2_foundation::CGSize::new(CAPSULE_WIDTH / scale, height / scale),
            ));
            objc2_quartz_core::CATransaction::commit();
        }
        if let Some(overlay) = self.capsule_tint.as_ref() {
            window_ext::relayout_overlay(
                overlay,
                objc2_foundation::NSRect::new(
                    objc2_foundation::NSPoint::new(0.0, 0.0),
                    objc2_foundation::NSSize::new(CAPSULE_WIDTH, height),
                ),
                radius,
                self.dark,
            );
        }
        window_ext::set_window_corner_radius(&windows.capsule, radius);
        unsafe {
            window_ext::resize_keeping_top_left(&windows.capsule, CAPSULE_WIDTH, height, false);
        }
    }

    /// Lay the fold handle along the top: an arc while open, a flat bar in
    /// the folded pill. The top edge never moves when folding, so the handle
    /// stays under the pointer; the path change animates (arc ⇄ bar).
    fn layout_collapse_bar(&mut self) {
        let Some(bar) = self.collapse_bar.as_ref() else {
            return;
        };
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        bar.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(0.0, 0.0),
            objc2_foundation::CGSize::new(CAPSULE_WIDTH, HANDLE_ZONE + 4.0),
        ));
        objc2_quartz_core::CATransaction::commit();
        let points = handle_points(self.collapsed);
        // First layout snaps; later ones (fold/unfold) morph.
        let first_layout = !self.handle_ready;
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(first_layout);
        objc2_quartz_core::CATransaction::setAnimationDuration(0.32);
        ball_view::set_polyline_path(bar, &points);
        objc2_quartz_core::CATransaction::commit();
        style_collapse_bar(bar, self.dark, self.collapsed);
        self.handle_ready = true;
    }

    /// A quota that jumps back up (window reset) gets a party: confetti, a
    /// spin and a "task done" face.
    fn celebrate_refills(&mut self, before: &[(String, f64)]) {
        let now = now_ms();
        let refilled: Vec<String> = self
            .model
            .plans()
            .iter()
            .filter(|plan| plan.ok)
            .filter_map(|plan| {
                let old = before.iter().find(|(id, _)| *id == plan.id)?.1;
                let new = plan.remaining_percent?;
                (new - old >= REFILL_JUMP).then(|| plan.id.clone())
            })
            .collect();
        for plan_id in refilled {
            if let Some(slot) = self
                .model
                .slots
                .iter_mut()
                .find(|slot| slot.plan_id == plan_id)
            {
                slot.ball.burst(Some(30));
                slot.ball.spin(Some(1.0), None);
            }
            self.model.react(&plan_id, "33", now, 1_800.0);
            self.last_interaction = Instant::now();
            self.needs_redraw = true;
        }
    }

    /// Keep the window outline, not just the tint overlay, at the current
    /// shape's radius (full capsule: a true half-circle at each end).
    fn sync_capsule_corners(&self) {
        if let Some(windows) = self.windows.as_ref() {
            let radius = if self.collapsed {
                PILL_HEIGHT / 2.0
            } else {
                CAPSULE_WIDTH / 2.0
            };
            window_ext::set_window_corner_radius(&windows.capsule, radius);
        }
    }

    /// Everything that happens when the pointer leaves the capsule: hover
    /// cleared (starting the card's leave-grace), gaze reset, spotlight off.
    fn capsule_pointer_left(&mut self) {
        self.model.set_hovered(None);
        self.model.reset_gaze();
        self.drop_lift();
        for view in &self.ball_views {
            view.set_dimmed(false);
        }
        self.update_pacing();
        self.needs_redraw = true;
    }

    /// Settle any hover-lifted ball back to rest.
    fn drop_lift(&mut self) {
        if let Some(index) = self.lifted.take() {
            if let Some(view) = self.ball_views.get_mut(index) {
                view.set_lifted(false);
            }
        }
    }

    /// Poke a ball: squash + ripple + a random reaction face. Rapid pokes
    /// stack: the third spins it dizzy, the fifth throws confetti.
    fn poke(&mut self, plan_id: &str) {
        let now = now_ms();
        let count = match self.poke_combo.as_ref() {
            Some((id, count, last)) if id == plan_id && now - last < POKE_COMBO_MS => count + 1,
            _ => 1,
        };
        self.poke_combo = Some((plan_id.to_string(), count, now));
        let Some(index) = self
            .model
            .slots
            .iter()
            .position(|slot| slot.plan_id == plan_id)
        else {
            return;
        };
        if let Some(view) = self.ball_views.get(index) {
            view.poke(1.0 + 0.2 * (count.min(4) as f64 - 1.0));
        }
        let slot = &mut self.model.slots[index];
        slot.ball.bounce();
        match count {
            3 => {
                slot.ball.spin(Some(1.0), None);
                self.model.react(plan_id, "17", now, 1_400.0);
            }
            n if n >= 5 => {
                slot.ball.burst(Some(26));
                slot.ball.spin(Some(2.0), None);
                self.model.react(plan_id, "10", now, 1_800.0);
                self.poke_combo = None;
            }
            _ => {
                let pick = POKE_EMOTIONS[(now as usize / 7 + count as usize) % POKE_EMOTIONS.len()];
                self.model.react(plan_id, pick, now, 1_100.0);
            }
        }
        self.needs_redraw = true;
    }

    /// The `NSWindow` backing a winit window, for direct AppKit calls.
    ///
    /// Returns a retained reference, because `-[NSView window]` does not
    /// transfer ownership and objc2 requires one.
    fn ns_window(&self, window: &Window) -> Option<Retained<objc2_app_kit::NSWindow>> {
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };
        let view: &objc2_app_kit::NSView = unsafe { &*appkit.ns_view.as_ptr().cast() };
        view.window()
    }

    /// Frame of the screen holding the capsule, as (left, top, right, bottom).
    /// The capsule's screen rect in winit's top-left origin logical coords.
    fn screen_frame(&self) -> (f64, f64, f64, f64) {
        let Some(windows) = self.windows.as_ref() else {
            return (0.0, 0.0, 1470.0, 956.0);
        };
        let Some(nswindow) = self.ns_window(&windows.capsule) else {
            return (0.0, 0.0, 1470.0, 956.0);
        };
        let frame = window_ext::screen_frame_for_top_left(&nswindow);
        (
            frame.origin.x,
            frame.origin.y,
            frame.origin.x + frame.size.width,
            frame.origin.y + frame.size.height,
        )
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) {
        let capsule = self.create_window(event_loop, "dango", CAPSULE_WIDTH, self.capsule_height());
        let card = self.create_window(event_loop, "dango-card", card::CARD_WIDTH, 200.0);

        // Real backing scale from the screen the capsule landed on; hardcoding
        // 2.0 breaks every coordinate conversion and blurs the layers on
        // non-Retina displays.
        self.scale = capsule.scale_factor();

        unsafe {
            window_ext::make_non_activating_panel(&capsule);
            window_ext::make_non_activating_panel(&card);
        }
        self.capsule_tint = match unsafe {
            window_ext::put_vibrancy_behind_content(
                &capsule,
                if self.collapsed {
                    PILL_HEIGHT / 2.0
                } else {
                    CAPSULE_WIDTH / 2.0
                },
                true,
                self.dark,
            )
        } {
            Ok(overlay) => overlay,
            Err(error) => {
                eprintln!("[capsule] vibrancy setup failed: {error}");
                None
            }
        };
        // Card does NOT get vibrancy: the blur makes the background transparent
        // and shows the desktop through the text. The card's own 92%-opaque
        // dark glass background (card_layer.rs) is enough.

        let capsule_root = self.layer_root(&capsule, 0.0);
        // 0: the card clips itself via its bubble path (tail included); a
        // rounded-rect clip on the root would cut the tail off.
        let card_root = self.layer_root(&card, 0.0);
        self.capsule_root = Some(capsule_root);
        self.card_root = Some(card_root);

        // The fold handle: a little bar riding the capsule's bottom curve.
        if let Some(root) = self.capsule_root.as_ref() {
            let bar = unsafe { CAShapeLayer::new() };
            bar.setOpaque(false);
            bar.setContentsScale(self.scale);
            unsafe {
                bar.setLineWidth(HANDLE_STROKE);
                bar.setLineCap(objc2_quartz_core::kCALineCapRound);
                bar.setLineJoin(objc2_quartz_core::kCALineJoinRound);
            }
            // Above the ball slots (they're added later at z 0), below the
            // glass hairline overlay (z 1000).
            bar.setZPosition(1.0);
            root.addSublayer(&bar);
            self.collapse_bar = Some(bar);
        }

        let card_view = CardView::new(self.card_root.as_ref().unwrap(), self.scale, self.dark);
        self.card_view = Some(card_view);

        // The card's glass is a window-level effect view, so fading its layers
        // is not enough: the window itself stays off screen until hover.
        if let Some(nswindow) = self.ns_window(&card) {
            nswindow.orderOut(None);
            // `ignoresMouseEvents` is what actually keeps the hidden card
            // inert. `orderOut` alone is NOT sufficient: macOS re-orders
            // windows on screen / Space / display changes, and a borderless
            // panel that comes back on screen while its layers are still
            // hidden (opacity 0) sits invisibly over the desktop swallowing
            // clicks — and its own tracking can feed hover straight back
            // into the capsule, which is how the first card "opened by
            // itself". With mouse events ignored the window can never
            // interact, whether or not it is momentarily on screen.
            nswindow.setIgnoresMouseEvents(true);
        }

        self.windows = Some(Windows { capsule, card });
        // `screen_frame()` 靠已 attach 的 `self.windows` 找 NSWindow.screen，
        // 在这之前它的 fallback 是写死的 1440x900。放在 `windows = Some` 之后
        // 才是真正能把 x=141 这样的边角位置还原成的屏幕坐标 —— 放早了会被
        // clamp 到 fallback 屏的右边缘（一次 launchctl 重启就把窗口从左下
        // 角落甩到屏幕最右边）。
        self.capsule_origin = self.load_saved_position();
        self.apply_capsule_position();
        self.rebuild_ball_views();
        // Restored collapsed state: hide the balls before the first frame so
        // the pill never flashes the full capsule on launch.
        if self.collapsed {
            for view in &self.ball_views {
                view.set_slot_hidden(true);
            }
        }
        self.layout_collapse_bar();
    }

    fn create_window(
        &self,
        event_loop: &ActiveEventLoop,
        title: &str,
        width: f64,
        height: f64,
    ) -> Arc<Window> {
        let attrs = WindowAttributes::default()
            .with_title(title)
            .with_inner_size(LogicalSize::new(width, height))
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            // Card starts hidden, capsule starts shown — flipped through the
            // AppKit calls below (orderFrontRegardless / orderOut), not through
            // winit's `with_visible`, which doesn't reliably flip the
            // on-screen bit for borderless overlays (the widget never
            // appeared at all until this change).
            .with_visible(false)
            .with_window_level(WindowLevel::AlwaysOnTop);
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        // Borderless winit overlays on macOS never make it to the screen on
        // their own: CGWindowList keeps reporting `kCGWindowIsOnscreen =
        // false`, and the widget stays invisible indefinitely. The `with_
        // visible(true)` call flips winit's internal visible flag; the
        // `orderFrontRegardless` actually places it on screen. Both are
        // needed — `set_visible` alone does nothing visible, and
        // `orderFrontRegardless` alone leaves winit thinking the window is
        // hidden, so the first `about_to_wait` would order it out again.
        if title == "dango" {
            window.set_visible(true);
            if let Ok(handle) = window.window_handle() {
                if let RawWindowHandle::AppKit(appkit) = handle.as_raw() {
                    unsafe {
                        let view: &objc2_app_kit::NSView = &*appkit.ns_view.as_ptr().cast();
                        if let Some(nswindow) = view.window() {
                            nswindow.orderFrontRegardless();
                        }
                    }
                }
            }
        }
        window
    }

    fn layer_root(&self, window: &Window, corner_radius: f64) -> Retained<CALayer> {
        let handle = window.window_handle().expect("window handle");
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            panic!("winit did not provide an AppKit window");
        };
        unsafe {
            let view: &objc2_app_kit::NSView = &*appkit.ns_view.as_ptr().cast();
            view.setWantsLayer(true);
            let layer = view.layer().expect("content view has a backing layer");
            layer.setOpaque(false);
            // AppKit manages the view-backed layer's geometry, so draw into our
            // own sublayer, resized in `window_event`. Do NOT set
            // `geometryFlipped`: winit's layer-backed view is already y-down on
            // macOS, so flipping again renders every ball upside down (wedge
            // pointed down, eyes below the mouth line). The earlier "balls are
            // upside down" symptom was the draw_root frame lagging the window
            // resize (fixed in `resize_capsule`), not a missing flip.
            let draw_root = CALayer::new();
            draw_root.setFrame(layer.bounds());
            draw_root.setContentsScale(self.scale);
            draw_root.setOpaque(false);
            // Do NOT clip here: the vibrancy view's own cornerRadius already
            // rounds the window. Clipping the draw root to a slightly different
            // radius is what left white slivers at the corners. Keep the draw
            // root rectangular and let the vibrancy clip the whole window.
            draw_root.setZPosition(1000.0);
            // The card has no vibrancy view to round it, so clip the draw
            // root itself — text and bars can't poke past the rounded glass.
            // (The capsule passes 0: its vibrancy already rounds the window,
            // and a slightly different clip radius left white slivers.)
            if corner_radius > 0.0 {
                draw_root.setCornerRadius(corner_radius);
                draw_root.setMasksToBounds(true);
            }
            layer.addSublayer(&draw_root);
            draw_root
        }
    }

    /// Restore the capsule position: `window.json`, then the legacy
    /// `windowX`/`windowY` in settings.json, then [`window_state::DEFAULT_POSITION`].
    fn load_saved_position(&self) -> (f64, f64) {
        let (x, y) = window_state::load();
        let screen = self.screen_frame();
        let (x, y) = window_state::clamp_to_screen(
            x,
            y,
            (CAPSULE_WIDTH, self.capsule_height()),
            screen,
            crate::app_model::SCREEN_MARGIN,
        );
        (x, y)
    }

    fn apply_capsule_position(&mut self) {
        let Some(windows) = self.windows.as_ref() else {
            return;
        };
        windows.capsule.set_outer_position(LogicalPosition::new(
            self.capsule_origin.0,
            self.capsule_origin.1,
        ));
    }

    fn resize_capsule(&mut self) {
        // Mid-fold the window is sized by the animation; finish_fold settles it.
        if self.fold_done_at.is_some() {
            return;
        }
        let Some(windows) = self.windows.as_ref() else {
            return;
        };
        let height = self.capsule_height();
        let _ = windows
            .capsule
            .request_inner_size(LogicalSize::new(CAPSULE_WIDTH, height));
        // Do NOT rely on `WindowEvent::Resized` to catch up: winit drops the
        // request when the size already matches, and even when it fires the
        // event can land before the snapshot has populated `slots`. A
        // `draw_root` left at the 62x62 bootstrap frame only covers the first
        // slot, and the balls below it fall outside the flipped layer into
        // the un-flipped view layer — upside down and out of order. Set the
        // frame here, where the new height is already known.
        if let Some(root) = self.capsule_root.as_ref() {
            let scale = self.scale;
            let frame = objc2_foundation::CGRect::new(
                objc2_foundation::CGPoint::new(0.0, 0.0),
                objc2_foundation::CGSize::new(CAPSULE_WIDTH / scale, height / scale),
            );
            objc2_quartz_core::CATransaction::begin();
            objc2_quartz_core::CATransaction::setDisableActions(true);
            root.setFrame(frame);
            objc2_quartz_core::CATransaction::commit();
        }
        // The glass tint overlay's rounded-rect path doesn't track window
        // size — re-lay it out for the pill / full capsule shape.
        if let Some(overlay) = self.capsule_tint.as_ref() {
            let radius = if self.collapsed {
                PILL_HEIGHT / 2.0
            } else {
                CAPSULE_WIDTH / 2.0
            };
            window_ext::relayout_overlay(
                overlay,
                objc2_foundation::NSRect::new(
                    objc2_foundation::NSPoint::new(0.0, 0.0),
                    objc2_foundation::NSSize::new(CAPSULE_WIDTH, height),
                ),
                radius,
                self.dark,
            );
        }
        self.sync_capsule_corners();
        // The handle must follow every resize (slots arrive after init, so
        // the one-time layout isn't enough).
        self.layout_collapse_bar();
    }

    fn rebuild_ball_views(&mut self) {
        let Some(root) = self.capsule_root.as_ref() else {
            return;
        };
        self.model.sync_slots();
        let count = self.model.slots.len();
        while self.ball_views.len() < count {
            let index = self.ball_views.len();
            let mut view = BallView::new(
                root,
                self.model.slot_frame(index),
                crate::app_model::SLOT_SIZE,
                self.model.ball_side(),
                self.scale,
            );
            // Fresh views inherit the live theme and fold state.
            view.set_track_color(self.dark);
            view.set_slot_hidden(self.collapsed);
            self.ball_views.push(view);
        }
        self.ball_views.truncate(count);
        self.layout_ball_views();
        self.resize_capsule();
    }

    /// Position every slot and refresh its progress ring from the snapshot.
    fn layout_ball_views(&mut self) {
        let plans = self.model.plans();
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        for (index, view) in self.ball_views.iter_mut().enumerate() {
            view.set_position(self.model.slot_frame(index));
            let plan = self
                .model
                .slots
                .get(index)
                .and_then(|slot| plans.iter().find(|plan| plan.id == slot.plan_id));
            if let Some(plan) = plan {
                view.set_ring(&crate::theme::ring_style(plan, &self.model.settings));
            }
        }
        objc2_quartz_core::CATransaction::commit();
    }

    /// Draw one animation frame.
    fn render_frame(&mut self) {
        let now_ms = now_ms();
        self.card_pointer_watchdog();
        self.follow_pointer();
        let idle = self.last_interaction.elapsed() >= crate::app_model::SLEEP_AFTER;
        for (index, sleeping) in self.model.update_sleep(idle && !self.collapsed, now_ms) {
            if let Some(view) = self.ball_views.get_mut(index) {
                view.set_sleeping(sleeping);
            }
        }
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        ball_view::render(&mut self.model, &mut self.ball_views, now_ms);
        if let Some(card) = self.card_view.as_mut() {
            card.tick_mini(now_ms);
        }
        objc2_quartz_core::CATransaction::commit();
        self.needs_redraw = false;
    }

    /// Belt and braces for the card: if the pointer is in neither the capsule
    /// nor the card, make sure the leave-grace is running even when AppKit
    /// never delivered the card's `mouseExited` (the "card won't go away" bug).
    fn card_pointer_watchdog(&mut self) {
        let (mx, my) = window_ext::global_mouse_pos();
        let inside = |(x, y, w, h): (f64, f64, f64, f64)| {
            mx >= x - 1.0 && mx <= x + w + 1.0 && my >= y - 1.0 && my <= y + h + 1.0
        };
        let (cx, cy) = self.capsule_origin;
        let in_capsule = inside((cx, cy, CAPSULE_WIDTH, self.capsule_height()));
        // AppKit sometimes never sends the capsule's mouseExited (a fast exit,
        // a cursor warp): the hover then sticks, the balls stay dimmed and the
        // card hangs around for good. Treat "pointer not over the capsule" as
        // having left it.
        if self.model.hovered.is_some() && !in_capsule {
            eprintln!("[hover] pointer left the capsule without an exit event; recovering");
            self.capsule_pointer_left();
        }
        let card_open = self
            .card_view
            .as_ref()
            .is_some_and(|card| card.is_visible());
        if !card_open || self.model.hovered.is_some() {
            return;
        }
        if inside(self.card_rect) || in_capsule {
            return;
        }
        if self.card_hovered {
            eprintln!("[hover] pointer left the card without an exit event; recovering");
            self.card_hovered = false;
        }
        self.model.ensure_grace();
    }

    /// Eyes follow the pointer anywhere near the capsule (not just over it).
    /// Hover and drag own the gaze while they are active.
    fn follow_pointer(&mut self) {
        if self.collapsed || self.model.hovered.is_some() || self.drag_last_x.is_some() {
            return;
        }
        let (mx, my) = window_ext::global_mouse_pos();
        let (ox, oy) = self.capsule_origin;
        let reach = 320.0;
        let mut near = false;
        for index in 0..self.model.slots.len() {
            let (sx, sy) = self.model.slot_frame(index);
            let cx = ox + sx + crate::app_model::SLOT_SIZE / 2.0;
            let cy = oy + sy + crate::app_model::SLOT_SIZE / 2.0;
            let (dx, dy) = (mx - cx, my - cy);
            let slot = &mut self.model.slots[index];
            if slot.sleeping {
                continue;
            }
            if dx.hypot(dy) < reach {
                near = true;
                slot.ball
                    .set_gaze((dx / 140.0).clamp(-1.0, 1.0), (dy / 140.0).clamp(-1.0, 1.0));
            } else {
                slot.ball.clear_gaze();
            }
        }
        if near {
            // Someone is around: that counts as company, no dozing.
            self.last_interaction = Instant::now();
        }
    }

    fn render_static(&mut self) {
        let now_ms = now_ms();
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        ball_view::render_static(&mut self.model, &mut self.ball_views, now_ms);
        objc2_quartz_core::CATransaction::commit();
        self.needs_redraw = false;
    }

    /// Show or refresh the detail card for the hovered plan.
    fn show_card(&mut self) {
        let Some(plan_id) = self.model.hovered.clone() else {
            return;
        };
        self.show_card_for(&plan_id);
    }

    /// Show the detail card for an explicit plan (hover peek or pinned).
    fn show_card_for(&mut self, plan_id: &str) {
        if self.collapsed {
            return;
        }
        let Some(windows) = self.windows.as_ref() else {
            return;
        };
        let Some(card_view) = self.card_view.as_mut() else {
            return;
        };
        let Some(index) = self
            .model
            .slots
            .iter()
            .position(|slot| slot.plan_id == plan_id)
        else {
            return;
        };
        let Some(plan) = self
            .model
            .plans()
            .iter()
            .find(|plan| plan.id == plan_id)
            .cloned()
        else {
            return;
        };
        let (_slot_x, slot_y) = self.model.slot_frame(index);
        // Slot centre (the ball is inset inside its 44 pt slot); using half
        // the ball side here left the card 5 pt high of the ball.
        let ball_center_y = slot_y + crate::app_model::SLOT_SIZE / 2.0;

        let now_ms = now_ms() as u64;
        let pinned = self.model.pinned.as_deref() == Some(plan_id);
        let fetched_at = self
            .model
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.fetched_at)
            .unwrap_or(0);
        // Read before `show_plan` overwrites `plan_id` — the pop-in fires only
        // when the card's content actually changes.
        let entering = card_view.plan_id() != Some(plan_id);
        // Already open (switching balls): keep the card up and just swap the
        // content — replaying the fade-in from 0 read as a blink.
        let was_open = card_view.is_visible();
        // Swap content with implicit animations OFF: otherwise Core Animation
        // cross-fades every text colour / frame / path change for 0.25 s and
        // the card visibly flashes on each hover.
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        let height = card_view.show_plan(&plan, &self.model.settings, fetched_at, now_ms, pinned);
        objc2_quartz_core::CATransaction::commit();
        self.last_countdown = Instant::now();

        let screen = self.screen_frame();
        let side = card::choose_side(
            self.capsule_origin.0,
            CAPSULE_WIDTH,
            card::CARD_WIDTH,
            screen,
        );
        self.card_side = side;
        let (x, y) = card::card_position(
            self.capsule_origin.0,
            self.capsule_origin.1,
            CAPSULE_WIDTH,
            ball_center_y,
            card::CARD_WIDTH,
            height,
            side,
            screen,
        );
        // Speech tail toward the capsule: the window grows by the tail on
        // that side, and the tail tip lines up with the hovered ball.
        let tail_left = side == CardSide::Right;
        self.card_tail_offset = if tail_left { card_layer_tail() } else { 0.0 };
        let window_x = x - self.card_tail_offset;
        let tail_y = self.capsule_origin.1 + ball_center_y - y;
        windows
            .card
            .set_outer_position(LogicalPosition::new(window_x, y));
        self.card_rect = (window_x, y, card::CARD_WIDTH + card_layer_tail(), height);
        let _ = windows.card.request_inner_size(LogicalSize::new(
            card::CARD_WIDTH + card_layer_tail(),
            height,
        ));
        if let Some(card_view) = self.card_view.as_mut() {
            objc2_quartz_core::CATransaction::begin();
            objc2_quartz_core::CATransaction::setDisableActions(true);
            card_view.set_tail(tail_left, tail_y);
            card_view.set_hidden(false);
            objc2_quartz_core::CATransaction::commit();
        }
        // winit 创建 macOS 窗口时虽然默认 visible=true，CGWindowList 里
        // kCGWindowIsOnscreen 仍是 false；得再点一次 visible 才能让窗口真正
        // 上屏（胶囊那边的 set_visible 在 create_window 里，卡片在这里每次
        // 重新拉起来之前也要补一下，省得淡出→重开后只剩 vibrancy 空窗）。
        windows.card.set_visible(true);
        self.card_order_out_at = None;
        // Pop-in only when the card's CONTENT changes (open or plan switch) —
        // not on the Moved re-layout during a capsule drag, where re-firing it
        // made the card flicker along the drag.
        if entering {
            if let Some(card_view) = self.card_view.as_ref() {
                if was_open {
                    card_view.animate_bars_in();
                } else {
                    card_view.animate_in(side);
                }
            }
        }
        // `orderFrontRegardless` shows the panel without making it key or
        // activating the app (winit's `set_visible` would make it key).
        if let Some(nswindow) = self.ns_window(&windows.card) {
            nswindow.setIgnoresMouseEvents(false);
            unsafe { nswindow.orderFrontRegardless() };
        }
    }

    fn hide_card(&mut self) {
        if let Some(card) = self.card_view.as_mut() {
            card.hide();
        }
        if let Some(windows) = self.windows.as_ref() {
            windows.card.set_visible(false);
            if let Some(nswindow) = self.ns_window(&windows.card) {
                nswindow.setIgnoresMouseEvents(true);
                nswindow.orderOut(None);
            }
        }
        self.card_order_out_at = None;
    }

    fn order_out_card_if_due(&mut self) {
        // No-op: hide_card now hides immediately, no deferred order-out needed.
        self.card_order_out_at = None;
    }

    /// Start or stop the display link according to the performance mode.
    ///
    /// Smooth ticks at 60 fps and balanced at 30 fps (both keep animating);
    /// saver only animates while a ball is hovered or the card is visible.
    fn update_pacing(&mut self) {
        let mode = self.perf.lock().map(|mode| *mode).unwrap_or_default();
        let Some(link) = self.display_link.as_mut() else {
            return;
        };
        match mode {
            PerfModeAction::Saver => {
                let busy = self.model.hovered.is_some()
                    || self
                        .card_view
                        .as_ref()
                        .is_some_and(|card| card.is_visible());
                if busy {
                    link.set_target_fps(60);
                    let _ = link.start();
                } else {
                    link.stop();
                }
            }
            PerfModeAction::Balanced => {
                link.set_target_fps(30);
                let _ = link.start();
            }
            PerfModeAction::Smooth => {
                link.set_target_fps(60);
                let _ = link.start();
            }
        }
    }

    fn handle_data_event(&mut self, payload: DataPayload) {
        let Some(state) = self.data.clone() else {
            return;
        };
        match payload {
            DataPayload::Snapshot => {
                let watched = state.snapshot_watch().borrow_and_update().clone();
                let Some(snapshot) = watched else {
                    return;
                };
                let slot_count = self.model.slots.len();
                let plan_count = snapshot.plans.len();
                let before: Vec<(String, f64)> = self
                    .model
                    .plans()
                    .iter()
                    .filter_map(|plan| Some((plan.id.clone(), plan.remaining_percent?)))
                    .collect();
                self.model.apply_snapshot((*snapshot).clone());
                self.celebrate_refills(&before);
                // Follow-system cheaply: a snapshot lands every minute, so a
                // system appearance flip reaches the widget within a cycle.
                self.refresh_theme();
                if plan_count != slot_count {
                    self.rebuild_ball_views();
                } else {
                    self.layout_ball_views();
                }
                self.needs_redraw = true;
                // `--debug-card[=plan]`: pin the card open on the first snapshot
                // so the real card can be screenshotted without a real hover.
                if let Some(want) = &self.config.debug_card {
                    let target = want
                        .clone()
                        .or_else(|| self.model.slots.first().map(|slot| slot.plan_id.clone()));
                    if let Some(plan_id) = target {
                        let already = self.card_view.as_ref().and_then(|card| card.plan_id())
                            == Some(plan_id.as_str());
                        if !already && self.model.plans().iter().any(|plan| plan.id == plan_id) {
                            self.model.pinned = Some(plan_id.clone());
                            self.model.hovered = Some(plan_id);
                            self.show_card();
                        }
                    }
                }
            }
            DataPayload::Settings => {
                let settings = state.settings_watch().borrow().clone();
                let ring_before = self.model.settings.ring_mode();
                self.model.apply_settings((*settings).clone());
                self.refresh_theme();
                let perf = PerfModeAction::from(self.model.perf_mode());
                if let Ok(mut current) = self.perf.lock() {
                    *current = perf;
                }
                if let Some(tray) = self.tray.as_ref() {
                    tray.sync_perf(perf);
                }
                self.rebuild_ball_views();
                // A new ring look replays the entrance so the change is seen.
                if self.model.settings.ring_mode() != ring_before && !self.collapsed {
                    for (index, view) in self.ball_views.iter().enumerate() {
                        view.animate_ring_enter(index as f64 * 0.07);
                    }
                }
                self.needs_redraw = true;
            }
            DataPayload::ProxyDetail => {}
        }
        self.update_pacing();
    }

    /// Which ball, if any, is under the given capsule-local point.
    fn slot_at(&self, x: f64, y: f64) -> Option<&str> {
        if self.collapsed {
            return None;
        }
        for (index, slot) in self.model.slots.iter().enumerate() {
            let (slot_x, slot_y) = self.model.slot_frame(index);
            if x >= slot_x
                && x <= slot_x + crate::app_model::SLOT_SIZE
                && y >= slot_y
                && y <= slot_y + crate::app_model::SLOT_SIZE
            {
                return Some(slot.plan_id.as_str());
            }
        }
        None
    }

    /// Write the capsule position out once the window has been still for
    /// [`window_state::SAVE_DEBOUNCE`].
    fn flush_pending_position(&mut self) {
        let Some(last_moved_at) = self.last_moved_at else {
            return;
        };
        let Some((x, y)) = self.pending_position.take() else {
            return;
        };
        if last_moved_at.elapsed() < window_state::SAVE_DEBOUNCE {
            self.pending_position = Some((x, y));
            return;
        }
        self.last_moved_at = None;
        // Landed: everyone hops once, top to bottom, and looks ahead again.
        if self.drag_last_x.take().is_some() && !self.collapsed {
            let now = now_ms();
            for (index, slot) in self.model.slots.iter_mut().enumerate() {
                slot.ball.clear_gaze();
                slot.ball.bounce_at(now + index as f64 * 45.0);
            }
            self.needs_redraw = true;
        }
        let screen = self.screen_frame();
        let (x, y) = window_state::clamp_to_screen(
            x,
            y,
            (CAPSULE_WIDTH, self.capsule_height()),
            screen,
            crate::app_model::SCREEN_MARGIN,
        );
        if let Err(error) = window_state::save_path(x, y, self.collapsed) {
            eprintln!("[window] save position failed: {error}");
        }
    }

    /// Tick the card's countdown once per second while it is visible.
    ///
    /// Called only from `about_to_wait`, which runs after every event batch —
    /// including every `DisplayTick` — so a running display link still gets
    /// per-frame opportunities, and saver mode (link stopped) still ticks.
    fn tick_card_countdown_if_due(&mut self) {
        if self.card_view.as_ref().is_some_and(|card| {
            card.is_visible() && self.last_countdown.elapsed() >= Duration::from_secs(1)
        }) {
            let now_ms = now_ms() as u64;
            if let Some(card) = self.card_view.as_mut() {
                card.tick_content(now_ms);
            }
            self.last_countdown = Instant::now();
        }
    }

    fn open_settings(&mut self, event_loop: &ActiveEventLoop, tab: &str) {
        if let Some(window) = self.settings_window.as_ref() {
            window.focus(tab);
        } else {
            self.settings_window = Some(SettingsWindow::new(event_loop, tab));
        }
    }
}

impl ApplicationHandler<UserEvent> for WidgetApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // After AppKit finished launching: setting the Dock icon before that
        // (as this used to) got reset to the generic "exec" tile.
        set_dock_icon();
        if self.windows.is_some() {
            return;
        }
        self.initialize(event_loop);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let state = DataState::new(data::http_client());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DataEvent>();
        let mut data_rx = data::spawn(
            Arc::clone(&state),
            data::DataConfig,
            runtime.handle(),
        );

        // Forward data events (from the data loops, the control API and the
        // tray) to the UI thread.
        let proxy = self.proxy.clone().expect("event loop proxy");
        runtime.spawn(async move {
            loop {
                let event = tokio::select! {
                    Some(event) = data_rx.recv() => event,
                    Some(event) = rx.recv() => event,
                    else => break,
                };
                let payload = match event {
                    DataEvent::Snapshot(_) => DataPayload::Snapshot,
                    DataEvent::Settings(_) => DataPayload::Settings,
                    DataEvent::ProxyDetail(_) => DataPayload::ProxyDetail,
                };
                if proxy.send_event(UserEvent::Data(payload)).is_err() {
                    break;
                }
            }
        });

        let (clipboard_tx, clipboard_rx) = tokio::sync::mpsc::unbounded_channel();
        self.clipboard_requests = Some(clipboard_rx);
        let clipboard_proxy = self.proxy.clone().expect("event loop proxy");
        let clipboard_notify = Arc::new(move || {
            let _ = clipboard_proxy.send_event(UserEvent::ClipboardPending);
        });

        // Localhost control API on 127.0.0.1:<control_port>.
        let control_port = self.config.control_port;
        let api_state = crate::api::ApiState {
            data: Arc::clone(&state),
            events: tx.clone(),
            clipboard: clipboard_tx,
            clipboard_notify,
            port: control_port,
        };
        let (api_ready_tx, api_ready_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("control api runtime");
            runtime.block_on(async move {
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], control_port));
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => {
                        let _ = api_ready_tx.send(Ok(()));
                        if let Err(error) =
                            axum::serve(listener, crate::api::router(api_state)).await
                        {
                            eprintln!("[control-api] serve 退出: {error}");
                        }
                    }
                    Err(error) => {
                        let _ = api_ready_tx.send(Err(error.to_string()));
                        eprintln!("[control-api] serve 退出: {error}");
                    }
                }
            });
        });
        match api_ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("[control-api] settings UI unavailable: {error}"),
            Err(error) => eprintln!("[control-api] startup timed out: {error}"),
        }

        self.data = Some(state);
        self.events = Some(tx);
        // The runtime must outlive the app, so it is leaked rather than dropped.
        self.runtime = Some(runtime.handle().clone());
        std::mem::forget(runtime);

        // Menu bar.
        crate::platform::macos::settings_window::install_edit_menu();
        let perf = Arc::clone(&self.perf);
        let events = self.events.clone().unwrap();
        let data = self.data.clone().unwrap();
        let runtime = self.runtime.clone().expect("tokio runtime");
        self.tray = Some(Tray::new(
            data,
            events,
            perf,
            runtime,
            self.proxy.clone().unwrap(),
        ));

        // Display link for vsync-paced animation.
        let proxy = self.proxy.clone().expect("event loop proxy");
        match DisplayLink::new(60, move || {
            let _ = proxy.send_event(UserEvent::DisplayTick);
        }) {
            Ok(link) => self.display_link = Some(link),
            Err(error) => eprintln!("[pacing] display link creation failed: {error}"),
        }
        self.update_pacing();

        self.render_static();
        if let Some(tab) = self.config.open_settings.clone() {
            self.open_settings(event_loop, &tab);
        }
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::DisplayTick => {
                self.needs_redraw = true;
            }
            UserEvent::Data(payload) => {
                self.handle_data_event(payload);
            }
            UserEvent::MenuRefresh => {
                if let (Some(state), Some(events), Some(runtime)) =
                    (self.data.clone(), self.events.clone(), self.runtime.clone())
                {
                    runtime.spawn(async move {
                        data::refresh_now(&state, &events).await;
                    });
                }
            }
            UserEvent::OpenSettings(tab) => self.open_settings(event_loop, tab),
            UserEvent::ClipboardPending => {
                if let Some(requests) = self.clipboard_requests.as_mut() {
                    while let Ok(request) = requests.try_recv() {
                        let result =
                            crate::platform::macos::settings_window::set_clipboard(&request.text);
                        let _ = request.result.send(result);
                    }
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(windows) = self.windows.as_ref() else {
            return;
        };
        let is_capsule = window_id == windows.capsule.id();
        let is_card = window_id == windows.card.id();
        let is_settings = self
            .settings_window
            .as_ref()
            .is_some_and(|window| window.id() == window_id);

        match event {
            WindowEvent::CloseRequested if is_settings => {
                if let Some(mut window) = self.settings_window.take() {
                    window.close();
                }
            }
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                let root = if is_capsule {
                    self.capsule_root.as_ref()
                } else if is_card {
                    self.card_root.as_ref()
                } else {
                    None
                };
                if let Some(root) = root {
                    let scale = if is_capsule {
                        windows.capsule.scale_factor()
                    } else {
                        windows.card.scale_factor()
                    };
                    let frame = objc2_foundation::CGRect::new(
                        objc2_foundation::CGPoint::new(0.0, 0.0),
                        objc2_foundation::CGSize::new(
                            size.width as f64 / scale,
                            size.height as f64 / scale,
                        ),
                    );
                    objc2_quartz_core::CATransaction::begin();
                    objc2_quartz_core::CATransaction::setDisableActions(true);
                    root.setFrame(frame);
                    objc2_quartz_core::CATransaction::commit();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if !is_capsule {
                    if is_card {
                        let scale = windows.card.scale_factor();
                        self.card_cursor = (position.x / scale, position.y / scale);
                    }
                    return;
                }
                let scale = windows.capsule.scale_factor();
                let x = position.x / scale;
                let y = position.y / scale;
                self.capsule_cursor = (x, y);
                self.last_interaction = Instant::now();
                let hovered = self.slot_at(x, y).map(str::to_string);
                if hovered != self.model.hovered {
                    let change = self.model.set_hovered(hovered);
                    let hovered_index =
                        self.model.hovered.as_deref().and_then(|id| {
                            self.model.slots.iter().position(|slot| slot.plan_id == id)
                        });
                    // Spotlight: everything but the hovered ball dims, and the
                    // hovered one lifts with a glow.
                    for (index, view) in self.ball_views.iter().enumerate() {
                        view.set_dimmed(hovered_index.is_some_and(|hit| hit != index));
                    }
                    if self.lifted != hovered_index {
                        self.drop_lift();
                        if let Some(hit) = hovered_index {
                            if let Some(view) = self.ball_views.get_mut(hit) {
                                view.set_lifted(true);
                            }
                            self.lifted = Some(hit);
                        }
                    }
                    // The hovered ball notices you; the others turn to look at it.
                    if let Some(hit) = hovered_index {
                        let id = self.model.slots[hit].plan_id.clone();
                        self.model.react(&id, NOTICE_EMOTION, now_ms(), 700.0);
                        for (index, slot) in self.model.slots.iter_mut().enumerate() {
                            if index != hit {
                                let dy = if index < hit { 0.9 } else { -0.9 };
                                slot.ball.set_gaze(0.0, dy);
                            }
                        }
                    }
                    match change {
                        HoverChange::Entered(_) => {
                            // Entering the capsule replays every ring's draw-in
                            // staggered top-down — the "hello" animation.
                            for (index, view) in self.ball_views.iter().enumerate() {
                                view.animate_ring_enter(index as f64 * 0.07);
                            }
                            self.show_card();
                            self.needs_redraw = true;
                        }
                        HoverChange::Switch(plan_id) => {
                            if let Some(index) = self
                                .model
                                .slots
                                .iter()
                                .position(|slot| slot.plan_id == plan_id)
                            {
                                if let Some(view) = self.ball_views.get(index) {
                                    view.animate_ring_enter(0.0);
                                }
                            }
                            self.show_card();
                            self.needs_redraw = true;
                        }
                        HoverChange::Left => {
                            self.needs_redraw = true;
                        }
                    }
                    self.update_pacing();
                }
                // Gaze follows the pointer over the hovered ball.
                if let Some(hovered) = self.model.hovered.clone() {
                    if let Some(index) = self
                        .model
                        .slots
                        .iter()
                        .position(|slot| slot.plan_id == hovered)
                    {
                        let (slot_x, slot_y) = self.model.slot_frame(index);
                        let side = self.model.ball_side();
                        let center_x = slot_x + crate::app_model::SLOT_SIZE / 2.0;
                        let center_y = slot_y + crate::app_model::SLOT_SIZE / 2.0;
                        let dx = ((x - center_x) / (side / 2.0)).clamp(-1.0, 1.0);
                        let dy = ((y - center_y) / (side / 2.0)).clamp(-1.0, 1.0);
                        if let Some(slot) = self
                            .model
                            .slots
                            .iter_mut()
                            .find(|slot| slot.plan_id == hovered)
                        {
                            slot.ball.set_gaze(dx, dy);
                        }
                    }
                }
            }
            WindowEvent::CursorEntered { .. } if is_card => {
                self.card_hovered = true;
            }
            WindowEvent::CursorLeft { .. } if is_card => {
                self.card_hovered = false;
                self.model.start_grace();
                self.update_pacing();
            }
            WindowEvent::CursorLeft { .. } if is_capsule => {
                self.capsule_pointer_left();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if is_capsule {
                    self.mouse_down = Some(self.capsule_cursor);
                    self.last_interaction = Instant::now();
                }
                if !is_capsule {
                    if is_card {
                        // Only the proxy row opens settings (per the design
                        // rule: 反代行才开设置); everywhere else the card is
                        // read-only and swallows the click.
                        let open = self.card_view.as_ref().and_then(|card| {
                            let plan_id = card.plan_id()?;
                            card.proxy_row_contains(
                                self.card_cursor.0 - self.card_tail_offset,
                                self.card_cursor.1,
                            )
                            .then(|| plan_id.to_string())
                        });
                        if let Some(tab) = open {
                            self.open_settings(event_loop, &tab);
                        }
                    }
                    return;
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                if !is_capsule {
                    return;
                }
                // Click = press + release within CLICK_SLOP; anything farther
                // was the start of a window drag, not a tap.
                let (x, y) = self.capsule_cursor;
                let down = self.mouse_down.take();
                let is_click = down.is_none_or(|(dx, dy)| {
                    (x - dx).abs() < CLICK_SLOP && (y - dy).abs() < CLICK_SLOP
                });
                if !is_click {
                    return;
                }
                // Folded pill: any tap unfolds.
                if self.collapsed {
                    self.set_collapsed(false);
                    return;
                }
                // The arc on top folds the capsule away.
                if self.handle_hit(x, y) {
                    self.set_collapsed(true);
                    return;
                }
                // Clicking a ball pokes it; the window never becomes key. Hit-test the stored cursor pos so
                // a click that lands without a preceding CursorMoved still
                // counts.
                let plan_id = self
                    .slot_at(x, y)
                    .map(str::to_string)
                    .or_else(|| self.model.hovered.clone());
                if let Some(plan_id) = plan_id {
                    // Click pokes the ball (no pin: the card already follows
                    // hover and stays while the pointer is on it).
                    self.poke(&plan_id);
                    self.show_card_for(&plan_id);
                    self.needs_redraw = true;
                    self.update_pacing();
                }
            }
            WindowEvent::Moved(position) if is_capsule => {
                // `Moved` reports physical pixels; everything else here is in points.
                let logical = position.to_logical::<f64>(windows.capsule.scale_factor());
                // Drag fling: the eyes lag behind the motion, like being swung.
                if let Some(last_x) = self.drag_last_x.replace(logical.x) {
                    let vx = logical.x - last_x;
                    for slot in self.model.slots.iter_mut().filter(|slot| !slot.sleeping) {
                        slot.ball.set_gaze((-vx / 6.0).clamp(-1.0, 1.0), -0.3);
                    }
                    self.needs_redraw = true;
                }
                self.last_interaction = Instant::now();
                self.capsule_origin = (logical.x, logical.y);
                // Debounced: a drag fires `Moved` continuously and the position
                // only matters once it stops.
                self.pending_position = Some(self.capsule_origin);
                self.last_moved_at = Some(Instant::now());
                if self
                    .card_view
                    .as_ref()
                    .is_some_and(|card| card.is_visible())
                {
                    // Dragging with a pinned card: `hovered` is None, so
                    // `show_card()` no-ops and the card was left behind.
                    let target = self
                        .model
                        .hovered
                        .clone()
                        .or_else(|| self.model.pinned.clone())
                        .or_else(|| {
                            self.card_view
                                .as_ref()
                                .and_then(|card| card.plan_id().map(str::to_string))
                        });
                    if let Some(plan_id) = target {
                        self.show_card_for(&plan_id);
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { .. } if is_capsule => {
                // Dragging between Retina and non-Retina screens changes the
                // backing scale; rebuild the ball layers at the new scale or
                // they stay blurry / mis-sized.
                let new_scale = windows.capsule.scale_factor();
                if (new_scale - self.scale).abs() > f64::EPSILON {
                    self.scale = new_scale;
                    if let Some(root) = self.capsule_root.as_ref() {
                        root.setContentsScale(new_scale);
                    }
                    self.ball_views.clear();
                    self.rebuild_ball_views();
                    self.needs_redraw = true;
                }
            }
            _ if is_settings => {}
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.flush_pending_position();
        // The 150 ms hover grace and the 1 s countdown tick are the only work
        // that still happens while the display link is stopped (saver mode).
        if self.model.grace_expired()
            && !self.card_hovered
            && self
                .card_view
                .as_ref()
                .is_some_and(|card| card.is_visible())
        {
            // A pinned card never leaves: the pointer leaving just reverts
            // the card back to the pinned plan after a peek at another ball.
            match self.model.pinned.clone() {
                Some(plan_id)
                    if self.card_view.as_ref().and_then(|card| card.plan_id())
                        != Some(plan_id.as_str()) =>
                {
                    self.show_card_for(&plan_id);
                }
                Some(_) => {}
                None => {
                    self.hide_card();
                    self.update_pacing();
                }
            }
        }
        if self
            .fold_done_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.finish_fold();
        }
        self.order_out_card_if_due();
        self.tick_card_countdown_if_due();
        if self.needs_redraw {
            self.render_frame();
        }
        // In saver mode nothing else wakes the loop, so timers must.
        let card_up = self
            .card_view
            .as_ref()
            .is_some_and(|card| card.is_visible());
        let grace = self
            .model
            .grace_deadline()
            .filter(|_| card_up && !self.card_hovered);
        let countdown = card_up.then(|| self.last_countdown + Duration::from_secs(1));
        let position_flush = self
            .last_moved_at
            .filter(|_| self.pending_position.is_some())
            .map(window_state::flush_deadline);
        let next = [
            grace,
            self.card_order_out_at,
            countdown,
            position_flush,
            self.fold_done_at,
        ]
        .into_iter()
        .flatten()
        .min();
        event_loop.set_control_flow(match next {
            Some(deadline) => ControlFlow::WaitUntil(deadline),
            None => ControlFlow::Wait,
        });
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as f64)
        .unwrap_or(0.0)
}

pub fn run(config: Config) {
    disable_app_nap();

    let mut builder = EventLoop::<UserEvent>::with_user_event();
    // Regular (not Accessory) so the app gets a Dock icon and shows in
    // Cmd-Tab / Force Quit — the menu-bar item alone is too easy to lose.
    builder.with_activation_policy(ActivationPolicy::Regular);
    let event_loop = builder.build().expect("build event loop");

    let mut app = WidgetApp::new(config);
    app.proxy = Some(event_loop.create_proxy());
    event_loop.run_app(&mut app).expect("run event loop");
}

/// The app icon (assets/AppIcon.svg rendered to 1024 px), baked into the binary.
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/AppIcon.png");

fn set_dock_icon() {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::ClassType;
    let Some(marker) = objc2_foundation::MainThreadMarker::new() else {
        return;
    };
    unsafe {
        let data: Retained<AnyObject> = objc2::msg_send_id![
            objc2::class!(NSData),
            dataWithBytes: APP_ICON_PNG.as_ptr().cast::<std::ffi::c_void>(),
            length: APP_ICON_PNG.len()
        ];
        let image: Option<Retained<objc2_app_kit::NSImage>> = objc2::msg_send_id![
            objc2_app_kit::NSImage::alloc(),
            initWithData: &*data
        ];
        match image {
            Some(image) => {
                objc2_app_kit::NSApplication::sharedApplication(marker)
                    .setApplicationIconImage(Some(&image));
            }
            None => eprintln!("[icon] AppIcon.png did not decode"),
        }
    }
}

/// Suppress App Nap so animation and the data loops keep running while the
/// widget is in the background.
fn disable_app_nap() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

    let info = NSProcessInfo::processInfo();
    let options = NSActivityOptions::NSActivityUserInitiatedAllowingIdleSystemSleep
        | NSActivityOptions::NSActivityLatencyCritical;
    let reason = NSString::from_str("dango animation and proxy monitor");
    let token = unsafe { info.beginActivityWithOptions_reason(options, &reason) };
    // The token must outlive the process, so intentionally leak it.
    std::mem::forget(token);
}
