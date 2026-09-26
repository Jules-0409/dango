// Grok Ball standalone engine | MIT License

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self::identity()
    }
}

impl Transform {
    pub const fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    pub fn translate(tx: f32, ty: f32) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }

    pub fn scale(sx: f32, sy: f32) -> Self {
        Self {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            e: 0.0,
            f: 0.0,
        }
    }

    pub fn rotate_rad(rad: f32) -> Self {
        let (sin, cos) = rad.sin_cos();
        Self {
            a: cos,
            b: sin,
            c: -sin,
            d: cos,
            e: 0.0,
            f: 0.0,
        }
    }

    pub fn rotate_deg(deg: f32) -> Self {
        Self::rotate_rad(deg.to_radians())
    }

    pub fn multiply(&self, rhs: &Self) -> Self {
        Self {
            a: self.a * rhs.a + self.c * rhs.b,
            b: self.b * rhs.a + self.d * rhs.b,
            c: self.a * rhs.c + self.c * rhs.d,
            d: self.b * rhs.c + self.d * rhs.d,
            e: self.a * rhs.e + self.c * rhs.f + self.e,
            f: self.b * rhs.e + self.d * rhs.f + self.f,
        }
    }

    pub fn transform_point(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
    pub hex: String,
}

impl Color {
    pub fn from_hex(hex: &str) -> Self {
        let trimmed = hex.trim_start_matches('#');
        let mut expanded = String::new();
        let s = if trimmed.len() == 3 {
            for c in trimmed.chars() {
                expanded.push(c);
                expanded.push(c);
            }
            &expanded
        } else {
            trimmed
        };
        let n = u32::from_str_radix(s, 16).unwrap_or(0);
        let r = ((n >> 16) & 255) as u8;
        let g = ((n >> 8) & 255) as u8;
        let b = (n & 255) as u8;
        Self {
            r,
            g,
            b,
            a: 1.0,
            hex: format!("#{:02x}{:02x}{:02x}", r, g, b),
        }
    }

    /// Parse a color for numeric renderers without formatting its hex representation.
    pub fn from_hex_numeric(hex: &str) -> Self {
        let bytes = hex.trim_start_matches('#').as_bytes();
        let digit = |byte: u8| (byte as char).to_digit(16).unwrap_or(0) as u8;
        let (r, g, b) = if bytes.len() == 3 {
            (
                digit(bytes[0]) * 17,
                digit(bytes[1]) * 17,
                digit(bytes[2]) * 17,
            )
        } else if bytes.len() >= 6 {
            (
                digit(bytes[0]) * 16 + digit(bytes[1]),
                digit(bytes[2]) * 16 + digit(bytes[3]),
                digit(bytes[4]) * 16 + digit(bytes[5]),
            )
        } else {
            (0, 0, 0)
        };
        Self {
            r,
            g,
            b,
            a: 1.0,
            hex: String::new(),
        }
    }

    pub fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            r,
            g,
            b,
            a: 1.0,
            hex: format!("#{:02x}{:02x}{:02x}", r, g, b),
        }
    }

    pub fn from_hsl(h_deg: f64, s_pct: f64, l_pct: f64) -> Self {
        let mut color = Self::from_hsl_numeric(h_deg, s_pct, l_pct);
        color.hex = format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b);
        color
    }

    /// Convert HSL to an sRGB color without formatting a hex string.
    pub fn from_hsl_numeric(h_deg: f64, s_pct: f64, l_pct: f64) -> Self {
        let h = ((h_deg % 360.0) + 360.0) % 360.0;
        let s = s_pct / 100.0;
        let l = l_pct / 100.0;
        let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
        let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
        let m = l - c / 2.0;
        let (r1, g1, b1) = if h < 60.0 {
            (c, x, 0.0)
        } else if h < 120.0 {
            (x, c, 0.0)
        } else if h < 180.0 {
            (0.0, c, x)
        } else if h < 240.0 {
            (0.0, x, c)
        } else if h < 300.0 {
            (x, 0.0, c)
        } else {
            (c, 0.0, x)
        };
        let r = ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        let g = ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        let b = ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        Self {
            r,
            g,
            b,
            a: 1.0,
            hex: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GradientStop {
    pub offset: f32,
    pub color: Color,
    pub raw_color: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RadialGradient {
    pub cx: f32,
    pub cy: f32,
    pub r: f32,
    pub stops: Vec<GradientStop>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearGradient {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub stops: Vec<GradientStop>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Paint {
    None,
    Solid(Color),
    RadialGradient(RadialGradient),
    LinearGradient(LinearGradient),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub color: Color,
    pub width: f32,
    pub opacity: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PathVerb {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    QuadTo(f32, f32, f32, f32),
    CubicTo(f32, f32, f32, f32, f32, f32),
    ArcTo {
        rx: f32,
        ry: f32,
        x_axis_rotation: f32,
        large_arc: bool,
        sweep: bool,
        x: f32,
        y: f32,
    },
    Close,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Path {
    pub verbs: Vec<PathVerb>,
    pub d: String,
}

impl Path {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_ring(ring: &[crate::types::Point]) -> Self {
        let mut verbs = Vec::with_capacity(ring.len() + 1);
        if let Some(first) = ring.first() {
            verbs.push(PathVerb::MoveTo(first.x as f32, first.y as f32));
            for p in &ring[1..] {
                verbs.push(PathVerb::LineTo(p.x as f32, p.y as f32));
            }
            verbs.push(PathVerb::Close);
        }
        let d = crate::geometry::ring_path_str(ring);
        Self { verbs, d }
    }

    /// Build path geometry without allocating the SVG `d` string.
    pub fn from_ring_numeric(ring: &[crate::types::Point]) -> Self {
        let mut path = Self::new();
        path.write_ring_numeric(ring);
        path
    }

    /// Reuse the existing verb allocation when replacing a numeric ring.
    pub fn write_ring_numeric(&mut self, ring: &[crate::types::Point]) {
        self.verbs.clear();
        if self.verbs.capacity() < ring.len() + 1 {
            self.verbs.reserve(ring.len() + 1);
        }
        if let Some(first) = ring.first() {
            self.verbs
                .push(PathVerb::MoveTo(first.x as f32, first.y as f32));
            for p in &ring[1..] {
                self.verbs.push(PathVerb::LineTo(p.x as f32, p.y as f32));
            }
            self.verbs.push(PathVerb::Close);
        }
        self.d.clear();
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextElement {
    pub text: String,
    pub font_family: String,
    pub font_size: f32,
    pub font_weight: u16,
    pub font_style: String,
    pub fill: Color,
    pub opacity: f32,
    pub transform: Transform,
    pub raw_transform: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathElement {
    pub id: Option<String>,
    pub path: Path,
    pub fill: Paint,
    pub stroke: Option<Stroke>,
    pub opacity: f32,
    pub transform: Transform,
    pub raw_transform: String,
    pub visible: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DrawElement {
    Path(PathElement),
    Text(TextElement),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub view_box: [f32; 4],
    pub elements: Vec<DrawElement>,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            view_box: [-15.0, -15.0, 259.0, 259.0],
            elements: Vec::with_capacity(16),
        }
    }
}

impl Frame {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.elements.clear();
    }
}

// Snapshot structures for testing / golden comparison
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StopSnapshot {
    pub offset: String,
    pub color: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeadSnapshot {
    pub d: String,
    pub transform: String,
    pub fill: String,
    pub stroke: String,
    pub stroke_width: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_opacity: Option<f64>,
    pub stops: Vec<StopSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EyeSnapshot {
    pub d: String,
    pub transform: String,
    pub fill: String,
    pub stroke: String,
    pub stroke_width: f64,
    pub visible: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZzzSnapshot {
    pub opacity: f64,
    pub font_size: f64,
    pub transform: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrailSnapshot {
    pub back_d: String,
    pub front_d: String,
    pub opacity: f64,
    pub stops: Vec<StopSnapshot>,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfettiSnapshot {
    pub kind: String,
    pub transform: String,
    pub opacity: f64,
    pub fill: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameSnapshot {
    pub frame_idx: usize,
    pub time_ms: f64,
    pub head: HeadSnapshot,
    pub eye_l: EyeSnapshot,
    pub eye_r: EyeSnapshot,
    pub zzz: Vec<ZzzSnapshot>,
    pub trails: Vec<TrailSnapshot>,
    pub confetti: Vec<ConfettiSnapshot>,
}
