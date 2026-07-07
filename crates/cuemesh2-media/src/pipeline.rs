//! Two-layer video engine built from three GStreamer pipelines.
//!
//! A persistent **display pipeline** composites two `intervideosrc` channels
//! and never stops running; each layer is fed by an independent, disposable
//! **producer pipeline** that pushes conformed frames into its channel:
//!
//! ```text
//! display (always PLAYING):
//!   intervideosrc ch=A ! caps ! queue ─┐
//!                                      ├─ compositor(I420) ! queue ! videoconvert ! sink
//!   intervideosrc ch=B ! caps ! queue ─┘
//!
//! producer, one per loaded layer (video):
//!   uridecodebin ! videoconvert ! videoscale ! videorate ! caps ! intervideosink ch=X
//! producer (image):
//!   uridecodebin ! imagefreeze ! videoconvert ! videoscale ! caps ! intervideosink ch=X
//! producer (testscreen):
//!   videotestsrc is-live=1 ! videoconvert ! caps ! intervideosink ch=X
//! ```
//!
//! Why this shape: `compositor` is an aggregator that waits on every linked
//! pad, so feeding it directly from per-cue decoders means loading, seeking,
//! or an errored file on one layer can stall the whole output. The
//! `intervideosrc` elements are live sources that emit the last (or black)
//! frame on their own, so the display never starves. Producers can preroll,
//! start, seek, change rate, and die without the operator's output ever
//! blinking — which is exactly the resilience a live show needs.
//!
//! All frames are conformed to one **canvas** (size/framerate) at the
//! producer tail, because inter channels do not convert formats.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use gstreamer as gst;
use gstreamer::prelude::*;
use tokio::sync::broadcast;

use cuemesh2_shared::protocol::Layer;

/// Errors returned by the media engine.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("gstreamer init failed: {0}")]
    Init(#[from] gst::glib::Error),
    #[error("gstreamer element creation failed: {0}")]
    ElementFactory(String),
    #[error("gstreamer link failed: {0}")]
    Link(#[from] gst::PadLinkError),
    #[error("gstreamer element link failed: {0}")]
    LinkElements(String),
    #[error("gstreamer state change failed: {0}")]
    StateChange(String),
    #[error("invalid file path: {0}")]
    BadPath(String),
    #[error("gstreamer add-many failed: {0}")]
    AddMany(String),
    #[error("layer {0:?} has no media loaded")]
    NoProducer(Layer),
}

/// What kind of media a producer pipeline decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    /// Stills are looped through `imagefreeze` into an endless video stream.
    Image,
}

/// Events published on the engine's broadcast channel.
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// A layer's producer reached end-of-stream.
    Eos(Layer),
    /// A producer errored (that layer is dead until the next `load`).
    Error {
        layer: Layer,
        source: String,
        message: String,
    },
}

/// Output canvas every producer conforms to.
#[derive(Debug, Clone, Copy)]
pub struct Canvas {
    pub width: i32,
    pub height: i32,
    pub fps_n: i32,
    pub fps_d: i32,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps_n: 30,
            fps_d: 1,
        }
    }
}

impl Canvas {
    fn caps(&self) -> gst::Caps {
        gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("width", self.width)
            .field("height", self.height)
            .field("framerate", gst::Fraction::new(self.fps_n, self.fps_d))
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build()
    }
}

fn make(factory: &str, name: Option<&str>) -> Result<gst::Element, MediaError> {
    let mut b = gst::ElementFactory::make(factory);
    if let Some(n) = name {
        b = b.name(n);
    }
    b.build()
        .map_err(|_| MediaError::ElementFactory(factory.to_string()))
}

static GST_INIT: OnceLock<()> = OnceLock::new();

fn ensure_init() -> Result<(), MediaError> {
    if GST_INIT.get().is_some() {
        return Ok(());
    }
    gst::init()?;
    let _ = GST_INIT.set(());
    Ok(())
}

fn channel_name(layer: Layer) -> &'static str {
    match layer {
        Layer::A => "cuemesh-layer-a",
        Layer::B => "cuemesh-layer-b",
    }
}

/// A running producer pipeline plus the flag that stops its bus-watch thread.
struct Producer {
    pipeline: gst::Pipeline,
    bus_shutdown: Arc<AtomicBool>,
}

impl Producer {
    fn teardown(self) {
        self.bus_shutdown.store(true, Ordering::SeqCst);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

struct LayerSlot {
    compositor_pad: gst::Pad,
    producer: Mutex<Option<Producer>>,
    /// Handle to the currently running fade task, if any. Aborted on new fade.
    fade: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct Inner {
    display: gst::Pipeline,
    layer_a: LayerSlot,
    layer_b: LayerSlot,
    canvas: Canvas,
    events_tx: broadcast::Sender<MediaEvent>,
}

/// Two-layer video engine. Clone is cheap (Arc-shared).
#[derive(Clone)]
pub struct MediaEngine {
    inner: Arc<Inner>,
}

impl MediaEngine {
    /// Build and start the display pipeline (black output) with the default
    /// 1080p30 canvas.
    pub fn new() -> Result<Self, MediaError> {
        Self::with_canvas(Canvas::default())
    }

    /// Build and start the display pipeline with an explicit canvas.
    pub fn with_canvas(canvas: Canvas) -> Result<Self, MediaError> {
        ensure_init()?;

        let display = gst::Pipeline::with_name("cuemesh2-display");

        let compositor = make("compositor", Some("comp"))?;
        compositor.set_property_from_str("background", "black");
        // The intervideosrc inputs are live, which leaves the pipeline with a
        // near-zero latency budget — frames arrive "late" at the sink after
        // any real work and get dropped. Budget two frame intervals.
        let two_frames_ns =
            2_000_000_000u64 * canvas.fps_d.max(1) as u64 / canvas.fps_n.max(1) as u64;
        compositor.set_property("latency", two_frames_ns);
        compositor.set_property("min-upstream-latency", two_frames_ns);
        // Pin the blending format. Left to negotiate freely, compositor can
        // settle on A444_16LE (16-bit 4:4:4 + alpha) and software-convert
        // every frame, which drops the frame rate to a crawl.
        let comp_caps = make("capsfilter", Some("comp_caps"))?;
        comp_caps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw").field("format", "I420").build(),
        );
        let out_queue = make("queue", Some("out_queue"))?;
        let out_convert = make("videoconvert", Some("out_convert"))?;
        let video_sink = Self::make_video_sink()?;

        display
            .add_many([&compositor, &comp_caps, &out_queue, &out_convert, &video_sink])
            .map_err(|e| MediaError::AddMany(e.to_string()))?;
        gst::Element::link_many([&compositor, &comp_caps, &out_queue, &out_convert, &video_sink])
            .map_err(|e| MediaError::LinkElements(e.to_string()))?;

        let layer_a = Self::build_display_input(&display, &compositor, &canvas, Layer::A, 0)?;
        let layer_b = Self::build_display_input(&display, &compositor, &canvas, Layer::B, 1)?;

        // Default: both layers transparent — output is black until a fade-in.
        layer_a.compositor_pad.set_property("alpha", 0.0f64);
        layer_b.compositor_pad.set_property("alpha", 0.0f64);

        let (events_tx, _rx) = broadcast::channel(64);
        let engine = MediaEngine {
            inner: Arc::new(Inner {
                display,
                layer_a,
                layer_b,
                canvas,
                events_tx,
            }),
        };

        engine.spawn_display_bus_watch();
        engine
            .inner
            .display
            .set_state(gst::State::Playing)
            .map_err(|e| MediaError::StateChange(format!("display start: {e}")))?;
        Ok(engine)
    }

    /// Pick the video sink for the compositor output window.
    ///
    /// On Linux we deliberately prefer the non-GL `xvimagesink` (XVideo,
    /// hardware scaling) over `glimagesink`. The client hosts an `eframe`
    /// (glow) GL context on its main thread; `glimagesink` spins up a second
    /// GLX context on a GStreamer thread, and two GLX contexts sharing one X
    /// display fight each other — the video window ends up mapped but frozen
    /// (visible in the taskbar, unresponsive, never raising). `xvimagesink`
    /// has no GL context, so it coexists cleanly. The standalone media
    /// examples have no eframe context and can still use GL fine.
    ///
    /// Override the whole decision with `CUEMESH_VIDEO_SINK=<factory>`.
    fn make_video_sink() -> Result<gst::Element, MediaError> {
        if let Ok(name) = std::env::var("CUEMESH_VIDEO_SINK") {
            let name = name.trim();
            let sink = gst::ElementFactory::make(name)
                .name("vsink")
                .build()
                .map_err(|_| {
                    MediaError::ElementFactory(format!("CUEMESH_VIDEO_SINK '{name}' unavailable"))
                })?;
            tracing::info!(factory = %name, "video sink selected (env override)");
            Self::configure_video_sink(&sink);
            return Ok(sink);
        }

        let candidates: &[&str] = if cfg!(target_os = "windows") {
            &["d3d11videosink", "glimagesink", "autovideosink"]
        } else if cfg!(target_os = "macos") {
            &["glimagesink", "osxvideosink", "autovideosink"]
        } else {
            &["xvimagesink", "glimagesink", "ximagesink", "autovideosink"]
        };
        for factory in candidates {
            if let Ok(sink) = gst::ElementFactory::make(factory).name("vsink").build() {
                tracing::info!(%factory, "video sink selected");
                Self::configure_video_sink(&sink);
                return Ok(sink);
            }
        }
        Err(MediaError::ElementFactory("no usable video sink".into()))
    }

    /// Apply properties common to every sink we might pick, guarding each one
    /// since not all sinks expose all of them.
    fn configure_video_sink(sink: &gst::Element) {
        // Letterbox instead of stretching the 16:9 canvas to the window.
        if sink.find_property("force-aspect-ratio").is_some() {
            sink.set_property("force-aspect-ratio", true);
        }
    }

    /// One display-side input branch: intervideosrc → caps → queue → comp pad.
    fn build_display_input(
        display: &gst::Pipeline,
        compositor: &gst::Element,
        canvas: &Canvas,
        layer: Layer,
        zorder: u32,
    ) -> Result<LayerSlot, MediaError> {
        let suffix = match layer {
            Layer::A => "a",
            Layer::B => "b",
        };
        let src = make("intervideosrc", Some(&format!("inter_src_{suffix}")))?;
        src.set_property("channel", channel_name(layer));
        // Hold the last frame forever when a producer pauses or dies; "black"
        // is expressed via alpha, never by the channel timing out.
        src.set_property("timeout", u64::MAX);

        let caps = make("capsfilter", Some(&format!("caps_{suffix}")))?;
        caps.set_property("caps", canvas.caps());
        let queue = make("queue", Some(&format!("queue_{suffix}")))?;

        display
            .add_many([&src, &caps, &queue])
            .map_err(|e| MediaError::AddMany(e.to_string()))?;
        gst::Element::link_many([&src, &caps, &queue])
            .map_err(|e| MediaError::LinkElements(e.to_string()))?;

        let compositor_pad = compositor
            .request_pad_simple("sink_%u")
            .ok_or_else(|| MediaError::LinkElements("compositor sink pad request failed".into()))?;
        compositor_pad.set_property("zorder", zorder);

        let queue_src = queue
            .static_pad("src")
            .ok_or_else(|| MediaError::LinkElements("queue src pad missing".into()))?;
        queue_src.link(&compositor_pad)?;

        Ok(LayerSlot {
            compositor_pad,
            producer: Mutex::new(None),
            fade: Mutex::new(None),
        })
    }

    fn slot(&self, layer: Layer) -> &LayerSlot {
        match layer {
            Layer::A => &self.inner.layer_a,
            Layer::B => &self.inner.layer_b,
        }
    }

    /// Subscribe to engine events (per-layer EOS / error).
    pub fn subscribe(&self) -> broadcast::Receiver<MediaEvent> {
        self.inner.events_tx.subscribe()
    }

    // ─── Producer lifecycle ────────────────────────────────────────────────

    /// Build a producer for `path` on `layer` and preroll it (PAUSED).
    /// Replaces any previous producer on that layer. Does not touch the
    /// display pipeline or the other layer.
    pub fn load(&self, layer: Layer, path: &Path, kind: MediaKind) -> Result<(), MediaError> {
        if !path.exists() {
            tracing::error!(path = %path.display(), ?layer, "load: file does not exist");
            return Err(MediaError::BadPath(format!("file not found: {}", path.display())));
        }
        let abs = path
            .canonicalize()
            .map_err(|e| MediaError::BadPath(format!("{}: {e}", path.display())))?;
        let uri = gst::glib::filename_to_uri(&abs, None)
            .map_err(|e| MediaError::BadPath(e.to_string()))?;

        tracing::info!(?layer, ?kind, %uri, "load: building producer");
        let pipeline = self.build_producer(layer, &uri, kind)?;
        self.install_producer(layer, pipeline, gst::State::Paused)
    }

    /// Show an SMPTE test pattern on `layer` (replaces any loaded media and
    /// starts immediately; caller sets alpha).
    pub fn load_testscreen(&self, layer: Layer) -> Result<(), MediaError> {
        let pipeline = gst::Pipeline::with_name(&format!("cuemesh2-test-{layer:?}"));
        let src = make("videotestsrc", None)?;
        src.set_property("is-live", true);
        src.set_property_from_str("pattern", "smpte");
        let convert = make("videoconvert", None)?;
        let scale = make("videoscale", None)?;
        let caps = make("capsfilter", None)?;
        caps.set_property("caps", self.inner.canvas.caps());
        let sink = make("intervideosink", None)?;
        sink.set_property("channel", channel_name(layer));

        pipeline
            .add_many([&src, &convert, &scale, &caps, &sink])
            .map_err(|e| MediaError::AddMany(e.to_string()))?;
        gst::Element::link_many([&src, &convert, &scale, &caps, &sink])
            .map_err(|e| MediaError::LinkElements(e.to_string()))?;

        self.install_producer(layer, pipeline, gst::State::Playing)
    }

    /// Decoder producer: uridecodebin → (imagefreeze) → convert/scale/rate →
    /// canvas caps → intervideosink.
    fn build_producer(
        &self,
        layer: Layer,
        uri: &str,
        kind: MediaKind,
    ) -> Result<gst::Pipeline, MediaError> {
        let pipeline = gst::Pipeline::with_name(&format!("cuemesh2-producer-{layer:?}"));

        let decode = make("uridecodebin", Some("decode"))?;
        decode.set_property("uri", uri);
        let convert = make("videoconvert", Some("convert"))?;
        let scale = make("videoscale", Some("scale"))?;
        let caps = make("capsfilter", Some("caps"))?;
        caps.set_property("caps", self.inner.canvas.caps());
        let sink = make("intervideosink", Some("inter_sink"))?;
        sink.set_property("channel", channel_name(layer));

        pipeline
            .add_many([&decode, &convert, &scale, &caps, &sink])
            .map_err(|e| MediaError::AddMany(e.to_string()))?;

        // Head of the static chain that decoded video pads get linked to.
        let chain_head = match kind {
            MediaKind::Video => {
                let rate = make("videorate", Some("rate"))?;
                pipeline.add(&rate).map_err(|e| MediaError::AddMany(e.to_string()))?;
                gst::Element::link_many([&convert, &scale, &rate, &caps, &sink])
                    .map_err(|e| MediaError::LinkElements(e.to_string()))?;
                convert.clone()
            }
            MediaKind::Image => {
                // imagefreeze turns the single decoded frame into an endless
                // stream at the canvas framerate.
                let freeze = make("imagefreeze", Some("freeze"))?;
                pipeline.add(&freeze).map_err(|e| MediaError::AddMany(e.to_string()))?;
                gst::Element::link_many([&freeze, &convert, &scale, &caps, &sink])
                    .map_err(|e| MediaError::LinkElements(e.to_string()))?;
                freeze
            }
        };

        // Route decoded pads: first video pad into the chain, everything else
        // (audio) into a throwaway fakesink — CueMesh2 is video-only, but an
        // unlinked decoder pad would error the pipeline.
        let head_weak = chain_head.downgrade();
        let pipeline_weak = pipeline.downgrade();
        decode.connect_pad_added(move |_src, pad| {
            let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
            let is_video = caps
                .structure(0)
                .map(|s| s.name().starts_with("video/") || s.name().starts_with("image/"))
                .unwrap_or(false);
            if is_video {
                if let Some(head) = head_weak.upgrade() {
                    if let Some(sink) = head.static_pad("sink") {
                        if !sink.is_linked() {
                            if let Err(e) = pad.link(&sink) {
                                tracing::warn!(?e, "failed to link video pad");
                            }
                            return;
                        }
                    }
                }
            }
            if let Some(pl) = pipeline_weak.upgrade() {
                let Ok(fakesink) = gst::ElementFactory::make("fakesink")
                    .property("sync", false)
                    .property("async", false)
                    .build()
                else {
                    return;
                };
                if pl.add(&fakesink).is_ok() {
                    let _ = fakesink.sync_state_with_parent();
                    if let Some(sink) = fakesink.static_pad("sink") {
                        if let Err(e) = pad.link(&sink) {
                            tracing::warn!(?e, "failed to link discard sink");
                        }
                    }
                }
            }
        });

        Ok(pipeline)
    }

    /// Swap in a new producer for `layer`, tearing down the old one, and bring
    /// it to `target` (PAUSED to preroll, PLAYING for live sources).
    fn install_producer(
        &self,
        layer: Layer,
        pipeline: gst::Pipeline,
        target: gst::State,
    ) -> Result<(), MediaError> {
        let shutdown = Arc::new(AtomicBool::new(false));
        self.spawn_producer_bus_watch(layer, &pipeline, shutdown.clone());

        let new = Producer {
            pipeline: pipeline.clone(),
            bus_shutdown: shutdown,
        };
        let old = {
            let slot = self.slot(layer);
            let mut guard = slot.producer.lock().unwrap_or_else(|p| p.into_inner());
            guard.replace(new)
        };
        if let Some(old) = old {
            old.teardown();
        }

        pipeline
            .set_state(target)
            .map_err(|e| MediaError::StateChange(format!("producer set_state({target:?}): {e}")))?;
        // Wait for preroll so failures (bad file, missing decoder) surface here.
        let (result, current, pending) = pipeline.state(gst::ClockTime::from_seconds(5));
        tracing::info!(?layer, ?result, ?current, ?pending, "producer preroll finished");
        if result.is_err() {
            return Err(MediaError::StateChange(format!(
                "producer preroll failed (state={current:?}) — see bus errors"
            )));
        }
        Ok(())
    }

    /// Run `f` with the layer's producer pipeline, or `NoProducer`.
    fn with_producer<T>(
        &self,
        layer: Layer,
        f: impl FnOnce(&gst::Pipeline) -> T,
    ) -> Result<T, MediaError> {
        let slot = self.slot(layer);
        let guard = slot.producer.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            Some(p) => Ok(f(&p.pipeline)),
            None => Err(MediaError::NoProducer(layer)),
        }
    }

    // ─── Transport ─────────────────────────────────────────────────────────

    /// Start (or resume) playback on a layer.
    pub fn play(&self, layer: Layer) -> Result<(), MediaError> {
        self.with_producer(layer, |p| {
            p.set_state(gst::State::Playing)
                .map(|_| ())
                .map_err(|e| MediaError::StateChange(format!("play({layer:?}): {e}")))
        })?
    }

    /// Freeze a layer in place (display keeps showing the last frame).
    pub fn pause(&self, layer: Layer) -> Result<(), MediaError> {
        self.with_producer(layer, |p| {
            p.set_state(gst::State::Paused)
                .map(|_| ())
                .map_err(|e| MediaError::StateChange(format!("pause({layer:?}): {e}")))
        })?
    }

    /// Freeze both layers (no-op on empty layers).
    pub fn pause_all(&self) {
        for layer in [Layer::A, Layer::B] {
            if let Err(e) = self.pause(layer) {
                if !matches!(e, MediaError::NoProducer(_)) {
                    tracing::warn!(?layer, %e, "pause_all");
                }
            }
        }
    }

    /// Tear down a layer's producer and make the layer transparent.
    pub fn stop(&self, layer: Layer) {
        self.abort_fade(layer);
        let old = {
            let slot = self.slot(layer);
            let mut guard = slot.producer.lock().unwrap_or_else(|p| p.into_inner());
            guard.take()
        };
        if let Some(old) = old {
            old.teardown();
        }
        self.slot(layer).compositor_pad.set_property("alpha", 0.0f64);
    }

    /// Cut everything to black: both producers torn down, alphas zeroed.
    /// The display pipeline keeps running (black frame).
    pub fn stop_all(&self) {
        self.stop(Layer::A);
        self.stop(Layer::B);
    }

    /// True if the layer currently has a producer (loaded or playing).
    pub fn is_loaded(&self, layer: Layer) -> bool {
        let slot = self.slot(layer);
        slot.producer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// Seek a layer to a position in ms.
    pub fn seek_ms(&self, layer: Layer, position_ms: u64) -> Result<(), MediaError> {
        self.with_producer(layer, |p| {
            p.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_mseconds(position_ms),
            )
            .map_err(|e| MediaError::StateChange(e.to_string()))
        })?
    }

    /// Adjust playback rate on a layer via a non-flushing position-preserving
    /// seek. Used sparingly by drift correction.
    pub fn set_rate(&self, layer: Layer, rate: f64) -> Result<(), MediaError> {
        if rate <= 0.0 {
            return Ok(());
        }
        self.with_producer(layer, |p| {
            let pos = p
                .query_position::<gst::ClockTime>()
                .unwrap_or(gst::ClockTime::ZERO);
            p.seek(
                rate,
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set,
                pos,
                gst::SeekType::End,
                gst::ClockTime::ZERO,
            )
            .map_err(|e| MediaError::StateChange(e.to_string()))
        })?
    }

    /// Current playback position of a layer in ms.
    pub fn position_ms(&self, layer: Layer) -> Option<u64> {
        self.with_producer(layer, |p| {
            p.query_position::<gst::ClockTime>().map(|t| t.mseconds())
        })
        .ok()
        .flatten()
    }

    /// Duration of the media on a layer in ms, if known.
    pub fn duration_ms(&self, layer: Layer) -> Option<u64> {
        self.with_producer(layer, |p| {
            p.query_duration::<gst::ClockTime>().map(|t| t.mseconds())
        })
        .ok()
        .flatten()
    }

    // ─── Alpha / fades ─────────────────────────────────────────────────────

    /// Set a compositor sink pad's alpha directly (no ramp).
    pub fn set_alpha(&self, layer: Layer, alpha: f64) {
        self.abort_fade(layer);
        self.slot(layer)
            .compositor_pad
            .set_property("alpha", alpha.clamp(0.0, 1.0));
    }

    /// Read the current compositor alpha for a layer.
    pub fn alpha(&self, layer: Layer) -> f64 {
        self.slot(layer).compositor_pad.property::<f64>("alpha")
    }

    fn abort_fade(&self, layer: Layer) {
        let slot = self.slot(layer);
        if let Ok(mut guard) = slot.fade.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }

    /// Replace this layer's active fade task with a new one.
    pub(crate) fn install_fade(&self, layer: Layer, handle: tokio::task::JoinHandle<()>) {
        let slot = self.slot(layer);
        if let Ok(mut guard) = slot.fade.lock() {
            if let Some(prev) = guard.replace(handle) {
                prev.abort();
            }
        }
    }

    /// Direct access to the compositor pad for the fade animator.
    pub(crate) fn compositor_pad(&self, layer: Layer) -> gst::Pad {
        self.slot(layer).compositor_pad.clone()
    }

    // ─── Bus watches ───────────────────────────────────────────────────────

    /// Display-pipeline problems are engine-fatal enough to log loudly, but we
    /// intentionally never forward them as layer events.
    fn spawn_display_bus_watch(&self) {
        let Some(bus) = self.inner.display.bus() else { return };
        std::thread::Builder::new()
            .name("cuemesh2-display-bus".into())
            .spawn(move || {
                for msg in bus.iter_timed(gst::ClockTime::NONE) {
                    use gst::MessageView as M;
                    match msg.view() {
                        M::Error(err) => {
                            tracing::error!(
                                source = %err.src().map(|s| s.path_string().to_string()).unwrap_or_default(),
                                error = %err.error(),
                                debug = %err.debug().map(|d| d.to_string()).unwrap_or_default(),
                                "display bus: ERROR"
                            );
                        }
                        M::Warning(w) => {
                            tracing::warn!(warning = %w.error(), "display bus: WARNING");
                        }
                        _ => {}
                    }
                }
            })
            .expect("spawn display bus watch");
    }

    /// Per-producer bus watch. Exits when the producer is torn down.
    fn spawn_producer_bus_watch(
        &self,
        layer: Layer,
        pipeline: &gst::Pipeline,
        shutdown: Arc<AtomicBool>,
    ) {
        let Some(bus) = pipeline.bus() else { return };
        let tx = self.inner.events_tx.clone();
        std::thread::Builder::new()
            .name(format!("cuemesh2-producer-bus-{layer:?}"))
            .spawn(move || {
                while !shutdown.load(Ordering::SeqCst) {
                    let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(300)) else {
                        continue;
                    };
                    use gst::MessageView as M;
                    match msg.view() {
                        M::Eos(_) => {
                            tracing::info!(?layer, "producer: EOS");
                            let _ = tx.send(MediaEvent::Eos(layer));
                        }
                        M::Error(err) => {
                            let source = err
                                .src()
                                .map(|s| s.path_string().to_string())
                                .unwrap_or_else(|| "unknown".into());
                            let dbg = err.debug().map(|d| d.to_string()).unwrap_or_default();
                            tracing::error!(?layer, source = %source, error = %err.error(), debug = %dbg, "producer: ERROR");
                            let _ = tx.send(MediaEvent::Error {
                                layer,
                                source,
                                message: err.error().to_string(),
                            });
                        }
                        M::Warning(w) => {
                            tracing::warn!(?layer, warning = %w.error(), "producer: WARNING");
                        }
                        _ => {}
                    }
                }
            })
            .expect("spawn producer bus watch");
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        for slot in [&self.layer_a, &self.layer_b] {
            if let Ok(mut guard) = slot.producer.lock() {
                if let Some(p) = guard.take() {
                    p.teardown();
                }
            }
        }
        let _ = self.display.set_state(gst::State::Null);
    }
}

/// Sleep helper used by tests; producers settle asynchronously.
#[cfg(test)]
fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(50));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_engine_and_starts_black() {
        let engine = MediaEngine::new().expect("build");
        settle();
        // Both layers start transparent.
        assert!((engine.alpha(Layer::A) - 0.0).abs() < 1e-6);
        assert!((engine.alpha(Layer::B) - 0.0).abs() < 1e-6);
        assert!(!engine.is_loaded(Layer::A));
        assert!(!engine.is_loaded(Layer::B));
    }

    #[test]
    fn set_alpha_direct() {
        let engine = MediaEngine::new().expect("build");
        engine.set_alpha(Layer::B, 0.5);
        assert!((engine.alpha(Layer::B) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn transport_errors_without_producer() {
        let engine = MediaEngine::new().expect("build");
        assert!(matches!(engine.play(Layer::A), Err(MediaError::NoProducer(Layer::A))));
        assert!(engine.position_ms(Layer::A).is_none());
        // stop on an empty layer is a harmless no-op.
        engine.stop(Layer::A);
    }

    #[test]
    fn testscreen_loads_and_stops() {
        let engine = MediaEngine::new().expect("build");
        engine.load_testscreen(Layer::A).expect("testscreen");
        assert!(engine.is_loaded(Layer::A));
        engine.stop(Layer::A);
        assert!(!engine.is_loaded(Layer::A));
    }
}
