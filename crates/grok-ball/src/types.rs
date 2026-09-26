// Grok Ball standalone engine | MIT License

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShapeKind {
    #[default]
    Blob,
    Wedge,
    Gem,
    Star,
    Cloud,
    Heart,
    Square,
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FaceParams {
    pub x: f64,
    pub y: f64,
    pub sx: f64,
    pub sy: f64,
    pub eye: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct ShapeData {
    pub ring: &'static [Point; 96],
    pub face: FaceParams,
    pub tilt_scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BounceSeg {
    pub h: f64,
    pub d: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BodyPose {
    pub x: f64,
    pub y: f64,
    pub scale: f64,
    pub rotate: f64,
    pub color: String,
    pub breathe: f64,
    pub ribbons: f64,
    pub confetti: f64,
    pub sketch: f64,
    pub zzz: f64,
    pub orbit: f64,
    #[serde(default)]
    pub yaw: f64,
}

impl Default for BodyPose {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            rotate: 0.0,
            color: "#F3F0EA".to_string(),
            breathe: 0.01,
            ribbons: 0.0,
            confetti: 0.0,
            sketch: 0.0,
            zzz: 0.0,
            orbit: 0.0,
            yaw: 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EyePose {
    pub x: f64,
    pub y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    pub rotate: f64,
    pub open: f64,
    pub color: String,
    pub look_x: f64,
    pub look_y: f64,
    #[serde(skip)]
    pub ring: Option<Vec<Point>>,
}

impl Default for EyePose {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            rotate: 0.0,
            open: 1.0,
            color: "#1A1A1A".to_string(),
            look_x: 0.0,
            look_y: 0.0,
            ring: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Pose {
    pub body: BodyPose,
    pub left: EyePose,
    pub right: EyePose,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Spring {
    pub x: f64,
    pub v: f64,
    pub t: f64,
}

impl Spring {
    pub const fn new(v0: f64) -> Self {
        Self {
            x: v0,
            v: 0.0,
            t: v0,
        }
    }

    pub fn step(&mut self, w: f64, z: f64, dt: f64) {
        self.v += (-2.0 * z * w * self.v - w * w * (self.x - self.t)) * dt;
        self.x += self.v * dt;
        if !self.x.is_finite() || !self.v.is_finite() {
            self.x = self.t;
            self.v = 0.0;
        }
    }
}
