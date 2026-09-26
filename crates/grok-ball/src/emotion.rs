// Grok Ball standalone engine | MIT License

use crate::geometry::{clamp, lerp, lerp_color, PI, TAU};
use crate::types::{BodyPose, EyePose, Pose};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmotionGroup {
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub en: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BodySpec {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub scale: Option<f64>,
    pub rotate: Option<f64>,
    pub color: Option<String>,
    pub breathe: Option<f64>,
    pub ribbons: Option<f64>,
    pub confetti: Option<f64>,
    pub sketch: Option<f64>,
    pub zzz: Option<f64>,
    pub orbit: Option<f64>,
    pub yaw: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EyeSpec {
    pub x: Option<f64>,
    pub y: Option<f64>,
    #[serde(rename = "scaleX")]
    pub scale_x: Option<f64>,
    #[serde(rename = "scaleY")]
    pub scale_y: Option<f64>,
    pub scale: Option<f64>,
    pub rotate: Option<f64>,
    pub open: Option<f64>,
    pub color: Option<String>,
    #[serde(rename = "lookX")]
    pub look_x: Option<f64>,
    #[serde(rename = "lookY")]
    pub look_y: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EyesSpec {
    pub both: Option<EyeSpec>,
    pub left: Option<EyeSpec>,
    pub right: Option<EyeSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnimSpec {
    pub target: String,
    pub prop: String,
    #[serde(rename = "type")]
    pub anim_type: String,
    #[serde(default)]
    pub amp: f64,
    pub period: Option<f64>,
    pub phase: Option<f64>,
    pub speed: Option<f64>,
    pub decay: Option<f64>,
    #[serde(rename = "phaseMs")]
    pub phase_ms: Option<f64>,
    pub interval: Option<f64>,
    pub dur: Option<f64>,
    pub depth: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RawSettle {
    String(String),
    Object { next: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SettleMode {
    Base,
    Hold,
    Next(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawSeqFrame {
    pub at: Option<f64>,
    pub body: Option<BodySpec>,
    pub eyes: Option<EyesSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawSequence {
    pub settle: Option<RawSettle>,
    pub frames: Vec<RawSeqFrame>,
}

fn default_blink_ms_value() -> serde_json::Value {
    serde_json::json!([6000.0, 14000.0])
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawEmotionSeed {
    pub id: String,
    pub name: String,
    pub group: String,
    #[serde(default)]
    pub desc: Option<String>,
    pub gaze: Option<bool>,
    pub transition: Option<f64>,
    pub pool: Option<Vec<usize>>,
    #[serde(rename = "poolMs")]
    pub pool_ms: Option<[f64; 2]>,
    #[serde(rename = "poolSpeed")]
    pub pool_speed: Option<f64>,
    #[serde(rename = "blinkMs", default = "default_blink_ms_value")]
    pub blink_ms: serde_json::Value,
    pub openness: Option<f64>,
    pub antics: Option<bool>,
    pub body: Option<BodySpec>,
    pub eyes: Option<EyesSpec>,
    pub anims: Option<Vec<AnimSpec>>,
    pub sequence: Option<RawSequence>,
}

#[derive(Clone, Debug)]
pub struct NormalizedFrame {
    pub at: f64,
    pub pose: Pose,
}

#[derive(Clone, Debug)]
pub struct NormalizedSequence {
    pub settle: SettleMode,
    pub frames: Vec<NormalizedFrame>,
}

#[derive(Clone, Debug)]
pub struct EmotionDef {
    pub id: String,
    pub name: String,
    pub group: String,
    pub desc: String,
    pub gaze: bool,
    pub transition: f64,
    pub pool: Vec<usize>,
    pub pool_ms: (f64, f64),
    pub pool_speed: f64,
    pub blink_ms: Option<(f64, f64)>,
    pub openness: f64,
    pub antics: bool,
    pub base: Pose,
    pub anims: Vec<AnimSpec>,
    pub sequence: Option<NormalizedSequence>,
}

pub fn apply_body_spec(body: &mut BodyPose, spec: &BodySpec) {
    if let Some(x) = spec.x {
        body.x = x;
    }
    if let Some(y) = spec.y {
        body.y = y;
    }
    if let Some(s) = spec.scale {
        body.scale = s;
    }
    if let Some(r) = spec.rotate {
        body.rotate = r;
    }
    if let Some(ref c) = spec.color {
        body.color = c.clone();
    }
    if let Some(b) = spec.breathe {
        body.breathe = b;
    }
    if let Some(r) = spec.ribbons {
        body.ribbons = r;
    }
    if let Some(c) = spec.confetti {
        body.confetti = c;
    }
    if let Some(s) = spec.sketch {
        body.sketch = s;
    }
    if let Some(z) = spec.zzz {
        body.zzz = z;
    }
    if let Some(o) = spec.orbit {
        body.orbit = o;
    }
    if let Some(y) = spec.yaw {
        body.yaw = y;
    }
}

pub fn apply_eye_spec(eye: &mut EyePose, spec: &EyeSpec) {
    if let Some(x) = spec.x {
        eye.x = x;
    }
    if let Some(y) = spec.y {
        eye.y = y;
    }
    if let Some(s) = spec.scale {
        eye.scale_x = s;
        eye.scale_y = s;
    }
    if let Some(sx) = spec.scale_x {
        eye.scale_x = sx;
    }
    if let Some(sy) = spec.scale_y {
        eye.scale_y = sy;
    }
    if let Some(r) = spec.rotate {
        eye.rotate = r;
    }
    if let Some(o) = spec.open {
        eye.open = o;
    }
    if let Some(ref c) = spec.color {
        eye.color = c.clone();
    }
    if let Some(lx) = spec.look_x {
        eye.look_x = lx;
    }
    if let Some(ly) = spec.look_y {
        eye.look_y = ly;
    }
}

pub fn apply_spec(pose: &mut Pose, body_spec: Option<&BodySpec>, eyes_spec: Option<&EyesSpec>) {
    if let Some(b) = body_spec {
        apply_body_spec(&mut pose.body, b);
    }
    if let Some(e) = eyes_spec {
        if let Some(ref both) = e.both {
            apply_eye_spec(&mut pose.left, both);
            apply_eye_spec(&mut pose.right, both);
        }
        if let Some(ref left) = e.left {
            apply_eye_spec(&mut pose.left, left);
        }
        if let Some(ref right) = e.right {
            apply_eye_spec(&mut pose.right, right);
        }
    }
}

pub fn lerp_pose(a: &Pose, b: &Pose, t: f64) -> Pose {
    Pose {
        body: BodyPose {
            x: lerp(a.body.x, b.body.x, t),
            y: lerp(a.body.y, b.body.y, t),
            scale: lerp(a.body.scale, b.body.scale, t),
            rotate: lerp(a.body.rotate, b.body.rotate, t),
            color: lerp_color(&a.body.color, &b.body.color, t),
            breathe: lerp(a.body.breathe, b.body.breathe, t),
            ribbons: lerp(a.body.ribbons, b.body.ribbons, t),
            confetti: lerp(a.body.confetti, b.body.confetti, t),
            sketch: lerp(a.body.sketch, b.body.sketch, t),
            zzz: lerp(a.body.zzz, b.body.zzz, t),
            orbit: lerp(a.body.orbit, b.body.orbit, t),
            yaw: lerp(a.body.yaw, b.body.yaw, t),
        },
        left: EyePose {
            x: lerp(a.left.x, b.left.x, t),
            y: lerp(a.left.y, b.left.y, t),
            scale_x: lerp(a.left.scale_x, b.left.scale_x, t),
            scale_y: lerp(a.left.scale_y, b.left.scale_y, t),
            rotate: lerp(a.left.rotate, b.left.rotate, t),
            open: lerp(a.left.open, b.left.open, t),
            color: lerp_color(&a.left.color, &b.left.color, t),
            look_x: lerp(a.left.look_x, b.left.look_x, t),
            look_y: lerp(a.left.look_y, b.left.look_y, t),
            ring: b.left.ring.clone(),
        },
        right: EyePose {
            x: lerp(a.right.x, b.right.x, t),
            y: lerp(a.right.y, b.right.y, t),
            scale_x: lerp(a.right.scale_x, b.right.scale_x, t),
            scale_y: lerp(a.right.scale_y, b.right.scale_y, t),
            rotate: lerp(a.right.rotate, b.right.rotate, t),
            open: lerp(a.right.open, b.right.open, t),
            color: lerp_color(&a.right.color, &b.right.color, t),
            look_x: lerp(a.right.look_x, b.right.look_x, t),
            look_y: lerp(a.right.look_y, b.right.look_y, t),
            ring: b.right.ring.clone(),
        },
    }
}

pub fn normalize_emotion(raw: &RawEmotionSeed) -> EmotionDef {
    let mut base = Pose::default();
    apply_spec(&mut base, raw.body.as_ref(), raw.eyes.as_ref());

    let pool = raw.pool.clone().unwrap_or_else(|| vec![0, 8]);
    let pool = if pool.is_empty() { vec![0] } else { pool };

    let blink_ms = match &raw.blink_ms {
        serde_json::Value::Array(arr) if arr.len() == 2 => {
            let min = arr[0].as_f64().unwrap_or(6000.0);
            let max = arr[1].as_f64().unwrap_or(14000.0);
            Some((min, max))
        }
        serde_json::Value::Null => None,
        _ => None,
    };

    let sequence = raw.sequence.as_ref().map(|raw_seq| {
        let settle = match &raw_seq.settle {
            Some(RawSettle::String(s)) => {
                if s == "hold" {
                    SettleMode::Hold
                } else {
                    SettleMode::Base
                }
            }
            Some(RawSettle::Object { next }) => SettleMode::Next(next.clone()),
            None => SettleMode::Base,
        };

        let mut frames: Vec<NormalizedFrame> = raw_seq
            .frames
            .iter()
            .map(|f| {
                let mut frame_pose = base.clone();
                apply_spec(&mut frame_pose, f.body.as_ref(), f.eyes.as_ref());
                NormalizedFrame {
                    at: f.at.unwrap_or(0.0),
                    pose: frame_pose,
                }
            })
            .collect();
        frames.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap());

        NormalizedSequence { settle, frames }
    });

    EmotionDef {
        id: raw.id.clone(),
        name: raw.name.clone(),
        group: raw.group.clone(),
        desc: raw.desc.clone().unwrap_or_default(),
        gaze: raw.gaze.unwrap_or(true),
        transition: raw.transition.unwrap_or(500.0),
        pool,
        pool_ms: raw
            .pool_ms
            .map(|arr| (arr[0], arr[1]))
            .unwrap_or((9000.0, 16000.0)),
        pool_speed: raw.pool_speed.unwrap_or(6.0),
        blink_ms,
        openness: raw.openness.unwrap_or(1.0),
        antics: raw.antics.unwrap_or(false),
        base,
        anims: raw.anims.clone().unwrap_or_default(),
        sequence,
    }
}

pub fn apply_anim(pose: &mut Pose, a: &AnimSpec, t: f64, seed: f64) {
    let v = match a.anim_type.as_str() {
        "sine" => {
            let period = a.period.unwrap_or(2000.0);
            let phase = a.phase.unwrap_or(0.0);
            a.amp * (TAU * t / period + phase).sin()
        }
        "pulse" => {
            let period = a.period.unwrap_or(1000.0);
            let phase = a.phase.unwrap_or(0.0);
            a.amp * 0.5 * (1.0 - (TAU * t / period + phase).cos())
        }
        "jitter" => {
            let speed = a.speed.unwrap_or(8.0);
            let s = t / 1000.0 * speed;
            let mut val = ((s * 3.1 + seed).sin()
                + (s * 5.7 + seed * 2.3).sin()
                + (s * 9.3 + seed * 4.1).sin())
                / 3.0
                * a.amp;
            if let Some(decay) = a.decay {
                val *= clamp(1.0 - t / decay, 0.0, 1.0);
            }
            val
        }
        "scan" => {
            let per = a.period.unwrap_or(800.0);
            let p = ((t + a.phase_ms.unwrap_or(0.0)) % per) / per;
            let tri = if p < 0.5 {
                p * 4.0 - 1.0
            } else {
                3.0 - p * 4.0
            };
            a.amp * tri
        }
        "glance" => {
            let per = a.period.unwrap_or(3600.0);
            let ph = TAU * (((t + a.phase_ms.unwrap_or(0.0)) % per) / per) + a.phase.unwrap_or(0.0);
            a.amp * (2.8 * ph.sin()).tanh()
        }
        "blink" => {
            let interval = a.interval.unwrap_or(3800.0);
            let dur = a.dur.unwrap_or(200.0);
            let p = (t + a.phase_ms.unwrap_or(0.0) + seed * 97.0) % interval;
            if p >= dur {
                0.0
            } else {
                let depth = a.depth.unwrap_or(1.0);
                -depth * (PI * (p / dur)).sin()
            }
        }
        _ => return,
    };

    match a.target.as_str() {
        "eyes" => {
            apply_prop_to_eye(&mut pose.left, &a.prop, v);
            apply_prop_to_eye(&mut pose.right, &a.prop, v);
        }
        "body" => {
            apply_prop_to_body(&mut pose.body, &a.prop, v);
        }
        "left" => {
            apply_prop_to_eye(&mut pose.left, &a.prop, v);
        }
        "right" => {
            apply_prop_to_eye(&mut pose.right, &a.prop, v);
        }
        _ => {}
    }
}

fn apply_prop_to_body(body: &mut BodyPose, prop: &str, v: f64) {
    match prop {
        "x" => body.x += v,
        "y" => body.y += v,
        "scale" => body.scale += v,
        "rotate" => body.rotate += v,
        "breathe" => body.breathe += v,
        _ => {}
    }
}

fn apply_prop_to_eye(eye: &mut EyePose, prop: &str, v: f64) {
    match prop {
        "x" => eye.x += v,
        "y" => eye.y += v,
        "scale" => {
            eye.scale_x += v;
            eye.scale_y += v;
        }
        "scaleX" => eye.scale_x += v,
        "scaleY" => eye.scale_y += v,
        "rotate" => eye.rotate += v,
        "open" => eye.open += v,
        "lookX" => eye.look_x += v,
        "lookY" => eye.look_y += v,
        _ => {}
    }
}

pub static BUILTIN_EMOTIONS: LazyLock<HashMap<String, EmotionDef>> = LazyLock::new(|| {
    let json_str = include_str!("emotions.json");
    let raw_list: Vec<RawEmotionSeed> =
        serde_json::from_str(json_str).expect("Valid emotions.json");
    let mut map = HashMap::with_capacity(raw_list.len());
    for raw in raw_list {
        let def = normalize_emotion(&raw);
        map.insert(def.id.clone(), def);
    }
    map
});

pub fn get_emotion(id: &str) -> Option<&'static EmotionDef> {
    BUILTIN_EMOTIONS.get(id)
}
