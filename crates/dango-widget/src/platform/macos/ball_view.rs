//! Core Animation renderer for one grok-ball.
//!
//! One pooled layer tree per ball: `CAShapeLayer` for solid strokes/fills and a
//! `CAGradientLayer` + `CAShapeLayer` mask pair for gradients. Paths are rebuilt
//! each frame, layers are reused, and all changes are wrapped in a
//! `CATransaction` with actions disabled so nothing animates implicitly.

use std::ffi::c_void;

use core_foundation::base::TCFType;
use core_graphics::color::CGColor;
use grok_ball::{Color, DrawElement, Frame, Paint as FramePaint, Path as FramePath, PathVerb};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::ClassType;
use objc2_quartz_core::{
    CAGradientLayer, CALayer, CAMediaTiming, CAShapeLayer, CATextLayer, CATransform3D,
};

use super::card_layer;
use crate::geometry::{self, Affine, Bounds};
use crate::theme::RingStyle;
use dango_lib::RingMode;

const LAYER_POOL: usize = 48;

const WHITE: Color = Color {
    r: 255,
    g: 255,
    b: 255,
    a: 1.0,
    hex: String::new(),
};

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {
    fn CACurrentMediaTime() -> f64;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPathCreateMutable() -> *mut c_void;
    fn CGPathRelease(path: *mut c_void);
    fn CGPathMoveToPoint(path: *mut c_void, transform: *const c_void, x: f64, y: f64);
    fn CGPathAddLineToPoint(path: *mut c_void, transform: *const c_void, x: f64, y: f64);
    fn CGPathAddQuadCurveToPoint(
        path: *mut c_void,
        transform: *const c_void,
        cpx: f64,
        cpy: f64,
        x: f64,
        y: f64,
    );
    fn CGPathAddCurveToPoint(
        path: *mut c_void,
        transform: *const c_void,
        cp1x: f64,
        cp1y: f64,
        cp2x: f64,
        cp2y: f64,
        x: f64,
        y: f64,
    );
    fn CGPathCloseSubpath(path: *mut c_void);
    fn CGPathAddArc(
        path: *mut c_void,
        transform: *const c_void,
        x: f64,
        y: f64,
        radius: f64,
        start_angle: f64,
        end_angle: f64,
        clockwise: bool,
    );
    // `rect` is `CGRect` by value, not a pointer (see card_layer.rs).
    fn CGPathAddEllipseInRect(path: *mut c_void, transform: *const c_void, rect: CGRectFfi);
    fn CGColorCreateSRGB(r: f64, g: f64, b: f64, a: f64) -> core_graphics::sys::CGColorRef;
}

pub struct BallView {
    slot: Retained<CALayer>,
    /// Quiet ring track behind the progress arc; recoloured per theme.
    ring_bg: Retained<CAShapeLayer>,
    ring_fg: Retained<CAShapeLayer>,
    /// Bright dot riding the end of the progress arc. Below ~15 % the arc is a
    /// sliver nobody can read; the dot is what actually signals "nearly out".
    ring_dot: Retained<CAShapeLayer>,
    /// Double ring: thin outer track + arc for the short (5 h) window.
    ring_outer_bg: Retained<CAShapeLayer>,
    ring_outer_fg: Retained<CAShapeLayer>,
    /// Beads ring: one small dot per 5 %.
    beads_box: Retained<CALayer>,
    beads: Vec<Retained<CAShapeLayer>>,
    /// Flow ring: a white highlight orbiting inside `comet_box`, which is
    /// masked to the filled arc so the light never leaves it.
    comet_box: Retained<CALayer>,
    comet_mask: Retained<CAShapeLayer>,
    comet: Retained<CAShapeLayer>,
    /// Trail ring: the arc split into slices of rising opacity.
    trail_box: Retained<CALayer>,
    trail: Vec<Retained<CAShapeLayer>>,
    dark: bool,
    /// Poke ripple: a ring that swells out and fades.
    ripple: Retained<CAShapeLayer>,
    /// Sleeping "z" drifting up off the ball.
    zz: Retained<CATextLayer>,
    sleeping: bool,
    /// Hovered: slot scaled up with a glow on the ring.
    lifted: bool,
    /// Colour of the live ring, for the hover glow and the ripple.
    ring_color: Color,
    pub root: Retained<CALayer>,
    pool: Vec<ElementLayers>,
    side: f64,
    ring: Option<RingStyle>,
}

impl Drop for BallView {
    fn drop(&mut self) {
        self.slot.removeFromSuperlayer();
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> objc2_foundation::CGRect {
    objc2_foundation::CGRect::new(
        objc2_foundation::CGPoint::new(x, y),
        objc2_foundation::CGSize::new(w, h),
    )
}

#[repr(C)]
struct CGRectFfi {
    origin: CGPointFfi,
    size: CGSizeFfi,
}

#[repr(C)]
struct CGPointFfi {
    x: f64,
    y: f64,
}

#[repr(C)]
struct CGSizeFfi {
    width: f64,
    height: f64,
}

fn rounded_rect(width: f64, height: f64) -> CGRectFfi {
    CGRectFfi {
        origin: CGPointFfi { x: 0.0, y: 0.0 },
        size: CGSizeFfi { width, height },
    }
}

/// A 44 pt ring circle from the retired web capsule (r 19.5, width 2.5), starting at
/// 12 o'clock and running counter-clockwise so `strokeEnd` is the remaining
/// fraction. Counter-clockwise matches the web ring (SVG `stroke-dashoffset`
/// fills from 12 o'clock to the left); a clockwise path would mirror the arc.
fn make_ring_layer(slot_size: f64, scale: f64) -> Retained<CAShapeLayer> {
    make_circle_layer(
        slot_size,
        scale,
        crate::app_model::RING_RADIUS,
        crate::app_model::RING_STROKE,
    )
}

/// Circle stroke layer filling the slot; `radius` / `width` are in 44 pt
/// slot units and scale with the slot (the card's mini ball is smaller).
fn make_circle_layer(
    slot_size: f64,
    scale: f64,
    radius: f64,
    width: f64,
) -> Retained<CAShapeLayer> {
    let k = slot_size / crate::app_model::SLOT_SIZE;
    let layer = unsafe { CAShapeLayer::new() };
    layer.setFrame(rect(0.0, 0.0, slot_size, slot_size));
    layer.setContentsScale(scale);
    let radius = radius * k;
    let center = slot_size / 2.0;
    let path = unsafe { CGPathCreateMutable() };
    unsafe {
        // Parent is y-down, so decreasing angles run visually counter-clockwise.
        CGPathAddArc(
            path,
            std::ptr::null(),
            center,
            center,
            radius,
            -std::f64::consts::FRAC_PI_2,
            -std::f64::consts::PI * 2.5,
            true,
        );
        send_cf_ptr(&layer, objc2::sel!(setPath:), path);
        CGPathRelease(path);
        layer.setLineWidth(width * k);
        layer.setLineCap(objc2_quartz_core::kCALineCapRound);
    }
    set_shape_color(&layer, "fill", None);
    layer
}

/// Ring geometry shared by the per-mode layers.
const BEAD_COUNT: usize = 20;
const TRAIL_SLICES: usize = 12;
const OUTER_RADIUS: f64 = 22.6;
const OUTER_STROKE: f64 = 1.5;

/// Unit-circle angle of `fraction` along the ring: starts at 12 o'clock and
/// runs the same way as the arc path (visually counter-clockwise, y-down).
fn ring_angle(fraction: f64) -> f64 {
    -std::f64::consts::FRAC_PI_2 - fraction * std::f64::consts::TAU
}

fn set_dot_path(layer: &CAShapeLayer, center: (f64, f64), radius: f64) {
    layer.setFrame(rect(
        center.0 - radius,
        center.1 - radius,
        radius * 2.0,
        radius * 2.0,
    ));
    let path = unsafe { CGPathCreateMutable() };
    unsafe {
        CGPathAddEllipseInRect(
            path,
            std::ptr::null(),
            rounded_rect(radius * 2.0, radius * 2.0),
        );
        send_cf_ptr(layer, objc2::sel!(setPath:), path);
        CGPathRelease(path);
    }
}

fn dash_pattern(values: &[f64]) -> Retained<objc2_foundation::NSArray<objc2_foundation::NSNumber>> {
    objc2_foundation::NSArray::from_vec(
        values
            .iter()
            .map(|v| objc2_foundation::NSNumber::new_f64(*v))
            .collect(),
    )
}

fn number(value: f64) -> Retained<objc2_foundation::NSNumber> {
    objc2_foundation::NSNumber::new_f64(value)
}

/// Infinite spin on `transform.rotation.z` (render-server side, like the
/// breathing animation). `period_s == 0` removes it.
fn set_orbit(layer: &CALayer, period_s: f64) {
    let key = objc2_foundation::NSString::from_str("orbit");
    unsafe {
        if period_s <= 0.0 {
            layer.removeAnimationForKey(&key);
            return;
        }
        if let Some(existing) = layer.animationForKey(&key) {
            if (existing.duration() - period_s).abs() < 0.01 {
                return;
            }
        }
        let animation = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
            &objc2_foundation::NSString::from_str("transform.rotation.z"),
        ));
        animation.setFromValue(Some(&number(0.0)));
        animation.setToValue(Some(&number(-std::f64::consts::TAU)));
        animation.setDuration(period_s);
        animation.setRepeatCount(f32::INFINITY);
        layer.addAnimation_forKey(&animation, Some(&key));
    }
}

/// Marching-ants dash phase on the error ring: a broken plan's ring creeps
/// instead of sitting still, so "this one is down" reads at a glance.
/// Render-server side, free like `set_breathing`.
fn set_marching(layer: &CAShapeLayer, on: bool) {
    let key = objc2_foundation::NSString::from_str("ants");
    unsafe {
        if !on {
            layer.removeAnimationForKey(&key);
            return;
        }
        if layer.animationForKey(&key).is_some() {
            return;
        }
        let animation = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
            &objc2_foundation::NSString::from_str("lineDashPhase"),
        ));
        animation.setFromValue(Some(&objc2_foundation::NSNumber::new_f64(0.0)));
        animation.setToValue(Some(&objc2_foundation::NSNumber::new_f64(-16.0)));
        animation.setDuration(1.1);
        animation.setRepeatCount(f32::INFINITY);
        layer.addAnimation_forKey(&animation, Some(&key));
    }
}

/// The web UI's `breathe` keyframes: opacity 1 → 0.35 → 1 over 2.4 s. Runs in
/// the render server, so it costs nothing on our frame loop.
fn set_breathing(layer: &CALayer, on: bool) {
    let key = objc2_foundation::NSString::from_str("breathe");
    unsafe {
        if !on {
            layer.removeAnimationForKey(&key);
            return;
        }
        if layer.animationForKey(&key).is_some() {
            return;
        }
        let animation = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
            &objc2_foundation::NSString::from_str("opacity"),
        ));
        animation.setFromValue(Some(&objc2_foundation::NSNumber::new_f64(1.0)));
        animation.setToValue(Some(&objc2_foundation::NSNumber::new_f64(0.35)));
        animation.setDuration(1.2);
        animation.setAutoreverses(true);
        animation.setRepeatCount(f32::INFINITY);
        // Sinusoidal in-and-out, not the default linear sawtooth: the web UI's
        // `ease-in-out` keyframes are what make it read as breathing.
        if let Some(curve) = card_layer::timing_curve([0.42, 0.0, 0.58, 1.0]) {
            let sel = objc2::runtime::Sel::register("setTimingFunction:");
            let _: () = objc2::msg_send![&animation, performSelector: sel, withObject: &*curve];
        }
        layer.addAnimation_forKey(&animation, Some(&key));
    }
}

/// Ring draw-in: a smooth deceleration (no overshoot — the dot riding the
/// tip would have to run past its mark and back).
const RING_DRAW_CURVE: [f64; 4] = [0.22, 1.0, 0.36, 1.0];

/// Longer arcs take a little longer, so the sweep speed feels constant.
fn ring_draw_duration(fraction: f64) -> f64 {
    0.45 + 0.35 * fraction.clamp(0.0, 1.0)
}

/// Hover lift scale.
const LIFT_SCALE: f64 = 1.14;

fn scale_transform(x: f64, y: f64) -> CATransform3D {
    CATransform3D {
        m11: x,
        m12: 0.0,
        m13: 0.0,
        m14: 0.0,
        m21: 0.0,
        m22: y,
        m23: 0.0,
        m24: 0.0,
        m31: 0.0,
        m32: 0.0,
        m33: 1.0,
        m34: 0.0,
        m41: 0.0,
        m42: 0.0,
        m43: 0.0,
        m44: 1.0,
    }
}

fn apply_curve(animation: &objc2_quartz_core::CAAnimation, points: [f64; 4]) {
    if let Some(curve) = card_layer::timing_curve(points) {
        let sel = objc2::runtime::Sel::register("setTimingFunction:");
        unsafe {
            let _: () = objc2::msg_send![animation, performSelector: sel, withObject: &*curve];
        }
    }
}

/// One-shot keyframe animation on a numeric key path. Presentation only:
/// the model value is left alone, so the layer lands where it already is.
#[allow(clippy::too_many_arguments)]
fn keyframes(
    layer: &CALayer,
    key_path: &str,
    values: &[f64],
    times: &[f64],
    duration: f64,
    begin: f64,
    key: &str,
) {
    keyframes_held(layer, key_path, values, times, duration, begin, key, false);
}

/// [`keyframes`], optionally holding the last value after it ends (until the
/// caller removes the animation) — for exits whose model value flips later.
#[allow(clippy::too_many_arguments)]
fn keyframes_held(
    layer: &CALayer,
    key_path: &str,
    values: &[f64],
    times: &[f64],
    duration: f64,
    begin: f64,
    key: &str,
    hold: bool,
) {
    unsafe {
        let key = objc2_foundation::NSString::from_str(key);
        layer.removeAnimationForKey(&key);
        let animation = objc2_quartz_core::CAKeyframeAnimation::animationWithKeyPath(Some(
            &objc2_foundation::NSString::from_str(key_path),
        ));
        let values: Retained<objc2_foundation::NSArray<objc2_foundation::NSNumber>> =
            objc2_foundation::NSArray::from_vec(values.iter().map(|v| number(*v)).collect());
        let values: &objc2_foundation::NSArray =
            &*(Retained::as_ptr(&values) as *const objc2_foundation::NSArray);
        animation.setValues(Some(values));
        let times = objc2_foundation::NSArray::from_vec(times.iter().map(|v| number(*v)).collect());
        animation.setKeyTimes(Some(&times));
        animation.setDuration(duration);
        animation.setBeginTime(begin);
        if hold {
            animation.setFillMode(objc2_quartz_core::kCAFillModeBoth);
            animation.setRemovedOnCompletion(false);
        } else {
            animation.setFillMode(objc2_quartz_core::kCAFillModeBackwards);
        }
        if let Some(curve) = card_layer::timing_curve([0.33, 0.0, 0.2, 1.0]) {
            let sel = objc2::runtime::Sel::register("setTimingFunction:");
            let _: () = objc2::msg_send![&animation, performSelector: sel, withObject: &*curve];
        }
        layer.addAnimation_forKey(&animation, Some(&key));
    }
}

struct ElementLayers {
    shape: Retained<CAShapeLayer>,
    gradient: Retained<CAGradientLayer>,
    mask: Retained<CAShapeLayer>,
}

impl BallView {
    /// `slot_origin` / `slot_size` place the ring square; the ball of `side`
    /// points is centered inside it. The parent is y-down (winit's layer-backed
    /// view already is; `app.rs` must not flip it again).
    pub fn new(
        parent: &CALayer,
        slot_origin: (f64, f64),
        slot_size: f64,
        side: f64,
        scale: f64,
    ) -> Self {
        let slot = CALayer::new();
        slot.setFrame(rect(slot_origin.0, slot_origin.1, slot_size, slot_size));
        slot.setContentsScale(scale);
        slot.setOpaque(false);
        parent.addSublayer(&slot);

        let ring_bg = make_ring_layer(slot_size, scale);
        let (track_hex, track_alpha) = crate::theme::ring_track(true);
        set_shape_color(&ring_bg, "stroke", Some(&Color::from_hex(track_hex)));
        ring_bg.setOpacity(track_alpha);
        slot.addSublayer(&ring_bg);
        let ring_fg = make_ring_layer(slot_size, scale);
        slot.addSublayer(&ring_fg);

        let ring_outer_bg = make_circle_layer(slot_size, scale, OUTER_RADIUS, OUTER_STROKE);
        set_shape_color(&ring_outer_bg, "stroke", Some(&Color::from_hex(track_hex)));
        ring_outer_bg.setOpacity(track_alpha);
        ring_outer_bg.setHidden(true);
        slot.addSublayer(&ring_outer_bg);
        let ring_outer_fg = make_circle_layer(slot_size, scale, OUTER_RADIUS, OUTER_STROKE);
        ring_outer_fg.setHidden(true);
        slot.addSublayer(&ring_outer_fg);

        let beads_box = CALayer::new();
        beads_box.setFrame(rect(0.0, 0.0, slot_size, slot_size));
        beads_box.setHidden(true);
        slot.addSublayer(&beads_box);
        let beads = (0..BEAD_COUNT)
            .map(|_| {
                let bead = unsafe { CAShapeLayer::new() };
                bead.setContentsScale(scale);
                beads_box.addSublayer(&bead);
                bead
            })
            .collect();

        let comet_box = CALayer::new();
        comet_box.setFrame(rect(0.0, 0.0, slot_size, slot_size));
        comet_box.setHidden(true);
        let comet_mask = make_circle_layer(
            slot_size,
            scale,
            crate::app_model::RING_RADIUS,
            crate::app_model::RING_STROKE + 1.5,
        );
        set_shape_color(&comet_mask, "stroke", Some(&WHITE));
        unsafe { comet_box.setMask(Some(&comet_mask)) };
        let comet = make_circle_layer(
            slot_size,
            scale,
            crate::app_model::RING_RADIUS,
            crate::app_model::RING_STROKE,
        );
        let mut glint = WHITE;
        glint.a = 0.9;
        set_shape_color(&comet, "stroke", Some(&glint));
        unsafe {
            comet.setStrokeStart(0.0);
            comet.setStrokeEnd(0.09);
        }
        comet_box.addSublayer(&comet);
        slot.addSublayer(&comet_box);

        let trail_box = CALayer::new();
        trail_box.setFrame(rect(0.0, 0.0, slot_size, slot_size));
        trail_box.setHidden(true);
        let trail = (0..TRAIL_SLICES)
            .map(|index| {
                let slice = make_ring_layer(slot_size, scale);
                if index + 1 < TRAIL_SLICES {
                    unsafe { slice.setLineCap(objc2_quartz_core::kCALineCapButt) };
                }
                trail_box.addSublayer(&slice);
                slice
            })
            .collect();
        slot.addSublayer(&trail_box);

        let ring_dot = unsafe { CAShapeLayer::new() };
        ring_dot.setContentsScale(scale);
        ring_dot.setHidden(true);
        slot.addSublayer(&ring_dot);

        let ripple = make_ring_layer(slot_size, scale);
        ripple.setOpacity(0.0);
        slot.addSublayer(&ripple);

        let zz = unsafe { CATextLayer::new() };
        zz.setContentsScale(scale);
        zz.setHidden(true);
        zz.setZPosition(2.0);
        unsafe {
            zz.setString(Some(&objc2_foundation::NSString::from_str("z")));
            let font = super::card_font::font_by_kind(super::card_font::FontKind::Ui, 11.0, 700);
            send_cf_ptr(
                &zz,
                objc2::sel!(setFont:),
                Retained::as_ptr(&font) as *const c_void,
            );
            zz.setFontSize(11.0);
        }
        zz.setFrame(rect(slot_size - 12.0, -2.0, 12.0, 14.0));
        slot.addSublayer(&zz);

        let inset = (slot_size - side) / 2.0;
        let root = CALayer::new();
        root.setFrame(rect(inset, inset, side, side));
        root.setContentsScale(scale);
        root.setOpaque(false);
        slot.addSublayer(&root);

        let pool = (0..LAYER_POOL)
            .map(|_| {
                let gradient = unsafe { CAGradientLayer::new() };
                gradient.setFrame(objc2_foundation::CGRect::new(
                    objc2_foundation::CGPoint::new(0.0, 0.0),
                    objc2_foundation::CGSize::new(side, side),
                ));
                gradient.setContentsScale(scale);
                gradient.setHidden(true);
                root.addSublayer(&gradient);

                let shape = unsafe { CAShapeLayer::new() };
                shape.setBounds(objc2_foundation::CGRect::new(
                    objc2_foundation::CGPoint::new(0.0, 0.0),
                    objc2_foundation::CGSize::new(
                        geometry::VIEW_SIZE as f64,
                        geometry::VIEW_SIZE as f64,
                    ),
                ));
                shape.setAnchorPoint(objc2_foundation::CGPoint::new(0.0, 0.0));
                shape.setPosition(objc2_foundation::CGPoint::new(0.0, 0.0));
                shape.setContentsScale(scale);
                unsafe { shape.setFillRule(objc2_quartz_core::kCAFillRuleNonZero) };
                unsafe { shape.setLineCap(objc2_quartz_core::kCALineCapRound) };
                unsafe { shape.setLineJoin(objc2_quartz_core::kCALineJoinRound) };
                root.addSublayer(&shape);

                let mask = unsafe { CAShapeLayer::new() };
                mask.setBounds(objc2_foundation::CGRect::new(
                    objc2_foundation::CGPoint::new(0.0, 0.0),
                    objc2_foundation::CGSize::new(
                        geometry::VIEW_SIZE as f64,
                        geometry::VIEW_SIZE as f64,
                    ),
                ));
                mask.setAnchorPoint(objc2_foundation::CGPoint::new(0.0, 0.0));
                mask.setPosition(objc2_foundation::CGPoint::new(0.0, 0.0));
                set_shape_color(&mask, "fill", Some(&WHITE));
                ElementLayers {
                    shape,
                    gradient,
                    mask,
                }
            })
            .collect();

        Self {
            slot,
            ring_bg,
            ring_fg,
            ring_dot,
            ring_outer_bg,
            ring_outer_fg,
            beads_box,
            beads,
            comet_box,
            comet_mask,
            comet,
            trail_box,
            trail,
            dark: true,
            ripple,
            zz,
            sleeping: false,
            lifted: false,
            ring_color: WHITE,
            root,
            pool,
            side,
            ring: None,
        }
    }

    /// Place the slot by bounds + centre position (not `frame`): the slot
    /// carries a scale transform while hovered, and `setFrame:` on a
    /// transformed layer would bake the scale into its bounds.
    pub fn set_position(&self, slot_origin: (f64, f64)) {
        let size = self.slot.bounds().size;
        self.slot.setPosition(objc2_foundation::CGPoint::new(
            slot_origin.0 + size.width / 2.0,
            slot_origin.1 + size.height / 2.0,
        ));
    }

    /// Update the progress ring; no-op when the style is unchanged.
    pub fn set_ring(&mut self, style: &RingStyle) {
        if self.ring.as_ref() == Some(style) {
            return;
        }
        // Start from a clean slate: every mode-specific layer hidden and idle.
        self.hide_mode_layers();
        let layer = &self.ring_fg;
        unsafe {
            layer.setLineCap(objc2_quartz_core::kCALineCapRound);
            self.ring_bg.setLineDashPattern(None);
        }
        self.ring_bg.setHidden(false);
        match style {
            RingStyle::Arc {
                fraction,
                color,
                alpha,
                low,
                mode,
                secondary,
                plan_color,
            } => {
                let mut color = Color::from_hex(color);
                color.a = *alpha;
                self.ring_color = color.clone();
                set_shape_color(layer, "stroke", Some(&color));
                unsafe {
                    layer.setLineDashPattern(None);
                    // The web ring shows a round-cap dot at 0%; Core Animation
                    // draws nothing at exactly 0, so keep a sliver. On this
                    // counter-clockwise path the sliver grows to the left of
                    // 12 o'clock, same side as the web dot.
                    layer.setStrokeStart(0.0);
                    layer.setStrokeEnd(fraction.max(0.0005));
                }
                layer.setHidden(false);
                set_breathing(layer, *low);
                // Only the trail wears an end dot (it is the comet's head);
                // on the other rings a dot on the tip just looks like a bug.
                if *mode == RingMode::Trail {
                    self.layout_ring_dot(*fraction, color.clone(), *low);
                } else {
                    self.ring_dot.setHidden(true);
                    set_breathing(&self.ring_dot, false);
                }
                match mode {
                    RingMode::Plain => {}
                    RingMode::Segments => {
                        // 8 rounded pills: the dash already accounts for the
                        // round caps eating into the gap.
                        let k = self.slot_scale();
                        let stroke = crate::app_model::RING_STROKE * k;
                        let period =
                            std::f64::consts::TAU * crate::app_model::RING_RADIUS * k / 8.0;
                        let gap = 3.6 * k + stroke;
                        let pattern = dash_pattern(&[period - gap, gap]);
                        unsafe {
                            layer.setLineDashPattern(Some(&pattern));
                            self.ring_bg.setLineDashPattern(Some(&pattern));
                        }
                        self.ring_dot.setHidden(true);
                    }
                    RingMode::Double => {
                        self.ring_outer_bg.setHidden(false);
                        if let Some(short) = secondary {
                            let mut short_color = if *short < 0.15 {
                                Color::from_hex(crate::theme::DANGER_COLOR)
                            } else if *short < 0.4 {
                                Color::from_hex(crate::theme::WARN_COLOR)
                            } else {
                                Color::from_hex(plan_color)
                            };
                            short_color.a = if *short < 0.4 { 1.0 } else { 0.85 };
                            set_shape_color(&self.ring_outer_fg, "stroke", Some(&short_color));
                            unsafe { self.ring_outer_fg.setStrokeEnd(short.max(0.0005)) };
                            self.ring_outer_fg.setHidden(false);
                        }
                    }
                    RingMode::Flow => {
                        if *fraction > 0.0 {
                            unsafe { self.comet_mask.setStrokeEnd(*fraction) };
                            // White reads as a hole on light glass; there the
                            // glint is a deeper shade of the arc instead.
                            let glint = if self.dark {
                                let mut white = WHITE;
                                white.a = 0.9;
                                white
                            } else {
                                let mut deep = color.clone();
                                deep.r = (deep.r as f64 * 0.62) as u8;
                                deep.g = (deep.g as f64 * 0.62) as u8;
                                deep.b = (deep.b as f64 * 0.62) as u8;
                                deep.a = 1.0;
                                deep
                            };
                            set_shape_color(&self.comet, "stroke", Some(&glint));
                            self.comet_box.setHidden(false);
                            let period = if *fraction < 0.15 {
                                1.1
                            } else if *fraction < 0.4 {
                                2.2
                            } else {
                                4.5
                            };
                            set_orbit(&self.comet, period);
                        }
                    }
                    RingMode::Beads => {
                        layer.setHidden(true);
                        self.ring_bg.setHidden(true);
                        self.ring_dot.setHidden(true);
                        set_breathing(layer, false);
                        self.layout_beads(*fraction, &color);
                        self.beads_box.setHidden(false);
                        set_breathing(&self.beads_box, *low);
                    }
                    RingMode::Trail => {
                        layer.setHidden(true);
                        set_breathing(layer, false);
                        let n = TRAIL_SLICES as f64;
                        for (index, slice) in self.trail.iter().enumerate() {
                            let i = index as f64;
                            let mut slice_color = color.clone();
                            slice_color.a = color.a * ((i + 1.0) / n).powf(1.6) as f32;
                            set_shape_color(slice, "stroke", Some(&slice_color));
                            unsafe {
                                // A hair of overlap hides the seam between butt caps.
                                slice.setStrokeStart((fraction * i / n - 0.002).max(0.0));
                                slice.setStrokeEnd((fraction * (i + 1.0) / n).max(0.0005));
                            }
                        }
                        self.trail_box.setHidden(false);
                        set_breathing(&self.trail_box, *low);
                    }
                }
            }
            RingStyle::Error { color } => {
                self.ring_color = Color::from_hex(color);
                set_shape_color(layer, "stroke", Some(&Color::from_hex(color)));
                let pattern = dash_pattern(&[3.0, 5.0]);
                unsafe {
                    layer.setLineDashPattern(Some(&pattern));
                    layer.setStrokeEnd(1.0);
                }
                layer.setHidden(false);
                set_breathing(layer, false);
                self.ring_dot.setHidden(true);
            }
            RingStyle::Unknown { color } => {
                self.ring_color = Color::from_hex(color);
                // Healthy but unmeasured: full ring in the plan colour, held
                // quiet at 45% so it reads as "here, just no number" instead
                // of alarm-red. Round caps would make the meeting point bulge,
                // so use butt caps for this static ring.
                let mut color = Color::from_hex(color);
                color.a = 0.45;
                set_shape_color(layer, "stroke", Some(&color));
                unsafe {
                    layer.setLineDashPattern(None);
                    layer.setStrokeEnd(1.0);
                    layer.setLineCap(objc2_quartz_core::kCALineCapButt);
                }
                layer.setHidden(false);
                set_breathing(layer, false);
                self.ring_dot.setHidden(true);
            }
        }
        set_marching(layer, matches!(style, RingStyle::Error { .. }));
        self.ring = Some(style.clone());
    }

    fn slot_scale(&self) -> f64 {
        self.slot.bounds().size.width / crate::app_model::SLOT_SIZE
    }

    fn hide_mode_layers(&self) {
        self.ring_outer_bg.setHidden(true);
        self.ring_outer_fg.setHidden(true);
        self.beads_box.setHidden(true);
        set_breathing(&self.beads_box, false);
        self.comet_box.setHidden(true);
        set_orbit(&self.comet, 0.0);
        self.trail_box.setHidden(true);
        set_breathing(&self.trail_box, false);
    }

    fn track_color(&self) -> Color {
        let (hex, alpha) = crate::theme::ring_track(self.dark);
        let mut color = Color::from_hex(hex);
        // Unlit beads have no underlying track, so they need a touch more ink.
        color.a = (alpha * 2.2).min(1.0);
        color
    }

    /// 20 dots around the ring, lit ones counter-clockwise from 12 o'clock.
    /// Any quota left lights at least one bead so "almost out" never reads
    /// as "empty".
    fn layout_beads(&self, fraction: f64, color: &Color) {
        let k = self.slot_scale();
        let center = self.slot.bounds().size.width / 2.0;
        let radius = crate::app_model::RING_RADIUS * k;
        let mut lit = (fraction * BEAD_COUNT as f64).round() as usize;
        if fraction > 0.0 && lit == 0 {
            lit = 1;
        }
        let mut bright = color.clone();
        bright.a = bright.a.max(0.6);
        let track = self.track_color();
        for (index, bead) in self.beads.iter().enumerate() {
            let angle = ring_angle(index as f64 / BEAD_COUNT as f64);
            let at = (center + radius * angle.cos(), center + radius * angle.sin());
            let on = index < lit;
            set_dot_path(bead, at, if on { 1.9 * k } else { 0.95 * k });
            set_shape_color(bead, "fill", Some(if on { &bright } else { &track }));
        }
    }

    /// Position the end-cap dot on the arc tip. The ring path starts at 12
    /// o'clock and runs counter-clockwise (y-down parent), so the arc end is
    /// `-90° − fraction·360°`.
    fn layout_ring_dot(&self, fraction: f64, color: Color, low: bool) {
        let slot_size = self.slot.bounds().size.width;
        let radius = crate::app_model::RING_RADIUS * slot_size / crate::app_model::SLOT_SIZE;
        let center = slot_size / 2.0;
        let angle = -std::f64::consts::FRAC_PI_2 - fraction.clamp(0.0, 1.0) * std::f64::consts::TAU;
        let x = center + radius * angle.cos();
        let y = center + radius * angle.sin();
        // Low quota gets a fatter dot so "nearly out" reads as a warning mark,
        // not a single stray pixel.
        let dot_r = if low { 3.4 } else { 2.6 };
        self.ring_dot
            .setFrame(rect(x - dot_r, y - dot_r, dot_r * 2.0, dot_r * 2.0));
        let path = unsafe { CGPathCreateMutable() };
        unsafe {
            CGPathAddEllipseInRect(
                path,
                std::ptr::null(),
                rounded_rect(dot_r * 2.0, dot_r * 2.0),
            );
            send_cf_ptr(&self.ring_dot, objc2::sel!(setPath:), path);
            CGPathRelease(path);
        }
        let mut bright = color;
        bright.a = 1.0;
        set_shape_color(&self.ring_dot, "fill", Some(&bright));
        self.ring_dot.setHidden(false);
        set_breathing(&self.ring_dot, low);
    }

    /// Replay the progress arc drawing itself in — pure garnish fired when the
    /// pointer enters the capsule (staggered per ball) or switches balls. The
    /// model value stays at `fraction`; this only animates the presentation.
    ///
    /// Three render-server animations, all with an easeOutBack-style curve so
    /// the arc visibly overshoots its target and settles back — an ordinary
    /// ease-out was too fast to read at this size.
    pub fn animate_ring_enter(&self, delay_s: f64) {
        let (fraction, mode) = match &self.ring {
            Some(RingStyle::Arc { fraction, mode, .. }) => (*fraction, *mode),
            _ => return,
        };
        let now = unsafe { CACurrentMediaTime() };
        match mode {
            RingMode::Beads => return self.animate_beads_enter(now + delay_s),
            RingMode::Trail => return self.animate_trail_enter(now + delay_s),
            RingMode::Double if !self.ring_outer_fg.isHidden() => {
                self.animate_stroke_in(&self.ring_outer_fg, now + delay_s + 0.12);
            }
            RingMode::Flow if !self.comet_box.isHidden() => {
                self.animate_stroke_in(&self.comet_mask, now + delay_s);
            }
            _ => {}
        }
        // The arc sweeps out from 12 o'clock and the end dot rides its tip —
        // one curve, one duration, so they never drift apart. (The old spin
        // + late dot pop read as two unrelated motions.)
        let duration = ring_draw_duration(fraction);
        unsafe {
            let key = objc2_foundation::NSString::from_str("drawIn");
            self.ring_fg.removeAnimationForKey(&key);
            self.ring_fg
                .removeAnimationForKey(&objc2_foundation::NSString::from_str("drawInSpin"));
            let animation = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
                &objc2_foundation::NSString::from_str("strokeEnd"),
            ));
            animation.setFromValue(Some(&number(0.0)));
            animation.setToValue(Some(&number(fraction.max(0.0005))));
            animation.setDuration(duration);
            animation.setBeginTime(now + delay_s);
            // Hold the empty ring during the stagger delay, then draw.
            animation.setFillMode(objc2_quartz_core::kCAFillModeBackwards);
            apply_curve(&animation, RING_DRAW_CURVE);
            self.ring_fg.addAnimation_forKey(&animation, Some(&key));
        }
        self.ride_dot(fraction, now + delay_s, duration);
    }

    /// Move the end dot along the arc from 12 o'clock to its resting place,
    /// in step with the arc's own draw-in.
    fn ride_dot(&self, fraction: f64, begin: f64, duration: f64) {
        if self.ring_dot.isHidden() {
            return;
        }
        let size = self.slot.bounds().size.width;
        let center = size / 2.0;
        let radius = crate::app_model::RING_RADIUS * self.slot_scale();
        unsafe {
            for stale in ["dotPop", "dotRide", "dotFade"] {
                self.ring_dot
                    .removeAnimationForKey(&objc2_foundation::NSString::from_str(stale));
            }
            let fade = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
                &objc2_foundation::NSString::from_str("opacity"),
            ));
            fade.setFromValue(Some(&number(0.0)));
            fade.setToValue(Some(&number(1.0)));
            fade.setDuration(0.12);
            fade.setBeginTime(begin);
            fade.setFillMode(objc2_quartz_core::kCAFillModeBackwards);
            self.ring_dot.addAnimation_forKey(
                &fade,
                Some(&objc2_foundation::NSString::from_str("dotFade")),
            );
            if fraction < 0.02 {
                return;
            }
            let path = CGPathCreateMutable();
            CGPathAddArc(
                path,
                std::ptr::null(),
                center,
                center,
                radius,
                ring_angle(0.0),
                ring_angle(fraction),
                true,
            );
            let ride = objc2_quartz_core::CAKeyframeAnimation::animationWithKeyPath(Some(
                &objc2_foundation::NSString::from_str("position"),
            ));
            let set_path: unsafe extern "C" fn(*mut c_void, objc2::runtime::Sel, *const c_void) =
                std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
            set_path(
                Retained::as_ptr(&ride) as *mut c_void,
                objc2::sel!(setPath:),
                path,
            );
            CGPathRelease(path);
            ride.setCalculationMode(objc2_quartz_core::kCAAnimationPaced);
            ride.setDuration(duration);
            ride.setBeginTime(begin);
            ride.setFillMode(objc2_quartz_core::kCAFillModeBackwards);
            apply_curve(&ride, RING_DRAW_CURVE);
            self.ring_dot.addAnimation_forKey(
                &ride,
                Some(&objc2_foundation::NSString::from_str("dotRide")),
            );
        }
    }

    /// easeOutBack `strokeEnd` 0 → current for a secondary stroke layer.
    fn animate_stroke_in(&self, layer: &CAShapeLayer, begin: f64) {
        let target = unsafe { layer.strokeEnd() };
        unsafe {
            let key = objc2_foundation::NSString::from_str("drawIn");
            layer.removeAnimationForKey(&key);
            let animation = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
                &objc2_foundation::NSString::from_str("strokeEnd"),
            ));
            animation.setFromValue(Some(&number(0.0)));
            animation.setToValue(Some(&number(target)));
            animation.setDuration(0.55);
            animation.setBeginTime(begin);
            animation.setFillMode(objc2_quartz_core::kCAFillModeBackwards);
            if let Some(curve) = card_layer::timing_curve([0.34, 1.45, 0.64, 1.0]) {
                let sel = objc2::runtime::Sel::register("setTimingFunction:");
                let _: () = objc2::msg_send![&animation, performSelector: sel, withObject: &*curve];
            }
            layer.addAnimation_forKey(&animation, Some(&key));
        }
    }

    /// Beads pop in one after another around the ring, like a string being
    /// threaded — lit and unlit alike, so the whole necklace arrives.
    fn animate_beads_enter(&self, begin: f64) {
        for (index, bead) in self.beads.iter().enumerate() {
            unsafe {
                let key = objc2_foundation::NSString::from_str("beadPop");
                bead.removeAnimationForKey(&key);
                let pop = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
                    &objc2_foundation::NSString::from_str("transform.scale"),
                ));
                pop.setFromValue(Some(&number(0.001)));
                pop.setToValue(Some(&number(1.0)));
                pop.setDuration(0.32);
                pop.setBeginTime(begin + index as f64 * 0.022);
                pop.setFillMode(objc2_quartz_core::kCAFillModeBackwards);
                if let Some(curve) = card_layer::timing_curve([0.34, 1.8, 0.64, 1.0]) {
                    let sel = objc2::runtime::Sel::register("setTimingFunction:");
                    let _: () = objc2::msg_send![&pop, performSelector: sel, withObject: &*curve];
                }
                bead.addAnimation_forKey(&pop, Some(&key));
            }
        }
    }

    /// The fading tail fades up while its head dot rides out to rest.
    fn animate_trail_enter(&self, begin: f64) {
        let fraction = match &self.ring {
            Some(RingStyle::Arc { fraction, .. }) => *fraction,
            _ => return,
        };
        let duration = ring_draw_duration(fraction);
        unsafe {
            let key = objc2_foundation::NSString::from_str("trailFade");
            self.trail_box.removeAnimationForKey(&key);
            self.trail_box
                .removeAnimationForKey(&objc2_foundation::NSString::from_str("drawInSpin"));
            let fade = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
                &objc2_foundation::NSString::from_str("opacity"),
            ));
            fade.setFromValue(Some(&number(0.0)));
            fade.setToValue(Some(&number(1.0)));
            fade.setDuration(duration);
            fade.setBeginTime(begin);
            fade.setFillMode(objc2_quartz_core::kCAFillModeBackwards);
            apply_curve(&fade, RING_DRAW_CURVE);
            self.trail_box.addAnimation_forKey(&fade, Some(&key));
        }
        self.ride_dot(fraction, begin, duration);
    }

    /// Hover lift: the slot swells to 1.14× with an overshoot and the ring
    /// picks up a soft glow in its own colour; beads ripple round in a wave.
    pub fn set_lifted(&mut self, lifted: bool) {
        if self.lifted == lifted {
            return;
        }
        self.lifted = lifted;
        let (from, to) = if lifted {
            (1.0, LIFT_SCALE)
        } else {
            (LIFT_SCALE, 1.0)
        };
        unsafe {
            // Model value with implicit actions OFF: an implicit `transform`
            // animation racing the explicit `transform.scale` one made the
            // slot flash huge and then vanish for half a second.
            objc2_quartz_core::CATransaction::begin();
            objc2_quartz_core::CATransaction::setDisableActions(true);
            self.slot.setTransform(scale_transform(to, to));
            objc2_quartz_core::CATransaction::commit();
            let key = objc2_foundation::NSString::from_str("lift");
            self.slot.removeAnimationForKey(&key);
            let lift = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
                &objc2_foundation::NSString::from_str("transform.scale"),
            ));
            lift.setFromValue(Some(&number(from)));
            lift.setToValue(Some(&number(to)));
            lift.setDuration(if lifted { 0.42 } else { 0.22 });
            apply_curve(
                &lift,
                if lifted {
                    [0.34, 1.45, 0.64, 1.0]
                } else {
                    [0.4, 0.0, 0.2, 1.0]
                },
            );
            self.slot.addAnimation_forKey(&lift, Some(&key));
        }
        let mut glow = self.ring_color.clone();
        glow.a = 1.0;
        let glow = cg_color(&glow);
        for layer in self.glow_layers() {
            unsafe {
                send_cf_ptr(
                    layer,
                    objc2::sel!(setShadowColor:),
                    glow.as_concrete_TypeRef().cast(),
                );
                layer.setShadowOffset(objc2_foundation::CGSize::new(0.0, 0.0));
                layer.setShadowRadius(4.0);
                let target: f64 = if lifted { 0.95 } else { 0.0 };
                let key = objc2_foundation::NSString::from_str("glow");
                layer.removeAnimationForKey(&key);
                let fade = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
                    &objc2_foundation::NSString::from_str("shadowOpacity"),
                ));
                fade.setFromValue(Some(&number(layer.shadowOpacity() as f64)));
                fade.setToValue(Some(&number(target)));
                fade.setDuration(0.25);
                layer.setShadowOpacity(target as f32);
                layer.addAnimation_forKey(&fade, Some(&key));
            }
        }
        if lifted && !self.beads_box.isHidden() {
            let now = unsafe { CACurrentMediaTime() };
            for (index, bead) in self.beads.iter().enumerate() {
                keyframes(
                    bead,
                    "transform.scale",
                    &[1.0, 1.8, 1.0],
                    &[0.0, 0.4, 1.0],
                    0.34,
                    now + index as f64 * 0.018,
                    "wave",
                );
            }
        }
    }

    fn glow_layers(&self) -> [&CALayer; 5] {
        [
            &self.ring_fg,
            &self.ring_outer_fg,
            &self.beads_box,
            &self.trail_box,
            &self.ring_dot,
        ]
    }

    /// Poke: squash-and-stretch on the whole slot plus a ripple leaving the
    /// ring. `intensity` grows with rapid repeat pokes.
    pub fn poke(&self, intensity: f64) {
        let base = if self.lifted { LIFT_SCALE } else { 1.0 };
        let squash = 0.22 * intensity.clamp(1.0, 1.8);
        let now = unsafe { CACurrentMediaTime() };
        let times = [0.0, 0.22, 0.5, 0.75, 1.0];
        keyframes(
            &self.slot,
            "transform.scale.x",
            &[
                base,
                base * (1.0 + squash),
                base * (1.0 - squash * 0.45),
                base * (1.0 + squash * 0.15),
                base,
            ],
            &times,
            0.5,
            now,
            "squashX",
        );
        keyframes(
            &self.slot,
            "transform.scale.y",
            &[
                base,
                base * (1.0 - squash),
                base * (1.0 + squash * 0.5),
                base * (1.0 - squash * 0.12),
                base,
            ],
            &times,
            0.5,
            now,
            "squashY",
        );
        let mut ripple = self.ring_color.clone();
        ripple.a = 1.0;
        set_shape_color(&self.ripple, "stroke", Some(&ripple));
        keyframes(
            &self.ripple,
            "transform.scale",
            &[1.0, 1.28 + 0.12 * intensity],
            &[0.0, 1.0],
            0.55,
            now,
            "rippleGrow",
        );
        keyframes(
            &self.ripple,
            "opacity",
            &[0.85, 0.0],
            &[0.0, 1.0],
            0.55,
            now,
            "rippleFade",
        );
        keyframes(
            &self.ripple,
            "lineWidth",
            &[crate::app_model::RING_STROKE * 1.6, 0.4],
            &[0.0, 1.0],
            0.55,
            now,
            "rippleThin",
        );
    }

    /// Sleeping: a small "z" drifts up and fades off the ball's shoulder, on
    /// a loop (render-server side).
    pub fn set_sleeping(&mut self, sleeping: bool) {
        if self.sleeping == sleeping {
            return;
        }
        self.sleeping = sleeping;
        let key = objc2_foundation::NSString::from_str("zz");
        self.zz.removeAnimationForKey(&key);
        self.zz.setHidden(!sleeping);
        if !sleeping {
            return;
        }
        let ink = {
            let (hex, _) = crate::theme::ring_track(self.dark);
            let mut c = Color::from_hex(hex);
            c.a = 0.55;
            cg_color(&c)
        };
        unsafe {
            send_cf_ptr(
                &self.zz,
                objc2::sel!(setForegroundColor:),
                ink.as_concrete_TypeRef().cast(),
            );
            let group = objc2_quartz_core::CAAnimationGroup::animation();
            let rise = objc2_quartz_core::CAKeyframeAnimation::animationWithKeyPath(Some(
                &objc2_foundation::NSString::from_str("transform.translation.y"),
            ));
            let v = objc2_foundation::NSArray::from_vec(vec![number(4.0), number(-12.0)]);
            rise.setValues(Some(
                &*(Retained::as_ptr(&v) as *const objc2_foundation::NSArray),
            ));
            let drift = objc2_quartz_core::CAKeyframeAnimation::animationWithKeyPath(Some(
                &objc2_foundation::NSString::from_str("transform.translation.x"),
            ));
            let v = objc2_foundation::NSArray::from_vec(vec![number(0.0), number(6.0)]);
            drift.setValues(Some(
                &*(Retained::as_ptr(&v) as *const objc2_foundation::NSArray),
            ));
            let fade = objc2_quartz_core::CAKeyframeAnimation::animationWithKeyPath(Some(
                &objc2_foundation::NSString::from_str("opacity"),
            ));
            let v =
                objc2_foundation::NSArray::from_vec(vec![number(0.0), number(1.0), number(0.0)]);
            fade.setValues(Some(
                &*(Retained::as_ptr(&v) as *const objc2_foundation::NSArray),
            ));
            let animations = objc2_foundation::NSArray::from_vec(vec![
                Retained::cast::<objc2_quartz_core::CAAnimation>(rise),
                Retained::cast::<objc2_quartz_core::CAAnimation>(drift),
                Retained::cast::<objc2_quartz_core::CAAnimation>(fade),
            ]);
            group.setAnimations(Some(&animations));
            group.setDuration(2.4);
            group.setRepeatCount(f32::INFINITY);
            self.zz.addAnimation_forKey(&group, Some(&key));
        }
    }

    /// Hover spotlight: the ball not under the pointer dims so the live one
    /// pops. Restored when the pointer leaves the capsule.
    pub fn set_dimmed(&self, dimmed: bool) {
        self.slot.setOpacity(if dimmed { 0.45 } else { 1.0 });
    }

    /// Hide/show the entire slot — the capsule's collapsed pill state hides
    /// every ball so only the handle bar remains.
    pub fn set_slot_hidden(&self, hidden: bool) {
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        for key in ["foldOut", "foldOutFade", "foldIn", "foldInFade"] {
            self.slot
                .removeAnimationForKey(&objc2_foundation::NSString::from_str(key));
        }
        self.slot.setHidden(hidden);
        objc2_quartz_core::CATransaction::commit();
    }

    /// Fold: shrink into the ball's centre and fade, then hold there until
    /// [`set_slot_hidden`] takes the slot away for real.
    pub fn animate_fold_out(&self, delay_s: f64) {
        let begin = unsafe { CACurrentMediaTime() } + delay_s;
        keyframes_held(
            &self.slot,
            "transform.scale",
            &[1.0, 1.06, 0.001],
            &[0.0, 0.3, 1.0],
            0.2,
            begin,
            "foldOut",
            true,
        );
        keyframes_held(
            &self.slot,
            "opacity",
            &[1.0, 1.0, 0.0],
            &[0.0, 0.4, 1.0],
            0.2,
            begin,
            "foldOutFade",
            true,
        );
    }

    /// Unfold: pop back in with a little overshoot.
    pub fn animate_fold_in(&self, delay_s: f64) {
        self.set_slot_hidden(false);
        let begin = unsafe { CACurrentMediaTime() } + delay_s;
        keyframes(
            &self.slot,
            "transform.scale",
            &[0.001, 1.12, 0.97, 1.0],
            &[0.0, 0.55, 0.8, 1.0],
            0.36,
            begin,
            "foldIn",
        );
        keyframes(
            &self.slot,
            "opacity",
            &[0.0, 1.0, 1.0],
            &[0.0, 0.35, 1.0],
            0.36,
            begin,
            "foldInFade",
        );
    }

    /// Recolour the quiet ring track for the current theme (white glass
    /// slot on dark, ink slot on light).
    pub fn set_track_color(&mut self, dark: bool) {
        let (hex, alpha) = crate::theme::ring_track(dark);
        set_shape_color(&self.ring_bg, "stroke", Some(&Color::from_hex(hex)));
        self.ring_bg.setOpacity(alpha);
        set_shape_color(&self.ring_outer_bg, "stroke", Some(&Color::from_hex(hex)));
        self.ring_outer_bg.setOpacity(alpha);
        if self.dark != dark {
            self.dark = dark;
            // Unlit beads carry the track colour themselves; force a redraw.
            if let Some(style) = self.ring.take() {
                self.set_ring(&style);
            }
        }
    }

    /// Paint a frame; callers should batch several `draw` calls inside one
    /// `CATransaction`.
    pub fn draw(&mut self, frame: &Frame) {
        let outer = geometry::frame_transform(frame, self.side as f32);
        let mut used = 0usize;
        for element in &frame.elements {
            if used >= self.pool.len() {
                break;
            }
            match element {
                DrawElement::Path(path) => {
                    let transform = outer.concat(Affine::from_frame(path.transform));
                    update_element(&self.pool[used], path, transform);
                    used += 1;
                }
                DrawElement::Text(text) => {
                    let transform = outer.concat(Affine::from_frame(text.transform));
                    update_text(&self.pool[used], text, transform, outer.scale_factor());
                    used += 1;
                }
            }
        }
        for layers in self.pool.iter().skip(used) {
            layers.shape.setHidden(true);
            layers.gradient.setHidden(true);
        }
    }
}

fn update_element(layers: &ElementLayers, element: &grok_ball::PathElement, transform: Affine) {
    if !element.visible {
        layers.shape.setHidden(true);
        layers.gradient.setHidden(true);
        return;
    }
    layers.gradient.setHidden(true);
    set_shape_path(&layers.shape, &element.path, transform, element.opacity);
    // The mask path is built only in `apply_gradient`, with the gradient's
    // bounds-relative transform; the mask layer is never in the layer tree on
    // its own (it is only ever attached via `gradient.setMask`), and the
    // gradient is hidden until `apply_gradient` runs, so nothing can see a
    // mask for solid or stroke-only elements.
    set_shape_color(&layers.shape, "fill", None);
    set_shape_color(
        &layers.shape,
        "stroke",
        element.stroke.as_ref().map(|stroke| &stroke.color),
    );
    if let Some(stroke) = &element.stroke {
        unsafe { layers.shape.setLineWidth(stroke.width as f64) };
        unsafe { layers.shape.setLineCap(objc2_quartz_core::kCALineCapRound) };
        unsafe {
            layers
                .shape
                .setLineJoin(objc2_quartz_core::kCALineJoinRound)
        };
        layers.shape.setOpacity(element.opacity * stroke.opacity);
    }

    match &element.fill {
        FramePaint::None => {}
        FramePaint::Solid(color) => {
            set_shape_color(&layers.shape, "fill", Some(color));
        }
        FramePaint::RadialGradient(gradient) => {
            let Some(bounds) = geometry::transformed_path_bounds(&element.path, transform) else {
                return;
            };
            apply_gradient(
                layers,
                &element.path,
                bounds,
                transform,
                element.opacity,
                |layer| unsafe {
                    layer.setType(objc2_quartz_core::kCAGradientLayerRadial);
                    layer.setStartPoint(objc2_foundation::CGPoint::new(
                        gradient.cx as f64,
                        gradient.cy as f64,
                    ));
                    layer.setEndPoint(objc2_foundation::CGPoint::new(
                        (gradient.cx + gradient.r) as f64,
                        (gradient.cy + gradient.r) as f64,
                    ));
                },
            );
            set_gradient_stops(&layers.gradient, &gradient.stops);
        }
        FramePaint::LinearGradient(gradient) => {
            let Some(bounds) = geometry::transformed_path_bounds(&element.path, transform) else {
                return;
            };
            let width = bounds.width();
            let height = bounds.height();
            let p1 = transform.apply(gradient.x1, gradient.y1);
            let p2 = transform.apply(gradient.x2, gradient.y2);
            apply_gradient(
                layers,
                &element.path,
                bounds,
                transform,
                element.opacity,
                |layer| unsafe {
                    layer.setType(objc2_quartz_core::kCAGradientLayerAxial);
                    layer.setStartPoint(objc2_foundation::CGPoint::new(
                        ((p1.0 - bounds.left) / width) as f64,
                        ((p1.1 - bounds.top) / height) as f64,
                    ));
                    layer.setEndPoint(objc2_foundation::CGPoint::new(
                        ((p2.0 - bounds.left) / width) as f64,
                        ((p2.1 - bounds.top) / height) as f64,
                    ));
                },
            );
            set_gradient_stops(&layers.gradient, &gradient.stops);
        }
    }

    layers.shape.setHidden(
        matches!(
            element.fill,
            FramePaint::RadialGradient(_) | FramePaint::LinearGradient(_)
        ) && element.stroke.is_none(),
    );
}

fn apply_gradient<F>(
    layers: &ElementLayers,
    path: &FramePath,
    bounds: Bounds,
    transform: Affine,
    opacity: f32,
    configure: F,
) where
    F: FnOnce(&CAGradientLayer),
{
    layers.gradient.setFrame(objc2_foundation::CGRect::new(
        objc2_foundation::CGPoint::new(bounds.left as f64, bounds.top as f64),
        objc2_foundation::CGSize::new(bounds.width() as f64, bounds.height() as f64),
    ));
    let mask_transform = Affine::translate(-bounds.left, -bounds.top).concat(transform);
    set_shape_path(&layers.mask, path, mask_transform, 1.0);
    layers.gradient.setHidden(false);
    unsafe {
        layers.gradient.setOpacity(opacity);
        layers.gradient.setMask(Some(&layers.mask));
    }
    configure(&layers.gradient);
}

fn set_gradient_stops(layer: &CAGradientLayer, stops: &[grok_ball::GradientStop]) {
    let colors: Vec<CGColor> = stops.iter().map(|stop| cg_color(&stop.color)).collect();
    let mut color_ptrs: Vec<std::ptr::NonNull<AnyObject>> = colors
        .iter()
        .map(|color| unsafe {
            std::ptr::NonNull::new_unchecked(
                color.as_concrete_TypeRef().cast::<AnyObject>() as *mut AnyObject
            )
        })
        .collect();
    let colors_array = unsafe {
        objc2_foundation::NSArray::<AnyObject>::initWithObjects_count(
            objc2_foundation::NSArray::alloc(),
            color_ptrs.as_mut_ptr(),
            color_ptrs.len(),
        )
    };
    let locations: Vec<Retained<objc2_foundation::NSNumber>> = stops
        .iter()
        .map(|stop| objc2_foundation::NSNumber::new_f32(stop.offset))
        .collect();
    let locations = objc2_foundation::NSArray::from_vec(locations);
    unsafe {
        layer.setColors(Some(&colors_array));
        layer.setLocations(Some(&locations));
    }
}

fn update_text(
    layers: &ElementLayers,
    text: &grok_ball::TextElement,
    transform: Affine,
    scale_factor: f32,
) {
    layers.gradient.setHidden(true);
    layers.shape.setHidden(false);
    // Grok-ball emits text as tiny strokes (the "zzz" glyphs), so a short
    // stroke-width path is enough and keeps the text in the same pooled layer.
    let font_scale = text.font_size / 12.0;
    let transform = transform.concat(Affine::scale(font_scale));
    let path = FramePath {
        verbs: text_verbs(),
        d: String::new(),
    };
    set_shape_path(&layers.shape, &path, transform, text.opacity);
    set_shape_color(&layers.shape, "fill", None);
    set_shape_color(&layers.shape, "stroke", Some(&text.fill));
    let stroke = (text.font_size * 0.10 * scale_factor).max(0.8);
    unsafe {
        layers.shape.setLineWidth(stroke as f64);
        layers.shape.setLineCap(objc2_quartz_core::kCALineCapRound);
        layers
            .shape
            .setLineJoin(objc2_quartz_core::kCALineJoinRound);
    }
}

fn text_verbs() -> Vec<PathVerb> {
    // A small "z" glyph built from three strokes, matching the JS reference.
    let mut verbs = Vec::with_capacity(12);
    for index in 0..3 {
        let x = index as f32 * 8.0;
        verbs.extend([
            PathVerb::MoveTo(x, 0.0),
            PathVerb::LineTo(x + 6.0, 0.0),
            PathVerb::LineTo(x, -8.0),
            PathVerb::LineTo(x + 6.0, -8.0),
        ]);
    }
    verbs
}

fn make_cg_path(path: &FramePath) -> *mut c_void {
    let cg_path = unsafe { CGPathCreateMutable() };
    let mut current = (0.0_f64, 0.0_f64);
    let mut subpath_start = current;
    for verb in &path.verbs {
        unsafe {
            match *verb {
                PathVerb::MoveTo(x, y) => {
                    current = (x as f64, y as f64);
                    subpath_start = current;
                    CGPathMoveToPoint(cg_path, std::ptr::null(), current.0, current.1);
                }
                PathVerb::LineTo(x, y) => {
                    current = (x as f64, y as f64);
                    CGPathAddLineToPoint(cg_path, std::ptr::null(), current.0, current.1);
                }
                PathVerb::QuadTo(x1, y1, x, y) => {
                    current = (x as f64, y as f64);
                    CGPathAddQuadCurveToPoint(
                        cg_path,
                        std::ptr::null(),
                        x1 as f64,
                        y1 as f64,
                        current.0,
                        current.1,
                    );
                }
                PathVerb::CubicTo(x1, y1, x2, y2, x, y) => {
                    current = (x as f64, y as f64);
                    CGPathAddCurveToPoint(
                        cg_path,
                        std::ptr::null(),
                        x1 as f64,
                        y1 as f64,
                        x2 as f64,
                        y2 as f64,
                        current.0,
                        current.1,
                    );
                }
                PathVerb::ArcTo {
                    rx,
                    ry,
                    x_axis_rotation,
                    large_arc,
                    sweep,
                    x,
                    y,
                } => {
                    let end = (x as f64, y as f64);
                    for (c1, c2, point) in geometry::arc_to_cubics(
                        current,
                        (rx as f64, ry as f64),
                        (x_axis_rotation as f64).to_radians(),
                        large_arc,
                        sweep,
                        end,
                    ) {
                        CGPathAddCurveToPoint(
                            cg_path,
                            std::ptr::null(),
                            c1.0,
                            c1.1,
                            c2.0,
                            c2.1,
                            point.0,
                            point.1,
                        );
                    }
                    current = end;
                }
                PathVerb::Close => {
                    CGPathCloseSubpath(cg_path);
                    current = subpath_start;
                }
            }
        }
    }
    cg_path
}

fn ca_transform(transform: Affine) -> CATransform3D {
    CATransform3D {
        m11: transform.sx as f64,
        m12: transform.ky as f64,
        m13: 0.0,
        m14: 0.0,
        m21: transform.kx as f64,
        m22: transform.sy as f64,
        m23: 0.0,
        m24: 0.0,
        m31: 0.0,
        m32: 0.0,
        m33: 1.0,
        m34: 0.0,
        m41: transform.tx as f64,
        m42: transform.ty as f64,
        m43: 0.0,
        m44: 1.0,
    }
}

/// `objc_msgSend` with the layer as receiver and one CoreGraphics pointer
/// (`CGPathRef` / `CGColorRef`). `msg_send!`'s debug encoding check rejects
/// every stand-in for `^{CGColor=}`, so these setters bypass it entirely.
type SetCfPtrFn = unsafe extern "C" fn(*mut c_void, objc2::runtime::Sel, *const c_void);

unsafe fn send_cf_ptr(layer: &CALayer, selector: objc2::runtime::Sel, ptr: *const c_void) {
    let send: SetCfPtrFn = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
    send((layer as *const CALayer).cast_mut().cast(), selector, ptr);
}

fn set_shape_path(shape: &CAShapeLayer, path: &FramePath, transform: Affine, opacity: f32) {
    let cg_path = make_cg_path(path);
    unsafe {
        send_cf_ptr(shape, objc2::sel!(setPath:), cg_path);
        shape.setTransform(ca_transform(transform));
        shape.setOpacity(opacity);
        CGPathRelease(cg_path);
    }
}

// `CGColor::rgb` uses Generic RGB (gamma 1.8), which washes out web hex colours.
fn cg_color(color: &Color) -> CGColor {
    unsafe {
        CGColor::wrap_under_create_rule(CGColorCreateSRGB(
            color.r as f64 / 255.0,
            color.g as f64 / 255.0,
            color.b as f64 / 255.0,
            color.a as f64,
        ))
    }
}

/// Set `fillColor` / `strokeColor`, which the generated bindings omit because
/// they take a `CGColorRef`.
fn set_shape_color(shape: &CAShapeLayer, selector: &str, color: Option<&Color>) {
    let cg_color = color.map(cg_color);
    let raw_color = cg_color
        .as_ref()
        .map(|color| color.as_concrete_TypeRef().cast::<c_void>())
        .unwrap_or(std::ptr::null());
    unsafe {
        let sel = if selector == "fill" {
            objc2::sel!(setFillColor:)
        } else {
            objc2::sel!(setStrokeColor:)
        };
        send_cf_ptr(shape, sel, raw_color);
    }
}

/// Advance every ball in the model and repaint its layer tree.
pub fn render(model: &mut crate::app_model::WidgetModel, views: &mut [BallView], now_ms: f64) {
    model.expire_reactions(now_ms);
    for (index, slot) in model.slots.iter_mut().enumerate() {
        let Some(view) = views.get_mut(index) else {
            continue;
        };
        slot.ball.tick_fast(now_ms);
        view.draw(slot.ball.frame());
    }
}

/// Paint a static frame for each ball (used before the first tick and in saver mode).
pub fn render_static(
    model: &mut crate::app_model::WidgetModel,
    views: &mut [BallView],
    now_ms: f64,
) {
    for (index, slot) in model.slots.iter_mut().enumerate() {
        let Some(view) = views.get_mut(index) else {
            continue;
        };
        slot.ball.render_static(now_ms);
        view.draw(slot.ball.frame());
    }
}

/// Stroke `layer` along a polyline (used by the capsule's fold handle arc).
/// Setting the path outside a disabled-actions transaction lets Core
/// Animation morph between two polylines with the same point count.
pub(crate) fn set_polyline_path(layer: &CAShapeLayer, points: &[(f64, f64)]) {
    let Some(first) = points.first() else {
        return;
    };
    let path = unsafe { CGPathCreateMutable() };
    unsafe {
        CGPathMoveToPoint(path, std::ptr::null(), first.0, first.1);
        for point in &points[1..] {
            CGPathAddLineToPoint(path, std::ptr::null(), point.0, point.1);
        }
        send_cf_ptr(layer, objc2::sel!(setPath:), path);
        CGPathRelease(path);
    }
}

pub(crate) fn set_stroke_rgba(layer: &CAShapeLayer, r: f64, g: f64, b: f64, a: f64) {
    let color = Color {
        r: (r * 255.0).round() as u8,
        g: (g * 255.0).round() as u8,
        b: (b * 255.0).round() as u8,
        a: a as f32,
        hex: String::new(),
    };
    set_shape_color(layer, "stroke", Some(&color));
    set_shape_color(layer, "fill", None);
}
