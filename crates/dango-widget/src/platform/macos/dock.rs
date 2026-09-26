//! Dock 图标：启动时在程序坞里，点它打开设置；设置窗口关掉后图标跟着收走
//! （切成 Accessory，只剩胶囊和菜单栏图标）。再从菜单栏打开设置，图标又回来。
//!
//! winit 不管「点程序坞图标」这个事件（applicationShouldHandleReopen:），
//! 这里在它的 NSApplicationDelegate 类上补一个实现，转成 `UserEvent::OpenSettings`。

use std::sync::OnceLock;

use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;

static PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();

extern "C" fn should_handle_reopen(
    _this: &AnyObject,
    _cmd: Sel,
    _app: &AnyObject,
    _has_visible_windows: Bool,
) -> Bool {
    if let Some(proxy) = PROXY.get() {
        let _ = proxy.send_event(UserEvent::OpenSettings("balls"));
    }
    // 自己处理了，AppKit 不用再做默认动作。
    Bool::NO
}

/// 在 AppKit 启动完成后调用（delegate 这时才在）。
pub fn install_reopen_handler(proxy: EventLoopProxy<UserEvent>) {
    let _ = PROXY.set(proxy);
    let Some(marker) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(marker);
    unsafe {
        let delegate: *mut AnyObject = msg_send![&*app, delegate];
        let Some(delegate) = delegate.as_ref() else {
            eprintln!("[dock] no NSApplication delegate; Dock clicks won't open settings");
            return;
        };
        let class: *const AnyClass = delegate.class();
        let imp: unsafe extern "C" fn() = std::mem::transmute(
            should_handle_reopen as extern "C" fn(&AnyObject, Sel, &AnyObject, Bool) -> Bool,
        );
        // 返回 BOOL，参数 (self, _cmd, NSApplication*, BOOL)
        objc2::ffi::class_replaceMethod(
            class as *mut objc2::ffi::objc_class,
            sel!(applicationShouldHandleReopen:hasVisibleWindows:).as_ptr(),
            Some(imp),
            c"B@:@B".as_ptr(),
        );
    }
}

/// 设置窗口开着时挂在程序坞上，关了就收走。
pub fn show_in_dock(show: bool) {
    let Some(marker) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(marker);
    let policy = if show {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    if unsafe { app.activationPolicy() } == policy {
        return;
    }
    app.setActivationPolicy(policy);
    if show {
        // 从 Accessory 切回来时程序坞会先放一个通用图标，再补一次自己的。
        crate::app::set_dock_icon();
    }
}
