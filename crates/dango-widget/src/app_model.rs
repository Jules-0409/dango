//! Platform-neutral widget model: capsule slots, detail card content, pacing.
//!
//! The platform layer owns windows/layers; this module owns *what* is shown and
//! *when* it must be redrawn, so the same rules can be reused by a future
//! Windows implementation.

use std::time::{Duration, Instant};

use dango_lib::models::{PlanQuota, Snapshot};
use dango_lib::settings::{PerfMode, Settings};
use dango_lib::PlanQuota as Plan;
use grok_ball::Ball;

use crate::theme;

/// Vertical capsule metrics, matching the retired web capsule (62 pt wide,
/// 44 pt slots).
pub const CAPSULE_WIDTH: f64 = 62.0;
pub const SLOT_SIZE: f64 = 44.0;
/// Half-gap between the ball and the ring's inner edge. The ring's inner
/// radius is `RING_RADIUS - RING_STROKE/2 = 18.25pt` while the ball reaches
/// `SLOT_SIZE/2 - BALL_INSET`; 5pt gives 1.25pt of breathing room, enough
/// that the pointed corners of `gem` / `wedge` / `star` never touch the ring.
pub const BALL_INSET: f64 = 5.0;
pub const SLOT_GAP: f64 = 9.0;
pub const CAPSULE_PADDING: f64 = 9.0;
/// Top strip the fold handle rides on. The capsule folds upward (top edge
/// fixed), so a handle up here stays under the pointer across fold/unfold.
pub const HANDLE_ZONE: f64 = 14.0;
/// Sleep: only balls at (effectively) 100 % doze, after this long untouched.
pub const SLEEP_FULL_PERCENT: f64 = 99.5;
pub const SLEEP_AFTER: Duration = Duration::from_secs(180);
const SLEEP_EMOTION: &str = "00";
const WAKE_EMOTION: &str = "01";
/// First slot's top edge: the handle strip plus a little less than the usual
/// padding, since the strip itself already reads as breathing room.
pub const SLOTS_TOP: f64 = HANDLE_ZONE + CAPSULE_PADDING - 4.0;
pub const CARD_GAP: f64 = 8.0;
pub const SCREEN_MARGIN: f64 = 8.0;

/// Ring geometry from the retired web capsule (viewBox 0 0 44 44, r 19.5).
pub const RING_RADIUS: f64 = 19.5;
pub const RING_STROKE: f64 = 2.5;

pub const FADE_MS: u64 = 140;
/// Long enough to cross the 8 pt gap between the capsule and the card slowly,
/// short enough that the card still feels responsive when the pointer really
/// leaves. 150 ms was too tight (the card collapsed as soon as the cursor
/// crossed the gap with any hesitation).
pub const HOVER_GRACE: Duration = Duration::from_millis(300);

/// A ball instance bound to one plan slot.
pub struct Slot {
    pub plan_id: String,
    pub ball: Ball,
    pub shape: String,
    pub color: String,
    pub emotion: String,
    pub lite: bool,
    /// While set, the ball is showing a short-lived reaction (noticed /
    /// poked) instead of `emotion`; the render tick restores it after.
    pub reaction_until: Option<f64>,
    /// Full quota and nobody around: the ball dozes until someone comes by.
    pub sleeping: bool,
}

pub struct WidgetModel {
    pub settings: Settings,
    pub snapshot: Option<Snapshot>,
    pub slots: Vec<Slot>,
    pub hovered: Option<String>,
    /// Plan whose detail card stays up after the pointer leaves. Only
    /// `--debug-card` sets it now (clicks poke instead). While pinned,
    /// hovering other balls still peeks their card, but the card reverts to
    /// the pinned plan on leave.
    pub pinned: Option<String>,
    pub card_visible: bool,
    hover_left_at: Option<Instant>,
    pub last_snapshot_at: Option<Instant>,
}

impl WidgetModel {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            snapshot: None,
            slots: Vec::new(),
            hovered: None,
            pinned: None,
            card_visible: false,
            hover_left_at: None,
            last_snapshot_at: None,
        }
    }

    pub fn perf_mode(&self) -> PerfMode {
        self.settings.perf_mode()
    }

    pub fn plans(&self) -> &[PlanQuota] {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.plans.as_slice())
            .unwrap_or(&[])
    }

    pub fn ordered_plans(&self) -> Vec<Plan> {
        crate::theme::ordered_plans(self.plans(), &self.settings)
    }

    pub fn capsule_height(&self) -> f64 {
        let count = self.slots.len();
        if count == 0 {
            return CAPSULE_WIDTH;
        }
        SLOTS_TOP + CAPSULE_PADDING + count as f64 * SLOT_SIZE + (count as f64 - 1.0) * SLOT_GAP
    }

    pub fn ball_side(&self) -> f64 {
        SLOT_SIZE - BALL_INSET * 2.0
    }

    pub fn slot_frame(&self, index: usize) -> (f64, f64) {
        let y = SLOTS_TOP + index as f64 * (SLOT_SIZE + SLOT_GAP);
        let x = (CAPSULE_WIDTH - SLOT_SIZE) / 2.0;
        (x, y)
    }

    pub fn hovered_plan(&self) -> Option<Plan> {
        let hovered = self.hovered.as_deref()?;
        self.plans().iter().find(|plan| plan.id == hovered).cloned()
    }

    /// Plan the card should show right now: the hovered ball while hovering,
    /// otherwise the pinned one.
    pub fn card_plan_id(&self) -> Option<&str> {
        self.hovered.as_deref().or(self.pinned.as_deref())
    }

    /// Show `emotion` on `plan_id`'s ball for `duration_ms`, then fall back
    /// to its quota mood. A broken plan keeps its error face — a cheerful
    /// reaction there would read as "fixed".
    pub fn react(&mut self, plan_id: &str, emotion: &str, now_ms: f64, duration_ms: f64) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.plan_id == plan_id) else {
            return false;
        };
        if slot.emotion == theme::ERROR_EMOTION {
            return false;
        }
        slot.ball.set_emotion_at(emotion, now_ms);
        slot.reaction_until = Some(now_ms + duration_ms);
        true
    }

    /// Doze or wake balls. Only a full, healthy plan sleeps (nothing to watch
    /// there); any interaction wakes everyone with a short "waking" face.
    /// Returns `(slot index, sleeping)` for every ball that changed.
    pub fn update_sleep(&mut self, idle: bool, now_ms: f64) -> Vec<(usize, bool)> {
        let full: Vec<bool> = self
            .slots
            .iter()
            .map(|slot| {
                self.plans().iter().any(|plan| {
                    plan.id == slot.plan_id
                        && plan.ok
                        && plan
                            .remaining_percent
                            .is_some_and(|p| p >= SLEEP_FULL_PERCENT)
                })
            })
            .collect();
        let mut changed = Vec::new();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let want = idle && full[index];
            if want == slot.sleeping {
                continue;
            }
            slot.sleeping = want;
            if want {
                slot.reaction_until = None;
                slot.ball.set_emotion_at(SLEEP_EMOTION, now_ms);
            } else {
                slot.ball.set_emotion_at(WAKE_EMOTION, now_ms);
                slot.reaction_until = Some(now_ms + 900.0);
            }
            changed.push((index, want));
        }
        changed
    }

    /// Restore the quota mood on balls whose reaction has run its course.
    pub fn expire_reactions(&mut self, now_ms: f64) {
        for slot in &mut self.slots {
            if slot.reaction_until.is_some_and(|until| now_ms >= until) {
                slot.reaction_until = None;
                slot.ball.set_emotion_at(&slot.emotion, now_ms);
            }
        }
    }

    /// Rebuild the ball list from the current snapshot and settings order.
    pub fn sync_slots(&mut self) {
        let plans = self.ordered_plans();
        let perf_lite = self.perf_mode() != PerfMode::Smooth;
        // Drop slots whose plan disappeared.
        self.slots
            .retain(|slot| plans.iter().any(|plan| plan.id == slot.plan_id));
        // A pinned plan that vanished can no longer hold the card open.
        if self
            .pinned
            .as_ref()
            .is_some_and(|id| !plans.iter().any(|plan| &plan.id == id))
        {
            self.pinned = None;
        }

        // Re-order to match settings, then add any new plan.
        let order: Vec<String> = plans.iter().map(|plan| plan.id.clone()).collect();
        self.slots.sort_by_key(|slot| {
            order
                .iter()
                .position(|id| id == &slot.plan_id)
                .unwrap_or(usize::MAX)
        });

        for plan in plans {
            if self.slots.iter().any(|slot| slot.plan_id == plan.id) {
                continue;
            }
            let shape = theme::shape_for(&plan.id, &self.settings);
            let color = theme::color_for_plan(&plan.id, &self.settings);
            let mut ball = Ball::new(grok_ball::BallOptions {
                shape,
                color: Some(color.clone()),
                eye_color: Some(theme::eye_color().to_string()),
                emotion: Some(theme::emotion_for(&plan).to_string()),
                lite: perf_lite,
                eye_scale: 1.15,
                ..Default::default()
            });
            ball.render_static(0.0);
            self.slots.push(Slot {
                plan_id: plan.id.clone(),
                ball,
                shape: shape_label(shape),
                color,
                emotion: theme::emotion_for(&plan).to_string(),
                lite: perf_lite,
                reaction_until: None,
                sleeping: false,
            });
        }
    }

    /// Apply a fresh snapshot: reorder slots, refresh emotions, report errors.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = Some(snapshot);
        self.last_snapshot_at = Some(Instant::now());
        self.sync_slots();

        // Borrow split: the plan lookup only needs `self.snapshot`, so field-
        // level borrows keep `self.slots` mutable at the same time.
        let snapshot = self.snapshot.as_ref();
        for slot in &mut self.slots {
            let Some(plan) = snapshot
                .and_then(|snapshot| snapshot.plans.iter().find(|plan| plan.id == slot.plan_id))
            else {
                continue;
            };
            let want_emotion = theme::emotion_for(plan).to_string();
            if slot.emotion != want_emotion {
                slot.ball.set_emotion(&want_emotion);
                slot.emotion = want_emotion;
            }
        }
    }

    /// Return every ball's gaze to centre (called when the pointer leaves the
    /// capsule entirely so no ball keeps looking off-screen).
    pub fn reset_gaze(&mut self) {
        for slot in &mut self.slots {
            slot.ball.set_gaze(0.0, 0.0);
        }
    }

    pub fn apply_settings(&mut self, settings: Settings) {
        self.settings = settings;
        let perf_lite = self.perf_mode() != PerfMode::Smooth;
        // Borrow split as in `apply_snapshot`: read from `self.snapshot` while
        // `self.slots` (and `self.settings` below) stay usable.
        let snapshot = self.snapshot.as_ref();
        for slot in &mut self.slots {
            let Some(plan) = snapshot
                .and_then(|snapshot| snapshot.plans.iter().find(|plan| plan.id == slot.plan_id))
            else {
                continue;
            };
            let shape = theme::shape_for(&plan.id, &self.settings);
            let color = theme::color_for_plan(&plan.id, &self.settings);
            let label = shape_label(shape);
            if slot.shape != label || slot.lite != perf_lite || slot.color != color {
                let mut ball = Ball::new(grok_ball::BallOptions {
                    shape,
                    color: Some(color.clone()),
                    eye_color: Some(theme::eye_color().to_string()),
                    emotion: Some(theme::emotion_for(plan).to_string()),
                    lite: perf_lite,
                    eye_scale: 1.15,
                    ..Default::default()
                });
                ball.render_static(0.0);
                slot.ball = ball;
                slot.shape = label;
                slot.color = color;
                slot.lite = perf_lite;
            }
        }
        self.sync_slots();
    }

    /// Hover changed. Returns whether the card should be shown, hidden, or kept.
    ///
    /// `Some(_)` → `Some(_)` is always a `Switch` (the card stays up); only a
    /// real leave or the idle transition collapses into `Left`.
    pub fn set_hovered(&mut self, plan_id: Option<String>) -> HoverChange {
        let previous = self.hovered.clone();
        self.hovered = plan_id;
        match (&previous, &self.hovered) {
            (Some(_), Some(next)) => {
                self.hover_left_at = None;
                HoverChange::Switch(next.clone())
            }
            (Some(_), None) => {
                self.hover_left_at = Some(Instant::now());
                HoverChange::Left
            }
            (None, Some(next)) => {
                // Sliding ball → gap → ball within the grace window is a
                // Switch, not an Entered — otherwise every slot gap replays
                // the whole-capsule draw-in and it gets noisy.
                let within_grace = self
                    .hover_left_at
                    .is_some_and(|left| left.elapsed() < HOVER_GRACE);
                self.hover_left_at = None;
                if within_grace {
                    HoverChange::Switch(next.clone())
                } else {
                    HoverChange::Entered(next.clone())
                }
            }
            (None, None) => {
                // No-op transitions must not re-arm the grace window — the
                // pointer crossing a 9 pt slot gap would otherwise reset the
                // countdown and leave the card stuck half-open.
                HoverChange::Left
            }
        }
    }

    /// Start the hover grace window, e.g. when the pointer leaves the card.
    pub fn start_grace(&mut self) {
        if self.hovered.is_none() {
            self.hover_left_at = Some(Instant::now());
        }
    }

    /// Start the grace window unless one is already running (the pointer
    /// watchdog calls this every frame; restarting would never expire).
    pub fn ensure_grace(&mut self) {
        if self.hovered.is_none() && self.hover_left_at.is_none() {
            self.hover_left_at = Some(Instant::now());
        }
    }

    /// When the hover grace window ends, if one is running.
    pub fn grace_deadline(&self) -> Option<Instant> {
        if self.hovered.is_some() {
            return None;
        }
        self.hover_left_at.map(|left| left + HOVER_GRACE)
    }

    /// True when the hover grace window (150 ms) has expired.
    pub fn grace_expired(&self) -> bool {
        self.hovered.is_none()
            && self
                .hover_left_at
                .is_some_and(|left| left.elapsed() >= HOVER_GRACE)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverChange {
    Entered(String),
    Switch(String),
    Left,
}

/// Copyable mirror of [`PerfMode`] for places that need `Send`/`Copy` values
/// (menu bar, display-link callbacks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PerfModeAction {
    Smooth,
    #[default]
    Balanced,
    Saver,
}

impl From<PerfMode> for PerfModeAction {
    fn from(mode: PerfMode) -> Self {
        match mode {
            PerfMode::Smooth => PerfModeAction::Smooth,
            PerfMode::Balanced => PerfModeAction::Balanced,
            PerfMode::Saver => PerfModeAction::Saver,
        }
    }
}

impl From<PerfModeAction> for PerfMode {
    fn from(mode: PerfModeAction) -> Self {
        match mode {
            PerfModeAction::Smooth => PerfMode::Smooth,
            PerfModeAction::Balanced => PerfMode::Balanced,
            PerfModeAction::Saver => PerfMode::Saver,
        }
    }
}

fn shape_label(shape: grok_ball::ShapeKind) -> String {
    match shape {
        grok_ball::ShapeKind::Blob => "blob".to_string(),
        grok_ball::ShapeKind::Gem => "gem".to_string(),
        grok_ball::ShapeKind::Wedge => "wedge".to_string(),
        grok_ball::ShapeKind::Star => "star".to_string(),
        grok_ball::ShapeKind::Cloud => "cloud".to_string(),
        grok_ball::ShapeKind::Heart => "heart".to_string(),
        grok_ball::ShapeKind::Square => "square".to_string(),
        grok_ball::ShapeKind::Drop => "drop".to_string(),
        grok_ball::ShapeKind::Whale => "whale".to_string(),
        grok_ball::ShapeKind::Cat => "cat".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dango_lib::models::Bucket;

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

    fn snapshot() -> Snapshot {
        Snapshot {
            plans: vec![
                plan("nova", true, Some(80.0)),
                plan("antigravity", false, None),
                plan("cursor", true, Some(10.0)),
            ],
            fetched_at: 1_700_000_000,
        }
    }

    /// Largest |coordinate| any drawn point reaches in the ball's view box.
    /// A healthy ball stays within roughly -15..245.
    fn frame_extent(frame: &grok_ball::Frame) -> f32 {
        let mut max = 0.0f32;
        for element in &frame.elements {
            let grok_ball::DrawElement::Path(path) = element else {
                continue;
            };
            if !path.visible {
                continue;
            }
            let t = &path.transform;
            let mut see = |x: f32, y: f32| {
                let (px, py) = (t.a * x + t.c * y + t.e, t.b * x + t.d * y + t.f);
                max = max.max(px.abs()).max(py.abs());
            };
            for verb in &path.path.verbs {
                match *verb {
                    grok_ball::PathVerb::MoveTo(x, y) | grok_ball::PathVerb::LineTo(x, y) => {
                        see(x, y)
                    }
                    grok_ball::PathVerb::QuadTo(_, _, x, y) => see(x, y),
                    grok_ball::PathVerb::CubicTo(_, _, _, _, x, y) => see(x, y),
                    _ => {}
                }
            }
        }
        max
    }

    #[test]
    fn hover_reaction_never_blows_the_ball_up() {
        for lite in [false, true] {
            let mut settings = Settings::default();
            settings.perf_mode = Some(if lite {
                PerfMode::Balanced
            } else {
                PerfMode::Smooth
            });
            let mut model = WidgetModel::new(settings);
            model.apply_snapshot(snapshot());
            let t0 = 1_790_390_000_000.0;
            let mut worst = 0.0f32;
            let mut t = t0;
            let step = |model: &mut WidgetModel, t: f64, worst: &mut f32| {
                model.expire_reactions(t);
                for slot in &mut model.slots {
                    slot.ball.tick_fast(t);
                    *worst = worst.max(frame_extent(slot.ball.frame()));
                }
            };
            while t < t0 + 2_000.0 {
                step(&mut model, t, &mut worst);
                t += 16.0;
            }
            // Hover nova: gaze at the pointer + "noticed" face, others look over.
            model.slots[0].ball.set_gaze(0.4, -0.2);
            model.react("nova", "03", t, 700.0);
            for slot in model.slots.iter_mut().skip(1) {
                slot.ball.set_gaze(0.0, -0.9);
            }
            while t < t0 + 4_000.0 {
                step(&mut model, t, &mut worst);
                t += 16.0;
            }
            assert!(
                worst < 400.0,
                "lite={lite}: ball drew out to {worst} (view box is ~259)"
            );
        }
    }

    #[test]
    fn only_full_balls_sleep_and_everyone_wakes_together() {
        let mut model = WidgetModel::new(Settings::default());
        let mut snap = snapshot();
        snap.plans.push(plan("devin", true, Some(100.0)));
        model.apply_snapshot(snap);
        let devin = model
            .slots
            .iter()
            .position(|slot| slot.plan_id == "devin")
            .unwrap();
        assert!(model.update_sleep(false, 0.0).is_empty());
        let dozed = model.update_sleep(true, 0.0);
        assert_eq!(dozed, vec![(devin, true)]);
        assert_eq!(model.slots[devin].ball.emotion_id(), "00");
        assert!(model.update_sleep(true, 10.0).is_empty());
        let woke = model.update_sleep(false, 20.0);
        assert_eq!(woke, vec![(devin, false)]);
        model.expire_reactions(1_000.0);
        assert_eq!(
            model.slots[devin].ball.emotion_id(),
            model.slots[devin].emotion
        );
    }

    #[test]
    fn reactions_expire_back_to_mood_and_skip_broken_plans() {
        let mut model = WidgetModel::new(Settings::default());
        model.apply_snapshot(snapshot());
        let healthy = model
            .slots
            .iter()
            .find(|slot| slot.plan_id == "nova")
            .unwrap()
            .emotion
            .clone();
        assert!(model.react("nova", "14", 1_000.0, 500.0));
        let slot = model
            .slots
            .iter()
            .find(|slot| slot.plan_id == "nova")
            .unwrap();
        assert_eq!(slot.ball.emotion_id(), "14");
        model.expire_reactions(1_400.0);
        assert!(model
            .slots
            .iter()
            .find(|slot| slot.plan_id == "nova")
            .unwrap()
            .reaction_until
            .is_some());
        model.expire_reactions(1_500.0);
        let slot = model
            .slots
            .iter()
            .find(|slot| slot.plan_id == "nova")
            .unwrap();
        assert!(slot.reaction_until.is_none());
        assert_eq!(slot.ball.emotion_id(), healthy);
        // A failed plan keeps its error face.
        assert!(!model.react("antigravity", "14", 1_000.0, 500.0));
        assert!(!model.react("nope", "14", 1_000.0, 500.0));
    }

    #[test]
    fn capsule_height_grows_with_ball_count() {
        let mut model = WidgetModel::new(Settings::default());
        model.apply_snapshot(snapshot());
        assert_eq!(model.slots.len(), 3);
        let expected = SLOTS_TOP + CAPSULE_PADDING + 3.0 * SLOT_SIZE + 2.0 * SLOT_GAP;
        // The first ball sits below the handle strip, not flush with the top.
        assert!(model.slot_frame(0).1 >= HANDLE_ZONE);
        assert!((model.capsule_height() - expected).abs() < 1e-9);
    }

    #[test]
    fn switching_perf_mode_rebuilds_balls_with_the_new_lite_flag() {
        let mut model = WidgetModel::new(Settings::default());
        model.apply_snapshot(snapshot());
        assert!(model.slots.iter().all(|slot| slot.lite));

        let mut smooth = Settings::default();
        smooth.perf_mode = Some(PerfMode::Smooth);
        model.apply_settings(smooth);
        assert!(model.slots.iter().all(|slot| !slot.lite));
    }

    #[test]
    fn slots_follow_settings_order() {
        let mut settings = Settings::default();
        settings.order = vec!["cursor".into(), "nova".into()];
        let mut model = WidgetModel::new(settings);
        model.apply_snapshot(snapshot());
        assert_eq!(
            model
                .slots
                .iter()
                .map(|s| s.plan_id.as_str())
                .collect::<Vec<_>>(),
            vec!["cursor", "nova", "antigravity"]
        );
    }

    #[test]
    fn failing_plan_gets_the_error_emotion() {
        let mut model = WidgetModel::new(Settings::default());
        model.apply_snapshot(snapshot());
        let antigravity = model
            .slots
            .iter()
            .find(|slot| slot.plan_id == "antigravity")
            .unwrap();
        assert_eq!(antigravity.emotion, "34");
    }

    #[test]
    fn hover_grace_expires_after_leaving() {
        let mut model = WidgetModel::new(Settings::default());
        assert_eq!(
            model.set_hovered(Some("nova".into())),
            HoverChange::Entered("nova".into())
        );
        assert_eq!(model.set_hovered(None), HoverChange::Left);
        assert!(!model.grace_expired());
        std::thread::sleep(HOVER_GRACE + Duration::from_millis(20));
        assert!(model.grace_expired());
    }

    #[test]
    fn hover_change_between_balls_is_a_switch() {
        let mut model = WidgetModel::new(Settings::default());
        model.set_hovered(Some("nova".into()));
        assert_eq!(
            model.set_hovered(Some("cursor".into())),
            HoverChange::Switch("cursor".into())
        );
    }

    #[test]
    fn reentering_within_grace_is_a_switch_not_an_entered() {
        let mut model = WidgetModel::new(Settings::default());
        model.set_hovered(Some("nova".into()));
        assert_eq!(model.set_hovered(None), HoverChange::Left);
        // Crossing a slot gap: back on a ball before the grace window ends.
        assert_eq!(
            model.set_hovered(Some("cursor".into())),
            HoverChange::Switch("cursor".into())
        );
    }

    #[test]
    fn reentering_after_grace_is_an_entered() {
        let mut model = WidgetModel::new(Settings::default());
        model.set_hovered(Some("nova".into()));
        assert_eq!(model.set_hovered(None), HoverChange::Left);
        std::thread::sleep(HOVER_GRACE + Duration::from_millis(20));
        assert_eq!(
            model.set_hovered(Some("cursor".into())),
            HoverChange::Entered("cursor".into())
        );
    }

    #[test]
    fn buckets_survive_into_the_model_for_the_card() {
        let mut model = WidgetModel::new(Settings::default());
        let mut snap = snapshot();
        snap.plans[0].buckets = vec![Bucket {
            label: "周额度".into(),
            remaining_percent: Some(80.0),
            detail: Some("80K / 100K".into()),
            resets_at: Some(1_790_208_000_000),
            pool: None,
            window: None,
        }];
        model.apply_snapshot(snap);
        let hovered = model.set_hovered(Some("nova".into()));
        assert!(matches!(hovered, HoverChange::Entered(_)));
        let plan = model.hovered_plan().expect("hovered plan");
        assert_eq!(plan.buckets.len(), 1);
        assert_eq!(plan.buckets[0].resets_at, Some(1_790_208_000_000));
    }
}
