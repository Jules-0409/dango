//! CVDisplayLink pacing that can be started and stopped at runtime.
//!
//! The callback only forwards every Nth vsync so we can ask for 30 or 60 fps
//! without recreating the link. Ticks are delivered through an arbitrary
//! callback (typically a winit event-loop proxy), which keeps this module free
//! of any dependency on the app's event type.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

pub type CVDisplayLinkRef = *mut c_void;
pub type CVReturn = i32;
pub type CVOptionFlags = u64;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CVTime {
    pub time_value: i64,
    pub time_scale: i32,
    pub flags: i32,
}

type CVDisplayLinkOutputCallback = Option<
    unsafe extern "C" fn(
        display_link: CVDisplayLinkRef,
        now: *const c_void,
        output_time: *const c_void,
        flags_in: CVOptionFlags,
        flags_out: *mut CVOptionFlags,
        context: *mut c_void,
    ) -> CVReturn,
>;

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVDisplayLinkCreateWithCGDisplay(
        display_id: u32,
        display_link_out: *mut CVDisplayLinkRef,
    ) -> CVReturn;
    fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(display_link: CVDisplayLinkRef) -> CVTime;
    fn CVDisplayLinkSetOutputCallback(
        display_link: CVDisplayLinkRef,
        callback: CVDisplayLinkOutputCallback,
        user_info: *mut c_void,
    ) -> CVReturn;
    fn CVDisplayLinkStart(display_link: CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkStop(display_link: CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkRelease(display_link: CVDisplayLinkRef);
}

struct LinkContext {
    stride: AtomicU64,
    callback_count: AtomicU64,
    on_tick: Box<dyn Fn() + Send + Sync + 'static>,
}

pub struct DisplayLink {
    link: CVDisplayLinkRef,
    context: *mut LinkContext,
    started: bool,
    refresh_hz: f64,
}

// The link and its context are only touched from the main thread, but the type
// must be storable in the app struct that winit owns.
unsafe impl Send for DisplayLink {}

impl DisplayLink {
    /// Create a link that calls `on_tick` at most `target_fps` times per second.
    pub fn new<F>(target_fps: u32, on_tick: F) -> Result<Self, String>
    where
        F: Fn() + Send + Sync + 'static,
    {
        let mut link = std::ptr::null_mut();
        let result = unsafe { CVDisplayLinkCreateWithCGDisplay(0, &mut link) };
        if result != 0 || link.is_null() {
            return Err(format!(
                "CVDisplayLinkCreateWithCGDisplay returned {result}"
            ));
        }
        let refresh = unsafe { CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link) };
        let refresh_hz = if refresh.time_value > 0 && refresh.time_scale > 0 {
            refresh.time_scale as f64 / refresh.time_value as f64
        } else {
            60.0
        };
        let stride = stride_for(refresh_hz, target_fps);
        let context = Box::into_raw(Box::new(LinkContext {
            stride: AtomicU64::new(stride),
            callback_count: AtomicU64::new(0),
            on_tick: Box::new(on_tick),
        }));
        let result = unsafe {
            CVDisplayLinkSetOutputCallback(link, Some(display_link_callback), context.cast())
        };
        if result != 0 {
            unsafe {
                drop(Box::from_raw(context));
                CVDisplayLinkRelease(link);
            }
            return Err(format!("CVDisplayLinkSetOutputCallback returned {result}"));
        }
        Ok(Self {
            link,
            context,
            started: false,
            refresh_hz,
        })
    }

    pub fn start(&mut self) -> Result<(), String> {
        if self.started {
            return Ok(());
        }
        let result = unsafe { CVDisplayLinkStart(self.link) };
        if result != 0 {
            return Err(format!("CVDisplayLinkStart returned {result}"));
        }
        self.started = true;
        Ok(())
    }

    pub fn stop(&mut self) {
        if self.started {
            unsafe { CVDisplayLinkStop(self.link) };
            self.started = false;
        }
    }

    /// Change the tick rate without recreating the link.
    pub fn set_target_fps(&mut self, target_fps: u32) {
        let stride = stride_for(self.refresh_hz, target_fps);
        unsafe { &*self.context }
            .stride
            .store(stride, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        self.started
    }

    pub fn refresh_hz(&self) -> f64 {
        self.refresh_hz
    }
}

fn stride_for(refresh_hz: f64, target_fps: u32) -> u64 {
    (refresh_hz / target_fps.max(1) as f64).round().max(1.0) as u64
}

impl Drop for DisplayLink {
    fn drop(&mut self) {
        unsafe {
            if self.started {
                CVDisplayLinkStop(self.link);
            }
            CVDisplayLinkRelease(self.link);
            if !self.context.is_null() {
                drop(Box::from_raw(self.context));
            }
        }
    }
}

unsafe extern "C" fn display_link_callback(
    _display_link: CVDisplayLinkRef,
    _now: *const c_void,
    _output_time: *const c_void,
    _flags_in: CVOptionFlags,
    _flags_out: *mut CVOptionFlags,
    context: *mut c_void,
) -> CVReturn {
    let context = &*(context as *const LinkContext);
    let count = context.callback_count.fetch_add(1, Ordering::Relaxed) + 1;
    if count.is_multiple_of(context.stride.load(Ordering::Relaxed)) {
        (context.on_tick)();
    }
    0
}
