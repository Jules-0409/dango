// One-off visual check: render one static frame of each shape to an SVG so
// we can eyeball the silhouette and eye placement before shipping.
//
// Run: cargo run -p grok-ball --example shape_preview -- /tmp/out.svg
use grok_ball::{Ball, BallOptions, DrawElement, Paint, ShapeKind};

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/shape_preview.svg".into());
    let shapes = [
        ("blob", ShapeKind::Blob),
        ("gem", ShapeKind::Gem),
        ("wedge", ShapeKind::Wedge),
        ("star", ShapeKind::Star),
        ("cloud", ShapeKind::Cloud),
        ("square", ShapeKind::Square),
        ("drop", ShapeKind::Drop),
    ];
    let cell = 240.0;
    let cols = 3.0;
    let rows = (shapes.len() as f64 / cols).ceil();
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" style=\"background:#2a2624\">",
        cell * cols,
        cell * rows
    );
    for (i, (name, kind)) in shapes.iter().enumerate() {
        let mut ball = Ball::new(BallOptions {
            shape: *kind,
            color: Some("#D8A7A0".into()),
            eye_color: Some("#F5F2ED".into()),
            emotion: Some("02".into()),
            lite: true,
            autostart: false,
            ..Default::default()
        });
        ball.set_active(true);
        let frame = ball.render_static(0.0);
        let ox = (i as f64 % cols) * cell;
        let oy = (i as f64 / cols).floor() * cell;
        svg.push_str(&format!("<g transform=\"translate({ox},{oy})\">"));
        for el in &frame.elements {
            if let DrawElement::Path(p) = el {
                if !p.visible {
                    continue;
                }
                let mut d = String::new();
                for v in &p.path.verbs {
                    match v {
                        grok_ball::PathVerb::MoveTo(x, y) => d.push_str(&format!("M{x:.1},{y:.1}")),
                        grok_ball::PathVerb::LineTo(x, y) => d.push_str(&format!("L{x:.1},{y:.1}")),
                        grok_ball::PathVerb::Close => d.push('Z'),
                        _ => {}
                    }
                }
                let fill = match &p.fill {
                    Paint::Solid(c) => format!("rgb({},{},{})", c.r, c.g, c.b),
                    _ => "#D8A7A0".into(),
                };
                let stroke = p
                    .stroke
                    .as_ref()
                    .map(|s| format!("rgb({},{},{})", s.color.r, s.color.g, s.color.b))
                    .unwrap_or_else(|| "none".into());
                svg.push_str(&format!(
                    "<path d=\"{d}\" fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"2\" opacity=\"{}\"/>",
                    p.opacity
                ));
            }
        }
        svg.push_str(&format!(
            "<text x=\"14\" y=\"30\" fill=\"#fff\" font-size=\"22\" font-family=\"sans-serif\">{name}</text></g>"
        ));
    }
    svg.push_str("</svg>");
    std::fs::write(&out, svg).unwrap();
    eprintln!("wrote {out}");
}
