//! Layer plumbing for the detail card: glass background, text pool, bars, dots.
//!
//! Text is drawn with `CATextLayer` using the system font, which resolves to
//! PingFang SC for Chinese text on macOS.

use std::ffi::c_void;

use core_foundation::base::TCFType;
use core_graphics::color::CGColor;
use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2_quartz_core::{
    kCAAlignmentCenter, kCAAlignmentLeft, kCAAlignmentRight, kCATruncationEnd, CALayer,
    CAMediaTiming, CAShapeLayer, CATextLayer,
};

use super::card::{Rgba, TextAlign, TextSpec};
use super::card_font;

/// Build a `CAMediaTimingFunction` from four Bezier control points.
///
/// objc2-quartz-core does not expose `CAMediaTimingFunction`, and its
/// `msg_send!` macro cannot spell the selector
/// `+[CAMediaTimingFunction functionWithControlPoints::::]` (four unnamed
/// arguments — the keyword fragments are all empty). So call `objc_msgSend`
/// through a typed function pointer instead.
///
/// The control points are C `float`s. Passing `f64` here (as this used to)
/// put the values in the wrong half of the FP registers on arm64: every
/// curve came out as garbage, so animations sat at their start value and
/// snapped at the end — or, with a large garbage y, overshot wildly (the
/// hover-lift that briefly blew a ball up over the whole capsule).
pub(crate) fn timing_curve(points: [f64; 4]) -> Option<Retained<objc2_foundation::NSObject>> {
    type CreateCurveFn = unsafe extern "C" fn(
        *mut objc2::runtime::AnyObject,
        Sel,
        f32,
        f32,
        f32,
        f32,
    ) -> *mut objc2_foundation::NSObject;

    let cls = objc2::runtime::AnyClass::get("CAMediaTimingFunction")?;
    let sel = objc2::runtime::Sel::register("functionWithControlPoints::::");
    unsafe {
        let send: CreateCurveFn = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
        let cls_ptr = cls as *const objc2::runtime::AnyClass as *mut objc2::runtime::AnyObject;
        let curve = send(
            cls_ptr,
            sel,
            points[0] as f32,
            points[1] as f32,
            points[2] as f32,
            points[3] as f32,
        );
        if curve.is_null() {
            return None;
        }
        Some(Retained::retain(curve).expect("curve is non-null"))
    }
}

/// Apple's standard deceleration curve (fast start, soft landing), the one the
/// system UI uses for sheets and popovers. Linear timing reads as mechanical.
pub(crate) fn apple_ease_out() -> Option<Retained<objc2_foundation::NSObject>> {
    timing_curve([0.16, 1.0, 0.3, 1.0])
}

/// Apply `curve` to the current `CATransaction`.
///
/// objc2-quartz-core models neither `CAMediaTimingFunction` nor
/// `+[CATransaction setAnimationTimingFunction:]`, so this forwards through
/// `performSelector:` — the class method exists on the real CATransaction, we
/// just cannot name its type.
#[allow(dead_code)]
fn set_transaction_timing_function(curve: &objc2_foundation::NSObject) {
    unsafe {
        let cls = objc2::runtime::AnyClass::get("CATransaction").expect("CATransaction");
        let sel = objc2::runtime::Sel::register("setAnimationTimingFunction:");
        let _: () = objc2::msg_send![cls, performSelector: sel, withObject: curve];
    }
}

/// Opaque handle to a CoreGraphics color.
///
/// objc2-quartz-core 0.2 does not model the CGColor-valued setters
/// (`backgroundColor`, `fillColor`, `foregroundColor`). Declaring them through
/// `extern_methods!` is impossible on foreign types, and `msg_send!`'s encoding
/// check rejects an opaque stand-in for `^{CGColor=}` (a real struct with
/// fields). So the messages go straight through `objc_msgSend`, with the
/// argument cast to the exact pointer type the runtime expects.
#[repr(C)]
struct CGColorOpaque {
    _priv: [u8; 0],
}

/// `objc_msgSend` with the layer as receiver and a single `CGColorRef` argument.
type SetColorFn = unsafe extern "C" fn(*mut c_void, Sel, *const CGColorOpaque);

unsafe fn send_set_color(layer: &CALayer, selector: Sel, color: &CGColor) {
    let send: SetColorFn = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
    send(
        (layer as *const CALayer).cast_mut().cast(),
        selector,
        color.as_concrete_TypeRef().cast(),
    );
}

unsafe fn set_fill_color(layer: &CALayer, color: &CGColor) {
    send_set_color(layer, objc2::sel!(setFillColor:), color);
}

/// `pub(crate)` — the collapse handle in `app.rs` paints through it too.
pub(crate) unsafe fn set_background_color(layer: &CALayer, color: &CGColor) {
    send_set_color(layer, objc2::sel!(setBackgroundColor:), color);
}

unsafe fn set_foreground_color(layer: &CATextLayer, color: &CGColor) {
    send_set_color(layer, objc2::sel!(setForegroundColor:), color);
}

/// `objc_msgSend` with the layer as receiver and one CoreGraphics pointer
/// (`CGPathRef`, or `CTFontRef` for `CATextLayer.font`). Same encoding-check
/// bypass as `send_set_color`.
type SetCfPtrFn = unsafe extern "C" fn(*mut c_void, Sel, *const c_void);

unsafe fn send_cf_ptr(layer: &CALayer, selector: Sel, ptr: *const c_void) {
    let send: SetCfPtrFn = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
    send((layer as *const CALayer).cast_mut().cast(), selector, ptr);
}

const MAX_TEXT_LAYERS: usize = 64;

// `#[link]` has to sit on the `extern` block itself to take effect.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPathCreateMutable() -> *mut c_void;
    fn CGPathMoveToPoint(path: *mut c_void, transform: *const c_void, x: f64, y: f64);
    fn CGPathAddLineToPoint(path: *mut c_void, transform: *const c_void, x: f64, y: f64);
    fn CGPathAddArcToPoint(
        path: *mut c_void,
        transform: *const c_void,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        radius: f64,
    );
    fn CGPathAddQuadCurveToPoint(
        path: *mut c_void,
        transform: *const c_void,
        cpx: f64,
        cpy: f64,
        x: f64,
        y: f64,
    );
    fn CGPathCloseSubpath(path: *mut c_void);
    fn CGPathRelease(path: *mut c_void);
    // `rect` is `CGRect` passed BY VALUE in the real signatures. An earlier
    // `*const c_void` declaration placed the pointer where the callee reads
    // the rect — the calls silently produced garbage, and the only reason the
    // card still showed a background was the root fallback colour.
    fn CGPathAddRoundedRect(
        path: *mut c_void,
        transform: *const c_void,
        rect: CGRectFfi,
        corner_width: f64,
        corner_height: f64,
    );
    fn CGPathAddEllipseInRect(path: *mut c_void, transform: *const c_void, rect: CGRectFfi);
    fn CGColorCreateSRGB(
        red: f64,
        green: f64,
        blue: f64,
        alpha: f64,
    ) -> core_graphics::sys::CGColorRef;
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

pub struct CardLayer {
    root: Retained<CALayer>,
    background: Retained<CAShapeLayer>,
    texts: Vec<Retained<CATextLayer>>,
    specs: Vec<TextSpec>,
    extra: Vec<Retained<CALayer>>,
    /// Bar fills of the current content, for the grow-in animation.
    fills: Vec<Retained<CAShapeLayer>>,
    scale: f64,
    dark: bool,
}

impl CardLayer {
    pub fn new(parent: &CALayer, scale: f64) -> Self {
        let root = CALayer::new();
        no_implicit_actions(&root);
        root.setOpaque(false);
        root.setOpacity(0.0);
        root.setContentsScale(scale);
        // The window's draw root is already flipped (y down); flipping again
        // here would put the card upside down.
        root.setHidden(true);
        parent.addSublayer(&root);

        let background = unsafe { CAShapeLayer::new() };
        no_implicit_actions(&background);
        background.setContentsScale(scale);
        background.setOpaque(true);
        // Nearly-opaque dark glass: 92% opacity so the card never shows the
        // desktop through it, with a subtle white hairline for definition.
        unsafe {
            set_fill_color(&background, &rgba(0.10, 0.10, 0.11, 0.92));
            send_set_color(
                &background,
                objc2::sel!(setStrokeColor:),
                &rgba(1.0, 1.0, 1.0, 0.10),
            );
            background.setLineWidth(0.5);
        };
        root.addSublayer(&background);

        // No rectangular fallback fill on `root`: the card is no longer
        // clipped to a rounded rect (the speech tail pokes out of it), so a
        // root background would show square corners.

        Self {
            root,
            background,
            texts: Vec::new(),
            specs: Vec::new(),
            extra: Vec::new(),
            fills: Vec::new(),
            scale,
            dark: true,
        }
    }

    /// Repaint the persistent chrome (glass bg + hairline) for the theme.
    /// Ephemeral layers (bars, separators, dots) are rebuilt by the next
    /// `show_plan`, which also supplies the matching palette.
    pub fn set_dark(&mut self, dark: bool) {
        self.dark = dark;
        let pal = super::card::card_palette(dark);
        let Rgba(r, g, b, a) = pal.bg;
        let Rgba(hr, hg, hb, ha) = pal.hairline;
        unsafe {
            set_fill_color(&self.background, &rgba(r, g, b, a));
            send_set_color(
                &self.background,
                objc2::sel!(setStrokeColor:),
                &rgba(hr, hg, hb, ha),
            );
        }
    }

    pub fn is_visible(&self) -> bool {
        // Query the model layer, not the presentation layer: right after
        // `set_visible(false)` the opacity is still animating down from 1.0,
        // so an opacity threshold keeps reporting "visible" for ~140 ms. The
        // ordering bug that produced: `hide_card` sets opacity 0, the card
        // still reports visible, the 150 ms hover grace never fires, and the
        // next `CursorMoved` over the capsule re-opens the card on its own.
        !self.root.isHidden()
    }

    /// Re-lay the card body with a speech tail pointing at the hovered ball.
    /// `tail_left`: the tail sits on the card's left edge (card is right of
    /// the capsule), so the whole card shifts right by [`TAIL_W`] inside the
    /// wider window. `tail_y` is the tail tip in card coordinates.
    pub fn layout_with_tail(&self, width: f64, height: f64, tail_left: bool, tail_y: f64) {
        self.layout(width, height);
        let offset = if tail_left { TAIL_W } else { 0.0 };
        self.root.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(offset, 0.0),
            objc2_foundation::CGSize::new(width, height),
        ));
        let path = bubble_path(width, height, super::card::CARD_RADIUS, tail_left, tail_y);
        unsafe {
            send_cf_ptr(&self.background, objc2::sel!(setPath:), path);
            CGPathRelease(path);
        }
    }

    pub fn layout(&self, width: f64, height: f64) {
        self.root.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(0.0, 0.0),
            objc2_foundation::CGSize::new(width, height),
        ));
        self.background.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(0.0, 0.0),
            objc2_foundation::CGSize::new(width, height),
        ));
        let path = rounded_rect_path(width, height, super::card::CARD_RADIUS);
        unsafe {
            send_cf_ptr(&self.background, objc2::sel!(setPath:), path);
            CGPathRelease(path);
        }
    }

    /// Hide every dynamic sublayer, keeping them allocated for reuse.
    pub fn reset(&mut self) {
        for text in &self.texts {
            text.setHidden(true);
        }
        for layer in &self.extra {
            layer.removeFromSuperlayer();
        }
        self.extra.clear();
        self.fills.clear();
        self.specs.clear();
    }

    pub fn set_visible(&self, visible: bool) {
        // No fade animation: show/hide instantly. The previous 160ms fade
        // caused the "empty frame lingers" bug when the event loop didn't
        // wake up to run the completion handler.
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        self.root.setOpacity(if visible { 1.0 } else { 0.0 });
        self.root.setHidden(!visible);
        objc2_quartz_core::CATransaction::commit();
    }

    /// Called once the fade-out animation has finished (`app.rs` runs this
    /// through `card_order_out_at`): the layer is hidden for real.
    pub fn set_hidden(&self, hidden: bool) {
        self.root.setHidden(hidden);
    }

    pub fn bar(&mut self, x: f64, y: f64, width: f64, height: f64, fraction: f64, color: Rgba) {
        let track = unsafe { CAShapeLayer::new() };
        track.setContentsScale(self.scale);
        track.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(x, y),
            objc2_foundation::CGSize::new(width, height),
        ));
        let track_path = rounded_rect_path(width, height, height / 2.0);
        let Rgba(tr, tg, tb, ta) = super::card::card_palette(self.dark).track;
        unsafe {
            send_cf_ptr(&track, objc2::sel!(setPath:), track_path);
            set_fill_color(&track, &rgba(tr, tg, tb, ta));
            CGPathRelease(track_path);
        }
        self.root.addSublayer(&track);
        self.extra.push(Retained::into_super(track));

        let fill = unsafe { CAShapeLayer::new() };
        fill.setContentsScale(self.scale);
        // Grow from the left edge: anchor there before placing the frame.
        fill.setAnchorPoint(objc2_foundation::CGPoint::new(0.0, 0.5));
        fill.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(x, y),
            objc2_foundation::CGSize::new(width, height),
        ));
        let fill_path = rounded_rect_path(width * fraction.clamp(0.0, 1.0), height, height / 2.0);
        unsafe {
            send_cf_ptr(&fill, objc2::sel!(setPath:), fill_path);
            set_fill_color(&fill, &rgba(color.0, color.1, color.2, color.3));
            CGPathRelease(fill_path);
        }
        self.root.addSublayer(&fill);
        self.fills.push(fill.clone());
        self.extra.push(Retained::into_super(fill));
    }

    /// Bars grow from empty to their value, one row after another, with a
    /// little overshoot — the card "fills up" as it opens.
    pub fn animate_bars_in(&self) {
        let now = unsafe { CACurrentMediaTime() };
        for (index, fill) in self.fills.iter().enumerate() {
            unsafe {
                let key = objc2_foundation::NSString::from_str("grow");
                fill.removeAnimationForKey(&key);
                let grow = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
                    &objc2_foundation::NSString::from_str("transform.scale.x"),
                ));
                // Not exactly 0: a zero scale is a singular transform that Core
                // Animation can't interpolate — the bar sat empty for the whole
                // animation and then snapped full.
                grow.setFromValue(Some(&objc2_foundation::NSNumber::new_f64(0.001)));
                grow.setToValue(Some(&objc2_foundation::NSNumber::new_f64(1.0)));
                grow.setDuration(0.7);
                grow.setBeginTime(now + 0.06 + index as f64 * 0.04);
                grow.setFillMode(objc2_quartz_core::kCAFillModeBackwards);
                if let Some(curve) = timing_curve([0.34, 1.3, 0.64, 1.0]) {
                    let sel = objc2::runtime::Sel::register("setTimingFunction:");
                    let _: () = objc2::msg_send![&grow, performSelector: sel, withObject: &*curve];
                }
                fill.addAnimation_forKey(&grow, Some(&key));
            }
        }
    }

    pub fn dot(&mut self, x: f64, y: f64, radius: f64, color: Rgba) {
        let dot = unsafe { CAShapeLayer::new() };
        dot.setContentsScale(self.scale);
        dot.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(x, y),
            objc2_foundation::CGSize::new(radius * 2.0, radius * 2.0),
        ));
        let path = circle_path(radius);
        unsafe {
            send_cf_ptr(&dot, objc2::sel!(setPath:), path);
            set_fill_color(&dot, &rgba(color.0, color.1, color.2, color.3));
            CGPathRelease(path);
        }
        self.root.addSublayer(&dot);
        self.extra.push(Retained::into_super(dot));
    }

    pub fn separator(&mut self, x: f64, y: f64, width: f64) {
        let line = CALayer::new();
        line.setContentsScale(self.scale);
        line.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(x, y),
            objc2_foundation::CGSize::new(width, 1.0),
        ));
        let Rgba(sr, sg, sb, sa) = super::card::card_palette(self.dark).separator;
        unsafe { set_background_color(&line, &rgba(sr, sg, sb, sa)) };
        self.root.addSublayer(&line);
        self.extra.push(line);
    }

    pub fn box_container(
        &mut self,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        radius: f64,
        fill: Rgba,
        border: Option<(f64, Rgba)>,
    ) {
        let box_layer = unsafe { CAShapeLayer::new() };
        box_layer.setContentsScale(self.scale);
        box_layer.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(x, y),
            objc2_foundation::CGSize::new(width, height),
        ));
        let path = rounded_rect_path(width, height, radius);
        unsafe {
            send_cf_ptr(&box_layer, objc2::sel!(setPath:), path);
            set_fill_color(&box_layer, &rgba(fill.0, fill.1, fill.2, fill.3));
            if let Some((border_width, border_color)) = border {
                box_layer.setLineWidth(border_width);
                send_set_color(
                    &box_layer,
                    objc2::sel!(setStrokeColor:),
                    &rgba(
                        border_color.0,
                        border_color.1,
                        border_color.2,
                        border_color.3,
                    ),
                );
            }
            CGPathRelease(path);
        }
        self.root.addSublayer(&box_layer);
        self.extra.push(Retained::into_super(box_layer));
    }

    /// Lay out a batch of text specs, reusing the pooled text layers.
    pub fn render_texts(&mut self, specs: &[TextSpec]) {
        while self.texts.len() < specs.len().min(MAX_TEXT_LAYERS) {
            let layer = unsafe { CATextLayer::new() };
            no_implicit_actions(&layer);
            layer.setContentsScale(self.scale);
            unsafe { layer.setAlignmentMode(kCAAlignmentLeft) };
            unsafe {
                layer.setTruncationMode(kCATruncationEnd);
                layer.setWrapped(false);
            }
            self.root.addSublayer(&layer);
            self.texts.push(layer);
        }
        for (index, spec) in specs.iter().enumerate() {
            let Some(layer) = self.texts.get(index) else {
                break;
            };
            layer.setHidden(false);
            unsafe { layer.setString(Some(&objc2_foundation::NSString::from_str(&spec.text))) };
            unsafe {
                layer.setFontSize(spec.size);
                set_foreground_color(
                    layer,
                    &rgba(spec.color.0, spec.color.1, spec.color.2, spec.color.3),
                );
            }
            let font = card_font::font_by_kind(spec.font, spec.size, spec.weight);
            // `CATextLayer.font` is a raw CFTypeRef, not exposed by the bindings.
            let font_ptr = Retained::as_ptr(&font) as *const c_void;
            unsafe {
                send_cf_ptr(layer, objc2::sel!(setFont:), font_ptr);
            }
            let align_mode = unsafe {
                match spec.align {
                    TextAlign::Left => kCAAlignmentLeft,
                    TextAlign::Right => kCAAlignmentRight,
                    TextAlign::Center => kCAAlignmentCenter,
                }
            };
            unsafe { layer.setAlignmentMode(align_mode) };

            let w = spec
                .width
                .unwrap_or_else(|| (super::card::CARD_WIDTH - spec.x - 14.0).max(1.0));
            layer.setFrame(objc2_foundation::CGRect::new(
                objc2_foundation::CGPoint::new(spec.x, spec.y),
                objc2_foundation::CGSize::new(w.max(1.0), spec.size * 1.35),
            ));
        }
        for layer in self.texts.iter().skip(specs.len()) {
            layer.setHidden(true);
        }
        self.specs = specs.to_vec();
    }

    /// Update the string (and optionally the colour) of a single text line
    /// without re-laying-out the pool — used for live countdown / freshness
    /// ticks where only one line's text changes.
    pub fn update_line(&mut self, index: usize, text: &str, color: Option<Rgba>) {
        let Some(layer) = self.texts.get(index) else {
            return;
        };
        let Some(spec) = self.specs.get_mut(index) else {
            return;
        };
        spec.text = text.to_string();
        if let Some(color) = color {
            spec.color = color;
        }
        unsafe {
            layer.setString(Some(&objc2_foundation::NSString::from_str(text)));
            set_foreground_color(
                layer,
                &rgba(spec.color.0, spec.color.1, spec.color.2, spec.color.3),
            );
        }
    }

    /// The specs currently on screen, for countdown-only updates.
    pub fn text_specs(&self) -> Option<Vec<TextSpec>> {
        (!self.specs.is_empty()).then(|| self.specs.clone())
    }

    /// The card's root layer, for attaching the live mini ball as a sublayer.
    pub fn root_layer(&self) -> &CALayer {
        &self.root
    }

    /// Backing scale the layer tree was built with (for new sublayers).
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Pop-in: fade + slide from `dx` + a hair of scale. Fire-and-forget
    /// animations — no completion handlers, so nothing can be left half-shown
    /// the way the old transaction fade was. Call after `set_visible(true)`.
    pub fn animate_in(&self, dx: f64) {
        unsafe {
            let add = |key_path: &str, from: f64, to: f64, duration: f64| {
                let key = objc2_foundation::NSString::from_str(key_path);
                self.root.removeAnimationForKey(&key);
                let animation =
                    objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(&key));
                animation.setFromValue(Some(&objc2_foundation::NSNumber::new_f64(from)));
                animation.setToValue(Some(&objc2_foundation::NSNumber::new_f64(to)));
                animation.setDuration(duration);
                if let Some(curve) = apple_ease_out() {
                    let sel = objc2::runtime::Sel::register("setTimingFunction:");
                    let _: () =
                        objc2::msg_send![&animation, performSelector: sel, withObject: &*curve];
                }
                self.root.addAnimation_forKey(&animation, Some(&key));
            };
            add("opacity", 0.0, 1.0, 0.16);
            add("transform.translation.x", dx, 0.0, 0.22);
            add("transform.scale", 0.96, 1.0, 0.22);
        }
    }
}

pub(crate) fn rgba(r: f64, g: f64, b: f64, a: f64) -> CGColor {
    // Generic-RGB colours wash out web hex values; sRGB matches the web UI.
    unsafe { CGColor::wrap_under_create_rule(CGColorCreateSRGB(r, g, b, a)) }
}

fn rounded_rect_path(width: f64, height: f64, radius: f64) -> *mut c_void {
    unsafe {
        let path = CGPathCreateMutable();
        // Uniform corner radius: CGPathAddRoundedRect takes (width, height) of
        // the corner ellipse. Passing min(w,h)/2 as the width made the corner
        // a squashed ellipse that never matched the vibrancy's round corners,
        // leaving white slivers at the four corners.
        CGPathAddRoundedRect(
            path,
            std::ptr::null(),
            rounded_rect(width, height),
            radius,
            radius,
        );
        path
    }
}

/// How far the speech tail pokes out of the card, and half its base.
pub const TAIL_W: f64 = 7.0;
const TAIL_HALF: f64 = 8.0;

/// Rounded card + speech tail as ONE outline, so the translucent fill and
/// the hairline never double up where tail and body meet.
fn bubble_path(width: f64, height: f64, radius: f64, tail_left: bool, tail_y: f64) -> *mut c_void {
    let (w, h, r) = (width, height, radius);
    let ty = tail_y.clamp(r + TAIL_HALF, (h - r - TAIL_HALF).max(r + TAIL_HALF));
    unsafe {
        let path = CGPathCreateMutable();
        let null = std::ptr::null();
        CGPathMoveToPoint(path, null, r, 0.0);
        CGPathAddLineToPoint(path, null, w - r, 0.0);
        CGPathAddArcToPoint(path, null, w, 0.0, w, r, r);
        if !tail_left {
            CGPathAddLineToPoint(path, null, w, ty - TAIL_HALF);
            CGPathAddQuadCurveToPoint(path, null, w, ty - TAIL_HALF * 0.25, w + TAIL_W, ty);
            CGPathAddQuadCurveToPoint(path, null, w, ty + TAIL_HALF * 0.25, w, ty + TAIL_HALF);
        }
        CGPathAddLineToPoint(path, null, w, h - r);
        CGPathAddArcToPoint(path, null, w, h, w - r, h, r);
        CGPathAddLineToPoint(path, null, r, h);
        CGPathAddArcToPoint(path, null, 0.0, h, 0.0, h - r, r);
        if tail_left {
            CGPathAddLineToPoint(path, null, 0.0, ty + TAIL_HALF);
            CGPathAddQuadCurveToPoint(path, null, 0.0, ty + TAIL_HALF * 0.25, -TAIL_W, ty);
            CGPathAddQuadCurveToPoint(path, null, 0.0, ty - TAIL_HALF * 0.25, 0.0, ty - TAIL_HALF);
        }
        CGPathAddLineToPoint(path, null, 0.0, r);
        CGPathAddArcToPoint(path, null, 0.0, 0.0, r, 0.0, r);
        CGPathCloseSubpath(path);
        path
    }
}

fn circle_path(radius: f64) -> *mut c_void {
    unsafe {
        let path = CGPathCreateMutable();
        CGPathAddEllipseInRect(
            path,
            std::ptr::null(),
            rounded_rect(radius * 2.0, radius * 2.0),
        );
        path
    }
}

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {
    fn CACurrentMediaTime() -> f64;
}

/// Null out the implicit actions that make Core Animation cross-fade card
/// content: without this, switching balls showed the old plan's text
/// ghosting under the new one for a few frames (a transaction's
/// `disableActions` did not cover every path that touches these layers).
fn no_implicit_actions(layer: &CALayer) {
    const KEYS: [&str; 10] = [
        "contents",
        "string",
        "foregroundColor",
        "fontSize",
        "position",
        "bounds",
        "frame",
        "hidden",
        "path",
        "fillColor",
    ];
    unsafe {
        let null: Retained<objc2::runtime::AnyObject> =
            objc2::msg_send_id![objc2::class!(NSNull), null];
        let keys: Vec<Retained<objc2_foundation::NSString>> = KEYS
            .iter()
            .map(|k| objc2_foundation::NSString::from_str(k))
            .collect();
        let key_refs: Vec<&objc2_foundation::NSString> = keys.iter().map(|k| &**k).collect();
        let values: Vec<Retained<objc2::runtime::AnyObject>> =
            KEYS.iter().map(|_| null.clone()).collect();
        let dict = objc2_foundation::NSDictionary::from_vec(&key_refs, values);
        let _: () = objc2::msg_send![layer, setActions: &*dict];
    }
}

#[cfg(test)]
mod timing_curve_tests {
    use super::timing_curve;

    /// Read a control point back out of the CAMediaTimingFunction.
    fn control_point(curve: &objc2_foundation::NSObject, index: usize) -> [f32; 2] {
        let mut out = [0f32; 2];
        type GetFn = unsafe extern "C" fn(
            *const objc2_foundation::NSObject,
            objc2::runtime::Sel,
            usize,
            *mut f32,
        );
        unsafe {
            let get: GetFn = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
            get(
                curve,
                objc2::sel!(getControlPointAtIndex:values:),
                index,
                out.as_mut_ptr(),
            );
        }
        out
    }

    #[test]
    fn control_points_survive_the_ffi_round_trip() {
        let curve = timing_curve([0.34, 1.45, 0.64, 1.0]).expect("curve");
        let p1 = control_point(&curve, 1);
        let p2 = control_point(&curve, 2);
        assert!(
            (p1[0] - 0.34).abs() < 1e-4 && (p1[1] - 1.45).abs() < 1e-4,
            "p1 = {p1:?}"
        );
        assert!(
            (p2[0] - 0.64).abs() < 1e-4 && (p2[1] - 1.0).abs() < 1e-4,
            "p2 = {p2:?}"
        );
    }
}
