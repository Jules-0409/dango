// Grok Ball standalone engine | MIT License

use crate::data::*;
use crate::emotion::*;
use crate::frame::*;
use crate::geometry::*;
use crate::prng::Mulberry32;
use crate::types::*;

fn shade_color_numeric(hex: &str, amount: f64) -> Color {
    let source = Color::from_hex_numeric(hex);
    let target = if amount < 0.0 { 0.0 } else { 255.0 };
    let scale = amount.abs();
    let shade = |channel: u8| {
        (channel as f64 + (target - channel as f64) * scale)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color {
        r: shade(source.r),
        g: shade(source.g),
        b: shade(source.b),
        a: source.a,
        hex: String::new(),
    }
}

#[derive(Clone, Debug)]
pub struct IdleConfig {
    pub standby_after: f64,
    pub sleep_after: f64,
    pub standby_id: String,
    pub sleep_id: String,
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            standby_after: 60000.0,
            sleep_after: 180000.0,
            standby_id: "02".to_string(),
            sleep_id: "00".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BallOptions {
    pub shape: ShapeKind,
    pub color: Option<String>,
    pub eye_color: Option<String>,
    pub emotion: Option<String>,
    pub lite: bool,
    pub eye_scale: f64,
    pub fallback_id: String,
    pub idle: Option<IdleConfig>,
    pub seed: Option<u32>,
    pub autostart: bool,
    pub sketch: f64,
}

impl Default for BallOptions {
    fn default() -> Self {
        Self {
            shape: ShapeKind::Blob,
            color: None,
            eye_color: None,
            emotion: Some("02".to_string()),
            lite: false,
            eye_scale: 1.0,
            fallback_id: "02".to_string(),
            idle: None,
            seed: None,
            autostart: true,
            sketch: 0.0,
        }
    }
}

#[derive(Clone, Debug)]
struct BlinkKey {
    at: f64,
    v: f64,
}

#[derive(Clone, Debug)]
struct Plane {
    tilt: f64,
    roll: f64,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct OrbitParams {
    lam: f64,
    lam_vel: f64,
    tilt: f64,
    roll: f64,
    rad: f64,
    rad_vel: f64,
    follow: f64,
    carry: f64,
    arc: f64,
    r: f64,
    hue: f64,
    hue_span: f64,
    hue_vel: f64,
}

#[derive(Clone, Debug)]
struct HistPoint {
    x: f64,
    y: f64,
    z: f64,
    l: f64,
}

#[derive(Clone, Debug)]
struct Trail {
    orbit_mode: bool,
    r: f64,
    life: f64,
    ret: f64,
    o: OrbitParams,
    hist: Vec<HistPoint>,
    back_d: String,
    front_d: String,
    opacity: f64,
    stops: Vec<StopSnapshot>,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

fn default_trail_stops() -> Vec<StopSnapshot> {
    (0..5)
        .map(|s| StopSnapshot {
            offset: format!("{:.3}", s as f64 / 4.0),
            color: String::new(),
        })
        .collect()
}

fn path_from_svg_d(d: &str, keep_d: bool) -> Path {
    let bytes = d.as_bytes();
    let mut cursor = 0;
    let mut verbs = Vec::new();
    while cursor < bytes.len() {
        while cursor < bytes.len() && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b',')
        {
            cursor += 1;
        }
        if cursor == bytes.len() {
            break;
        }
        let command = bytes[cursor] as char;
        cursor += 1;
        match command {
            'M' | 'L' => {
                let x = path_number(bytes, &mut cursor);
                let y = path_number(bytes, &mut cursor);
                verbs.push(if command == 'M' {
                    PathVerb::MoveTo(x, y)
                } else {
                    PathVerb::LineTo(x, y)
                });
            }
            'A' => {
                let rx = path_number(bytes, &mut cursor);
                let ry = path_number(bytes, &mut cursor);
                let x_axis_rotation = path_number(bytes, &mut cursor);
                let large_arc = path_number(bytes, &mut cursor) != 0.0;
                let sweep = path_number(bytes, &mut cursor) != 0.0;
                let x = path_number(bytes, &mut cursor);
                let y = path_number(bytes, &mut cursor);
                verbs.push(PathVerb::ArcTo {
                    rx,
                    ry,
                    x_axis_rotation,
                    large_arc,
                    sweep,
                    x,
                    y,
                });
            }
            'Z' | 'z' => verbs.push(PathVerb::Close),
            _ => unreachable!("unsupported generated SVG path command: {command}"),
        }
    }
    Path {
        verbs,
        d: if keep_d { d.to_owned() } else { String::new() },
    }
}

fn path_number(bytes: &[u8], cursor: &mut usize) -> f32 {
    while *cursor < bytes.len() && (bytes[*cursor].is_ascii_whitespace() || bytes[*cursor] == b',')
    {
        *cursor += 1;
    }
    let start = *cursor;
    while *cursor < bytes.len()
        && !bytes[*cursor].is_ascii_whitespace()
        && bytes[*cursor] != b','
        && (!bytes[*cursor].is_ascii_alphabetic()
            || bytes[*cursor] == b'e'
            || bytes[*cursor] == b'E')
    {
        *cursor += 1;
    }
    std::str::from_utf8(&bytes[start..*cursor])
        .expect("generated SVG path number is UTF-8")
        .parse()
        .expect("generated SVG path number")
}

fn parse_trail_hsl(value: &str) -> (f64, f64, f64) {
    let mut parts = value
        .trim_start_matches("hsl(")
        .trim_end_matches(')')
        .split_whitespace();
    let (Some(hue), Some(saturation), Some(lightness), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return (0.0, 56.0, 56.0);
    };
    (
        hue.parse().unwrap_or(0.0),
        saturation.trim_end_matches('%').parse().unwrap_or(56.0),
        lightness.trim_end_matches('%').parse().unwrap_or(56.0),
    )
}

fn trail_paint(trail: &Trail, fast_output: bool) -> Paint {
    let stops = trail
        .stops
        .iter()
        .map(|stop| {
            let (hue, saturation, lightness) = parse_trail_hsl(&stop.color);
            GradientStop {
                offset: stop.offset.parse::<f32>().unwrap_or(0.0),
                color: if fast_output {
                    Color::from_hsl_numeric(hue, saturation, lightness)
                } else {
                    Color::from_hsl(hue, saturation, lightness)
                },
                raw_color: if fast_output {
                    String::new()
                } else {
                    stop.color.clone()
                },
            }
        })
        .collect();
    Paint::LinearGradient(LinearGradient {
        x1: trail.x1 as f32,
        y1: trail.y1 as f32,
        x2: trail.x2 as f32,
        y2: trail.y2 as f32,
        stops,
    })
}

fn confetti_path(kind: &ConfettiKind, keep_d: bool) -> Path {
    match kind {
        ConfettiKind::Star => path_from_svg_d(STAR_PATH, keep_d),
        ConfettiKind::Circle => Path {
            verbs: vec![
                PathVerb::MoveTo(1.0, 0.0),
                PathVerb::ArcTo {
                    rx: 1.0,
                    ry: 1.0,
                    x_axis_rotation: 0.0,
                    large_arc: false,
                    sweep: true,
                    x: -1.0,
                    y: 0.0,
                },
                PathVerb::ArcTo {
                    rx: 1.0,
                    ry: 1.0,
                    x_axis_rotation: 0.0,
                    large_arc: false,
                    sweep: true,
                    x: 1.0,
                    y: 0.0,
                },
                PathVerb::Close,
            ],
            d: String::new(),
        },
        ConfettiKind::Rect => {
            // SVG rect x/y=-0.5, width/height=1, rx=0.24 as cubic corners.
            const K: f32 = 0.132_548_33; // 0.24 * 4/3 * (sqrt(2) - 1)
            const R: f32 = 0.24;
            Path {
                verbs: vec![
                    PathVerb::MoveTo(-0.5 + R, -0.5),
                    PathVerb::LineTo(0.5 - R, -0.5),
                    PathVerb::CubicTo(0.5 - R + K, -0.5, 0.5, -0.5 + R - K, 0.5, -0.5 + R),
                    PathVerb::LineTo(0.5, 0.5 - R),
                    PathVerb::CubicTo(0.5, 0.5 - R + K, 0.5 - R + K, 0.5, 0.5 - R, 0.5),
                    PathVerb::LineTo(-0.5 + R, 0.5),
                    PathVerb::CubicTo(-0.5 + R - K, 0.5, -0.5, 0.5 - R + K, -0.5, 0.5 - R),
                    PathVerb::LineTo(-0.5, -0.5 + R),
                    PathVerb::CubicTo(-0.5, -0.5 + R - K, -0.5 + R - K, -0.5, -0.5 + R, -0.5),
                    PathVerb::Close,
                ],
                d: String::new(),
            }
        }
    }
}

#[derive(Clone, Debug)]
enum ConfettiKind {
    Star,
    Circle,
    Rect,
}

#[derive(Clone, Debug)]
struct ConfettiPiece {
    kind: ConfettiKind,
    color: String,
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    life: f64,
    max: f64,
    r: f64,
    rot: f64,
    vr: f64,
    stretch: f64,
    transform: String,
    opacity: f64,
}

#[derive(Clone, Debug)]
struct SeqState {
    done: bool,
    settle: SettleMode,
    frames: Vec<NormalizedFrame>,
}

#[derive(Clone, Debug)]
struct GazeState {
    x: f64,
    y: f64,
    tx: f64,
    ty: f64,
}

#[derive(Clone, Debug)]
struct EyeState {
    ring: Option<Vec<Point>>,
    centroid: Point,
    d: String,
    last_transform: String,
}

pub struct Ball {
    rng: Mulberry32,
    seed_val: f64,
    opts: BallOptions,
    shape_data: ShapeData,
    sil_profile: SilProfile,
    theme: Option<(String, String)>,
    gaze: GazeState,
    style_sketch: f64,
    last_tick: f64,
    prev_now: f64,
    dt: f64,
    active: bool,
    last_activity: f64,
    def: &'static EmotionDef,
    emotion_id: String,
    emo_start: f64,
    last_pose: Option<Pose>,
    prev_pose: Option<Pose>,
    trans_start: f64,
    trans_dur: f64,
    seq: Option<SeqState>,
    expr_idx: usize,
    ring_src: [Vec<Point>; 2],
    ring_dst: [Vec<Point>; 2],
    ring_cur: [Vec<Point>; 2],
    ring_spring: Spring,
    ring_speed: f64,
    open_spring: Spring,
    spin_spring: Option<Spring>,
    bounce_at: f64,
    blink_q: Vec<BlinkKey>,
    blink_next: f64,
    pool_pos: usize,
    pool_next: f64,
    antic_next: f64,
    zzz_state: [ZzzSnapshot; 3],
    // Eye state caching
    eye_l: EyeState,
    eye_r: EyeState,
    base_c: [Point; 2],
    // FX state
    was_fast: bool,
    prev_yaw: f64,
    spawn_at: Vec<f64>,
    planes: Vec<Plane>,
    plane_g: usize,
    base_hue: f64,
    spawn_idx: usize,
    trails: Vec<Trail>,
    orbit_next_at: f64,
    confetti: Vec<ConfettiPiece>,
    // Reusable display list frame
    frame: Frame,
    // Numeric renderers need PathVerb geometry but not SVG/debug strings.
    fast_output: bool,
    fast_frame_cursor: usize,
    // Reusable snapshot storage for golden tests
    last_snapshot: Option<FrameSnapshot>,
}

impl Ball {
    fn store_path_element(&mut self, mut element: PathElement, ring: &[Point]) {
        if self.fast_output {
            let reuse = self.opts.lite;
            let index = if reuse {
                let index = self.fast_frame_cursor;
                self.fast_frame_cursor += 1;
                Some(index)
            } else {
                None
            };
            if let Some(index) = index {
                if let Some(DrawElement::Path(existing)) = self.frame.elements.get_mut(index) {
                    existing.id = None;
                    existing.path.write_ring_numeric(ring);
                    existing.fill = element.fill;
                    existing.stroke = element.stroke;
                    existing.opacity = element.opacity;
                    existing.transform = element.transform;
                    existing.raw_transform.clear();
                    existing.visible = element.visible;
                    return;
                }
            }
            element.id = None;
            element.path.write_ring_numeric(ring);
            element.raw_transform.clear();
            self.frame.elements.push(DrawElement::Path(element));
        } else {
            self.frame.elements.push(DrawElement::Path(element));
        }
    }

    pub fn new(opts: BallOptions) -> Self {
        let seed = opts.seed.unwrap_or(1337);
        let mut rng = Mulberry32::new(seed);
        let seed_val = rng.next_f64() * 100.0;

        let shape_data = get_shape_data(opts.shape);
        let sil_profile = SilProfile::from_ring(shape_data.ring, HEAD_C);

        let theme = opts.color.as_ref().map(|c| {
            (
                c.clone(),
                opts.eye_color
                    .clone()
                    .unwrap_or_else(|| "#FFFFFF".to_string()),
            )
        });

        let default_expr_l: Vec<Point> = EXPRESSIONS[0][0].to_vec();
        let default_expr_r: Vec<Point> = EXPRESSIONS[0][1].to_vec();
        let base_c = [centroid(&default_expr_l), centroid(&default_expr_r)];

        let eye_l = EyeState {
            ring: Some(default_expr_l.clone()),
            centroid: base_c[0],
            d: ring_path_str(&default_expr_l),
            last_transform: String::new(),
        };
        let eye_r = EyeState {
            ring: Some(default_expr_r.clone()),
            centroid: base_c[1],
            d: ring_path_str(&default_expr_r),
            last_transform: String::new(),
        };

        let initial_emo = opts
            .emotion
            .clone()
            .unwrap_or_else(|| opts.fallback_id.clone());
        let def = get_emotion(&initial_emo)
            .or_else(|| get_emotion(&opts.fallback_id))
            .or_else(|| get_emotion("02"))
            .expect("Default emotion '02' must exist");

        let style_sketch = opts.sketch;
        let autostart = opts.autostart;

        let mut ball = Self {
            rng,
            seed_val,
            opts,
            shape_data,
            sil_profile,
            theme,
            gaze: GazeState {
                x: 0.0,
                y: 0.0,
                tx: 0.0,
                ty: 0.0,
            },
            style_sketch,
            last_tick: 0.0,
            prev_now: 0.0,
            dt: 1.0 / 60.0,
            active: false,
            last_activity: 0.0,
            def,
            emotion_id: def.id.clone(),
            emo_start: 0.0,
            last_pose: None,
            prev_pose: None,
            trans_start: 0.0,
            trans_dur: 0.0,
            seq: None,
            expr_idx: 0,
            ring_src: [default_expr_l.clone(), default_expr_r.clone()],
            ring_dst: [default_expr_l.clone(), default_expr_r.clone()],
            ring_cur: [default_expr_l, default_expr_r],
            ring_spring: Spring::new(1.0),
            ring_speed: 7.0,
            open_spring: Spring::new(1.0),
            spin_spring: None,
            bounce_at: -1.0,
            blink_q: Vec::new(),
            blink_next: f64::INFINITY,
            pool_pos: 0,
            pool_next: 0.0,
            antic_next: 0.0,
            zzz_state: [
                ZzzSnapshot {
                    opacity: 0.0,
                    font_size: 12.0,
                    transform: String::new(),
                },
                ZzzSnapshot {
                    opacity: 0.0,
                    font_size: 12.0,
                    transform: String::new(),
                },
                ZzzSnapshot {
                    opacity: 0.0,
                    font_size: 12.0,
                    transform: String::new(),
                },
            ],
            eye_l,
            eye_r,
            base_c,
            was_fast: false,
            prev_yaw: 0.0,
            spawn_at: Vec::new(),
            planes: Vec::new(),
            plane_g: 3,
            base_hue: 0.0,
            spawn_idx: 0,
            trails: Vec::new(),
            orbit_next_at: 0.0,
            confetti: Vec::new(),
            frame: Frame::new(),
            fast_output: false,
            fast_frame_cursor: 0,
            last_snapshot: None,
        };

        ball.set_emotion_internal(&initial_emo, true, 0.0);
        if autostart {
            ball.active = true;
        } else {
            ball.render_static(0.0);
        }

        ball
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    pub fn set_emotion(&mut self, id: &str) -> bool {
        self.set_emotion_at(id, self.last_tick)
    }

    pub fn set_emotion_at(&mut self, id: &str, now_ms: f64) -> bool {
        self.set_emotion_internal(id, false, now_ms)
    }

    fn set_emotion_internal(&mut self, id: &str, is_auto: bool, now_ms: f64) -> bool {
        let next_def = match get_emotion(id) {
            Some(d) => d,
            None => {
                if !self.opts.fallback_id.is_empty() && id != self.opts.fallback_id {
                    let fb = self.opts.fallback_id.clone();
                    return self.set_emotion_internal(&fb, is_auto, now_ms);
                }
                return false;
            }
        };

        let now = now_ms;
        let prev_id = if self.emotion_id.is_empty() {
            None
        } else {
            Some(self.emotion_id.clone())
        };

        self.def = next_def;
        self.emotion_id = next_def.id.clone();
        self.emo_start = now;
        self.trans_start = now;
        self.trans_dur = if self.last_pose.is_some() {
            next_def.transition
        } else {
            0.0
        };
        self.prev_pose = self.last_pose.clone();

        if !is_auto {
            self.last_activity = now;
        }

        self.pool_pos = 0;
        let pool_speed = if next_def.pool_speed >= 10.0 {
            10.0
        } else {
            8.0
        };
        self.set_expr(next_def.pool[0], pool_speed);
        self.pool_next = now + self.rng.rand_range(next_def.pool_ms.0, next_def.pool_ms.1);

        if let Some(ref pid) = prev_id {
            if pid != &next_def.id && next_def.blink_ms.is_some() {
                self.blink_now(now);
            }
        }

        self.blink_next = match next_def.blink_ms {
            Some((min, max)) => now + self.rng.rand_range(min, max),
            None => f64::INFINITY,
        };
        self.antic_next = now + self.rng.rand_range(2500.0, 5000.0);
        self.open_spring.t = next_def.openness;

        if let Some(ref seq) = next_def.sequence {
            self.seq = Some(SeqState {
                done: false,
                settle: seq.settle.clone(),
                frames: seq.frames.clone(),
            });
        } else {
            self.seq = None;
        }

        if self.active {
            let fx = &next_def.base.body;
            if fx.ribbons > 0.0 {
                self.spin_internal(Some(if fx.ribbons >= 1.0 { 2.0 } else { 1.0 }), None);
            }
            if fx.confetti > 0.0 {
                self.burst(Some(20));
            }
        } else {
            self.render_static(now);
        }

        true
    }

    fn set_expr(&mut self, idx: usize, speed: f64) {
        if idx >= EXPRESSIONS.len() {
            return;
        }
        if idx == self.expr_idx && self.ring_spring.x >= 0.999 {
            return;
        }
        let s = clamp(self.ring_spring.x, 0.0, 1.0);
        self.ring_src = [
            lerp_ring(&self.ring_src[0], &self.ring_dst[0], s),
            lerp_ring(&self.ring_src[1], &self.ring_dst[1], s),
        ];
        self.ring_dst = [EXPRESSIONS[idx][0].to_vec(), EXPRESSIONS[idx][1].to_vec()];
        self.ring_spring.x = 0.0;
        self.ring_spring.v = 0.0;
        self.ring_spring.t = 1.0;
        self.ring_speed = speed;
        self.expr_idx = idx;
    }

    fn blink_now(&mut self, t: f64) {
        self.blink_q.push(BlinkKey { at: t, v: 0.05 });
        self.blink_q.push(BlinkKey {
            at: t + 70.0,
            v: 0.05,
        });
        self.blink_q.push(BlinkKey {
            at: t + 150.0,
            v: 1.08,
        });
        self.blink_q.push(BlinkKey {
            at: t + 300.0,
            v: 1.0,
        });
        if self.rng.pick_bool(0.14) {
            self.blink_q.push(BlinkKey {
                at: t + 370.0,
                v: 0.05,
            });
            self.blink_q.push(BlinkKey {
                at: t + 480.0,
                v: 1.0,
            });
        }
    }

    pub fn set_gaze(&mut self, nx: f64, ny: f64) -> &mut Self {
        self.gaze.tx = clamp(nx, -1.0, 1.0) * 16.0;
        self.gaze.ty = clamp(ny, -1.0, 1.0) * 12.0;
        self
    }

    pub fn clear_gaze(&mut self) -> &mut Self {
        self.gaze.tx = 0.0;
        self.gaze.ty = 0.0;
        self
    }

    pub fn spin(&mut self, turns: Option<f64>, dir: Option<f64>) -> &mut Self {
        self.spin_internal(turns, dir);
        self
    }

    fn spin_internal(&mut self, turns: Option<f64>, dir: Option<f64>) {
        if self.spin_spring.is_some() {
            return;
        }
        let d = match dir {
            Some(d) if d != 0.0 => d,
            _ => self.rng.pick_sign(),
        };
        let t = (turns.unwrap_or(1.0).round().max(1.0)) * TAU * d;
        self.spin_spring = Some(Spring { x: 0.0, v: 0.0, t });
    }

    pub fn bounce_at(&mut self, time_ms: f64) -> &mut Self {
        if self.bounce_at < 0.0 {
            self.bounce_at = time_ms;
        }
        self
    }

    pub fn bounce(&mut self) -> &mut Self {
        self.bounce_at(self.last_tick)
    }

    pub fn burst(&mut self, count: Option<usize>) -> &mut Self {
        if self.opts.lite {
            return self;
        }
        let count = count.unwrap_or(20);
        let mut i = 0;
        while i < count && self.confetti.len() < 60 {
            let ang = (i as f64 / count as f64) * TAU + self.rng.rand_range(-0.35, 0.35);
            let spd = self.rng.rand_range(170.0, 360.0);
            let star = self.rng.pick_bool(0.18);
            let round = !star && self.rng.pick_bool(0.3);

            let (kind, color) = if star {
                (ConfettiKind::Star, STAR_GOLD.to_string())
            } else {
                let color_idx = (self.rng.next_f64() * CONFETTI_COLORS.len() as f64) as usize;
                let c = CONFETTI_COLORS[color_idx.min(CONFETTI_COLORS.len() - 1)].to_string();
                if round {
                    (ConfettiKind::Circle, c)
                } else {
                    (ConfettiKind::Rect, c)
                }
            };

            let x = HEAD_C + ang.cos() * self.rng.rand_range(96.0, 116.0);
            let y = HEAD_C + ang.sin() * self.rng.rand_range(96.0, 116.0);
            let vx = ang.cos() * spd;
            let vy = ang.sin() * spd - self.rng.rand_range(20.0, 75.0);
            let life = 0.0;
            let max = self.rng.rand_range(0.45, 0.85);
            let r = if star {
                self.rng.rand_range(4.0, 7.0)
            } else {
                self.rng.rand_range(3.5, 8.0)
            };
            let rot = self.rng.rand_range(0.0, 360.0);
            let vr = self.rng.rand_range(-260.0, 260.0);
            let stretch = if !star && !round { 1.9 } else { 1.0 };

            self.confetti.push(ConfettiPiece {
                kind,
                color,
                x,
                y,
                vx,
                vy,
                life,
                max,
                r,
                rot,
                vr,
                stretch,
                transform: String::new(),
                opacity: 0.0,
            });
            i += 1;
        }
        self
    }

    pub fn set_style_sketch(&mut self, sketch: f64) -> &mut Self {
        self.style_sketch = sketch;
        self
    }

    pub fn reset_idle(&mut self) {
        self.last_activity = self.last_tick;
    }

    pub fn emotion_id(&self) -> &str {
        &self.emotion_id
    }

    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    pub fn last_snapshot(&self) -> Option<&FrameSnapshot> {
        self.last_snapshot.as_ref()
    }

    pub fn render_static(&mut self, now_ms: f64) -> &Frame {
        self.trans_dur = 0.0;
        self.ring_spring.x = 1.0;
        self.ring_spring.v = 0.0;
        self.open_spring.x = self.def.openness;
        self.open_spring.v = 0.0;
        let seq = self.seq.take();
        self.tick(now_ms);
        self.seq = seq;
        &self.frame
    }

    pub fn tick(&mut self, now_ms: f64) -> &Frame {
        self.tick_with_output(now_ms, false)
    }

    /// Advance and expose numeric path geometry without constructing SVG strings.
    ///
    /// The returned frame keeps `PathVerb` and `Transform` values populated, while
    /// path `d` and raw-transform strings are empty. Use `tick` when snapshots or
    /// SVG serialization are required.
    pub fn tick_fast(&mut self, now_ms: f64) -> &Frame {
        self.tick_with_output(now_ms, true)
    }

    fn tick_with_output(&mut self, now_ms: f64, fast_output: bool) -> &Frame {
        self.fast_output = fast_output;
        self.dt = if self.last_tick > 0.0 {
            clamp((now_ms - self.last_tick) / 1000.0, 0.001, 0.05)
        } else {
            1.0 / 60.0
        };
        self.last_tick = now_ms;

        if let Some(ref idle) = self.opts.idle.clone() {
            if self.active {
                let elapsed = now_ms - self.last_activity;
                let cur = self.emotion_id.clone();
                if elapsed >= idle.sleep_after {
                    if cur != idle.sleep_id {
                        self.set_emotion_internal(&idle.sleep_id, true, now_ms);
                    }
                } else if elapsed >= idle.standby_after {
                    if cur != idle.standby_id && cur != idle.sleep_id {
                        self.set_emotion_internal(&idle.standby_id, true, now_ms);
                    }
                }
            }
        }

        let pose = self.compose(now_ms, 0);
        self.apply_pose(&pose, now_ms);
        self.last_pose = Some(pose);
        &self.frame
    }

    fn compose(&mut self, now: f64, depth: usize) -> Pose {
        let t = now - self.emo_start;

        let mut pose = if let Some(ref mut seq) = self.seq {
            let last_idx = seq.frames.len() - 1;
            let last_frame = &seq.frames[last_idx];
            if t >= last_frame.at {
                if !seq.done {
                    seq.done = true;
                    match &seq.settle {
                        SettleMode::Base => {
                            self.prev_pose = self
                                .last_pose
                                .clone()
                                .or_else(|| Some(last_frame.pose.clone()));
                            self.trans_start = now;
                            self.trans_dur = self.def.transition;
                            self.seq = None;
                            self.def.base.clone()
                        }
                        SettleMode::Next(next_id) => {
                            let next = next_id.clone();
                            self.set_emotion_internal(&next, true, now);
                            if depth < 4 {
                                return self.compose(now, depth + 1);
                            } else {
                                self.def.base.clone()
                            }
                        }
                        SettleMode::Hold => last_frame.pose.clone(),
                    }
                } else {
                    last_frame.pose.clone()
                }
            } else if t <= seq.frames[0].at {
                seq.frames[0].pose.clone()
            } else {
                let mut found = None;
                for i in 0..seq.frames.len() - 1 {
                    let a = &seq.frames[i];
                    let b = &seq.frames[i + 1];
                    if t >= a.at && t < b.at {
                        let k = ease_in_out_cubic((t - a.at) / (b.at - a.at));
                        found = Some(lerp_pose(&a.pose, &b.pose, k));
                        break;
                    }
                }
                found.unwrap_or_else(|| last_frame.pose.clone())
            }
        } else {
            self.def.base.clone()
        };

        // Built-in breathing
        let br = pose.body.breathe;
        if br != 0.0 {
            let ph = TAU * now / 3600.0;
            pose.body.scale += br * ph.sin();
            pose.body.y += br * 55.0 * (ph + 0.6).sin();
        }

        // Animators
        let seed = self.seed_val;
        for a in &self.def.anims {
            apply_anim(&mut pose, a, t, seed);
        }

        pose.body.sketch = pose.body.sketch.max(self.style_sketch);

        let dt = self.dt;

        // Pool rotation
        if self.active && now >= self.pool_next {
            if self.def.pool.len() > 1 {
                let pool_len = self.def.pool.len();
                let rand_offset = self.rng.rand_range(0.0, (pool_len - 1) as f64).floor() as usize;
                self.pool_pos = (self.pool_pos + 1 + rand_offset) % pool_len;
                let next_expr = self.def.pool[self.pool_pos];
                let speed = self.def.pool_speed;
                self.set_expr(next_expr, speed);
            }
            self.pool_next = now + self.rng.rand_range(self.def.pool_ms.0, self.def.pool_ms.1);
        }

        // Blink schedule
        if self.active && self.def.blink_ms.is_some() && now >= self.blink_next {
            self.blink_now(now);
            if let Some((min, max)) = self.def.blink_ms {
                self.blink_next = now + self.rng.rand_range(min, max);
            }
        }

        let mut open_key = None;
        while let Some(first) = self.blink_q.first() {
            if now >= first.at {
                open_key = Some(first.v);
                self.blink_q.remove(0);
            } else {
                break;
            }
        }

        self.open_spring.t = if let Some(ok) = open_key {
            ok
        } else if !self.blink_q.is_empty() {
            self.open_spring.t
        } else {
            self.def.openness
        };

        // Antics
        if self.active && self.def.antics && now >= self.antic_next {
            if self.spin_spring.is_none() && self.bounce_at < 0.0 {
                let pick = self.rng.next_f64();
                if pick < 0.45 {
                    self.spin_internal(Some(1.0), None);
                } else if pick < 0.8 {
                    self.bounce();
                } else {
                    self.blink_now(now);
                }
            }
            self.antic_next = now + self.rng.rand_range(9000.0, 18000.0);
        }

        // Springs step
        let steps = 1.0_f64.max((dt / (1.0 / 120.0)).ceil()) as usize;
        let j = dt / (steps as f64);
        for _ in 0..steps {
            self.ring_spring.step(self.ring_speed, 1.0, j);
            self.open_spring.step(26.0, 1.0, j);
            if let Some(ref mut spin) = self.spin_spring {
                spin.step(6.2, 1.0, j);
                if (spin.t - spin.x).abs() < 0.01 && spin.v.abs() < 0.05 {
                    self.spin_spring = None;
                }
            }
        }

        pose.body.yaw = self.spin_spring.as_ref().map(|s| s.x).unwrap_or(0.0);

        // Bounce
        if self.bounce_at >= 0.0 {
            let be = (now - self.bounce_at) / 1000.0;
            if be >= BOUNCE_TOTAL {
                self.bounce_at = -1.0;
            } else {
                let mut acc = 0.0;
                let mut bi = 0;
                while bi < BOUNCE_SEGS.len() && be >= acc + BOUNCE_SEGS[bi].d {
                    acc += BOUNCE_SEGS[bi].d;
                    bi += 1;
                }
                let seg = BOUNCE_SEGS[bi.min(BOUNCE_SEGS.len() - 1)];
                let bn = (be - acc) / seg.d;
                pose.body.y += -4.0 * seg.h * bn * (1.0 - bn);
            }
        }

        // Eye ring interpolation
        if self.ring_spring.x < 0.999 || self.ring_spring.v.abs() > 0.001 {
            let rs = clamp(self.ring_spring.x, 0.0, 1.35);
            self.ring_cur = [
                lerp_ring(&self.ring_src[0], &self.ring_dst[0], rs),
                lerp_ring(&self.ring_src[1], &self.ring_dst[1], rs),
            ];
        } else {
            self.ring_cur = self.ring_dst.clone();
        }
        pose.left.ring = Some(self.ring_cur[0].clone());
        pose.right.ring = Some(self.ring_cur[1].clone());

        // Gaze smoothing
        let k = 1.0 - (-5.66 * dt).exp();
        let gx = if self.def.gaze { self.gaze.tx } else { 0.0 };
        let gy = if self.def.gaze { self.gaze.ty } else { 0.0 };
        self.gaze.x += (gx - self.gaze.x) * k;
        self.gaze.y += (gy - self.gaze.y) * k;
        pose.left.look_x += self.gaze.x;
        pose.right.look_x += self.gaze.x;
        pose.left.look_y += self.gaze.y;
        pose.right.look_y += self.gaze.y;

        if self.def.gaze {
            let w = now / 1000.0;
            pose.left.look_x += 1.4 * (0.42 * w).sin() + 0.5 * (1.0 * w).sin();
            pose.right.look_x += 1.4 * (0.42 * w + 1.0).sin() + 0.5 * (1.0 * w + 2.0).sin();
            pose.left.look_y += 0.9 * (0.58 * w).sin();
            pose.right.look_y += 0.9 * (0.58 * w + 1.0).sin();
        }

        // Eye scale
        if (self.opts.eye_scale - 1.0).abs() > 1e-6 {
            pose.left.scale_x *= self.opts.eye_scale;
            pose.left.scale_y *= self.opts.eye_scale;
            pose.right.scale_x *= self.opts.eye_scale;
            pose.right.scale_y *= self.opts.eye_scale;
        }

        // Theme color override
        if let Some((ref body_col, ref eye_col)) = self.theme {
            pose.body.color = body_col.clone();
            if pose.left.color == "#1A1A1A" {
                pose.left.color = eye_col.clone();
            }
            if pose.right.color == "#1A1A1A" {
                pose.right.color = eye_col.clone();
            }
        }

        // Openness & clamp
        let open_s = clamp(self.open_spring.x, 0.02, 1.5);
        pose.left.open = clamp(pose.left.open, 0.0, 1.3) * open_s;
        pose.right.open = clamp(pose.right.open, 0.0, 1.3) * open_s;
        pose.left.scale_x = pose.left.scale_x.max(0.05);
        pose.left.scale_y = pose.left.scale_y.max(0.05);
        pose.right.scale_x = pose.right.scale_x.max(0.05);
        pose.right.scale_y = pose.right.scale_y.max(0.05);

        // Emotion transition lerp
        let tt = now - self.trans_start;
        if self.trans_dur > 0.0 && tt < self.trans_dur {
            if let Some(ref prev) = self.prev_pose {
                pose = lerp_pose(prev, &pose, ease_in_out_cubic(tt / self.trans_dur));
            }
        }

        pose
    }

    fn make_planes(&mut self) {
        let base = self.rng.rand_range(-0.85, 0.85);
        self.planes = vec![Plane {
            tilt: self.rng.rand_range(0.16, 0.5),
            roll: base + self.rng.rand_range(-0.12, 0.12),
        }];
        self.plane_g = self.rng.rand_range(3.0, 5.0).round() as usize;
        self.base_hue = self.rng.rand_range(0.0, 360.0);
        self.spawn_idx = 0;
    }

    fn spawn_trail(&mut self, lam0: f64, dir: f64) {
        if self.trails.len() > 8 {
            return;
        }
        let pl = if !self.planes.is_empty() {
            self.planes[0].clone()
        } else {
            Plane {
                tilt: 0.0,
                roll: 0.0,
            }
        };
        let tier_step = 38.0 / (self.plane_g.saturating_sub(1).max(1) as f64);
        let rw = if self.plane_g <= 3 {
            self.rng.rand_range(8.0, 10.5)
        } else if self.plane_g == 4 {
            self.rng.rand_range(6.6, 8.6)
        } else {
            self.rng.rand_range(5.6, 7.4)
        };

        let lam_vel = dir * self.rng.rand_range(0.5, 1.1);
        let tilt = pl.tilt + self.rng.rand_range(-0.04, 0.04);
        let roll = pl.roll + self.rng.rand_range(-0.05, 0.05);
        let rad = 116.0 + (self.spawn_idx as f64) * tier_step + self.rng.rand_range(-1.5, 1.5);
        let rad_vel = self.rng.rand_range(0.0, 2.5);
        let follow = self.rng.rand_range(0.74, 0.94);
        let carry = 0.0;
        let arc = self.rng.rand_range(2.2, 3.4);
        let hue = self.base_hue
            + 360.0 * (self.spawn_idx as f64) / (self.plane_g.max(1) as f64)
            + self.rng.rand_range(-14.0, 14.0);

        let hue_span =
            self.rng.rand_range(45.0, 95.0) * if self.rng.next_f64() < 0.5 { 1.0 } else { -1.0 };
        let hue_vel =
            self.rng.rand_range(18.0, 42.0) * if self.rng.next_f64() < 0.5 { 1.0 } else { -1.0 };

        self.spawn_idx += 1;

        let o = OrbitParams {
            lam: lam0,
            lam_vel,
            tilt,
            roll,
            rad,
            rad_vel,
            follow,
            carry,
            arc,
            r: rw,
            hue,
            hue_span,
            hue_vel,
        };

        self.trails.push(Trail {
            orbit_mode: false,
            r: rw,
            life: 0.0,
            ret: 0.0,
            o,
            hist: Vec::new(),
            back_d: String::new(),
            front_d: String::new(),
            opacity: 0.0,
            stops: default_trail_stops(),
            x1: 0.0,
            y1: 0.0,
            x2: 0.0,
            y2: 0.0,
        });
    }

    fn spawn_orbit(&mut self, idx: usize) {
        if self.trails.len() > 8 {
            return;
        }
        let lam = self.rng.rand_range(0.0, TAU);
        let lam_vel_sign = if self.rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
        let lam_vel = lam_vel_sign * self.rng.rand_range(1.7, 2.3);
        let tilt = self.rng.rand_range(0.1, 0.22);
        let roll = self.rng.rand_range(-0.12, 0.12);
        let rad = 124.0 + (idx as f64) * 16.0;
        let rad_vel = 0.0;
        let follow = 0.8;
        let carry = 0.0;
        let arc = self.rng.rand_range(2.4, 3.2);
        let r = self.rng.rand_range(5.5, 7.0);
        let hue = self.rng.rand_range(0.0, 360.0);

        let hue_span =
            self.rng.rand_range(45.0, 95.0) * if self.rng.next_f64() < 0.5 { 1.0 } else { -1.0 };
        let hue_vel =
            self.rng.rand_range(18.0, 42.0) * if self.rng.next_f64() < 0.5 { 1.0 } else { -1.0 };

        let o = OrbitParams {
            lam,
            lam_vel,
            tilt,
            roll,
            rad,
            rad_vel,
            follow,
            carry,
            arc,
            r,
            hue,
            hue_span,
            hue_vel,
        };

        self.trails.push(Trail {
            orbit_mode: true,
            r,
            life: 0.0,
            ret: 0.0,
            o,
            hist: Vec::new(),
            back_d: String::new(),
            front_d: String::new(),
            opacity: 0.0,
            stops: default_trail_stops(),
            x1: 0.0,
            y1: 0.0,
            x2: 0.0,
            y2: 0.0,
        });
    }

    fn orbit_point(o: &OrbitParams, lam: f64) -> HistPoint {
        let hx = o.rad * lam.sin();
        let hy = -o.rad * lam.cos() * o.tilt.sin();
        let ca = o.roll.cos();
        let sa = o.roll.sin();
        HistPoint {
            x: HEAD_C + hx * ca - hy * sa,
            y: HEAD_C + hx * sa + hy * ca,
            z: lam.cos() * o.tilt.cos(),
            l: lam,
        }
    }

    fn build_trail(pts: &[HistPoint], width: f64) -> (String, String) {
        let n = pts.len();
        if n < 2 {
            return (String::new(), String::new());
        }
        let mut nx = Vec::with_capacity(n);
        let mut ny = Vec::with_capacity(n);
        for e in 0..n {
            let p0 = if e > 0 { &pts[e - 1] } else { &pts[0] };
            let p1 = if e < n - 1 { &pts[e + 1] } else { &pts[n - 1] };
            let mut dx = p1.x - p0.x;
            let mut dy = p1.y - p0.y;
            let mut h = dx.hypot(dy);
            if h == 0.0 {
                h = 1.0;
            }
            dx /= h;
            dy /= h;
            let d = width * (0.5 + (e as f64 / (n - 1) as f64) * 0.5) / 2.0;
            nx.push(-dy * d);
            ny.push(dx * d);
        }

        let cap = |idx: usize| -> String {
            let hw = nx[idx].hypot(ny[idx]).max(0.2);
            format!("A{} {} 0 0 0 ", r2(hw), r2(hw))
        };

        let seg = |a: usize, b: usize| -> String {
            let mut s = String::new();
            for k in a..=b {
                let prefix = if k == a { "M" } else { "L" };
                s.push_str(&format!(
                    "{}{} {}",
                    prefix,
                    r2(pts[k].x + nx[k]),
                    r2(pts[k].y + ny[k])
                ));
            }
            if b == n - 1 {
                s.push_str(&cap(b));
            } else {
                s.push('L');
            }
            for k in (a..=b).rev() {
                let prefix = if k == b { "" } else { "L" };
                s.push_str(&format!(
                    "{}{} {}",
                    prefix,
                    r2(pts[k].x - nx[k]),
                    r2(pts[k].y - ny[k])
                ));
            }
            if a == 0 {
                s.push_str(&cap(0));
                s.push_str(&format!(
                    "{} {}",
                    r2(pts[0].x + nx[0]),
                    r2(pts[0].y + ny[0])
                ));
            }
            s.push('Z');
            s
        };

        let mut front = String::new();
        let mut back = String::new();
        let mut d0 = 0;
        while d0 < n {
            let is_f = pts[d0].z >= 0.0;
            let mut i2 = d0;
            while i2 + 1 < n && (pts[i2 + 1].z >= 0.0) == is_f {
                i2 += 1;
            }
            let a2 = if d0 > 0 { d0 - 1 } else { 0 };
            let b2 = (i2 + 1).min(n - 1);
            if b2 > a2 {
                let str_seg = seg(a2, b2);
                if is_f {
                    front.push_str(&str_seg);
                } else {
                    back.push_str(&str_seg);
                }
            }
            d0 = i2 + 1;
        }

        (front, back)
    }

    fn apply_pose(&mut self, pose: &Pose, now: f64) {
        let fast_output = self.fast_output;
        let reuse_numeric_frame = fast_output && self.opts.lite;
        let dt = if self.prev_now > 0.0 {
            clamp((now - self.prev_now) / 1000.0, 0.001, 0.05)
        } else {
            1.0 / 60.0
        };
        self.prev_now = now;

        if reuse_numeric_frame {
            self.fast_frame_cursor = 0;
        } else {
            self.frame.clear();
        }

        let b = &pose.body;
        let face = self.shape_data.face;
        let sil_min_y = self.sil_profile.sil_min_y;
        let sil_max_y = self.sil_profile.sil_max_y;

        // Head transform
        let head_tf_str = if fast_output {
            String::new()
        } else {
            format!(
                "translate({} {}) rotate({}) scale({}) translate({} {})",
                r2(HEAD_C + b.x),
                r2(HEAD_C + b.y),
                r2(b.rotate),
                r2(b.scale),
                r2(-HEAD_C),
                r2(-HEAD_C)
            )
        };

        let body_tf = Transform::translate((HEAD_C + b.x) as f32, (HEAD_C + b.y) as f32)
            .multiply(&Transform::rotate_deg(b.rotate as f32))
            .multiply(&Transform::scale(b.scale as f32, b.scale as f32))
            .multiply(&Transform::translate(-HEAD_C as f32, -HEAD_C as f32));

        let head_fill = if b.sketch > 0.5 {
            Paint::None
        } else {
            let (stop_a, stop_b, stop_c) = if fast_output {
                (
                    shade_color_numeric(&b.color, 0.22),
                    Color::from_hex_numeric(&b.color),
                    shade_color_numeric(&b.color, -0.12),
                )
            } else {
                let stop_a = shade(&b.color, 0.22);
                let stop_b = b.color.clone();
                let stop_c = shade(&b.color, -0.12);
                (
                    Color::from_hex(&stop_a),
                    Color::from_hex(&stop_b),
                    Color::from_hex(&stop_c),
                )
            };
            Paint::RadialGradient(RadialGradient {
                cx: 0.38,
                cy: 0.32,
                r: 0.75,
                stops: vec![
                    GradientStop {
                        offset: 0.0,
                        color: stop_a,
                        raw_color: if fast_output {
                            String::new()
                        } else {
                            shade(&b.color, 0.22)
                        },
                    },
                    GradientStop {
                        offset: 0.62,
                        color: stop_b,
                        raw_color: if fast_output {
                            String::new()
                        } else {
                            b.color.clone()
                        },
                    },
                    GradientStop {
                        offset: 1.0,
                        color: stop_c,
                        raw_color: if fast_output {
                            String::new()
                        } else {
                            shade(&b.color, -0.12)
                        },
                    },
                ],
            })
        };

        let head_stroke = if b.sketch > 0.5 {
            Some(Stroke {
                color: if fast_output {
                    shade_color_numeric(&b.color, -0.6)
                } else {
                    Color::from_hex(&shade(&b.color, -0.6))
                },
                width: 2.0,
                opacity: 0.85,
            })
        } else {
            None
        };

        let head_path = if fast_output {
            Path::new()
        } else {
            Path::from_ring(self.shape_data.ring)
        };

        // Update eyes
        let update_eye = |eye_state: &mut EyeState,
                          pose_eye: &EyePose,
                          k: usize,
                          sketch: f64,
                          yaw: f64,
                          sil_profile: &SilProfile,
                          _base_c: &[Point; 2]|
         -> (Option<PathElement>, EyeSnapshot) {
            if let Some(ref ring) = pose_eye.ring {
                eye_state.ring = Some(ring.clone());
                eye_state.centroid = centroid(ring);
                if !fast_output {
                    eye_state.d = ring_path_str(ring);
                }
            }

            let base = eye_state.centroid;
            let open = clamp(pose_eye.open, 0.02, 2.4);
            let sy = clamp(pose_eye.scale_y * open * face.eye, 0.02, 2.4);
            let sx_base = pose_eye.scale_x * face.eye;

            let half_h = EYE_HALF * sy + 2.0;
            let mut ey0 =
                HEAD_C + face.y + (base.y - HEAD_C) * face.sy + pose_eye.y + pose_eye.look_y;
            ey0 = clamp(ey0, sil_min_y + half_h, sil_max_y - half_h);

            let sil = sil_profile.sil_at(ey0);
            let cx0 = (sil.0 + sil.1) / 2.0;
            let hw = ((sil.1 - sil.0) / 2.0).max(12.0);

            let ox = face.x + (base.x - HEAD_C) * face.sx + pose_eye.x + pose_eye.look_x;
            let theta = clamp(ox / hw, -1.15, 1.15);
            let total = theta + yaw;
            let cn = total.cos();

            let visible = cn > 0.02;

            let fill_color = if fast_output {
                String::new()
            } else if sketch > 0.5 {
                "none".to_string()
            } else {
                pose_eye.color.clone()
            };
            let stroke_color = if fast_output {
                String::new()
            } else if sketch > 0.5 {
                pose_eye.color.clone()
            } else {
                "none".to_string()
            };

            if !visible {
                let snapshot = EyeSnapshot {
                    d: if fast_output {
                        String::new()
                    } else {
                        eye_state.d.clone()
                    },
                    transform: if fast_output {
                        String::new()
                    } else {
                        eye_state.last_transform.clone()
                    },
                    fill: fill_color,
                    stroke: stroke_color,
                    stroke_width: 1.6,
                    visible: false,
                };
                return (None, snapshot);
            }

            let rot_str = if fast_output {
                String::new()
            } else if pose_eye.rotate != 0.0 {
                format!(" rotate({})", r2(pose_eye.rotate))
            } else {
                String::new()
            };

            let ex = cx0 + hw * total.sin() * 0.985;
            let dy_n = (ey0 - HEAD_C) / 130.0;
            let fy = (1.0 - dy_n * dy_n * 0.22).sqrt();

            let transform_str = if fast_output {
                String::new()
            } else {
                format!(
                    "translate({} {}){} scale({} {}) translate({} {})",
                    r2(ex),
                    r2(ey0),
                    rot_str,
                    r2(sx_base * cn),
                    r2(sy * fy),
                    r2(-base.x),
                    r2(-base.y)
                )
            };
            if !fast_output {
                eye_state.last_transform = transform_str.clone();
            }

            let snapshot = EyeSnapshot {
                d: if fast_output {
                    String::new()
                } else {
                    eye_state.d.clone()
                },
                transform: transform_str.clone(),
                fill: fill_color.clone(),
                stroke: stroke_color.clone(),
                stroke_width: 1.6,
                visible: true,
            };

            let elem_tf = Transform::translate(ex as f32, ey0 as f32)
                .multiply(&Transform::rotate_deg(pose_eye.rotate as f32))
                .multiply(&Transform::scale((sx_base * cn) as f32, (sy * fy) as f32))
                .multiply(&Transform::translate(-base.x as f32, -base.y as f32));

            let elem = PathElement {
                id: if fast_output {
                    None
                } else {
                    Some(if k == 0 {
                        "eyeL".to_string()
                    } else {
                        "eyeR".to_string()
                    })
                },
                path: if fast_output {
                    Path::new()
                } else {
                    eye_state
                        .ring
                        .as_ref()
                        .map(|ring| Path::from_ring(ring))
                        .unwrap_or_default()
                },
                fill: if sketch > 0.5 {
                    Paint::None
                } else {
                    Paint::Solid(if fast_output {
                        Color::from_hex_numeric(&pose_eye.color)
                    } else {
                        Color::from_hex(&pose_eye.color)
                    })
                },
                stroke: if sketch > 0.5 {
                    Some(Stroke {
                        color: Color::from_hex(&pose_eye.color),
                        width: 1.6,
                        opacity: 1.0,
                    })
                } else {
                    None
                },
                opacity: 1.0,
                transform: elem_tf,
                raw_transform: transform_str,
                visible: true,
            };

            (Some(elem), snapshot)
        };

        let yaw = b.yaw;
        let (eye_l_elem, eye_l_snap) = update_eye(
            &mut self.eye_l,
            &pose.left,
            0,
            b.sketch,
            yaw,
            &self.sil_profile,
            &self.base_c,
        );
        let (eye_r_elem, eye_r_snap) = update_eye(
            &mut self.eye_r,
            &pose.right,
            1,
            b.sketch,
            yaw,
            &self.sil_profile,
            &self.base_c,
        );

        let mut zzz_snaps = Vec::new();
        let mut trail_snaps = Vec::new();
        let mut confetti_snaps = Vec::new();
        let mut zzz_elements = Vec::new();

        if !self.opts.lite {
            // ZZZ text
            let z_on = b.zzz > 0.0;
            for z in 0..3 {
                if !z_on {
                    self.zzz_state[z].opacity = 0.0;
                    zzz_snaps.push(self.zzz_state[z].clone());
                } else {
                    let zp = (now * 0.00033 + (z as f64) / 3.0) % 1.0;
                    let zo = (if zp < 0.18 {
                        zp / 0.18
                    } else {
                        1.0 - (zp - 0.18) / 0.82
                    }) * 0.8
                        * b.zzz;
                    let font_size = 12.0 + zp * 11.0;
                    let tx = 180.0 + zp * 34.0 + 4.0 * (zp * 9.0).sin();
                    let ty = 48.0 - zp * 42.0;
                    let rot = -10.0 + zp * 14.0;
                    let tf_str = format!("translate({} {}) rotate({})", r2(tx), r2(ty), r2(rot));

                    let snap = ZzzSnapshot {
                        opacity: r2(zo * 1000.0) / 1000.0,
                        font_size: (font_size * 10.0).round() / 10.0,
                        transform: tf_str.clone(),
                    };
                    self.zzz_state[z] = snap.clone();
                    zzz_snaps.push(snap);

                    let txt_tf = Transform::translate(tx as f32, ty as f32)
                        .multiply(&Transform::rotate_deg(rot as f32));

                    zzz_elements.push(DrawElement::Text(TextElement {
                        text: "z".to_string(),
                        font_family: "sans-serif".to_string(),
                        font_size: font_size as f32,
                        font_weight: 700,
                        font_style: "italic".to_string(),
                        fill: Color::from_hex("#A8A296"),
                        opacity: zo as f32,
                        transform: txt_tf,
                        raw_transform: tf_str,
                    }));
                }
            }

            // Trails & Orbits
            let mut d_yaw = yaw - self.prev_yaw;
            if !d_yaw.is_finite() || d_yaw.abs() > 1.2 {
                d_yaw = 0.0;
            }
            self.prev_yaw = yaw;
            let vel = d_yaw / dt;
            let fast = vel.abs() >= 0.9;
            let dir = if vel >= 0.0 { 1.0 } else { -1.0 };

            if fast && !self.was_fast {
                self.make_planes();
                self.spawn_at.clear();
                for q in 0..self.plane_g {
                    self.spawn_at
                        .push(now + (q as f64) * self.rng.rand_range(55.0, 105.0));
                }
            }
            if !fast {
                self.spawn_at.clear();
            }
            self.was_fast = fast;

            if vel.abs() >= 5.0 {
                while !self.spawn_at.is_empty() && now >= self.spawn_at[0] {
                    self.spawn_at.remove(0);
                    let lam0 = yaw - self.rng.rand_range(0.0, 0.18) * dir;
                    self.spawn_trail(lam0, dir);
                }
            }

            let orbit_want = b.orbit > 0.0;
            if orbit_want && now >= self.orbit_next_at {
                let orbit_count = self.trails.iter().filter(|t| t.orbit_mode).count();
                if orbit_count < 2 {
                    self.spawn_orbit(orbit_count);
                }
                self.orbit_next_at = now + 700.0;
            }

            // Update trails & orbits (iterating backwards)
            let mut ti = self.trails.len();
            while ti > 0 {
                ti -= 1;
                let rb = &mut self.trails[ti];
                rb.life += dt;
                let retract = if rb.orbit_mode {
                    !orbit_want
                } else {
                    !fast || rb.life > 5.0
                };
                let delta_ret = if retract { dt / 0.5 } else { -dt / 0.35 };
                rb.ret = clamp(rb.ret + delta_ret, 0.0, 1.0);
                if retract && rb.ret >= 1.0 {
                    self.trails.remove(ti);
                    continue;
                }

                let o = &mut rb.o;
                if rb.orbit_mode {
                    o.lam += o.lam_vel * dt + d_yaw * o.follow;
                } else if fast {
                    o.carry = vel * o.follow;
                    o.lam += d_yaw * o.follow + o.lam_vel * dt;
                } else {
                    o.lam += (o.carry + o.lam_vel) * dt;
                    o.carry *= (-2.6 * dt).exp();
                    o.lam_vel *= (-2.6 * dt).exp();
                }
                o.rad += o.rad_vel * dt;

                let last_l = rb.hist.last().map(|p| p.l).unwrap_or(o.lam - 0.001 * dir);
                let dl = o.lam - last_l;
                let steps = ((dl.abs() / 0.09).ceil() as usize).clamp(1, 24);
                for st in 1..=steps {
                    let frac = st as f64 / steps as f64;
                    rb.hist.push(Self::orbit_point(o, last_l + dl * frac));
                }
                if rb.hist.is_empty() {
                    rb.hist.push(Self::orbit_point(o, o.lam));
                }

                let span = o.arc * (1.0 - rb.ret * rb.ret * (3.0 - 2.0 * rb.ret));
                while rb.hist.len() > 2 && (o.lam - rb.hist[0].l).abs() > span {
                    rb.hist.remove(0);
                }
                let over = (o.lam - rb.hist[0].l).abs() - span;
                if rb.hist.len() >= 2 && over > 0.0 {
                    let sign = if o.lam - rb.hist[0].l >= 0.0 {
                        1.0
                    } else {
                        -1.0
                    };
                    let tl = rb.hist[0].l + sign * over;
                    rb.hist[0] = Self::orbit_point(o, tl);
                }
                if rb.hist.len() > 48 {
                    let cut = rb.hist.len() - 48;
                    rb.hist.drain(0..cut);
                }

                let z_head = o.lam.cos() * o.tilt.cos();
                let pz = 0.72 + 0.28 * clamp(z_head, 0.0, 1.0);
                let mut grow = (rb.life / 0.34).min(1.0);
                grow = grow * grow * (3.0 - 2.0 * grow);
                let width = rb.r * pz * 1.7 * grow * (1.0 - 0.72 * rb.ret * rb.ret);
                let fade = format!("{:.3}", (rb.life / 0.26).min(1.0))
                    .parse::<f64>()
                    .unwrap_or(1.0);

                if rb.hist.len() < 2 || width < 0.5 {
                    rb.opacity = 0.0;
                    continue;
                }
                let (front_d, back_d) = Self::build_trail(&rb.hist, width);
                rb.front_d = front_d;
                rb.back_d = back_d;
                rb.opacity = fade;

                let hue = rb.o.hue + rb.o.hue_vel * rb.life;
                rb.stops.clear();
                for si in 0..5 {
                    let frac = si as f64 / 4.0;
                    let hv = hue + frac * rb.o.hue_span;
                    let deg = (((hv % 360.0) + 360.0) % 360.0).round() as i64;
                    let lit = (56.0 + 11.0 * frac).round() as i64;
                    rb.stops.push(StopSnapshot {
                        offset: format!("{:.3}", frac),
                        color: format!("hsl({} 56% {}%)", deg, lit),
                    });
                }

                if let (Some(tail), Some(head_p)) = (rb.hist.first(), rb.hist.last()) {
                    rb.x1 = format!("{:.1}", tail.x).parse::<f64>().unwrap_or(0.0);
                    rb.y1 = format!("{:.1}", tail.y).parse::<f64>().unwrap_or(0.0);
                    rb.x2 = format!("{:.1}", head_p.x).parse::<f64>().unwrap_or(0.0);
                    rb.y2 = format!("{:.1}", head_p.y).parse::<f64>().unwrap_or(0.0);
                }
            }

            for rb in &self.trails {
                trail_snaps.push(TrailSnapshot {
                    back_d: rb.back_d.clone(),
                    front_d: rb.front_d.clone(),
                    opacity: rb.opacity,
                    stops: rb.stops.clone(),
                    x1: rb.x1,
                    y1: rb.y1,
                    x2: rb.x2,
                    y2: rb.y2,
                });
            }

            // Update confetti
            let mut ci = 0;
            while ci < self.confetti.len() {
                let pc = &mut self.confetti[ci];
                pc.life += dt;
                if pc.life >= pc.max {
                    self.confetti.remove(ci);
                    continue;
                }
                pc.x += pc.vx * dt;
                pc.y += pc.vy * dt;
                let drag = 0.94_f64.powf(60.0 * dt);
                pc.vx *= drag;
                pc.vy = pc.vy * drag + 40.0 * dt;
                pc.rot += pc.vr * dt;
                let u = pc.life / pc.max;
                let fd = if u < 0.1 {
                    u / 0.1
                } else {
                    (1.0 - (u - 0.1) / 0.9).powf(1.7)
                };
                let sz = (pc.r * (1.0 - 0.4 * u)).max(0.5);

                let tf_str = format!(
                    "translate({} {}) rotate({}) scale({} {})",
                    r2(pc.x),
                    r2(pc.y),
                    r2(pc.rot),
                    r2(sz),
                    r2(sz * pc.stretch)
                );
                pc.transform = tf_str.clone();
                pc.opacity = (fd * 1000.0).round() / 1000.0;

                let (kind_str, d_val) = match pc.kind {
                    ConfettiKind::Star => ("path".to_string(), Some(STAR_PATH.to_string())),
                    ConfettiKind::Circle => ("circle".to_string(), None),
                    ConfettiKind::Rect => ("rect".to_string(), None),
                };

                let fill_str = match pc.kind {
                    ConfettiKind::Star => STAR_GOLD.to_string(),
                    _ => pc.color.clone(),
                };

                confetti_snaps.push(ConfettiSnapshot {
                    kind: kind_str,
                    transform: tf_str,
                    opacity: r2(fd * 1000.0) / 1000.0,
                    fill: fill_str,
                    d: d_val,
                });

                ci += 1;
            }
        }

        // fxBack is below the head in the SVG; render each ribbon back half first.
        if !self.opts.lite {
            for (index, trail) in self.trails.iter().enumerate() {
                self.frame.elements.push(DrawElement::Path(PathElement {
                    id: if fast_output {
                        None
                    } else {
                        Some(format!("trail-back-{index}"))
                    },
                    path: path_from_svg_d(&trail.back_d, !fast_output),
                    fill: trail_paint(trail, fast_output),
                    stroke: None,
                    opacity: trail.opacity as f32,
                    transform: Transform::identity(),
                    raw_transform: String::new(),
                    visible: trail.opacity > 0.0 && !trail.back_d.is_empty(),
                }));
            }
        }

        // Add Head to frame
        let head_element = PathElement {
            id: if fast_output {
                None
            } else {
                Some("head".to_string())
            },
            path: head_path,
            fill: head_fill,
            stroke: head_stroke,
            opacity: 1.0,
            transform: body_tf,
            raw_transform: head_tf_str.clone(),
            visible: true,
        };
        self.store_path_element(head_element, self.shape_data.ring);

        if let Some(l) = eye_l_elem {
            if let Some(ring) = pose.left.ring.as_deref() {
                self.store_path_element(l, ring);
            }
        } else if fast_output && reuse_numeric_frame {
            self.store_path_element(
                PathElement {
                    id: None,
                    path: Path::new(),
                    fill: Paint::None,
                    stroke: None,
                    opacity: 0.0,
                    transform: Transform::identity(),
                    raw_transform: String::new(),
                    visible: false,
                },
                pose.left.ring.as_deref().unwrap_or_default(),
            );
        }
        if let Some(r) = eye_r_elem {
            if let Some(ring) = pose.right.ring.as_deref() {
                self.store_path_element(r, ring);
            }
        } else if fast_output && reuse_numeric_frame {
            self.store_path_element(
                PathElement {
                    id: None,
                    path: Path::new(),
                    fill: Paint::None,
                    stroke: None,
                    opacity: 0.0,
                    transform: Transform::identity(),
                    raw_transform: String::new(),
                    visible: false,
                },
                pose.right.ring.as_deref().unwrap_or_default(),
            );
        }

        if !self.opts.lite {
            // fxFront layers are ordered after both eyes: front ribbons, confetti, then zzz.
            for (index, trail) in self.trails.iter().enumerate() {
                self.frame.elements.push(DrawElement::Path(PathElement {
                    id: if fast_output {
                        None
                    } else {
                        Some(format!("trail-front-{index}"))
                    },
                    path: path_from_svg_d(&trail.front_d, !fast_output),
                    fill: trail_paint(trail, fast_output),
                    stroke: None,
                    opacity: trail.opacity as f32,
                    transform: Transform::identity(),
                    raw_transform: String::new(),
                    visible: trail.opacity > 0.0 && !trail.front_d.is_empty(),
                }));
            }

            for (index, piece) in self.confetti.iter().enumerate() {
                let rounded_x = r2(piece.x) as f32;
                let rounded_y = r2(piece.y) as f32;
                let rounded_rotation = r2(piece.rot) as f32;
                let u = piece.life / piece.max;
                let sz = (piece.r * (1.0 - 0.4 * u)).max(0.5);
                let rounded_sx = r2(sz) as f32;
                let rounded_sy = r2(sz * piece.stretch) as f32;
                let transform = Transform::translate(rounded_x, rounded_y)
                    .multiply(&Transform::rotate_deg(rounded_rotation))
                    .multiply(&Transform::scale(rounded_sx, rounded_sy));
                let fill = match piece.kind {
                    ConfettiKind::Star => STAR_GOLD,
                    ConfettiKind::Circle | ConfettiKind::Rect => piece.color.as_str(),
                };
                self.frame.elements.push(DrawElement::Path(PathElement {
                    id: if fast_output {
                        None
                    } else {
                        Some(format!("confetti-{index}"))
                    },
                    path: confetti_path(&piece.kind, !fast_output),
                    fill: Paint::Solid(if fast_output {
                        Color::from_hex_numeric(fill)
                    } else {
                        Color::from_hex(fill)
                    }),
                    stroke: None,
                    opacity: piece.opacity as f32,
                    transform,
                    raw_transform: if fast_output {
                        String::new()
                    } else {
                        piece.transform.clone()
                    },
                    visible: piece.opacity > 0.0,
                }));
            }
            self.frame.elements.extend(zzz_elements);
        }

        if fast_output {
            self.last_snapshot = None;
            return;
        }

        // Build snapshot
        let stops = if b.sketch > 0.5 {
            Vec::new()
        } else {
            vec![
                StopSnapshot {
                    offset: "0%".to_string(),
                    color: shade(&b.color, 0.22),
                },
                StopSnapshot {
                    offset: "62%".to_string(),
                    color: b.color.clone(),
                },
                StopSnapshot {
                    offset: "100%".to_string(),
                    color: shade(&b.color, -0.12),
                },
            ]
        };

        let head_snap = HeadSnapshot {
            d: ring_path_str(self.shape_data.ring),
            transform: head_tf_str,
            fill: if b.sketch > 0.5 {
                "none".to_string()
            } else {
                "url(#eb0g)".to_string()
            },
            stroke: if b.sketch > 0.5 {
                shade(&b.color, -0.6)
            } else {
                "none".to_string()
            },
            stroke_width: 2.0,
            stroke_opacity: if b.sketch > 0.5 { Some(0.85) } else { None },
            stops,
        };

        self.last_snapshot = Some(FrameSnapshot {
            frame_idx: 0,
            time_ms: now,
            head: head_snap,
            eye_l: eye_l_snap,
            eye_r: eye_r_snap,
            zzz: zzz_snaps,
            trails: trail_snaps,
            confetti: confetti_snaps,
        });
    }
}
