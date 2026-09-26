//! Render the native detail card offscreen to PNG for eyeball verification.
//!
//! The card is a CALayer tree, so `renderInContext:` can draw it without a
//! window — the same pixels the compositor would show, minus the shadow.
//!
//! Usage:
//!   cargo run -p dango-widget --example card_preview -- /tmp/card_preview
//!
//! Writes <dir>/{healthy,low,error,blank}.png at 2x.

use dango_widget::platform::macos::card::{CardView, CARD_WIDTH};
use dango_widget::{Bucket, PlanQuota, ProxyStatus, Settings};
use objc2_quartz_core::CALayer;
use std::ffi::{c_void, CString};

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFURLCreateWithFileSystemPath(
        alloc: *const c_void,
        path: *const c_void,
        style: isize,
        is_dir: bool,
    ) -> *mut c_void;
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const u8, encoding: u32) -> *mut c_void;
    fn CFRelease(object: *const c_void);
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *const c_void,
        bitmap_info: u32,
    ) -> *mut c_void;
    fn CGBitmapContextCreateImage(context: *const c_void) -> *mut c_void;
    fn CGContextTranslateCTM(c: *mut c_void, tx: f64, ty: f64);
    fn CGContextScaleCTM(c: *mut c_void, sx: f64, sy: f64);
    fn CGContextSetGrayFillColor(c: *mut c_void, gray: f64, alpha: f64);
    fn CGContextFillRect(c: *mut c_void, rect: CGRectFfi);
    fn CGContextDrawImage(c: *mut c_void, rect: CGRectFfi, image: *const c_void);
}

#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    fn CGImageDestinationCreateWithURL(
        url: *const c_void,
        uti: *const c_void,
        count: usize,
        options: *const c_void,
    ) -> *mut c_void;
    fn CGImageDestinationAddImage(dest: *mut c_void, image: *const c_void, props: *const c_void);
    fn CGImageDestinationFinalize(dest: *mut c_void) -> bool;
}

/// Plain-`CGRect`-layout struct passed BY VALUE — the C signatures of
/// `CGContextDrawImage`/`FillRect`/`CGPathAdd*InRect` take `CGRect`, not a
/// pointer. Declaring a pointer lands pointer bits where the callee reads
/// the rect and the call silently no-ops (or draws offscreen).
#[repr(C)]
#[derive(Clone, Copy)]
struct CGRectFfi {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_millis() as u64)
        .unwrap_or(0)
}

fn bucket(label: &str, pct: f64, detail: &str, resets_in_ms: u64) -> Bucket {
    Bucket {
        label: label.into(),
        remaining_percent: Some(pct),
        detail: Some(detail.into()),
        resets_at: Some(now_ms() + resets_in_ms),
        pool: None,
        window: None,
    }
}

fn plan(
    id: &str,
    name: &str,
    pct: f64,
    note: &str,
    buckets: Vec<Bucket>,
    proxy: bool,
) -> PlanQuota {
    PlanQuota {
        id: id.into(),
        name: name.into(),
        ok: true,
        error: None,
        remaining_percent: Some(pct),
        buckets,
        note: Some(note.into()),
        headline_label: None,
        resets_at: None,
        proxy: proxy.then(|| ProxyStatus {
            name: "本地反代".into(),
            url: "http://127.0.0.1:8050/healthz".into(),
            ok: true,
            latency_ms: Some(3),
            detail: None,
            summary: Some("127.0.0.1:8050".into()),
            upstream_status: None,
            requests_today: Some(42),
            requests_total: None,
            in_flight: None,
            last_upstream_at: None,
            token_available: None,
            accounts_available: None,
            accounts_cooling: None,
        }),
    }
}

fn bitmap(w: usize, h: usize, space: *const c_void) -> *mut c_void {
    unsafe {
        let ctx = CGBitmapContextCreate(std::ptr::null_mut(), w, h, 8, w * 4, space, 1);
        assert!(!ctx.is_null(), "bitmap context");
        ctx
    }
}

/// Render `layer`'s tree into a PNG at 2x, on a mid-gray canvas.
///
/// Two contexts: the layer renders y-up into `a` (whole tree comes out
/// vertically flipped as a unit); `b` draws that image flipped over, which
/// flips pixels rather than glyphs — flipping the text context directly
/// mirrors every CATextLayer glyph instead.
fn render_png(layer: &CALayer, width: f64, height: f64, path: &str) {
    let scale = 2.0;
    let w = (width * scale) as usize;
    let h = (height * scale) as usize;
    unsafe {
        let space = CGColorSpaceCreateDeviceRGB();
        let a = bitmap(w, h, space);
        CGContextScaleCTM(a, scale, scale);
        let _: () = objc2::msg_send![layer, renderInContext: a as *const c_void];
        let flipped = CGBitmapContextCreateImage(a);
        assert!(!flipped.is_null(), "image a");
        write_png(flipped, &format!("{path}.a_raw.png"));

        let b = bitmap(w, h, space);
        let full = CGRectFfi {
            x: 0.0,
            y: 0.0,
            w: w as f64,
            h: h as f64,
        };
        CGContextSetGrayFillColor(b, 0.35, 1.0);
        CGContextFillRect(b, full);
        // A is the card pixel-mirrored as a unit (geometryFlipped put every
        // element at mirrored Y with mirrored glyphs); flipping the IMAGE
        // restores the true card.
        CGContextTranslateCTM(b, 0.0, h as f64);
        CGContextScaleCTM(b, 1.0, -1.0);
        CGContextDrawImage(b, full, flipped);
        let image = CGBitmapContextCreateImage(b);
        assert!(!image.is_null(), "image b");
        write_png(image, path);
        for object in [image, flipped, b, a, space] {
            CFRelease(object);
        }
    }
    println!("wrote {path}");
}

fn write_png(image: *const c_void, path: &str) {
    unsafe {
        let c_path = CString::new(path).unwrap();
        let cf_path = CFStringCreateWithCString(std::ptr::null(), c_path.as_ptr() as _, 0x08000100);
        let url = CFURLCreateWithFileSystemPath(std::ptr::null(), cf_path, 0, false);
        let uti = CFStringCreateWithCString(std::ptr::null(), b"public.png\0".as_ptr(), 0x08000100);
        let dest = CGImageDestinationCreateWithURL(url, uti, 1, std::ptr::null());
        assert!(!dest.is_null(), "image destination");
        CGImageDestinationAddImage(dest, image, std::ptr::null());
        assert!(CGImageDestinationFinalize(dest), "finalize {path}");
        for object in [dest, url, cf_path, uti] {
            CFRelease(object);
        }
    }
}

/// Offscreen repro for the "dark disc" under the error ball: one BallView
/// (blob + emotion 34 + Error ring) composited over a light background —
/// if the disc is ours it shows up against light gray, if it was background
/// bleed the ball floats clean.
fn ball_disc_probe(out_dir: &str) {
    use dango_widget::platform::macos::ball_view::BallView;
    use dango_widget::theme::RingStyle;

    let root = CALayer::new();
    root.setOpaque(false);
    root.setGeometryFlipped(true);
    root.setFrame(objc2_foundation::CGRect::new(
        objc2_foundation::CGPoint::new(0.0, 0.0),
        objc2_foundation::CGSize::new(140.0, 140.0),
    ));

    let mut view = BallView::new(&root, (40.0, 40.0), 62.0, 44.0, 2.0);
    view.set_ring(&RingStyle::Error {
        color: "#D33F35".into(),
    });
    let mut ball = grok_ball::Ball::new(grok_ball::BallOptions {
        shape: grok_ball::ShapeKind::Blob,
        color: Some("#D8A7A0".into()),
        eye_color: Some("#F5F2ED".into()),
        emotion: Some("34".into()),
        lite: true,
        eye_scale: 1.15,
        ..Default::default()
    });
    ball.render_static(now_ms() as f64);
    view.draw(ball.frame());
    // render_png composites over mid-gray, which stands in for pale
    // wallpaper: a real dark disc can't hide on it.
    render_png(&root, 140.0, 140.0, &format!("{out_dir}/ball_disc.png"));
}

fn main() {
    let out_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/card_preview".into());
    std::fs::create_dir_all(&out_dir).expect("mkdir");
    let settings = Settings::default();
    let now = now_ms();
    ball_disc_probe(&out_dir);

    let mut cases: Vec<(PlanQuota, bool, &str)> = vec![
        (
            plan(
                "antigravity",
                "Antigravity",
                69.8,
                "2 个账号 · Gemini 桥",
                vec![
                    bucket("5 小时", 69.8, "612 / 877", 2 * 3_600_000 + 14 * 60_000),
                    bucket("周额度", 88.2, "8.4M / 9.6M", 4 * 86_400_000),
                ],
                true,
            ),
            true,
            "healthy_pinned",
        ),
        (
            plan(
                "cursor",
                "Cursor",
                11.3,
                "Cursor CLI",
                vec![bucket("周额度", 11.3, "1.2K / 10.9K", 61 * 60_000)],
                false,
            ),
            false,
            "low",
        ),
        {
            let mut p = plan("nova", "Nova Pro", 0.0, "", vec![], true);
            p.ok = false;
            p.remaining_percent = None;
            p.error = Some("http 401".into());
            (p, false, "error")
        },
        (
            plan(
                "devin",
                "Devin",
                100.0,
                "Devin Desktop",
                vec![bucket("并发", 100.0, "0 / 3", 0)],
                false,
            ),
            false,
            "full",
        ),
        // Light glass: the same plan re-rendered on the light palette so the
        // ink/hairline/track switch can be eye-balled in one shot.
        (
            plan(
                "antigravity",
                "Antigravity",
                69.8,
                "2 个账号 · Gemini 桥",
                vec![
                    bucket("5 小时", 69.8, "612 / 877", 2 * 3_600_000 + 14 * 60_000),
                    bucket("周额度", 88.2, "8.4M / 9.6M", 4 * 86_400_000),
                ],
                true,
            ),
            true,
            "light",
        ),
    ];

    for (plan, pinned, name) in cases.drain(..) {
        let root = CALayer::new();
        root.setOpaque(false);
        // Match the live window: winit's layer-backed view is y-down, which is
        // what the card tree is laid out against. Without this the layer
        // renders y-up (positions mirrored) and any CTM flip then mirrors
        // every CATextLayer glyph instead.
        root.setGeometryFlipped(true);
        let mut card = CardView::new(&root, 2.0, name != "light");
        let height = card.show_plan(&plan, &settings, now / 1000, now, pinned);
        root.setFrame(objc2_foundation::CGRect::new(
            objc2_foundation::CGPoint::new(0.0, 0.0),
            objc2_foundation::CGSize::new(CARD_WIDTH, height),
        ));
        render_png(&root, CARD_WIDTH, height, &format!("{out_dir}/{name}.png"));
    }
}
