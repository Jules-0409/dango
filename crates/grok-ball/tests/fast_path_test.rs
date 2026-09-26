use grok_ball::{Ball, BallOptions, DrawElement, PathVerb, ShapeKind};

fn path_numbers(d: &str) -> Vec<f32> {
    let mut numbers = Vec::new();
    let mut token = String::new();
    for ch in d.chars() {
        if ch.is_ascii_alphabetic() || ch.is_whitespace() || ch == ',' {
            if !token.is_empty() {
                numbers.push(token.parse().expect("SVG path number"));
                token.clear();
            }
        } else {
            token.push(ch);
        }
    }
    if !token.is_empty() {
        numbers.push(token.parse().expect("SVG path number"));
    }
    numbers
}

fn verb_numbers(path: &[PathVerb]) -> Vec<f32> {
    let mut numbers = Vec::new();
    for verb in path {
        match *verb {
            PathVerb::MoveTo(x, y) | PathVerb::LineTo(x, y) => numbers.extend([x, y]),
            PathVerb::QuadTo(x1, y1, x, y) => numbers.extend([x1, y1, x, y]),
            PathVerb::CubicTo(x1, y1, x2, y2, x, y) => numbers.extend([x1, y1, x2, y2, x, y]),
            PathVerb::ArcTo {
                rx,
                ry,
                x_axis_rotation,
                large_arc,
                sweep,
                x,
                y,
            } => numbers.extend([
                rx,
                ry,
                x_axis_rotation,
                large_arc as u8 as f32,
                sweep as u8 as f32,
                x,
                y,
            ]),
            PathVerb::Close => {}
        }
    }
    numbers
}

#[test]
fn fast_tick_has_same_path_numbers_without_svg_strings() {
    let options = BallOptions {
        shape: ShapeKind::Gem,
        emotion: Some("10".to_owned()),
        lite: true,
        autostart: true,
        seed: Some(42),
        ..Default::default()
    };
    let mut string_ball = Ball::new(options.clone());
    let mut fast_ball = Ball::new(options);

    let string_frame = string_ball.tick(16.667).clone();
    let fast_frame = fast_ball.tick_fast(16.667).clone();
    assert_eq!(string_frame.elements.len(), fast_frame.elements.len());

    for (string_element, fast_element) in string_frame.elements.iter().zip(&fast_frame.elements) {
        let (DrawElement::Path(string_path), DrawElement::Path(fast_path)) =
            (string_element, fast_element)
        else {
            panic!("lite path fixture should contain only paths");
        };
        let parsed = path_numbers(&string_path.path.d);
        let numeric = verb_numbers(&fast_path.path.verbs);
        assert_eq!(parsed.len(), numeric.len());
        for (from_string, from_fast) in parsed.iter().zip(numeric) {
            assert!(
                (from_string - from_fast).abs() <= 0.011,
                "SVG value {from_string} differs from numeric value {from_fast}"
            );
        }
        assert!(fast_path.path.d.is_empty());
        assert!(fast_path.raw_transform.is_empty());
        assert!(fast_path.id.is_none());
        if let grok_ball::Paint::RadialGradient(gradient) = &fast_path.fill {
            assert!(gradient.stops.iter().all(|stop| stop.raw_color.is_empty()));
        }
    }
}

fn paint_colors(paint: &grok_ball::Paint) -> Vec<(u8, u8, u8, f32, f32)> {
    match paint {
        grok_ball::Paint::None => vec![],
        grok_ball::Paint::Solid(c) => vec![(c.r, c.g, c.b, c.a, 0.0)],
        grok_ball::Paint::RadialGradient(g) => g
            .stops
            .iter()
            .map(|s| (s.color.r, s.color.g, s.color.b, s.color.a, s.offset))
            .collect(),
        grok_ball::Paint::LinearGradient(g) => g
            .stops
            .iter()
            .map(|s| (s.color.r, s.color.g, s.color.b, s.color.a, s.offset))
            .collect(),
    }
}

fn assert_frames_match(label: &str, frame_idx: usize, a: &grok_ball::Frame, b: &grok_ball::Frame) {
    let ctx = format!("{label} frame {frame_idx}");
    // The fast path keeps fixed slots in lite mode, padding hidden eyes with invisible elements.
    let shown = |f: &grok_ball::Frame| -> Vec<DrawElement> {
        f.elements
            .iter()
            .filter(|e| !matches!(e, DrawElement::Path(p) if !p.visible))
            .cloned()
            .collect()
    };
    let (a_shown, b_shown) = (shown(a), shown(b));
    assert_eq!(a_shown.len(), b_shown.len(), "visible element count, {ctx}");
    for (i, (ea, eb)) in a_shown.iter().zip(&b_shown).enumerate() {
        match (ea, eb) {
            (DrawElement::Path(pa), DrawElement::Path(pb)) => {
                assert_eq!(pa.visible, pb.visible, "visible, {ctx} el {i}");
                if !pa.visible {
                    continue;
                }
                if !pa.path.d.is_empty() {
                    let parsed = path_numbers(&pa.path.d);
                    let numeric = verb_numbers(&pb.path.verbs);
                    assert_eq!(parsed.len(), numeric.len(), "path len, {ctx} el {i}");
                    for (x, y) in parsed.iter().zip(&numeric) {
                        assert!((x - y).abs() <= 0.011, "path {x} vs {y}, {ctx} el {i}");
                    }
                } else {
                    let va = verb_numbers(&pa.path.verbs);
                    let vb = verb_numbers(&pb.path.verbs);
                    assert_eq!(va.len(), vb.len(), "path verbs len, {ctx} el {i}");
                    for (x, y) in va.iter().zip(&vb) {
                        assert!((x - y).abs() <= 0.011, "path {x} vs {y}, {ctx} el {i}");
                    }
                }
                assert!(
                    (pa.opacity - pb.opacity).abs() <= 1e-4,
                    "opacity, {ctx} el {i}"
                );
                let (ta, tb) = (pa.transform, pb.transform);
                for (x, y) in [ta.a, ta.b, ta.c, ta.d, ta.e, ta.f]
                    .iter()
                    .zip([tb.a, tb.b, tb.c, tb.d, tb.e, tb.f])
                {
                    assert!((x - y).abs() <= 1e-3, "transform, {ctx} el {i}");
                }
                assert_eq!(
                    paint_colors(&pa.fill),
                    paint_colors(&pb.fill),
                    "fill, {ctx} el {i}"
                );
                assert_eq!(
                    pa.stroke
                        .as_ref()
                        .map(|s| (s.color.r, s.color.g, s.color.b)),
                    pb.stroke
                        .as_ref()
                        .map(|s| (s.color.r, s.color.g, s.color.b)),
                    "stroke, {ctx} el {i}"
                );
            }
            (DrawElement::Text(ta), DrawElement::Text(tb)) => {
                assert_eq!(ta.text, tb.text, "text, {ctx} el {i}");
                assert!(
                    (ta.opacity - tb.opacity).abs() <= 1e-4,
                    "text opacity, {ctx} el {i}"
                );
                assert!(
                    (ta.font_size - tb.font_size).abs() <= 1e-3,
                    "font size, {ctx} el {i}"
                );
            }
            _ => panic!("element kind mismatch, {ctx} el {i}"),
        }
    }
}

#[test]
fn fast_tick_matches_string_tick_over_long_runs() {
    let cases: [(&str, ShapeKind, &str, bool); 5] = [
        ("blob-lite", ShapeKind::Blob, "02", true),
        ("wedge-lite", ShapeKind::Wedge, "03", true),
        ("gem-full", ShapeKind::Gem, "30", false),
        ("blob-full", ShapeKind::Blob, "00", false),
        ("wedge-full", ShapeKind::Wedge, "21", false),
    ];
    for (label, shape, emotion, lite) in cases {
        let options = BallOptions {
            shape,
            emotion: Some(emotion.to_owned()),
            lite,
            autostart: true,
            seed: Some(7),
            ..Default::default()
        };
        let mut string_ball = Ball::new(options.clone());
        let mut fast_ball = Ball::new(options);
        let mut saw_effects = false;
        for frame_idx in 0..600 {
            let now = 16.667 * (frame_idx + 1) as f64;
            match frame_idx {
                60 => {
                    string_ball.burst(Some(20));
                    fast_ball.burst(Some(20));
                }
                120 => {
                    string_ball.spin(Some(1.0), Some(1.0));
                    fast_ball.spin(Some(1.0), Some(1.0));
                }
                200 => {
                    string_ball.set_gaze(0.6, -0.3);
                    fast_ball.set_gaze(0.6, -0.3);
                }
                300 => {
                    string_ball.set_emotion_at("14", now);
                    fast_ball.set_emotion_at("14", now);
                }
                330 => {
                    string_ball.set_emotion_at("05", now);
                    fast_ball.set_emotion_at("05", now);
                }
                420 => {
                    string_ball.bounce_at(now);
                    fast_ball.bounce_at(now);
                }
                _ => {}
            }
            let a = string_ball.tick(now).clone();
            let b = fast_ball.tick_fast(now).clone();
            assert_frames_match(label, frame_idx, &a, &b);
            saw_effects |= !lite
                && b.elements
                    .iter()
                    .filter(|element| match element {
                        DrawElement::Path(path) => path.visible,
                        DrawElement::Text(text) => text.opacity > 0.0,
                    })
                    .count()
                    > 3;
        }
        if !lite {
            assert!(saw_effects, "{label} never rendered trail/confetti effects");
        }
    }
}
