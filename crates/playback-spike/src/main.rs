//! Playback-smoothness test bench. Four methods, same media, same machine:
//!
//! ```text
//! cargo run -p playback-spike --release -- playbin   <file> [sink-factory]
//! cargo run -p playback-spike --release -- current   <file>
//! cargo run -p playback-spike --release -- fixed     <file>
//! cargo run -p playback-spike --release -- crossfade <fileA> <fileB>
//! ```
//!
//! - `playbin` — stock playbin + a real video sink in its own window.
//!   Ground truth: what GStreamer can do on this box (≈ VLC).
//! - `current` — faithful replica of the shipped engine path: conform to a
//!   1080p30 I420 canvas (videoscale+videorate), intervideo channel,
//!   compositor, appsink sync=false, egui polling at 16ms and re-creating
//!   the texture every frame.
//! - `fixed` — appsink path with the suspected problems removed: native
//!   resolution and framerate (no videorate), sync=true so the pipeline
//!   clock paces frames, repaint driven by frame arrival, texture updated
//!   in place.
//! - `crossfade` — like `fixed` but two decoders into a compositor with an
//!   automatic alpha ramp every few seconds, to check that the two-layer
//!   crossfade architecture survives the fixes.
//!
//! Every mode prints frame-interval stats every 3 s:
//! arrivals = frames delivered by GStreamer, paints = egui repaints.
//! For smooth playback you want arrivals locked to the file's frame rate
//! with a low p95/max spread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

// ─── interval statistics ────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    last: Option<Instant>,
    deltas_ms: Vec<f64>,
}

#[derive(Clone)]
struct StatsHandle(Arc<Mutex<Stats>>);

impl StatsHandle {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Stats::default())))
    }

    fn tick(&self) {
        let mut s = self.0.lock().unwrap();
        let now = Instant::now();
        if let Some(prev) = s.last {
            s.deltas_ms.push(now.duration_since(prev).as_secs_f64() * 1000.0);
        }
        s.last = Some(now);
    }

    /// Drain collected deltas and format a one-line summary, or None if idle.
    fn summarize(&self) -> Option<String> {
        let mut s = self.0.lock().unwrap();
        if s.deltas_ms.is_empty() {
            return None;
        }
        let mut d = std::mem::take(&mut s.deltas_ms);
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = d.len();
        let avg = d.iter().sum::<f64>() / n as f64;
        let p95 = d[((n as f64 * 0.95) as usize).min(n - 1)];
        let max = d[n - 1];
        Some(format!(
            "n={n:3}  fps={:5.2}  avg={avg:6.2}ms  p95={p95:6.2}ms  max={max:6.2}ms",
            1000.0 / avg
        ))
    }
}

fn spawn_stats_printer(labels: Vec<(&'static str, StatsHandle)>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(3));
        for (label, h) in &labels {
            if let Some(line) = h.summarize() {
                eprintln!("[{label:8}] {line}");
            }
        }
    });
}

// ─── shared frame slot (gst callback → egui) ────────────────────────────────

struct FrameSlot {
    /// (width, height, rgba bytes)
    frame: Mutex<Option<(usize, usize, Vec<u8>)>>,
    /// Set once the egui app starts; lets the appsink callback wake the UI.
    egui_ctx: OnceLock<egui::Context>,
    arrivals: StatsHandle,
}

impl FrameSlot {
    fn new(arrivals: StatsHandle) -> Arc<Self> {
        Arc::new(Self {
            frame: Mutex::new(None),
            egui_ctx: OnceLock::new(),
            arrivals,
        })
    }
}

/// Build an RGBA appsink that stores frames in `slot`.
/// `sync` decides whether the pipeline clock paces delivery.
/// If `wake_ui`, each frame requests an egui repaint (the "fixed" behaviour).
fn make_appsink(slot: Arc<FrameSlot>, sync: bool, wake_ui: bool) -> Result<gst::Element> {
    let sink = gst::ElementFactory::make("appsink")
        .name("spike_sink")
        .property("max-buffers", 2u32)
        .property("drop", true)
        .property("sync", sync)
        .build()
        .context("make appsink")?;
    sink.set_property(
        "caps",
        gst::Caps::builder("video/x-raw").field("format", "RGBA").build(),
    );

    let typed = sink.clone().dynamic_cast::<gst_app::AppSink>().unwrap();
    typed.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let st = caps.structure(0).ok_or(gst::FlowError::Error)?;
                let w = st.get::<i32>("width").map_err(|_| gst::FlowError::Error)? as usize;
                let h = st.get::<i32>("height").map_err(|_| gst::FlowError::Error)? as usize;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                *slot.frame.lock().unwrap() = Some((w, h, map.as_slice().to_vec()));
                slot.arrivals.tick();
                if wake_ui {
                    if let Some(ctx) = slot.egui_ctx.get() {
                        ctx.request_repaint();
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(sink)
}

// ─── decode producer helpers ────────────────────────────────────────────────

fn file_uri(path: &str) -> Result<String> {
    let abs = std::fs::canonicalize(path).with_context(|| format!("file not found: {path}"))?;
    Ok(gst::glib::filename_to_uri(&abs, None)?.to_string())
}

/// uridecodebin whose first video pad links into `head`; audio pads are
/// discarded into fakesinks (same policy as the real engine).
fn add_video_decoder(pipeline: &gst::Pipeline, uri: &str, head: &gst::Element) -> Result<()> {
    let decode = gst::ElementFactory::make("uridecodebin")
        .property("uri", uri)
        .build()
        .context("make uridecodebin")?;
    pipeline.add(&decode)?;

    let head_weak = head.downgrade();
    let pipeline_weak = pipeline.downgrade();
    decode.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let is_video = caps
            .structure(0)
            .map(|s| s.name().starts_with("video/") || s.name().starts_with("image/"))
            .unwrap_or(false);
        if is_video {
            if let Some(head) = head_weak.upgrade() {
                if let Some(sink) = head.static_pad("sink") {
                    if !sink.is_linked() {
                        let _ = pad.link(&sink);
                        return;
                    }
                }
            }
        }
        if let Some(pl) = pipeline_weak.upgrade() {
            if let Ok(fake) = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
            {
                if pl.add(&fake).is_ok() {
                    let _ = fake.sync_state_with_parent();
                    if let Some(sink) = fake.static_pad("sink") {
                        let _ = pad.link(&sink);
                    }
                }
            }
        }
    });
    Ok(())
}

/// Print bus errors/warnings; loop the pipeline on EOS if `loop_on_eos`.
fn spawn_bus_watch(pipeline: &gst::Pipeline, loop_on_eos: bool) {
    let Some(bus) = pipeline.bus() else { return };
    let weak = pipeline.downgrade();
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView as M;
            match msg.view() {
                M::Error(e) => {
                    eprintln!(
                        "[bus] ERROR from {}: {} ({})",
                        e.src().map(|s| s.path_string().to_string()).unwrap_or_default(),
                        e.error(),
                        e.debug().map(|d| d.to_string()).unwrap_or_default()
                    );
                }
                M::Warning(w) => eprintln!("[bus] WARNING: {}", w.error()),
                M::Eos(_) => {
                    if let Some(pl) = weak.upgrade() {
                        if loop_on_eos {
                            eprintln!("[bus] EOS — looping");
                            let _ = pl.seek_simple(
                                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                gst::ClockTime::ZERO,
                            );
                        } else {
                            eprintln!("[bus] EOS");
                        }
                    }
                }
                _ => {}
            }
        }
    });
}

// ─── egui viewer app ────────────────────────────────────────────────────────

/// Minimal video viewer over a FrameSlot.
///
/// `recreate_texture` + `poll_16ms` reproduce the shipped client's behaviour;
/// both false is the "fixed" presentation.
struct Viewer {
    slot: Arc<FrameSlot>,
    texture: Option<egui::TextureHandle>,
    paints: StatsHandle,
    recreate_texture: bool,
    poll_16ms: bool,
    title: &'static str,
}

impl eframe::App for Viewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.poll_16ms {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else {
            // Fallback so the UI stays alive if frames stop coming.
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        if let Some((w, h, rgba)) = self.slot.frame.lock().unwrap().take() {
            if rgba.len() >= w * h * 4 {
                let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                let opts = egui::TextureOptions::LINEAR;
                match (&mut self.texture, self.recreate_texture) {
                    (Some(tex), false) => tex.set(image, opts),
                    _ => self.texture = Some(ctx.load_texture("spike-video", image, opts)),
                }
                self.paints.tick();
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(self.title)
                        .color(egui::Color32::WHITE)
                        .size(14.0),
                );
                if let Some(tex) = &self.texture {
                    let avail = ui.available_size();
                    let img = tex.size_vec2();
                    let scale = (avail.x / img.x).min(avail.y / img.y);
                    ui.centered_and_justified(|ui| {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(img * scale));
                    });
                }
            });
    }
}

fn run_viewer(
    slot: Arc<FrameSlot>,
    paints: StatsHandle,
    recreate_texture: bool,
    poll_16ms: bool,
    title: &'static str,
) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 760.0]),
        vsync: true,
        ..Default::default()
    };
    let slot_for_app = slot.clone();
    eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            let _ = slot_for_app.egui_ctx.set(cc.egui_ctx.clone());
            Ok(Box::new(Viewer {
                slot: slot_for_app,
                texture: None,
                paints,
                recreate_texture,
                poll_16ms,
                title,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;
    Ok(())
}

// ─── mode: playbin ──────────────────────────────────────────────────────────

fn mode_playbin(file: &str, sink_factory: Option<&str>) -> Result<()> {
    let playbin = gst::ElementFactory::make("playbin")
        .property("uri", file_uri(file)?)
        .build()
        .context("make playbin")?;

    if let Some(factory) = sink_factory {
        let sink = gst::ElementFactory::make(factory)
            .build()
            .with_context(|| format!("sink factory '{factory}' unavailable"))?;
        playbin.set_property("video-sink", &sink);
        eprintln!("[playbin] video-sink = {factory}");
    } else {
        eprintln!("[playbin] video-sink = autovideosink (default)");
    }

    playbin.set_state(gst::State::Playing)?;
    eprintln!("[playbin] playing — close with Ctrl-C; watch for smoothness in the sink's own window");

    let bus = playbin.bus().context("playbin bus")?;
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView as M;
        match msg.view() {
            M::Eos(_) => {
                eprintln!("[playbin] EOS");
                break;
            }
            M::Error(e) => {
                eprintln!("[playbin] ERROR: {} ({:?})", e.error(), e.debug());
                break;
            }
            _ => {}
        }
    }
    playbin.set_state(gst::State::Null)?;
    Ok(())
}

// ─── mode: current (replica of the shipped engine path) ─────────────────────

fn mode_current(file: &str) -> Result<()> {
    const CH: &str = "spike-inter";
    let canvas_caps = gst::Caps::builder("video/x-raw")
        .field("format", "I420")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .build();

    // Producer: decode → convert → scale → rate → 1080p30 caps → intervideosink
    let producer = gst::Pipeline::with_name("spike-producer");
    let convert = gst::ElementFactory::make("videoconvert").build().unwrap();
    let scale = gst::ElementFactory::make("videoscale").build().unwrap();
    let rate = gst::ElementFactory::make("videorate").build().unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property("caps", &canvas_caps)
        .build()
        .unwrap();
    let isink = gst::ElementFactory::make("intervideosink")
        .property("channel", CH)
        .build()
        .context("intervideosink (gst-plugins-bad)")?;
    producer.add_many([&convert, &scale, &rate, &caps, &isink])?;
    gst::Element::link_many([&convert, &scale, &rate, &caps, &isink])?;
    add_video_decoder(&producer, &file_uri(file)?, &convert)?;

    // Display: intervideosrc → caps → queue → compositor → I420 caps → queue
    //          → convert → appsink(sync=false)
    let display = gst::Pipeline::with_name("spike-display");
    let isrc = gst::ElementFactory::make("intervideosrc")
        .property("channel", CH)
        .property("timeout", u64::MAX)
        .build()
        .unwrap();
    let in_caps = gst::ElementFactory::make("capsfilter")
        .property("caps", &canvas_caps)
        .build()
        .unwrap();
    let in_queue = gst::ElementFactory::make("queue").build().unwrap();
    let comp = gst::ElementFactory::make("compositor").build().unwrap();
    comp.set_property_from_str("background", "black");
    let two_frames_ns = 2_000_000_000u64 / 30;
    comp.set_property("latency", two_frames_ns);
    comp.set_property("min-upstream-latency", two_frames_ns);
    let comp_caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw").field("format", "I420").build(),
        )
        .build()
        .unwrap();
    let out_queue = gst::ElementFactory::make("queue").build().unwrap();
    let out_convert = gst::ElementFactory::make("videoconvert").build().unwrap();

    let arrivals = StatsHandle::new();
    let paints = StatsHandle::new();
    let slot = FrameSlot::new(arrivals.clone());
    let appsink = make_appsink(slot.clone(), /* sync */ false, /* wake_ui */ false)?;

    display.add_many([
        &isrc, &in_caps, &in_queue, &comp, &comp_caps, &out_queue, &out_convert, &appsink,
    ])?;
    gst::Element::link_many([&isrc, &in_caps, &in_queue])?;
    let comp_pad = comp.request_pad_simple("sink_%u").context("compositor pad")?;
    comp_pad.set_property("alpha", 1.0f64);
    in_queue.static_pad("src").unwrap().link(&comp_pad).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    gst::Element::link_many([&comp, &comp_caps, &out_queue, &out_convert, &appsink])?;

    spawn_bus_watch(&producer, true);
    spawn_bus_watch(&display, false);
    spawn_stats_printer(vec![("arrivals", arrivals), ("paints", paints.clone())]);

    display.set_state(gst::State::Playing)?;
    producer.set_state(gst::State::Playing)?;
    eprintln!("[current] replica of shipped path: 1080p30 canvas, intervideo, compositor, appsink sync=false, egui 16ms poll + texture re-create");

    run_viewer(slot, paints, /* recreate_texture */ true, /* poll_16ms */ true, "current (shipped path)")?;
    let _ = producer.set_state(gst::State::Null);
    let _ = display.set_state(gst::State::Null);
    Ok(())
}

// ─── mode: fixed ────────────────────────────────────────────────────────────

fn mode_fixed(file: &str) -> Result<()> {
    let pipeline = gst::Pipeline::with_name("spike-fixed");
    let convert = gst::ElementFactory::make("videoconvert")
        .property("n-threads", 4u32)
        .build()
        .unwrap();
    let queue = gst::ElementFactory::make("queue").build().unwrap();

    let arrivals = StatsHandle::new();
    let paints = StatsHandle::new();
    let slot = FrameSlot::new(arrivals.clone());
    let appsink = make_appsink(slot.clone(), /* sync */ true, /* wake_ui */ true)?;

    pipeline.add_many([&convert, &queue, &appsink])?;
    gst::Element::link_many([&convert, &queue, &appsink])?;
    add_video_decoder(&pipeline, &file_uri(file)?, &convert)?;

    spawn_bus_watch(&pipeline, true);
    spawn_stats_printer(vec![("arrivals", arrivals), ("paints", paints.clone())]);

    pipeline.set_state(gst::State::Playing)?;
    eprintln!("[fixed] native size/fps, appsink sync=true, repaint-on-frame, texture.set()");

    run_viewer(slot, paints, /* recreate_texture */ false, /* poll_16ms */ false, "fixed (paced appsink)")?;
    let _ = pipeline.set_state(gst::State::Null);
    Ok(())
}

// ─── mode: crossfade ────────────────────────────────────────────────────────

fn mode_crossfade(file_a: &str, file_b: &str) -> Result<()> {
    let pipeline = gst::Pipeline::with_name("spike-crossfade");

    let comp = gst::ElementFactory::make("compositor").build().unwrap();
    comp.set_property_from_str("background", "black");
    let comp_caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw").field("format", "I420").build(),
        )
        .build()
        .unwrap();
    let out_convert = gst::ElementFactory::make("videoconvert")
        .property("n-threads", 4u32)
        .build()
        .unwrap();

    let arrivals = StatsHandle::new();
    let paints = StatsHandle::new();
    let slot = FrameSlot::new(arrivals.clone());
    let appsink = make_appsink(slot.clone(), /* sync */ true, /* wake_ui */ true)?;

    pipeline.add_many([&comp, &comp_caps, &out_convert, &appsink])?;
    gst::Element::link_many([&comp, &comp_caps, &out_convert, &appsink])?;

    // Two decode branches straight into the compositor — no intervideo, no
    // videorate. Each branch conforms size only (keep native framerate).
    let mut pads = Vec::new();
    for (i, file) in [file_a, file_b].iter().enumerate() {
        let convert = gst::ElementFactory::make("videoconvert").build().unwrap();
        let scale = gst::ElementFactory::make("videoscale").build().unwrap();
        let queue = gst::ElementFactory::make("queue").build().unwrap();
        pipeline.add_many([&convert, &scale, &queue])?;
        gst::Element::link_many([&convert, &scale, &queue])?;
        add_video_decoder(&pipeline, &file_uri(file)?, &convert)?;

        let pad = comp.request_pad_simple("sink_%u").context("compositor pad")?;
        pad.set_property("zorder", i as u32);
        pad.set_property("alpha", if i == 0 { 1.0f64 } else { 0.0f64 });
        queue.static_pad("src").unwrap().link(&pad).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        pads.push(pad);
    }

    spawn_bus_watch(&pipeline, true);
    spawn_stats_printer(vec![("arrivals", arrivals), ("paints", paints.clone())]);

    // Alpha animator: crossfade A↔B every 5 s over 1.5 s at 60 steps/s.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let pads = pads.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut a_on_top = true;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(5));
                let (from, to) = if a_on_top { (0, 1) } else { (1, 0) };
                eprintln!("[xfade] ramping {} → {}", from, to);
                let steps = 90; // 1.5s @ 60Hz
                for i in 0..=steps {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let t = i as f64 / steps as f64;
                    pads[from].set_property("alpha", 1.0 - t);
                    pads[to].set_property("alpha", t);
                    std::thread::sleep(Duration::from_millis(1000 / 60));
                }
                a_on_top = !a_on_top;
            }
        });
    }

    pipeline.set_state(gst::State::Playing)?;
    eprintln!("[crossfade] two decoders → compositor (native fps) → paced appsink; auto-crossfade every 5s");

    run_viewer(slot, paints, false, false, "crossfade (two-layer, paced)")?;
    stop.store(true, Ordering::Relaxed);
    let _ = pipeline.set_state(gst::State::Null);
    Ok(())
}

// ─── entry ──────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    gst::init()?;

    match args.as_slice() {
        [m, file] if m == "playbin" => mode_playbin(file, None),
        [m, file, sink] if m == "playbin" => mode_playbin(file, Some(sink)),
        [m, file] if m == "current" => mode_current(file),
        [m, file] if m == "fixed" => mode_fixed(file),
        [m, a, b] if m == "crossfade" => mode_crossfade(a, b),
        _ => {
            bail!(
                "usage:\n  playback-spike playbin   <file> [sink-factory]\n  playback-spike current   <file>\n  playback-spike fixed     <file>\n  playback-spike crossfade <fileA> <fileB>"
            );
        }
    }
}
