use grok_ball::frame::{DrawElement, FrameSnapshot, Paint, PathVerb, Transform};
use grok_ball::{Ball, BallOptions, IdleConfig, ShapeKind, STAR_PATH};
use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct Fixture {
    scenario: String,
    opts: FixtureOpts,
    step_ms: f64,
    #[serde(default)]
    duration_ms: f64,
    #[serde(default)]
    events: Vec<ScenarioEvent>,
    sampled_frames: Vec<FrameSnapshot>,
    #[serde(default)]
    digests: Vec<FrameDigest>,
}

#[derive(Debug, Deserialize)]
struct ScenarioEvent {
    time_ms: f64,
    #[serde(rename = "type")]
    event_type: String,
    emotion: Option<String>,
    count: Option<usize>,
    turns: Option<f64>,
    dir: Option<f64>,
    active: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct FrameDigest {
    #[allow(dead_code)]
    frame_idx: usize,
    #[allow(dead_code)]
    time_ms: f64,
    head_transform: String,
    head_d_len: usize,
    #[allow(dead_code)]
    head_fill: String,
    eye_l_transform: String,
    eye_r_transform: String,
    eye_l_d_len: usize,
    eye_r_d_len: usize,
    eye_l_visible: bool,
    eye_r_visible: bool,
    zzz_count: usize,
    trails_count: usize,
    confetti_count: usize,
}

#[derive(Debug, Deserialize)]
struct FixtureOpts {
    shape: Option<String>,
    emotion: Option<String>,
    lite: Option<bool>,
    seed: Option<u32>,
    idle: Option<FixtureIdle>,
}

#[derive(Debug, Deserialize)]
struct FixtureIdle {
    #[serde(rename = "standbyAfter")]
    standby_after: Option<f64>,
    #[serde(rename = "sleepAfter")]
    sleep_after: Option<f64>,
    #[serde(rename = "standbyId")]
    standby_id: Option<String>,
    #[serde(rename = "sleepId")]
    sleep_id: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Num(f64),
}

fn tokenize_transform(s: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || c == '(' || c == ')' || c == ',' {
            chars.next();
            continue;
        }

        if c.is_alphabetic() {
            let mut word = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                    word.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push(Token::Word(word));
        } else if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' {
            let mut num_str = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_ascii_digit()
                    || ch == '.'
                    || ch == '-'
                    || ch == '+'
                    || ch == 'e'
                    || ch == 'E'
                {
                    num_str.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            if let Ok(v) = num_str.parse::<f64>() {
                tokens.push(Token::Num(v));
            } else {
                tokens.push(Token::Word(num_str));
            }
        } else {
            chars.next();
        }
    }

    tokens
}

fn assert_transform_approx_eq(actual: &str, expected: &str, tol: f64, context: &str) {
    let actual_tokens = tokenize_transform(actual);
    let expected_tokens = tokenize_transform(expected);

    assert_eq!(
        actual_tokens.len(),
        expected_tokens.len(),
        "Token count mismatch for {context}\nActual:   {actual}\nExpected: {expected}"
    );

    for (i, (a, e)) in actual_tokens.iter().zip(expected_tokens.iter()).enumerate() {
        match (a, e) {
            (Token::Word(w1), Token::Word(w2)) => {
                assert_eq!(w1, w2, "Word token {i} mismatch for {context}");
            }
            (Token::Num(n1), Token::Num(n2)) => {
                let diff = (n1 - n2).abs();
                assert!(
                    diff <= tol,
                    "Number token {i} mismatch for {context}: actual {n1} vs expected {n2} (diff {diff} > tol {tol})\nActual:   {actual}\nExpected: {expected}"
                );
            }
            _ => {
                panic!("Token type mismatch at {i} for {context}: {a:?} vs {e:?}");
            }
        }
    }
}

fn expected_confetti_transform(transform: &str) -> Transform {
    let values: Vec<f32> = tokenize_transform(transform)
        .into_iter()
        .filter_map(|token| match token {
            Token::Num(value) => Some(value as f32),
            Token::Word(_) => None,
        })
        .collect();
    assert_eq!(
        values.len(),
        5,
        "Expected translate/rotate/scale: {transform}"
    );
    Transform::translate(values[0], values[1])
        .multiply(&Transform::rotate_deg(values[2]))
        .multiply(&Transform::scale(values[3], values[4]))
}

fn assert_transform_values(actual: Transform, expected: Transform, context: &str) {
    for (name, a, e) in [
        ("a", actual.a, expected.a),
        ("b", actual.b, expected.b),
        ("c", actual.c, expected.c),
        ("d", actual.d, expected.d),
        ("e", actual.e, expected.e),
        ("f", actual.f, expected.f),
    ] {
        assert!(
            (a - e).abs() <= 1e-3,
            "Transform {name} mismatch at {context}: {a} vs {e}"
        );
    }
}

fn assert_frame_effects(frame: &grok_ball::Frame, snap: &FrameSnapshot, context: &str) {
    let find_path = |id: &str| -> &grok_ball::PathElement {
        frame
            .elements
            .iter()
            .find_map(|element| match element {
                DrawElement::Path(path) if path.id.as_deref() == Some(id) => Some(path),
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing rendered element {id} at {context}"))
    };

    let rendered_trails = frame
        .elements
        .iter()
        .filter(|element| {
            matches!(element, DrawElement::Path(path) if path.id.as_deref().is_some_and(|id| id.starts_with("trail-")))
        })
        .count();
    assert_eq!(
        rendered_trails,
        snap.trails.len() * 2,
        "Trail element count at {context}"
    );
    for (index, expected) in snap.trails.iter().enumerate() {
        for (half, path_data) in [("back", &expected.back_d), ("front", &expected.front_d)] {
            let element = find_path(&format!("trail-{half}-{index}"));
            assert_eq!(
                element.path.d, *path_data,
                "Trail {index} {half} geometry at {context}"
            );
            assert!(
                (element.opacity as f64 - expected.opacity).abs() <= 1e-3,
                "Trail {index} {half} opacity at {context}: {} vs {}",
                element.opacity,
                expected.opacity
            );
            assert_eq!(
                element.visible,
                expected.opacity > 0.0 && !path_data.is_empty(),
                "Trail {index} {half} visibility at {context}"
            );
            let Paint::LinearGradient(gradient) = &element.fill else {
                panic!("Trail {index} {half} must use a linear gradient at {context}");
            };
            for (name, actual, expected) in [
                ("x1", gradient.x1 as f64, expected.x1),
                ("y1", gradient.y1 as f64, expected.y1),
                ("x2", gradient.x2 as f64, expected.x2),
                ("y2", gradient.y2 as f64, expected.y2),
            ] {
                assert!(
                    (actual - expected).abs() <= 1e-3,
                    "Trail {index} {name} at {context}: {actual} vs {expected}"
                );
            }
            assert_eq!(
                gradient.stops.len(),
                expected.stops.len(),
                "Trail {index} {half} gradient stop count at {context}"
            );
            for (stop_index, (actual, expected)) in
                gradient.stops.iter().zip(&expected.stops).enumerate()
            {
                assert!(
                    (actual.offset as f64 - expected.offset.parse::<f64>().unwrap()).abs() <= 1e-4,
                    "Trail {index} stop {stop_index} offset at {context}"
                );
                assert_eq!(
                    actual.raw_color, expected.color,
                    "Trail {index} stop {stop_index} color at {context}"
                );
            }
        }
    }

    let rendered_confetti: Vec<_> = frame
        .elements
        .iter()
        .filter_map(|element| match element {
            DrawElement::Path(path)
                if path
                    .id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("confetti-")) =>
            {
                Some(path)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        rendered_confetti.len(),
        snap.confetti.len(),
        "Confetti element count at {context}"
    );
    for (index, (actual, expected)) in rendered_confetti.iter().zip(&snap.confetti).enumerate() {
        assert_eq!(
            actual.id.as_deref(),
            Some(format!("confetti-{index}").as_str()),
            "Confetti {index} order at {context}"
        );
        let Paint::Solid(color) = &actual.fill else {
            panic!("Confetti {index} must have a solid fill at {context}");
        };
        let expected_color = grok_ball::Color::from_hex(&expected.fill);
        assert_eq!(
            (color.r, color.g, color.b),
            (expected_color.r, expected_color.g, expected_color.b),
            "Confetti {index} color at {context}"
        );
        assert!(
            (actual.opacity as f64 - expected.opacity).abs() <= 1e-3,
            "Confetti {index} opacity at {context}: {} vs {}",
            actual.opacity,
            expected.opacity
        );
        assert_eq!(
            actual.raw_transform, expected.transform,
            "Confetti {index} transform coordinates at {context}"
        );
        assert_transform_values(
            actual.transform,
            expected_confetti_transform(&expected.transform),
            &format!("confetti {index} at {context}"),
        );
        match expected.kind.as_str() {
            "path" => {
                assert_eq!(
                    actual.path.d, STAR_PATH,
                    "Confetti {index} star path at {context}"
                );
            }
            "circle" => assert!(
                matches!(actual.path.verbs.first(), Some(PathVerb::MoveTo(1.0, 0.0))),
                "Confetti {index} circle shape at {context}"
            ),
            "rect" => assert!(
                actual.path.verbs.len() > 4,
                "Confetti {index} rounded rectangle shape at {context}"
            ),
            kind => panic!("Unknown confetti kind {kind} at {context}"),
        }
    }
}

fn run_golden_scenario(fixture_filename: &str) {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(fixture_filename);

    let content = fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("Failed to read fixture {}: {e}", fixture_path.display()));

    let fixture: Fixture = serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {e}", fixture_path.display()));

    let shape = match fixture.opts.shape.as_deref() {
        Some("blob") | None => ShapeKind::Blob,
        Some("wedge") => ShapeKind::Wedge,
        Some("gem") => ShapeKind::Gem,
        Some(other) => panic!("Unknown shape: {other}"),
    };

    let idle = fixture.opts.idle.map(|id| IdleConfig {
        standby_after: id.standby_after.unwrap_or(20_000.0),
        sleep_after: id.sleep_after.unwrap_or(60_000.0),
        standby_id: id.standby_id.unwrap_or_else(|| "02".to_string()),
        sleep_id: id.sleep_id.unwrap_or_else(|| "00".to_string()),
    });

    let mut ball = Ball::new(BallOptions {
        shape,
        emotion: fixture.opts.emotion.clone(),
        lite: fixture.opts.lite.unwrap_or(true),
        autostart: false,
        seed: fixture.opts.seed,
        idle,
        ..Default::default()
    });
    ball.set_active(true);

    let total_frames = if fixture.duration_ms > 0.0 {
        (fixture.duration_ms / fixture.step_ms).round() as usize
    } else {
        fixture
            .sampled_frames
            .last()
            .map(|f| f.frame_idx)
            .unwrap_or(0)
    };

    let mut sampled_idx = 0;
    let mut event_idx = 0;

    for frame_idx in 0..=total_frames {
        let time_ms = (frame_idx as f64 * fixture.step_ms * 1000.0).round() / 1000.0;

        // Apply events scheduled at or before this time
        while event_idx < fixture.events.len() && time_ms >= fixture.events[event_idx].time_ms {
            let ev = &fixture.events[event_idx];
            match ev.event_type.as_str() {
                "set_emotion" => {
                    if let Some(ref em) = ev.emotion {
                        ball.set_emotion_at(em, time_ms);
                    }
                }
                "burst" => {
                    ball.burst(ev.count);
                }
                "spin" => {
                    ball.spin(ev.turns, ev.dir);
                }
                "bounce" => {
                    ball.bounce_at(time_ms);
                }
                "set_active" => {
                    ball.set_active(ev.active.unwrap_or(true));
                }
                _ => {}
            }
            event_idx += 1;
        }

        ball.tick(time_ms);
        let snap = ball.last_snapshot().expect("snapshot exists");

        // 1. Verify every single frame via digest
        if frame_idx < fixture.digests.len() {
            let exp_d = &fixture.digests[frame_idx];
            let ctx = format!(
                "scenario {} frame {} digest (t={}ms)",
                fixture.scenario, frame_idx, time_ms
            );
            assert_transform_approx_eq(
                &snap.head.transform,
                &exp_d.head_transform,
                0.02,
                &format!("head transform at {ctx}"),
            );
            assert_transform_approx_eq(
                &snap.eye_l.transform,
                &exp_d.eye_l_transform,
                0.02,
                &format!("eye_l transform at {ctx}"),
            );
            assert_transform_approx_eq(
                &snap.eye_r.transform,
                &exp_d.eye_r_transform,
                0.02,
                &format!("eye_r transform at {ctx}"),
            );
            assert_eq!(
                snap.head.d.len(),
                exp_d.head_d_len,
                "Head path d length mismatch at {ctx}"
            );
            assert_eq!(
                snap.eye_l.d.len(),
                exp_d.eye_l_d_len,
                "Eye L path d length mismatch at {ctx}"
            );
            assert_eq!(
                snap.eye_r.d.len(),
                exp_d.eye_r_d_len,
                "Eye R path d length mismatch at {ctx}"
            );
            assert_eq!(
                snap.eye_l.visible, exp_d.eye_l_visible,
                "Eye L visible mismatch at {ctx}"
            );
            assert_eq!(
                snap.eye_r.visible, exp_d.eye_r_visible,
                "Eye R visible mismatch at {ctx}"
            );
            assert_eq!(
                snap.zzz.len(),
                exp_d.zzz_count,
                "Zzz count mismatch at {ctx}"
            );
            assert_eq!(
                snap.trails.len(),
                exp_d.trails_count,
                "Trails count mismatch at {ctx}"
            );
            assert_eq!(
                snap.confetti.len(),
                exp_d.confetti_count,
                "Confetti count mismatch at {ctx}"
            );
        }

        // 2. Verify sampled full frames (every 10th frame)
        if sampled_idx < fixture.sampled_frames.len()
            && fixture.sampled_frames[sampled_idx].frame_idx == frame_idx
        {
            let expected = &fixture.sampled_frames[sampled_idx];
            let ctx = format!(
                "scenario {} frame {} (t={}ms)",
                fixture.scenario, frame_idx, time_ms
            );
            assert_frame_effects(ball.frame(), snap, &ctx);

            // Compare head
            assert_eq!(
                snap.head.d, expected.head.d,
                "Head path d mismatch at {ctx}"
            );
            assert_transform_approx_eq(
                &snap.head.transform,
                &expected.head.transform,
                0.02,
                &format!("head transform at {ctx}"),
            );
            assert!(
                snap.head.fill.eq_ignore_ascii_case(&expected.head.fill),
                "Head fill mismatch at {ctx}"
            );
            assert!(
                snap.head.stroke.eq_ignore_ascii_case(&expected.head.stroke),
                "Head stroke mismatch at {ctx}"
            );
            assert_eq!(
                snap.head.stops.len(),
                expected.head.stops.len(),
                "Head stops len mismatch at {ctx}"
            );
            for (s_idx, (act_s, exp_s)) in snap
                .head
                .stops
                .iter()
                .zip(expected.head.stops.iter())
                .enumerate()
            {
                assert_eq!(
                    act_s.offset, exp_s.offset,
                    "Stop {s_idx} offset mismatch at {ctx}"
                );
                assert!(
                    act_s.color.eq_ignore_ascii_case(&exp_s.color),
                    "Stop {s_idx} color mismatch at {ctx}"
                );
            }

            // Compare eye_l
            assert_eq!(
                snap.eye_l.d, expected.eye_l.d,
                "Eye L path d mismatch at {ctx}"
            );
            assert_transform_approx_eq(
                &snap.eye_l.transform,
                &expected.eye_l.transform,
                0.02,
                &format!("eye_l transform at {ctx}"),
            );
            assert!(
                snap.eye_l.fill.eq_ignore_ascii_case(&expected.eye_l.fill),
                "Eye L fill mismatch at {ctx}"
            );
            assert!(
                snap.eye_l
                    .stroke
                    .eq_ignore_ascii_case(&expected.eye_l.stroke),
                "Eye L stroke mismatch at {ctx}"
            );
            assert_eq!(
                snap.eye_l.visible, expected.eye_l.visible,
                "Eye L visible mismatch at {ctx}"
            );

            // Compare eye_r
            assert_eq!(
                snap.eye_r.d, expected.eye_r.d,
                "Eye R path d mismatch at {ctx}"
            );
            assert_transform_approx_eq(
                &snap.eye_r.transform,
                &expected.eye_r.transform,
                0.02,
                &format!("eye_r transform at {ctx}"),
            );
            assert!(
                snap.eye_r.fill.eq_ignore_ascii_case(&expected.eye_r.fill),
                "Eye R fill mismatch at {ctx}"
            );
            assert!(
                snap.eye_r
                    .stroke
                    .eq_ignore_ascii_case(&expected.eye_r.stroke),
                "Eye R stroke mismatch at {ctx}"
            );
            assert_eq!(
                snap.eye_r.visible, expected.eye_r.visible,
                "Eye R visible mismatch at {ctx}"
            );

            // Compare zzz
            if !expected.zzz.is_empty() {
                assert_eq!(
                    snap.zzz.len(),
                    expected.zzz.len(),
                    "Zzz count mismatch at {ctx}"
                );
                for (z_i, (act_z, exp_z)) in snap.zzz.iter().zip(expected.zzz.iter()).enumerate() {
                    assert!(
                        (act_z.opacity - exp_z.opacity).abs() <= 0.02,
                        "Zzz {z_i} opacity mismatch at {ctx}: {} vs {}",
                        act_z.opacity,
                        exp_z.opacity
                    );
                    assert!(
                        (act_z.font_size - exp_z.font_size).abs() <= 0.5,
                        "Zzz {z_i} font_size mismatch at {ctx}: {} vs {}",
                        act_z.font_size,
                        exp_z.font_size
                    );
                    assert_transform_approx_eq(
                        &act_z.transform,
                        &exp_z.transform,
                        0.05,
                        &format!("zzz {z_i} transform at {ctx}"),
                    );
                }
            }

            // Compare trails
            if !expected.trails.is_empty() {
                assert_eq!(
                    snap.trails.len(),
                    expected.trails.len(),
                    "Trails count mismatch at {ctx}"
                );
                for (t_i, (act_t, exp_t)) in
                    snap.trails.iter().zip(expected.trails.iter()).enumerate()
                {
                    assert_eq!(
                        act_t.back_d, exp_t.back_d,
                        "Trail {t_i} back_d mismatch at {ctx}"
                    );
                    assert_eq!(
                        act_t.front_d, exp_t.front_d,
                        "Trail {t_i} front_d mismatch at {ctx}"
                    );
                    assert!(
                        (act_t.opacity - exp_t.opacity).abs() <= 0.02,
                        "Trail {t_i} opacity mismatch at {ctx}: {} vs {}",
                        act_t.opacity,
                        exp_t.opacity
                    );
                    assert!(
                        (act_t.x1 - exp_t.x1).abs() <= 0.2,
                        "Trail {t_i} x1 mismatch at {ctx}: {} vs {}",
                        act_t.x1,
                        exp_t.x1
                    );
                    assert!(
                        (act_t.y1 - exp_t.y1).abs() <= 0.2,
                        "Trail {t_i} y1 mismatch at {ctx}: {} vs {}",
                        act_t.y1,
                        exp_t.y1
                    );
                    assert!(
                        (act_t.x2 - exp_t.x2).abs() <= 0.2,
                        "Trail {t_i} x2 mismatch at {ctx}: {} vs {}",
                        act_t.x2,
                        exp_t.x2
                    );
                    assert!(
                        (act_t.y2 - exp_t.y2).abs() <= 0.2,
                        "Trail {t_i} y2 mismatch at {ctx}: {} vs {}",
                        act_t.y2,
                        exp_t.y2
                    );
                    assert_eq!(
                        act_t.stops.len(),
                        exp_t.stops.len(),
                        "Trail {t_i} stops len mismatch at {ctx}"
                    );
                    for (st_i, (act_st, exp_st)) in
                        act_t.stops.iter().zip(exp_t.stops.iter()).enumerate()
                    {
                        assert_eq!(
                            act_st.offset, exp_st.offset,
                            "Trail {t_i} stop {st_i} offset mismatch at {ctx}"
                        );
                        assert!(
                            act_st.color.eq_ignore_ascii_case(&exp_st.color),
                            "Trail {t_i} stop {st_i} color mismatch at {ctx}"
                        );
                    }
                }
            }

            // Compare confetti
            if !expected.confetti.is_empty() {
                assert_eq!(
                    snap.confetti.len(),
                    expected.confetti.len(),
                    "Confetti count mismatch at {ctx}"
                );
                for (c_i, (act_c, exp_c)) in snap
                    .confetti
                    .iter()
                    .zip(expected.confetti.iter())
                    .enumerate()
                {
                    assert_eq!(
                        act_c.kind, exp_c.kind,
                        "Confetti {c_i} kind mismatch at {ctx}"
                    );
                    assert!(
                        act_c.fill.eq_ignore_ascii_case(&exp_c.fill),
                        "Confetti {c_i} fill mismatch at {ctx}"
                    );
                    assert!(
                        (act_c.opacity - exp_c.opacity).abs() <= 0.02,
                        "Confetti {c_i} opacity mismatch at {ctx}: {} vs {}",
                        act_c.opacity,
                        exp_c.opacity
                    );
                    assert_transform_approx_eq(
                        &act_c.transform,
                        &exp_c.transform,
                        0.05,
                        &format!("confetti {c_i} transform at {ctx}"),
                    );
                }
            }

            sampled_idx += 1;
        }
    }

    assert_eq!(
        sampled_idx,
        fixture.sampled_frames.len(),
        "All sampled frames must be verified for {}",
        fixture.scenario
    );
}

#[test]
fn test_golden_shape_blob() {
    run_golden_scenario("shape_blob.json");
}

#[test]
fn test_golden_shape_wedge() {
    run_golden_scenario("shape_wedge.json");
}

#[test]
fn test_golden_shape_gem() {
    run_golden_scenario("shape_gem.json");
}

#[test]
fn test_golden_lite_false() {
    run_golden_scenario("lite_false.json");
}

#[test]
fn test_golden_group_life() {
    run_golden_scenario("group_life.json");
}

#[test]
fn test_golden_group_emotion() {
    run_golden_scenario("group_emotion.json");
}

#[test]
fn test_golden_group_agent() {
    run_golden_scenario("group_agent.json");
}

#[test]
fn test_golden_group_custom() {
    run_golden_scenario("group_custom.json");
}

#[test]
fn test_golden_transition() {
    run_golden_scenario("transition.json");
}

#[test]
fn test_golden_idle_standby_sleep() {
    run_golden_scenario("idle_standby_sleep.json");
}

#[test]
fn test_golden_confetti_burst() {
    run_golden_scenario("confetti_burst.json");
}

#[test]
fn test_golden_orbit_trails() {
    run_golden_scenario("orbit_trails.json");
}

#[test]
fn test_golden_rapid_emotions() {
    run_golden_scenario("rapid_emotions.json");
}

#[test]
fn test_golden_overlapping_fx() {
    run_golden_scenario("overlapping_fx.json");
}
