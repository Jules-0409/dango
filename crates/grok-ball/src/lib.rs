// Grok Ball standalone engine | MIT License | Copyright (c) 2026 tycoding
// Upstream: https://github.com/tycoding/grok-ball
// Ported from ui/grok-ball.js to a pure Rust backend-agnostic display list crate.

pub mod ball;
pub mod data;
pub mod emotion;
pub mod frame;
pub mod geometry;
pub mod prng;
pub mod types;

// Re-export primary types
pub use ball::{Ball, BallOptions, IdleConfig};
pub use data::{CONFETTI_COLORS, EYE_HALF, HEAD_C, STAR_GOLD, STAR_PATH};
pub use emotion::{get_emotion, EmotionDef, EmotionGroup, BUILTIN_EMOTIONS};
pub use frame::{
    Color, ConfettiSnapshot, DrawElement, EyeSnapshot, Frame, FrameSnapshot, GradientStop,
    HeadSnapshot, LinearGradient, Paint, Path, PathElement, PathVerb, RadialGradient, StopSnapshot,
    Stroke, TextElement, TrailSnapshot, Transform, ZzzSnapshot,
};
pub use geometry::{
    centroid, clamp, ease_in_out_cubic, lerp, lerp_color, r2, ring_path_str, shade, SilProfile,
};
pub use prng::Mulberry32;
pub use types::{
    BodyPose, BounceSeg, EyePose, FaceParams, Point, Pose, ShapeData, ShapeKind, Spring,
};
