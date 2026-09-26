use grok_ball::{Ball, BallOptions, ShapeKind};
use std::time::Instant;

#[test]
fn bench_single_ball_tick() {
    let mut ball_lite = Ball::new(BallOptions {
        shape: ShapeKind::Blob,
        emotion: Some("10".to_string()),
        lite: true,
        autostart: true,
        seed: Some(42),
        ..Default::default()
    });

    let mut ball_full = Ball::new(BallOptions {
        shape: ShapeKind::Gem,
        emotion: Some("00".to_string()),
        lite: false,
        autostart: true,
        seed: Some(42),
        ..Default::default()
    });
    ball_full.burst(Some(40));
    let mut ball_fast = Ball::new(BallOptions {
        shape: ShapeKind::Blob,
        emotion: Some("10".to_string()),
        lite: true,
        autostart: true,
        seed: Some(42),
        ..Default::default()
    });

    // Warm up
    for i in 0..1000 {
        let t = i as f64 * 16.667;
        ball_lite.tick(t);
        ball_full.tick(t);
        std::hint::black_box(ball_fast.tick_fast(t));
    }

    // Benchmark Lite
    let iterations = 20_000;
    let start_lite = Instant::now();
    for i in 0..iterations {
        let t = 1000.0 * 16.667 + i as f64 * 16.667;
        let _frame = ball_lite.tick(t);
    }
    let dur_lite = start_lite.elapsed();
    let per_tick_lite_us = dur_lite.as_micros() as f64 / iterations as f64;

    // Benchmark Full (with particles / trails / zzz)
    let start_full = Instant::now();
    for i in 0..iterations {
        let t = 1000.0 * 16.667 + i as f64 * 16.667;
        let _frame = ball_full.tick(t);
    }
    let dur_full = start_full.elapsed();
    let per_tick_full_us = dur_full.as_micros() as f64 / iterations as f64;

    let start_fast = Instant::now();
    for i in 0..iterations {
        let t = 1000.0 * 16.667 + i as f64 * 16.667;
        std::hint::black_box(ball_fast.tick_fast(t));
    }
    let dur_fast = start_fast.elapsed();
    let per_tick_fast_us = dur_fast.as_secs_f64() * 1_000_000.0 / iterations as f64;

    println!("\n================ Performance Benchmark ================");
    println!("Iterations: {} frames", iterations);
    println!(
        "Lite mode (dango-widget default): {:.3} µs / frame (~{:.0} fps on 1 thread)",
        per_tick_lite_us,
        1_000_000.0 / per_tick_lite_us.max(0.001)
    );
    println!(
        "Fast numeric path (lite): {:.3} µs / frame (~{:.0} fps on 1 thread)",
        per_tick_fast_us,
        1_000_000.0 / per_tick_fast_us.max(0.001)
    );
    println!(
        "Full mode (particles + trails + zzz): {:.3} µs / frame (~{:.0} fps on 1 thread)",
        per_tick_full_us,
        1_000_000.0 / per_tick_full_us.max(0.001)
    );
    println!("=======================================================\n");
}
