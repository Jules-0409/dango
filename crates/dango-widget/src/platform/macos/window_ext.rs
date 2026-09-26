//! AppKit window plumbing: non-activating glass panels, tracking areas and
//! screen geometry. Everything here is `unsafe` Objective-C messaging behind a
//! small safe-ish surface.

use std::ffi::c_void;
use std::sync::OnceLock;

use objc2::ffi::{class_addMethod, class_replaceMethod};
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::ClassType;
use objc2_app_kit::{
    NSAppearanceCustomization, NSColor, NSEvent, NSScreen, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSWindow,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CAMediaTiming;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use window_vibrancy::NSVisualEffectViewTagged;
use winit::window::Window;

/// Configure a winit window as a non-activating overlay that can never become
/// key.
///
/// winit's `WinitWindow` hardcodes `canBecomeKeyWindow`/`canBecomeMainWindow`
/// to `YES` (winit 0.30 `window.rs`), so even borderless widget windows can
/// take key. A key widget window is exactly the state that feeds the
/// `objc_loadWeakRetained` crash in `-[NSWindow resignKeyWindow]` on app
/// deactivation, so here we restore AppKit's own default rule — only titled
/// windows can become key/main (that is what plain `NSWindow` answers) — by
/// replacing the two methods on winit's `WinitWindow` class. The settings
/// window is titled and stays keyable; the borderless capsule and card
/// windows can never become key, so they never resign key either.
///
/// Do NOT:
/// - add `NSWindowStyleMaskNonactivatingPanel` (0x80): AppKit rejects that bit
///   on a plain `NSWindow`, and the churn around it is what let the borderless
///   windows take key;
/// - isa-swizzle the window instances to a custom subclass: winit's window
///   delegate KVO-observes `effectiveAppearance` on the window, so by the time
///   we see the object its runtime class is already the private, runtime-
///   generated `NSKVONotifying_WinitWindow`. Subclassing that and swapping the
///   isa breaks the object's identity predicates (`isKindOfClass:` returns NO
///   even for `NSObject`), and AppKit's `aResponder == nil ||
///   [aResponder isKindOfClass:[NSResponder class]]` assertion in
///   `-[NSWindow setContentView:]` then aborts the app at startup.
pub unsafe fn make_non_activating_panel(window: &Window) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit_handle) = handle.as_raw() else {
        return;
    };
    let view_ptr = appkit_handle.ns_view.as_ptr() as *mut AnyObject;
    let nswindow: *mut AnyObject = msg_send![view_ptr, window];
    if nswindow.is_null() {
        return;
    }
    disable_key_window_for_borderless_windows();
    let current_cb: usize = msg_send![nswindow, collectionBehavior];
    let _: () = msg_send![nswindow, setCollectionBehavior: current_cb | 1 | 16 | 64];
    let _: () = msg_send![nswindow, setHidesOnDeactivate: false];
    let _: () = msg_send![nswindow, setAcceptsMouseMovedEvents: true];
    // The widget is a desktop fixture: it follows background-window drags like
    // any other overlay. `window_state` persists the position once it settles.
    let _: () = msg_send![nswindow, setMovableByWindowBackground: true];
}

/// `NSWindowStyleMaskTitled`.
const NS_WINDOW_STYLE_MASK_TITLED: usize = 1 << 0;

unsafe extern "C-unwind" fn can_become_key_or_main(this: *mut AnyObject, _cmd: Sel) -> Bool {
    // AppKit's own rule for a plain NSWindow: a window can become key/main
    // only when it is titled. Borderless overlays (capsule, card) answer NO.
    let mask: usize = msg_send![this, styleMask];
    if mask & NS_WINDOW_STYLE_MASK_TITLED != 0 {
        Bool::YES
    } else {
        Bool::NO
    }
}

/// Replace `canBecomeKeyWindow`/`canBecomeMainWindow` on winit's
/// `WinitWindow` class (once per process) with the titled-window rule.
///
/// This is a class-method patch, not an isa-swap: the window instances keep
/// their runtime class (including the KVO subclass AppKit/winit install for
/// `effectiveAppearance` observation), and the settings window — created with
/// decorations — still becomes key as it must for the WKWebView's text input.
fn disable_key_window_for_borderless_windows() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let Some(winit_window) = AnyClass::get("WinitWindow") else {
            // winit registers `WinitWindow` lazily on first window creation;
            // by the time we are configuring a window it always exists.
            return;
        };
        let imp: Imp = unsafe {
            std::mem::transmute(
                can_become_key_or_main as unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool,
            )
        };
        let types = c"B@:".as_ptr();
        let cls_ptr = winit_window as *const AnyClass as *mut objc2::ffi::objc_class;
        for sel_name in ["canBecomeKeyWindow", "canBecomeMainWindow"] {
            let sel = Sel::register(sel_name);
            unsafe {
                let added = class_addMethod(cls_ptr, sel.as_ptr(), Some(imp), types);
                if !added {
                    class_replaceMethod(cls_ptr, sel.as_ptr(), Some(imp), types);
                }
            }
        }
    });
}

/// `NSAppearanceNameDarkAqua` for dark glass, `NSAppearanceNameAqua` for
/// light — the vibrancy material and any HUD-ish behaviour both follow it.
fn appearance_for(dark: bool) -> Option<Retained<objc2_app_kit::NSAppearance>> {
    unsafe {
        let name = if dark {
            objc2_app_kit::NSAppearanceNameDarkAqua
        } else {
            objc2_app_kit::NSAppearanceNameAqua
        };
        objc2_app_kit::NSAppearance::appearanceNamed(name)
    }
}

/// True when macOS is in dark mode — the System theme's resolver.
pub fn system_prefers_dark() -> bool {
    let Some(marker) = MainThreadMarker::new() else {
        return true;
    };
    let app = objc2_app_kit::NSApplication::sharedApplication(marker);
    unsafe {
        app.effectiveAppearance()
            .name()
            .to_string()
            .to_lowercase()
            .contains("dark")
    }
}

/// Re-set a window's appearance, e.g. when the theme changes at runtime.
pub unsafe fn set_window_appearance(window: &Window, dark: bool) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit_handle) = handle.as_raw() else {
        return;
    };
    let view: &objc2_app_kit::NSView = &*appkit_handle.ns_view.as_ptr().cast();
    if let Some(nswindow) = view.window() {
        if let Some(appearance) = appearance_for(dark) {
            nswindow.setAppearance(Some(&appearance));
        }
    }
}

/// Resize a window around its top-left corner using AppKit's own smooth
/// resize animation — the collapse/expand transition.
pub unsafe fn resize_keeping_top_left(window: &Window, width: f64, height: f64, animate: bool) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit_handle) = handle.as_raw() else {
        return;
    };
    let view: &objc2_app_kit::NSView = &*appkit_handle.ns_view.as_ptr().cast();
    if let Some(nswindow) = view.window() {
        let frame = nswindow.frame();
        let top = frame.origin.y + frame.size.height;
        nswindow.setFrame_display_animate(
            NSRect::new(
                NSPoint::new(frame.origin.x, top - height),
                NSSize::new(width, height),
            ),
            true,
            animate,
        );
    }
}

/// Install a HUDWindow vibrancy view behind the window's content.
///
/// `glass_tint` adds the shared glass recipe (theme-tuned tint + 0.5pt
/// hairline) as an overlay. Only the capsule needs it: the detail card draws
/// its own tint and hairline inside `card_layer`, and a second one at the
/// window level would sit above the card's `draw_root` (same z-position, later
/// sibling wins) and blank out every label.
///
/// Returns the tint overlay when one was installed so `retint_overlay` can
/// recolour it on theme change.
pub unsafe fn put_vibrancy_behind_content(
    window: &Window,
    radius: f64,
    glass_tint: bool,
    dark: bool,
) -> Result<Option<Retained<objc2_quartz_core::CAShapeLayer>>, String> {
    let handle = window
        .window_handle()
        .map_err(|error| format!("window handle: {error}"))?;
    let RawWindowHandle::AppKit(appkit_handle) = handle.as_raw() else {
        return Err("window does not have an AppKit handle".into());
    };

    let marker = MainThreadMarker::new().ok_or("not on the AppKit main thread")?;
    let view: &objc2_app_kit::NSView = &*appkit_handle.ns_view.as_ptr().cast();
    let nswindow = view.window().ok_or("AppKit view has no window")?;
    let effect = NSVisualEffectViewTagged::initWithFrame(marker.alloc(), view.frame(), 0);
    // HUDWindow + BehindWindow: the blur is applied behind the window's
    // content, giving the glass effect. The window's shape is determined by
    // the contentView's layer cornerRadius.
    effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    effect.setState(NSVisualEffectState::Active);
    effect.setAutoresizingMask(
        objc2_app_kit::NSAutoresizingMaskOptions::NSViewWidthSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::NSViewHeightSizable,
    );

    nswindow.setOpaque(false);
    let clear = NSColor::clearColor();
    nswindow.setBackgroundColor(Some(&clear));
    // The window shadow is outside the vibrancy's rounded rect; without this
    // the corners show the desktop through the shadow region.
    nswindow.setHasShadow(false);
    // Round the window's own shape (not just the vibrancy view's contents) so
    // the corners are transparent instead of showing the window background.
    // The frame view is the content view's superview; masking it clips the
    // whole window including the vibrancy effect.
    if let Some(frame_view) = view.superview() {
        frame_view.setWantsLayer(true);
        if let Some(layer) = frame_view.layer() {
            layer.setCornerRadius(radius);
            layer.setMasksToBounds(true);
        }
    }
    // Appearance follows the widget theme — dark glass or light glass.
    if let Some(appearance) = appearance_for(dark) {
        nswindow.setAppearance(Some(&appearance));
    }

    if let Some(layer) = view.layer() {
        layer.setOpaque(false);
    } else {
        view.setWantsLayer(true);
        if let Some(layer) = view.layer() {
            layer.setOpaque(false);
        }
    }

    // The design language has exactly one glass recipe, shared by the capsule
    // and the detail card: a translucent tint over the vibrancy blur, plus a
    // 0.5pt hairline. Without the tint the capsule read as a different
    // material from the card that unfolds from it.
    let tint_overlay = if glass_tint {
        add_glass_tint_overlay(view, radius, dark)
    } else {
        None
    };

    // Do NOT install the effect view as the window's contentView: winit's
    // window delegate resolves its view via `window.contentView()` and casts
    // it to `WinitView`. Handing it the vibrancy view means every delegate
    // callback (e.g. windowDidResignKey -> view().reset_modifiers()) reads
    // winit ivars on a plain NSView — debug panics on uninitialized ivars,
    // release reads the effect view's own fields (e.g. cornerRadius 250.0)
    // as a weak pointer and segfaults inside objc_loadWeakRetained. Keep
    // winit's view as contentView and park the vibrancy view behind it in
    // the frame view instead.
    if let Some(frame_view) = view.superview() {
        frame_view.addSubview_positioned_relativeTo(
            &effect,
            objc2_app_kit::NSWindowOrderingMode::NSWindowBelow,
            Some(view),
        );
    }
    // Round the corners AFTER the effect view is installed in the hierarchy,
    // so the radius applies to its final frame, not the initial bounds.
    effect.setCornerRadius(radius);
    effect.setWantsLayer(true);
    if let Some(layer) = effect.layer() {
        layer.setCornerRadius(radius);
        layer.setMasksToBounds(true);
    }
    // The winit view stays contentView and already tracks the window's size.
    view.setAutoresizingMask(
        objc2_app_kit::NSAutoresizingMaskOptions::NSViewWidthSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::NSViewHeightSizable,
    );
    // The window retains the vibrancy view once it is in the hierarchy;
    // return it to the normal autorelease lifetime.
    drop(effect);

    // A vibrancy view swallows mouse events; make it transparent to hit testing
    // so hover and drag reach the layer-backed content view underneath.
    disable_effect_view_hit_test();
    // Do NOT call attach_tracking_to_view here: winit's WinitView manages its
    // own tracking area, and adding another one on top of it (or on its
    // subviews) crashes AppKit with "invalid NSTrackingRectTag". The vibrancy
    // view is already hit-test-transparent, so winit's own tracking works.
    Ok(tint_overlay)
}

/// The `send` plumbing for CGColor/CGPath-valued setters — the generated
/// bindings can't express `CGColorRef` parameters.
type SetColorFn = unsafe extern "C" fn(*mut c_void, Sel, *const c_void);

unsafe fn send_layer_ptr(
    layer: &objc2_quartz_core::CAShapeLayer,
    selector: Sel,
    ptr: *const c_void,
) {
    let send: SetColorFn = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
    send(
        (layer as *const objc2_quartz_core::CAShapeLayer)
            .cast_mut()
            .cast(),
        selector,
        ptr,
    );
}

/// Repaint the capsule's glass tint for the other theme without rebuilding
/// the layer tree (runtime theme switch).
pub fn retint_overlay(overlay: &objc2_quartz_core::CAShapeLayer, dark: bool) {
    let (r, g, b, a) = crate::theme::capsule_tint(dark);
    let (hr, hg, hb, ha) = crate::theme::capsule_hairline(dark);
    paint_overlay(overlay, r, g, b, a, hr, hg, hb, ha);
}

fn paint_overlay(
    overlay: &objc2_quartz_core::CAShapeLayer,
    r: f64,
    g: f64,
    b: f64,
    a: f64,
    hr: f64,
    hg: f64,
    hb: f64,
    ha: f64,
) {
    use super::card_layer;
    use core_foundation::base::TCFType;
    let tint = card_layer::rgba(r, g, b, a);
    let stroke = card_layer::rgba(hr, hg, hb, ha);
    unsafe {
        send_layer_ptr(
            overlay,
            objc2::sel!(setFillColor:),
            tint.as_CFTypeRef().cast(),
        );
        send_layer_ptr(
            overlay,
            objc2::sel!(setStrokeColor:),
            stroke.as_CFTypeRef().cast(),
        );
    }
}

/// Reframe + repath the tint overlay after a window resize (the collapse
/// pill needs its own little rounded rect — the old path would draw a huge
/// clipped outline).
pub fn relayout_overlay(
    overlay: &objc2_quartz_core::CAShapeLayer,
    bounds: NSRect,
    radius: f64,
    dark: bool,
) {
    unsafe extern "C" {
        fn CGPathCreateMutable() -> *mut c_void;
        fn CGPathRelease(path: *mut c_void);
        fn CGPathAddRoundedRect(
            path: *mut c_void,
            transform: *const c_void,
            rect: objc2_foundation::CGRect,
            corner_width: f64,
            corner_height: f64,
        );
    }
    objc2_quartz_core::CATransaction::begin();
    objc2_quartz_core::CATransaction::setDisableActions(true);
    overlay.setFrame(bounds);
    let path = unsafe {
        let path = CGPathCreateMutable();
        CGPathAddRoundedRect(path, std::ptr::null(), bounds, radius, radius);
        path
    };
    unsafe {
        send_layer_ptr(overlay, objc2::sel!(setPath:), path);
        CGPathRelease(path);
        overlay.setCornerRadius(radius);
    }
    retint_overlay(overlay, dark);
    objc2_quartz_core::CATransaction::commit();
}

/// Paint the shared glass recipe (theme tint + 0.5pt hairline) over a
/// vibrancy window's content, matching the detail card exactly. Returns the
/// overlay layer so `retint_overlay` can recolour it later.
///
/// Same plumbing as `card_layer.rs`: the CGColor/CGPath-valued setters are
/// reached through `objc_msgSend` with a real pointer.
fn add_glass_tint_overlay(
    view: &objc2_app_kit::NSView,
    radius: f64,
    dark: bool,
) -> Option<Retained<objc2_quartz_core::CAShapeLayer>> {
    use objc2_quartz_core::CAShapeLayer;

    let bounds = view.bounds();
    let overlay = unsafe { CAShapeLayer::new() };
    overlay.setOpaque(false);
    unsafe {
        overlay.setLineWidth(0.5);
    }
    overlay.setMasksToBounds(true);
    // Above the balls: sibling layers sit at z 0, so this only has to beat
    // the vibrancy view's own contents.
    overlay.setZPosition(1_000.0);
    relayout_overlay(&overlay, bounds, radius, dark);
    unsafe {
        if let Some(layer) = view.layer() {
            layer.addSublayer(&overlay);
        }
    }
    Some(overlay)
}

/// Make `NSVisualEffectViewTagged` ignore hit testing.
pub unsafe fn disable_effect_view_hit_test() {
    unsafe extern "C-unwind" fn hit_test_nil(
        _this: *mut c_void,
        _cmd: Sel,
        _point: NSPoint,
    ) -> *mut c_void {
        std::ptr::null_mut()
    }

    if let Some(cls) = AnyClass::get("NSVisualEffectViewTagged") {
        let sel = Sel::register("hitTest:");
        let imp: Imp =
            std::mem::transmute(hit_test_nil as unsafe extern "C-unwind" fn(_, _, _) -> _);
        let types = c"@:{NSPoint=dd}".as_ptr();
        let cls_ptr = cls as *const AnyClass as *mut objc2::ffi::objc_class;
        let added = unsafe { class_addMethod(cls_ptr, sel.as_ptr(), Some(imp), types) };
        if !added {
            unsafe { class_replaceMethod(cls_ptr, sel.as_ptr(), Some(imp), types) };
        }
    }
}

/// Attach an always-active mouse tracking area to a view (and its subviews).
pub unsafe fn attach_tracking_to_view(view: &objc2_app_kit::NSView) {
    use objc2_app_kit::{NSTrackingArea, NSTrackingAreaOptions};

    let name = view.class().name().to_string();
    if name.contains("NSVisualEffectViewTagged") {
        return;
    }

    let areas = view.trackingAreas();
    for area in areas.iter() {
        if area
            .options()
            .contains(NSTrackingAreaOptions::NSTrackingActiveAlways)
        {
            view.removeTrackingArea(area);
        }
    }

    let rect = view.bounds();
    let options = NSTrackingAreaOptions::NSTrackingActiveAlways
        | NSTrackingAreaOptions::NSTrackingMouseMoved
        | NSTrackingAreaOptions::NSTrackingMouseEnteredAndExited
        | NSTrackingAreaOptions::NSTrackingInVisibleRect;
    let tracking_area = NSTrackingArea::initWithRect_options_owner_userInfo(
        NSTrackingArea::alloc(),
        rect,
        options,
        Some(view),
        None,
    );
    view.addTrackingArea(&tracking_area);

    let subviews = view.subviews();
    for sub in subviews.iter() {
        attach_tracking_to_view(sub);
    }
}

/// Re-round the window's own outline (the frame view clip set up in
/// [`put_vibrancy_behind_content`]). It must follow fold/unfold: a clip left
/// at the pill's 9 pt radius makes the full capsule read as a squarish box.
pub fn set_window_corner_radius(window: &winit::window::Window, radius: f64) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit_handle) = handle.as_raw() else {
        return;
    };
    let view: &objc2_app_kit::NSView = unsafe { &*appkit_handle.ns_view.as_ptr().cast() };
    if let Some(frame_view) = unsafe { view.superview() } {
        if let Some(layer) = unsafe { frame_view.layer() } {
            layer.setCornerRadius(radius);
        }
        // The blur view behind the content keeps its own radius; left at the
        // capsule's 31 pt it squeezes into a lens inside the 18 pt pill.
        let effect_class = objc2_app_kit::NSVisualEffectView::class();
        for sibling in unsafe { frame_view.subviews() }.iter() {
            let is_effect: bool = unsafe { msg_send![&*sibling, isKindOfClass: effect_class] };
            if !is_effect {
                continue;
            }
            let responds: bool =
                unsafe { msg_send![&*sibling, respondsToSelector: objc2::sel!(setCornerRadius:)] };
            if responds {
                let _: () = unsafe { msg_send![&*sibling, setCornerRadius: radius] };
            }
            if let Some(layer) = unsafe { sibling.layer() } {
                layer.setCornerRadius(radius);
            }
        }
    }
}

/// Global mouse location in top-left origin screen coordinates.
pub fn global_mouse_pos() -> (f64, f64) {
    let loc = unsafe { NSEvent::mouseLocation() };
    // Flip against the PRIMARY display (screens[0], the one whose top-left
    // is winit's origin) — `mainScreen` is whichever screen has the key
    // window and gives wrong y on multi-monitor setups.
    let screens = NSScreen::screens(MainThreadMarker::new().unwrap());
    let screen_h = unsafe { screens.firstObject() }
        .map(|screen| screen.frame().size.height)
        .unwrap_or(900.0);
    (loc.x, screen_h - loc.y)
}

/// Frame of the screen containing `window`, in AppKit's bottom-left origin
/// coordinates. Single-screen machines always get (0, 0, w, h) regardless of
/// which producer asks; callers comparing against winit's logical space should
/// think of that y as already top-down (AppKit's bottom-left origin is the
/// (0, 0) point of the screen — equating "bottom-left" with a positive y
/// already matches winit's top-down convention when origin.y == 0).
pub fn screen_frame_for(nswindow: &NSWindow) -> NSRect {
    let _ = nswindow;
    let screen = NSScreen::mainScreen(MainThreadMarker::new().unwrap());
    let Some(screen) = screen else {
        return NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0));
    };
    let frame = screen.frame();
    NSRect::new(
        NSPoint::new(frame.origin.x, frame.origin.y),
        NSSize::new(frame.size.width, frame.size.height),
    )
}

/// Same as [`screen_frame_for`], but with the y axis already in winit's
/// top-left logical space (origin (0, 0) assumes AppKit's bottom-left origin
/// is the very top of macOS's virtual display; single-screen users keep
/// axis conventions aligned naturally).
pub fn screen_frame_for_top_left(_nswindow: &NSWindow) -> NSRect {
    let marker = objc2_foundation::MainThreadMarker::new().unwrap();
    let screen = NSScreen::mainScreen(marker);
    let Some(screen) = screen else {
        return NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1470.0, 956.0));
    };
    let frame = screen.frame();
    NSRect::new(
        NSPoint::new(frame.origin.x, frame.origin.y),
        NSSize::new(frame.size.width, frame.size.height),
    )
}

/// Position a window in top-left-origin logical coordinates.
pub fn set_window_position_top_left(nswindow: &NSWindow, x: f64, y: f64) {
    let frame = nswindow.frame();
    let height = frame.size.height;
    let screen_height = screen_frame_for(nswindow).size.height;
    let flipped_y = screen_height - y - height;
    nswindow.setFrameTopLeftPoint(NSPoint::new(x, flipped_y));
}

pub fn set_window_level_floating(nswindow: &NSWindow) {
    // NSFloatingWindowLevel = 3
    nswindow.setLevel(3);
}

pub fn nsstring(value: &str) -> Retained<NSString> {
    NSString::from_str(value)
}

/// `NSWindowStyleMaskTitled`-free borderless mask (value 0).
#[allow(dead_code)]
pub const fn style_mask_borderless() -> usize {
    0
}

fn frame_view(window: &winit::window::Window) -> Option<Retained<objc2_app_kit::NSView>> {
    let Ok(handle) = window.window_handle() else {
        return None;
    };
    let RawWindowHandle::AppKit(appkit_handle) = handle.as_raw() else {
        return None;
    };
    let view: &objc2_app_kit::NSView = unsafe { &*appkit_handle.ns_view.as_ptr().cast() };
    unsafe { view.superview() }
}

/// Top-anchored capsule outline `height` tall inside a window `window_h`
/// tall, in the frame view's layer coordinates (y-up unless flipped).
fn outline_path(width: f64, height: f64, window_h: f64, flipped: bool) -> *mut c_void {
    unsafe extern "C" {
        fn CGPathCreateMutable() -> *mut c_void;
        fn CGPathAddRoundedRect(
            path: *mut c_void,
            transform: *const c_void,
            rect: objc2_foundation::CGRect,
            corner_width: f64,
            corner_height: f64,
        );
    }
    let y = if flipped { 0.0 } else { window_h - height };
    let radius = width.min(height) / 2.0;
    unsafe {
        let path = CGPathCreateMutable();
        CGPathAddRoundedRect(
            path,
            std::ptr::null(),
            objc2_foundation::CGRect::new(
                objc2_foundation::CGPoint::new(0.0, y),
                objc2_foundation::CGSize::new(width, height),
            ),
            radius,
            radius,
        );
        path
    }
}

/// Clip the whole window (glass, tint, balls) to a top-anchored capsule and
/// morph it from `from_h` to `to_h` points tall. This is what folds and
/// unfolds the capsule: the window itself snaps between sizes (AppKit's
/// animated resize moves the glass and the content on different clocks), and
/// only this mask moves. The window must be `max(from_h, to_h)` tall while
/// the mask is on; [`clear_window_outline`] takes it off afterwards.
pub fn animate_window_outline(
    window: &winit::window::Window,
    width: f64,
    from_h: f64,
    to_h: f64,
    duration: f64,
    delay: f64,
) {
    unsafe extern "C" {
        fn CGPathRelease(path: *mut c_void);
        fn CACurrentMediaTime() -> f64;
    }
    let Some(frame_view) = frame_view(window) else {
        return;
    };
    let Some(layer) = (unsafe { frame_view.layer() }) else {
        return;
    };
    let flipped = frame_view.isFlipped();
    let window_h = from_h.max(to_h);
    let from = outline_path(width, from_h, window_h, flipped);
    let to = outline_path(width, to_h, window_h, flipped);
    unsafe {
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        let mask = objc2_quartz_core::CAShapeLayer::new();
        mask.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(0.0, 0.0),
            objc2_foundation::CGSize::new(width, window_h),
        ));
        send_layer_ptr(&mask, objc2::sel!(setPath:), to);
        let animation = objc2_quartz_core::CABasicAnimation::animationWithKeyPath(Some(
            &NSString::from_str("path"),
        ));
        animation.setFromValue(Some(&*(from as *const AnyObject)));
        animation.setToValue(Some(&*(to as *const AnyObject)));
        animation.setDuration(duration);
        animation.setBeginTime(CACurrentMediaTime() + delay);
        animation.setFillMode(objc2_quartz_core::kCAFillModeBackwards);
        if let Some(curve) = super::card_layer::timing_curve([0.32, 0.72, 0.0, 1.0]) {
            let sel = Sel::register("setTimingFunction:");
            let _: () = msg_send![&animation, performSelector: sel, withObject: &*curve];
        }
        mask.addAnimation_forKey(&animation, Some(&NSString::from_str("outline")));
        layer.setMask(Some(&mask));
        objc2_quartz_core::CATransaction::commit();
        CGPathRelease(from);
        CGPathRelease(to);
    }
}

/// Drop the fold/unfold clip once the window has its final size.
pub fn clear_window_outline(window: &winit::window::Window) {
    if let Some(layer) = frame_view(window).and_then(|view| unsafe { view.layer() }) {
        objc2_quartz_core::CATransaction::begin();
        objc2_quartz_core::CATransaction::setDisableActions(true);
        unsafe { layer.setMask(None) };
        objc2_quartz_core::CATransaction::commit();
    }
}
