//! Display-list geometry helpers shared by every platform renderer.
//!
//! `grok_ball::Frame` carries `PathVerb` / `Transform` values in a 259-unit view box.
//! These helpers convert that into the affine math and bounding boxes the
//! platform layer needs; the actual painting (CAShapeLayer on macOS) stays in
//! `platform/`.

use grok_ball::{Frame, Path as FramePath, PathVerb, Transform as FrameTransform};

/// Size of the grok-ball design space, matching `Frame::default().view_box`.
pub const VIEW_SIZE: f32 = 259.0;

/// 2D affine (row-vector convention, same as `Transform`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub sx: f32,
    pub ky: f32,
    pub kx: f32,
    pub sy: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Affine {
    pub const IDENTITY: Self = Self {
        sx: 1.0,
        ky: 0.0,
        kx: 0.0,
        sy: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub fn from_frame(local: FrameTransform) -> Self {
        Self {
            sx: local.a,
            ky: local.b,
            kx: local.c,
            sy: local.d,
            tx: local.e,
            ty: local.f,
        }
    }

    pub fn scale(uniform: f32) -> Self {
        Self {
            sx: uniform,
            ky: 0.0,
            kx: 0.0,
            sy: uniform,
            tx: 0.0,
            ty: 0.0,
        }
    }

    pub fn translate(tx: f32, ty: f32) -> Self {
        Self {
            sx: 1.0,
            ky: 0.0,
            kx: 0.0,
            sy: 1.0,
            tx,
            ty,
        }
    }

    /// `self ∘ b`: the result applies `b` first, then `self` (SVG / CSS order,
    /// so `outer.concat(inner)` scales the inner translation by the outer scale).
    pub fn concat(self, b: Self) -> Self {
        Self {
            sx: self.sx * b.sx + self.kx * b.ky,
            ky: self.ky * b.sx + self.sy * b.ky,
            kx: self.sx * b.kx + self.kx * b.sy,
            sy: self.ky * b.kx + self.sy * b.sy,
            tx: self.sx * b.tx + self.kx * b.ty + self.tx,
            ty: self.ky * b.tx + self.sy * b.ty + self.ty,
        }
    }

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.sx * x + self.kx * y + self.tx,
            self.ky * x + self.sy * y + self.ty,
        )
    }

    /// Uniform scale factor of the transform (sqrt of the determinant).
    pub fn scale_factor(&self) -> f32 {
        (self.sx * self.sy - self.ky * self.kx).abs().sqrt()
    }
}

/// Map a frame's view box into a `side`-point square at the origin.
///
/// Layer-backed renderers work in logical points, so unlike the bitmap path
/// there is no extra retina `contentsScale` multiplication here.
pub fn frame_transform(frame: &Frame, side: f32) -> Affine {
    let [view_x, view_y, _, _] = frame.view_box;
    let scale = side / VIEW_SIZE;
    Affine {
        sx: scale,
        ky: 0.0,
        kx: 0.0,
        sy: scale,
        tx: -view_x * scale,
        ty: -view_y * scale,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Bounds {
    pub fn width(&self) -> f32 {
        (self.right - self.left).max(0.001)
    }

    pub fn height(&self) -> f32 {
        (self.bottom - self.top).max(0.001)
    }
}

pub fn raw_path_bounds(path: &FramePath) -> Option<Bounds> {
    let mut bounds = Bounds {
        left: f32::INFINITY,
        top: f32::INFINITY,
        right: f32::NEG_INFINITY,
        bottom: f32::NEG_INFINITY,
    };
    let mut include = |x: f32, y: f32| {
        bounds.left = bounds.left.min(x);
        bounds.top = bounds.top.min(y);
        bounds.right = bounds.right.max(x);
        bounds.bottom = bounds.bottom.max(y);
    };
    for verb in &path.verbs {
        match *verb {
            PathVerb::MoveTo(x, y) | PathVerb::LineTo(x, y) => include(x, y),
            PathVerb::QuadTo(x1, y1, x, y) => {
                include(x1, y1);
                include(x, y);
            }
            PathVerb::CubicTo(x1, y1, x2, y2, x, y) => {
                include(x1, y1);
                include(x2, y2);
                include(x, y);
            }
            PathVerb::ArcTo { x, y, .. } => include(x, y),
            PathVerb::Close => {}
        }
    }
    bounds.left.is_finite().then_some(bounds)
}

pub fn transformed_path_bounds(path: &FramePath, transform: Affine) -> Option<Bounds> {
    let bounds = raw_path_bounds(path)?;
    let corners = [
        transform.apply(bounds.left, bounds.top),
        transform.apply(bounds.right, bounds.top),
        transform.apply(bounds.right, bounds.bottom),
        transform.apply(bounds.left, bounds.bottom),
    ];
    let mut bounds = Bounds {
        left: f32::INFINITY,
        top: f32::INFINITY,
        right: f32::NEG_INFINITY,
        bottom: f32::NEG_INFINITY,
    };
    for (x, y) in corners {
        bounds.left = bounds.left.min(x);
        bounds.top = bounds.top.min(y);
        bounds.right = bounds.right.max(x);
        bounds.bottom = bounds.bottom.max(y);
    }
    Some(bounds)
}

/// A cubic Bezier curve segment represented by two control points and an end point.
pub type CubicSegment = ((f64, f64), (f64, f64), (f64, f64));

/// Convert an SVG arc into cubic Bezier segments (endpoint parameterisation).
pub fn arc_to_cubics(
    from: (f64, f64),
    radii: (f64, f64),
    phi: f64,
    large_arc: bool,
    sweep: bool,
    to: (f64, f64),
) -> Vec<CubicSegment> {
    let (mut rx, mut ry) = (radii.0.abs(), radii.1.abs());
    if from == to {
        return Vec::new();
    }
    if rx == 0.0 || ry == 0.0 {
        let dx = (to.0 - from.0) / 3.0;
        let dy = (to.1 - from.1) / 3.0;
        return vec![(
            (from.0 + dx, from.1 + dy),
            (from.0 + 2.0 * dx, from.1 + 2.0 * dy),
            to,
        )];
    }

    let (sin_phi, cos_phi) = phi.sin_cos();
    let dx2 = (from.0 - to.0) / 2.0;
    let dy2 = (from.1 - to.1) / 2.0;
    let x1p = cos_phi * dx2 + sin_phi * dy2;
    let y1p = -sin_phi * dx2 + cos_phi * dy2;
    let lambda = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if lambda > 1.0 {
        let factor = lambda.sqrt();
        rx *= factor;
        ry *= factor;
    }
    let numerator = (rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p).max(0.0);
    let denominator = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let sign = if large_arc == sweep { -1.0 } else { 1.0 };
    let coef = if denominator <= f64::EPSILON {
        0.0
    } else {
        sign * (numerator / denominator).sqrt()
    };
    let cxp = coef * rx * y1p / ry;
    let cyp = coef * -ry * x1p / rx;
    let center = (
        cos_phi * cxp - sin_phi * cyp + (from.0 + to.0) / 2.0,
        sin_phi * cxp + cos_phi * cyp + (from.1 + to.1) / 2.0,
    );
    let angle = |ux: f64, uy: f64, vx: f64, vy: f64| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let vx = (-x1p - cxp) / rx;
    let vy = (-y1p - cyp) / ry;
    let start_angle = uy.atan2(ux);
    let mut delta = angle(ux, uy, vx, vy);
    if !sweep && delta > 0.0 {
        delta -= std::f64::consts::TAU;
    } else if sweep && delta < 0.0 {
        delta += std::f64::consts::TAU;
    }
    let segments = (delta.abs() / std::f64::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = delta / segments as f64;
    let ellipse_point = |theta: f64| {
        (
            center.0 + rx * cos_phi * theta.cos() - ry * sin_phi * theta.sin(),
            center.1 + rx * sin_phi * theta.cos() + ry * cos_phi * theta.sin(),
        )
    };
    let ellipse_derivative = |theta: f64| {
        (
            -rx * cos_phi * theta.sin() - ry * sin_phi * theta.cos(),
            -rx * sin_phi * theta.sin() - ry * cos_phi * theta.cos(),
        )
    };
    let mut output = Vec::with_capacity(segments);
    for i in 0..segments {
        let a0 = start_angle + i as f64 * step;
        let a1 = a0 + step;
        let alpha = (4.0 / 3.0) * (step / 4.0).tan();
        let p0 = ellipse_point(a0);
        let p1 = ellipse_point(a1);
        let d0 = ellipse_derivative(a0);
        let d1 = ellipse_derivative(a1);
        let c1 = (p0.0 + alpha * d0.0, p0.1 + alpha * d0.1);
        let c2 = (p1.0 - alpha * d1.0, p1.1 - alpha * d1.1);
        output.push((c1, c2, if i + 1 == segments { to } else { p1 }));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn transform(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) -> FrameTransform {
        FrameTransform { a, b, c, d, e, f }
    }

    #[test]
    fn frame_transform_maps_view_box_origin_to_zero() {
        let frame = Frame::default();
        let affine = frame_transform(&frame, 44.0);
        assert_eq!(affine.apply(-15.0, -15.0), (0.0, 0.0));
        let (x, y) = affine.apply(244.0, 244.0);
        assert!((x - 44.0).abs() < 1e-3 && (y - 44.0).abs() < 1e-3);
    }

    #[test]
    fn concat_matches_manual_composition() {
        let outer = Affine::scale(2.0);
        let inner = Affine::translate(1.0, 3.0);
        let combined = outer.concat(inner);
        // Inner translate first, then outer scale: (1+1, 1+3) * 2.
        assert_eq!(combined.apply(1.0, 1.0), (4.0, 8.0));
    }

    #[test]
    fn concat_with_rotation_matches_sequential_apply() {
        let outer = Affine {
            sx: 0.5,
            ky: 0.0,
            kx: 0.0,
            sy: 0.5,
            tx: 3.0,
            ty: -2.0,
        };
        let inner = Affine {
            sx: 0.0,
            ky: 1.0,
            kx: -1.0,
            sy: 0.0,
            tx: 10.0,
            ty: 20.0,
        };
        let (ix, iy) = inner.apply(4.0, 7.0);
        assert_eq!(outer.concat(inner).apply(4.0, 7.0), outer.apply(ix, iy));
    }

    #[test]
    fn arc_to_cubics_returns_no_segments_for_degenerate_arc() {
        assert!(arc_to_cubics((1.0, 1.0), (5.0, 5.0), 0.0, false, true, (1.0, 1.0)).is_empty());
    }

    #[test]
    fn arc_to_cubics_semicircle_ends_at_target() {
        let segments = arc_to_cubics((10.0, 0.0), (5.0, 5.0), 0.0, false, true, (-10.0, 0.0));
        assert!(!segments.is_empty());
        let (_, _, last) = segments.last().unwrap();
        assert!((last.0 - -10.0).abs() < 1e-6 && (last.1).abs() < 1e-6);
    }

    #[test]
    fn path_bounds_cover_all_verbs() {
        let path = FramePath {
            verbs: vec![
                PathVerb::MoveTo(0.0, 0.0),
                PathVerb::LineTo(10.0, 4.0),
                PathVerb::QuadTo(12.0, 8.0, 6.0, 9.0),
                PathVerb::Close,
            ],
            d: String::new(),
        };
        let bounds = raw_path_bounds(&path).expect("bounds");
        assert_eq!(bounds.left, 0.0);
        assert_eq!(bounds.top, 0.0);
        assert_eq!(bounds.right, 12.0);
        assert_eq!(bounds.bottom, 9.0);
    }

    #[test]
    fn empty_path_has_no_bounds() {
        let path = FramePath {
            verbs: vec![],
            d: String::new(),
        };
        assert!(raw_path_bounds(&path).is_none());
    }
}
