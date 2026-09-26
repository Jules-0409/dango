//! Native, closable WKWebView window for the settings UI.

use objc2::rc::Retained;
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSApplication,
    NSAutoresizingMaskOptions, NSColor, NSPasteboard, NSPasteboardTypeString, NSView,
};
use objc2_foundation::{MainThreadMarker, NSString, NSURLRequest, NSURL};
use objc2_web_kit::{WKWebView, WKWebViewConfiguration};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::dpi::LogicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes, WindowId};

const SETTINGS_URL: &str = "http://127.0.0.1:8049/ui/settings.html";

pub struct SettingsWindow {
    window: Window,
    web_view: Retained<WKWebView>,
}

impl SettingsWindow {
    pub fn new(event_loop: &ActiveEventLoop, tab: &str) -> Self {
        let window = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title("Dango 设置")
                    .with_inner_size(LogicalSize::new(780.0, 580.0))
                    .with_min_inner_size(LogicalSize::new(560.0, 420.0))
                    .with_resizable(true)
                    .with_decorations(true)
                    .with_visible(false),
            )
            .expect("create settings window");

        let handle = window.window_handle().expect("settings window handle");
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            panic!("settings window is not an AppKit window");
        };
        let content_view: &NSView = unsafe { &*appkit.ns_view.as_ptr().cast() };
        let ns_window = content_view.window().expect("settings NSWindow");
        let marker = MainThreadMarker::new().expect("settings window on main thread");
        ns_window.setOpaque(false);
        let background =
            unsafe { NSColor::colorWithSRGBRed_green_blue_alpha(0.10, 0.08, 0.07, 1.0) };
        ns_window.setBackgroundColor(Some(&background));
        if let Some(dark) = unsafe { NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua) } {
            unsafe { ns_window.setAppearance(Some(&dark)) };
        }

        let bounds = content_view.bounds();
        let configuration = unsafe { WKWebViewConfiguration::new() };
        let web_view = unsafe {
            WKWebView::initWithFrame_configuration(marker.alloc(), bounds, &configuration)
        };
        unsafe {
            web_view.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable
                    | NSAutoresizingMaskOptions::NSViewHeightSizable,
            );
            content_view.addSubview(&web_view);
        }
        Self { window, web_view }.show(tab)
    }

    fn show(self, tab: &str) -> Self {
        let url = unsafe {
            NSURL::URLWithString(&NSString::from_str(&format!(
                "{SETTINGS_URL}?tab={0}#{0}",
                valid_tab(tab),
            )))
        }
        .expect("valid settings URL");
        let request = unsafe { NSURLRequest::requestWithURL(&url) };
        unsafe { self.web_view.loadRequest(&request) };
        self.activate();
        self
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn focus(&self, tab: &str) {
        let encoded = serde_json::to_string(valid_tab(tab)).expect("serialize tab id");
        let script = NSString::from_str(&format!("location.hash = {encoded};"));
        unsafe {
            self.web_view
                .evaluateJavaScript_completionHandler(&script, None);
        }
        self.activate();
    }

    fn activate(&self) {
        let marker = MainThreadMarker::new().expect("settings window on main thread");
        let app = NSApplication::sharedApplication(marker);
        unsafe {
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            self.window.set_visible(true);
            if let Ok(handle) = self.window.window_handle() {
                if let RawWindowHandle::AppKit(appkit) = handle.as_raw() {
                    let view: &NSView = &*appkit.ns_view.as_ptr().cast();
                    if let Some(ns_window) = view.window() {
                        ns_window.makeKeyAndOrderFront(None);
                    }
                }
            }
        }
    }

    pub fn close(&mut self) {
        unsafe { self.web_view.stopLoading() };
        unsafe { self.web_view.removeFromSuperview() };
        self.window.set_visible(false);
    }
}

fn valid_tab(tab: &str) -> &str {
    // 页签 id 是小写 slug；不认识的 id 交给前端回落到 balls。
    if !tab.is_empty()
        && tab
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        tab
    } else {
        "balls"
    }
}

pub fn set_clipboard(text: &str) -> Result<(), String> {
    let _marker = MainThreadMarker::new().ok_or("clipboard write must run on the main thread")?;
    let pasteboard = unsafe { NSPasteboard::generalPasteboard() };
    unsafe { pasteboard.clearContents() };
    let value = NSString::from_str(text);
    let string_type = unsafe { NSPasteboardTypeString };
    if unsafe { pasteboard.setString_forType(&value, string_type) } {
        Ok(())
    } else {
        Err("NSPasteboard rejected the text".into())
    }
}

/// Install an Edit menu with standard responder-chain selectors for WKWebView.
pub fn install_edit_menu() {
    use objc2::sel;
    use objc2_app_kit::{NSMenu, NSMenuItem};
    use objc2_foundation::NSString;

    let marker = MainThreadMarker::new().expect("main menu on main thread");
    let app = NSApplication::sharedApplication(marker);
    let main_menu = NSMenu::new(marker);
    let edit_item = NSMenuItem::new(marker);
    unsafe { edit_item.setTitle(&NSString::from_str("编辑")) };
    let edit_menu = NSMenu::new(marker);

    for (label, selector, key) in [
        ("撤销", sel!(undo:), "z"),
        ("剪切", sel!(cut:), "x"),
        ("拷贝", sel!(copy:), "c"),
        ("粘贴", sel!(paste:), "v"),
        ("全选", sel!(selectAll:), "a"),
    ] {
        let item = unsafe {
            edit_menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str(label),
                Some(selector),
                &NSString::from_str(key),
            )
        };
        item.setKeyEquivalentModifierMask(
            objc2_app_kit::NSEventModifierFlags::NSEventModifierFlagCommand,
        );
    }
    let redo = unsafe {
        edit_menu.insertItemWithTitle_action_keyEquivalent_atIndex(
            &NSString::from_str("重做"),
            Some(sel!(redo:)),
            &NSString::from_str("z"),
            1,
        )
    };
    redo.setKeyEquivalentModifierMask(
        objc2_app_kit::NSEventModifierFlags::NSEventModifierFlagCommand
            | objc2_app_kit::NSEventModifierFlags::NSEventModifierFlagShift,
    );
    edit_item.setSubmenu(Some(&edit_menu));
    main_menu.addItem(&edit_item);
    app.setMainMenu(Some(&main_menu));
}
