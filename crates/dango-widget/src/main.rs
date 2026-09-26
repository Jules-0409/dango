//! dango native widget.
//!
//! One process runs: a vertical capsule window with one animated ball per plan,
//! a detail card that slides out beside it, a menu-bar status item, the data loop
//! (quotas 60 s / proxies 10 s) and the localhost control API
//! on 127.0.0.1:8049.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Winit/AppKit callbacks are `extern "C"`: a panic there aborts with only
    // "panic in a function that cannot unwind". Log where it really came from.
    std::panic::set_hook(Box::new(|info| {
        eprintln!(
            "[panic] {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
    }));

    #[cfg(target_os = "macos")]
    {
        dango_widget::app::run(dango_widget::app::Config::from_args());
    }

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("Dango currently targets macOS only");
        std::process::exit(2);
    }
}
