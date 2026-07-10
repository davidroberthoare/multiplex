//! Manual smoke test for the killer feature: play file A, preload file B
//! while A is playing (must not disturb A), then crossfade A → B.
//!
//! Usage: cargo run -p cuemesh2-media --example crossfade -- <fileA> <fileB> [fade_ms]

use std::time::Duration;

use cuemesh2_media::{fades, Layer, MediaEngine, MediaKind};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let (Some(a), Some(b)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: crossfade <fileA> <fileB> [fade_ms]");
    };
    let fade_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = rt.enter();

    let engine = MediaEngine::new()?;

    // 1. Fade in cue A from black.
    engine.load(Layer::A, std::path::Path::new(&a), MediaKind::Video)?;
    engine.play(Layer::A)?;
    fades::fade(&engine, Layer::A, 1.0, Duration::from_millis(500));
    println!("A playing (fade-in 500ms)");
    std::thread::sleep(Duration::from_secs(3));

    // 2. Preload cue B while A is on air. A must not glitch here.
    let pos_before = engine.position_ms(Layer::A);
    engine.load(Layer::B, std::path::Path::new(&b), MediaKind::Video)?;
    std::thread::sleep(Duration::from_millis(600));
    let pos_after = engine.position_ms(Layer::A);
    println!(
        "B preloaded; A position {pos_before:?} → {pos_after:?} (must keep advancing)"
    );
    std::thread::sleep(Duration::from_secs(2));

    // 3. Crossfade A → B.
    engine.play(Layer::B)?;
    fades::crossfade(&engine, Layer::A, Layer::B, Duration::from_millis(fade_ms));
    println!("crossfading over {fade_ms}ms…");
    std::thread::sleep(Duration::from_millis(fade_ms + 500));
    engine.stop(Layer::A);
    println!(
        "crossfade done: alphas A={:.2} B={:.2}, B pos {:?}",
        engine.alpha(Layer::A),
        engine.alpha(Layer::B),
        engine.position_ms(Layer::B)
    );
    std::thread::sleep(Duration::from_secs(3));

    // 4. Fade B to black and stop.
    fades::fade(&engine, Layer::B, 0.0, Duration::from_millis(1000));
    std::thread::sleep(Duration::from_millis(1200));
    engine.stop_all();
    println!("done");
    Ok(())
}
