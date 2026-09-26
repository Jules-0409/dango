//! Menu-bar status item: 设置… / 立即刷新 / 性能模式 / 退出.
//!
//! Menu actions are dispatched through an Objective-C target object created with
//! `declare_class!`, so the menu survives independently of the widget windows.
//! Each performance-mode item carries its mode in the item *tag*, which keeps the
//! radio buttons stateless and avoids any downcast of `representedObject`.

use objc2::rc::Retained;
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSApplication, NSBezierPath, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSImage,
    NSMenu, NSMenuItem, NSStatusBar,
};
use objc2_foundation::NSSize;
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::app_model::PerfModeAction;
use crate::data::{DataEvent, DataState};

const ACTION_SETTINGS: isize = 1;
const ACTION_REFRESH: isize = 2;
const ACTION_QUIT: isize = 4;
/// Perf-mode submenu tags start at `PERF_TAG_BASE + mode`, kept far away from
/// the action tags: overloading `ACTION_PERF + 1` collided with `ACTION_QUIT`
/// and made the "balanced" menu item quit the app.
const PERF_TAG_BASE: isize = 100;

/// Performance mode, encoded in a menu item's tag (offset from `ACTION_PERF`).
const MODE_SMOOTH: isize = 0;
const MODE_BALANCED: isize = 1;
const MODE_SAVER: isize = 2;

/// Ivars for `TrayTarget`. Public because `declare_class!` exposes it in the
/// generated class type, but it carries nothing a caller can misuse.
pub struct TrayIvars {
    state: std::sync::Arc<DataState>,
    events: tokio::sync::mpsc::UnboundedSender<DataEvent>,
    runtime: tokio::runtime::Handle,
    perf: std::sync::Arc<std::sync::Mutex<PerfModeAction>>,
    event_loop_proxy: EventLoopProxy<UserEvent>,
}

declare_class!(
    pub struct TrayTarget;

    unsafe impl ClassType for TrayTarget {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "DangoTrayTarget";
    }

    impl DeclaredClass for TrayTarget {
        type Ivars = TrayIvars;
    }

    unsafe impl TrayTarget {
        #[method(handleMenuAction:)]
        fn handle_menu_action(&self, sender: &NSMenuItem) {
            let tag = unsafe { sender.tag() };
            let ivars = self.ivars();
            match tag {
                ACTION_SETTINGS => {
                    let _ = ivars
                        .event_loop_proxy
                        .send_event(UserEvent::OpenSettings("balls"));
                }
                ACTION_REFRESH => {
                    let state = std::sync::Arc::clone(&ivars.state);
                    let events = ivars.events.clone();
                    ivars.runtime.spawn(async move {
                        crate::data::refresh_now(&state, &events).await;
                    });
                }
                tag if (PERF_TAG_BASE..=PERF_TAG_BASE + MODE_SAVER).contains(&tag) => {
                    // The mode is encoded as `PERF_TAG_BASE + mode`.
                    let mode = mode_from_tag(tag - PERF_TAG_BASE);
                    let state = std::sync::Arc::clone(&ivars.state);
                    let events = ivars.events.clone();
                    let runtime = ivars.runtime.clone();
                    runtime.spawn(async move {
                        let mut settings = state.settings().await;
                        settings.perf_mode = Some(mode.into());
                        if let Ok(saved) = state.save_settings(settings).await {
                            let _ = events.send(DataEvent::Settings(saved));
                        }
                    });
                }
                ACTION_QUIT => {
                    let marker = MainThreadMarker::new().expect("main thread");
                    let app = NSApplication::sharedApplication(marker);
                    app.stop(None);
                    std::process::exit(0);
                }
                _ => {}
            }
        }
    }

    unsafe impl NSObjectProtocol for TrayTarget {}
);

impl TrayTarget {
    fn new(
        state: std::sync::Arc<DataState>,
        events: tokio::sync::mpsc::UnboundedSender<DataEvent>,
        runtime: tokio::runtime::Handle,
        perf: std::sync::Arc<std::sync::Mutex<PerfModeAction>>,
        event_loop_proxy: EventLoopProxy<UserEvent>,
    ) -> Retained<Self> {
        let mtm = MainThreadMarker::new().expect("tray must be built on the main thread");
        let this = mtm.alloc().set_ivars(TrayIvars {
            state,
            events,
            runtime,
            perf,
            event_loop_proxy,
        });
        unsafe { msg_send_id![super(this), init] }
    }
}

pub struct Tray {
    _item: Retained<objc2_app_kit::NSStatusItem>,
    _target: Retained<TrayTarget>,
    perf_items: Vec<(PerfModeAction, Retained<NSMenuItem>)>,
}

impl Tray {
    pub fn new(
        state: std::sync::Arc<DataState>,
        events: tokio::sync::mpsc::UnboundedSender<DataEvent>,
        perf: std::sync::Arc<std::sync::Mutex<PerfModeAction>>,
        runtime: tokio::runtime::Handle,
        event_loop_proxy: EventLoopProxy<UserEvent>,
    ) -> Self {
        let marker = MainThreadMarker::new().expect("tray must be built on the main thread");
        let target = TrayTarget::new(state, events, runtime, perf, event_loop_proxy);

        let menu = NSMenu::new(marker);
        unsafe { menu.setAutoenablesItems(false) };

        let settings_item = unsafe {
            menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str("设置…"),
                Some(objc2::sel!(handleMenuAction:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            settings_item.setTarget(Some(&target));
            settings_item.setTag(ACTION_SETTINGS);
        }

        let refresh_item = unsafe {
            menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str("立即刷新"),
                Some(objc2::sel!(handleMenuAction:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            refresh_item.setTarget(Some(&target));
            refresh_item.setTag(ACTION_REFRESH);
        }

        let perf_root = NSMenuItem::new(marker);
        unsafe { perf_root.setTitle(&NSString::from_str("性能模式")) };
        let perf_menu = NSMenu::new(marker);
        let mut perf_items = Vec::new();
        for (label, mode) in [
            ("流畅 (smooth)", PerfModeAction::Smooth),
            ("均衡 (balanced)", PerfModeAction::Balanced),
            ("省电 (saver)", PerfModeAction::Saver),
        ] {
            let item = unsafe {
                perf_menu.addItemWithTitle_action_keyEquivalent(
                    &NSString::from_str(label),
                    Some(objc2::sel!(handleMenuAction:)),
                    &NSString::from_str(""),
                )
            };
            unsafe {
                item.setTarget(Some(&target));
                item.setTag(PERF_TAG_BASE + mode_to_isize(mode));
            }
            perf_items.push((mode, item));
        }
        perf_root.setSubmenu(Some(&perf_menu));
        menu.addItem(&perf_root);

        let quit_item = unsafe {
            menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str("退出"),
                Some(objc2::sel!(handleMenuAction:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            quit_item.setTarget(Some(&target));
            quit_item.setTag(ACTION_QUIT);
        }

        let status_bar = unsafe { NSStatusBar::systemStatusBar() };
        let item = unsafe { status_bar.statusItemWithLength(-1.0) };
        unsafe {
            item.setMenu(Some(&menu));
            if let Some(button) = item.button(marker) {
                button.setImage(Some(&tray_icon()));
            }
        }

        let tray = Self {
            _item: item,
            _target: target,
            perf_items,
        };
        // Show the persisted mode as the checked radio item right away.
        let current = tray
            ._target
            .ivars()
            .perf
            .lock()
            .map(|mode| *mode)
            .unwrap_or_default();
        tray.sync_perf(current);
        tray
    }

    /// Reflect the active performance mode with radio-style check marks.
    pub fn sync_perf(&self, mode: PerfModeAction) {
        for (item_mode, item) in &self.perf_items {
            let state = if *item_mode == mode {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            };
            unsafe { item.setState(state) };
        }
    }
}

/// Draw the menu-bar icon: a small ball with two eyes, matching the capsule.
///
/// Template image (black + alpha) so macOS tints it for light/dark menu bar.
fn tray_icon() -> Retained<NSImage> {
    ball_icon(NSSize::new(18.0, 18.0), true)
}

/// Shared ball icon renderer. `template` = alpha holes for the menu bar;
/// `!template` = the app-icon tile for the Dock.
fn ball_icon(size: NSSize, template: bool) -> Retained<NSImage> {
    unsafe {
        let image = NSImage::initWithSize(NSImage::alloc(), size);
        #[allow(deprecated)]
        image.lockFocus();

        let w = size.width;
        let h = size.height;
        let body_d = w * 0.82;
        let body_x = (w - body_d) / 2.0;
        let body_y = (h - body_d) / 2.0;

        // Eyes: two vertical ovals, side by side, slightly above centre.
        let eye_w = body_d * 0.14;
        let eye_h = body_d * 0.28;
        let eye_gap = body_d * 0.10;
        let eye_y = body_y + body_d * 0.52;
        let left_x = body_x + body_d * 0.5 - eye_gap / 2.0 - eye_w;
        let right_x = body_x + body_d * 0.5 + eye_gap / 2.0;
        let eye = |x: f64| {
            objc2_foundation::NSRect::new(
                objc2_foundation::NSPoint::new(x, eye_y),
                NSSize::new(eye_w, eye_h),
            )
        };

        if template {
            // One even-odd path: the eyes punch real alpha holes out of the
            // disc, so the menu-bar tint reads "ball with eyes" instead of
            // the solid blob a black-on-black draw produced.
            let path = NSBezierPath::bezierPathWithOvalInRect(objc2_foundation::NSRect::new(
                objc2_foundation::NSPoint::new(body_x, body_y),
                NSSize::new(body_d, body_d),
            ));
            path.setWindingRule(objc2_app_kit::NSWindingRule::EvenOdd);
            path.appendBezierPathWithOvalInRect(eye(left_x));
            path.appendBezierPathWithOvalInRect(eye(right_x));
            NSColor::blackColor().setFill();
            path.fill();
        } else {
            // Dock icon: rounded tile + rose ball + a progress-ring arc —
            // the capsule's whole story in one glyph.
            let tile_inset = w * 0.02;
            let tile = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                objc2_foundation::NSRect::new(
                    objc2_foundation::NSPoint::new(tile_inset, tile_inset),
                    NSSize::new(w - tile_inset * 2.0, h - tile_inset * 2.0),
                ),
                w * 0.24,
                w * 0.24,
            );
            NSColor::colorWithSRGBRed_green_blue_alpha(0.11, 0.11, 0.13, 1.0).setFill();
            tile.fill();

            let ring_r = body_d * 0.62;
            let ring = NSBezierPath::bezierPath();
            ring.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
                objc2_foundation::NSPoint::new(w / 2.0, h / 2.0),
                ring_r,
                90.0,
                90.0 - 260.0,
                false,
            );
            ring.setLineWidth(w * 0.045);
            ring.setLineCapStyle(objc2_app_kit::NSLineCapStyle::Round);
            NSColor::colorWithSRGBRed_green_blue_alpha(0.66, 0.78, 0.64, 0.9).setStroke();
            ring.stroke();

            let body = NSBezierPath::bezierPathWithOvalInRect(objc2_foundation::NSRect::new(
                objc2_foundation::NSPoint::new(body_x, body_y),
                NSSize::new(body_d, body_d),
            ));
            NSColor::colorWithSRGBRed_green_blue_alpha(0.847, 0.655, 0.627, 1.0).setFill();
            body.fill();

            NSColor::colorWithSRGBRed_green_blue_alpha(0.961, 0.949, 0.929, 1.0).setFill();
            NSBezierPath::bezierPathWithOvalInRect(eye(left_x)).fill();
            NSBezierPath::bezierPathWithOvalInRect(eye(right_x)).fill();
        }

        #[allow(deprecated)]
        image.unlockFocus();
        image.setTemplate(template);
        image
    }
}

/// Decode the mode from a menu item tag minus `ACTION_PERF`.
fn mode_from_tag(value: isize) -> PerfModeAction {
    match value {
        MODE_SMOOTH => PerfModeAction::Smooth,
        MODE_SAVER => PerfModeAction::Saver,
        _ => PerfModeAction::Balanced,
    }
}

/// Encode a mode for a menu item tag.
fn mode_to_isize(mode: PerfModeAction) -> isize {
    match mode {
        PerfModeAction::Smooth => MODE_SMOOTH,
        PerfModeAction::Balanced => MODE_BALANCED,
        PerfModeAction::Saver => MODE_SAVER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_mode_round_trips_through_the_menu_tag() {
        for mode in [
            PerfModeAction::Smooth,
            PerfModeAction::Balanced,
            PerfModeAction::Saver,
        ] {
            let tag = PERF_TAG_BASE + mode_to_isize(mode);
            assert_eq!(mode_from_tag(tag - PERF_TAG_BASE), mode);
        }
    }

    #[test]
    fn perf_tags_never_collide_with_action_tags() {
        // Regression: `ACTION_PERF + MODE_BALANCED` used to equal `ACTION_QUIT`,
        // so clicking "balanced" quit the app.
        for mode in [
            PerfModeAction::Smooth,
            PerfModeAction::Balanced,
            PerfModeAction::Saver,
        ] {
            let tag = PERF_TAG_BASE + mode_to_isize(mode);
            for action in [ACTION_SETTINGS, ACTION_REFRESH, ACTION_QUIT] {
                assert_ne!(tag, action, "perf tag {tag} collides with action {action}");
            }
        }
    }

    #[test]
    fn an_unknown_tag_falls_back_to_balanced() {
        assert_eq!(mode_from_tag(99), PerfModeAction::Balanced);
    }
}
