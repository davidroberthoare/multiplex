//! Simple linear alpha animation for fades and crossfades.
//!
//! The animator ticks every ~16ms (≈60fps) and drives the compositor pad's
//! `alpha` property. Starting a new fade on a layer cancels the previous one.

use std::time::Duration;

use gstreamer::prelude::*;
use tokio::time;

use cuemesh2_shared::protocol::Layer;

use crate::pipeline::MediaEngine;

const TICK: Duration = Duration::from_millis(16);

/// Spawn a linear alpha fade on `layer` from its *current* alpha to `target`
/// over `duration`. Cancels any in-flight fade on that layer.
pub fn fade(engine: &MediaEngine, layer: Layer, target: f64, duration: Duration) {
    let pad = engine.compositor_pad(layer);
    let engine_clone = engine.clone();
    let handle = tokio::spawn(async move {
        run_fade(pad, target, duration).await;
        // On completion, engine keeps its Option<JoinHandle> pointing at us;
        // no need to clear it — the next fade will replace it, and if a
        // caller cares they can subscribe to their own signal.
        let _ = engine_clone;
    });
    engine.install_fade(layer, handle);
}

/// Cross-fade: ramp `from_layer` down to 0 and `to_layer` up to 1 over the
/// same duration.
pub fn crossfade(engine: &MediaEngine, from_layer: Layer, to_layer: Layer, duration: Duration) {
    fade(engine, from_layer, 0.0, duration);
    fade(engine, to_layer, 1.0, duration);
}

async fn run_fade(pad: gstreamer::Pad, target: f64, duration: Duration) {
    let start = pad.property::<f64>("alpha");
    let target = target.clamp(0.0, 1.0);

    if duration.is_zero() || (target - start).abs() < 1e-6 {
        pad.set_property("alpha", target);
        return;
    }

    let steps = (duration.as_millis() as u64 / TICK.as_millis() as u64).max(1);
    let mut interval = time::interval(TICK);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    interval.tick().await; // consume first immediate tick

    for i in 1..=steps {
        interval.tick().await;
        let t = (i as f64) / (steps as f64);
        let value = start + (target - start) * t;
        pad.set_property("alpha", value);
    }
    // Ensure we land exactly on target.
    pad.set_property("alpha", target);
}
