// Grok Ball standalone engine | MIT License

use crate::types::Point;
use std::fmt::Write;

pub const TAU: f64 = std::f64::consts::TAU;
pub const PI: f64 = std::f64::consts::PI;

#[inline]
pub fn clamp(v: f64, a: f64, b: f64) -> f64 {
    if v < a {
        a
    } else if v > b {
        b
    } else {
        v
    }
}

#[inline]
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

#[inline]
pub fn r2(v: f64) -> f64 {
    let r = (v * 100.0).round() / 100.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

pub fn ease_in_out_cubic(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

pub fn hex_to_rgb(hex: &str) -> (u8, u8, u8) {
    let mut h = hex.trim_start_matches('#');
    let mut expanded = String::new();
    if h.len() == 3 {
        for c in h.chars() {
            expanded.push(c);
            expanded.push(c);
        }
        h = &expanded;
    }
    let n = u32::from_str_radix(h, 16).unwrap_or(0);
    (
        ((n >> 16) & 255) as u8,
        ((n >> 8) & 255) as u8,
        (n & 255) as u8,
    )
}

pub fn rgb_to_hex(r: u8, g: u8, b: u8) -> String {
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

pub fn shade(hex: &str, amt: f64) -> String {
    let (r, g, b) = hex_to_rgb(hex);
    let target = if amt < 0.0 { 0.0 } else { 255.0 };
    let a = amt.abs();
    let r_out = clamp((r as f64 + (target - r as f64) * a).round(), 0.0, 255.0) as u8;
    let g_out = clamp((g as f64 + (target - g as f64) * a).round(), 0.0, 255.0) as u8;
    let b_out = clamp((b as f64 + (target - b as f64) * a).round(), 0.0, 255.0) as u8;
    rgb_to_hex(r_out, g_out, b_out)
}

pub fn lerp_color(a: &str, b: &str, t: f64) -> String {
    if a == b {
        return b.to_string();
    }
    let (ar, ag, ab) = hex_to_rgb(a);
    let (br, bg, bb) = hex_to_rgb(b);
    let r = clamp(lerp(ar as f64, br as f64, t).round(), 0.0, 255.0) as u8;
    let g = clamp(lerp(ag as f64, bg as f64, t).round(), 0.0, 255.0) as u8;
    let b = clamp(lerp(ab as f64, bb as f64, t).round(), 0.0, 255.0) as u8;
    rgb_to_hex(r, g, b)
}

pub fn centroid(ring: &[Point]) -> Point {
    if ring.is_empty() {
        return Point::new(0.0, 0.0);
    }
    let mut x = 0.0;
    let mut y = 0.0;
    for p in ring {
        x += p.x;
        y += p.y;
    }
    let len = ring.len() as f64;
    Point::new(x / len, y / len)
}

pub fn lerp_ring(a: &[Point], b: &[Point], t: f64) -> Vec<Point> {
    let mut out = Vec::with_capacity(a.len());
    for i in 0..a.len() {
        out.push(Point::new(
            a[i].x + (b[i].x - a[i].x) * t,
            a[i].y + (b[i].y - a[i].y) * t,
        ));
    }
    out
}

pub fn ring_path_str(ring: &[Point]) -> String {
    let mut s = String::with_capacity(ring.len() * 16 + 2);
    s.push('M');
    for (i, p) in ring.iter().enumerate() {
        if i > 0 {
            s.push('L');
        }
        write!(s, "{:.2} {:.2}", p.x, p.y).unwrap();
    }
    s.push('Z');
    s
}

#[derive(Clone, Debug)]
pub struct SilProfile {
    pub sil_min_y: f64,
    pub sil_max_y: f64,
    pub sil_step: f64,
    pub rows: Vec<(f64, f64)>,
}

impl SilProfile {
    pub fn from_ring(ring: &[Point], head_c: f64) -> Self {
        let mut sil_min_y = 1e9;
        let mut sil_max_y = -1e9;
        for p in ring {
            if p.y < sil_min_y {
                sil_min_y = p.y;
            }
            if p.y > sil_max_y {
                sil_max_y = p.y;
            }
        }
        let sil_step = 2.0;
        let rows_count = ((sil_max_y - sil_min_y) / sil_step).ceil() as usize + 1;
        let mut rows = Vec::with_capacity(rows_count);

        for r in 0..rows_count {
            let y = sil_min_y + (r as f64) * sil_step;
            let mut lo = 1e9;
            let mut hi = -1e9;
            let n = ring.len();
            for e in 0..n {
                let a = ring[e];
                let b = ring[(e + 1) % n];
                let y0 = a.y;
                let y1 = b.y;
                if (y0 <= y && y1 >= y) || (y1 <= y && y0 >= y) {
                    let t = if (y1 - y0).abs() < 1e-9 {
                        0.0
                    } else {
                        (y - y0) / (y1 - y0)
                    };
                    let x = a.x + (b.x - a.x) * t;
                    if x < lo {
                        lo = x;
                    }
                    if x > hi {
                        hi = x;
                    }
                }
            }
            if lo > hi {
                lo = head_c - 4.0;
                hi = head_c + 4.0;
            }
            rows.push((lo, hi));
        }

        Self {
            sil_min_y,
            sil_max_y,
            sil_step,
            rows,
        }
    }

    pub fn sil_at(&self, y: f64) -> (f64, f64) {
        let clamped_y = clamp(y, self.sil_min_y, self.sil_max_y);
        let r = ((clamped_y - self.sil_min_y) / self.sil_step).round() as usize;
        let idx = r.min(self.rows.len() - 1);
        self.rows[idx]
    }
}
