# CueMesh2 — Agent Guidance

## What Is This Project?

CueMesh2 is an **open-source, portable, offline, LAN-only synchronized video playback system** for small theatres. A technician runs cues from a **Controller** machine, and one or more **Client** nodes play media in sync (~50–150ms). The killer feature over hobby-grade solutions: **crossfades between videos** and clean fades to/from black, plus **cross-platform binaries** that run on almost any machine a small theatre might have lying around.

This is a **fresh project** — not a port of anything. It is inspired by an earlier Python prototype (see `../CueMesh/`) for the general concept, but the design decisions below take precedence over anything in that older code.

---

## Design Goals

- **Portable** — runs from a folder; no installer; USB-stick friendly.
- **Offline / LAN-only** — no internet dependency; works on consumer routers.
- **Cross-platform is the #1 priority** — the same binary approach should work on Linux, Windows, macOS, and ARM Linux (Raspberry Pi 3 and up).
- **Local mode** — controller and client must both run happily on a single machine (both for testing and for solo operators).
- **Sync target** — medium sync: ~50–150ms on a typical LAN.
- **Resilient clients** — a client that loses network must keep playing its current cue smoothly, without crashing, until reconnect.
- **File format** — hand-editable TOML (`.cuemesh.toml`).
- **Single fullscreen display per client**.
- **Minimal UI** — small, functional, no design flourishes. The video engine is what matters.
- **MIT licensed**.

---

## Tech Stack

| Concern | Choice | Why |
|---------|--------|-----|
| Language | **Rust** | Cross-compiles cleanly to every target below; no GC pauses; small binaries |
| Media engine | **GStreamer** (via `gstreamer-rs`) | Handles multi-layer compositing, crossfades, hardware accel, and runs on every target platform |
| Async runtime | **`tokio`** | Networking, IPC, timers |
| WebSocket | **`tokio-tungstenite`** | Controller ↔ client transport |
| Service discovery | **`mdns-sd`** | Pure-Rust mDNS/zeroconf, no libdbus needed |
| Serialization | **`serde` + `serde_json` + `toml`** | JSON on the wire, TOML on disk |
| Hashing | **`sha2`** | Preflight media verification |
| UI | **`egui` + `eframe`** | Pure Rust, cross-platform, no system Qt/GTK dep |
| Logging | **`tracing` + `tracing-subscriber`** | Structured logs, JSONL client-side |
| Packaging | **`cargo` + platform installers** (`.deb`, `.msi`, `.dmg` where useful) | Simple, boring, works |

**Notes on GStreamer:** it is a runtime dependency, not statically linked.
- **Linux / Pi:** relies on the distro's GStreamer packages. The installer script/`.deb` declares them.
- **Windows / macOS:** **we bundle the official GStreamer runtime with our installer.** Users get one download; no separate GStreamer install. Expect ~50–80 MB of extra installer weight per platform. Worth it.

---

## Target Platforms

| Platform | Support level | Notes |
|----------|--------------|-------|
| Linux x86_64 | **First class** | Controller + client |
| Linux aarch64 (Pi 4/5) | **First class** | Client (and controller if the operator wants) |
| Linux armv7 (Pi 3) | **Best effort** | Client only; expect 720p/30 ceiling; may need `v4l2` hardware decoding |
| Windows x86_64 | **First class** | Controller + client |
| macOS (Intel + Apple Silicon) | **First class** | Controller + client |
| Android / iOS | **Not planned** | If it falls out easily one day, fine — do not design around it |

Any Rust-native dependency chosen for this project **must** compile on all first-class targets. If it doesn't, pick something else.

---

## Architecture

Hub-and-spoke over WebSocket:

```
Controller (hub, port 9420)
    │
    ├── Client 1  (local or remote)
    ├── Client 2
    └── Client N
```

The controller and a client can run in the same process (single-box mode) or on separate machines. Single-box mode is not a special case in the protocol — the client just connects to `127.0.0.1`.

### Workspace Layout

```
CueMesh2/
├── Cargo.toml              # workspace root
├── crates/
│   ├── cuemesh2-shared/    # protocol, show file, clock sync, hashing, logging
│   ├── cuemesh2-controller/# server, discovery, preflight, UI
│   ├── cuemesh2-client/    # connection, media engine, discovery, UI
│   └── cuemesh2-media/     # GStreamer pipeline wrapper (used by client)
├── examples/
│   └── example_show.cuemesh.toml
├── docs/
└── CLAUDE.md
```

Splitting the media engine into its own crate lets us test the pipeline in isolation and swap the backend later if we ever have to.

---

## Media Engine (`cuemesh2-media`)

The media engine is the heart of this project. Everything else exists to serve it.

### Pipeline Shape

Two video layers feed a `compositor` (or `glvideomixer` for GL-accelerated blending) which renders to a single fullscreen sink. Each layer has its own `uridecodebin` and an `alpha` control we drive from Rust.

```
Layer A: filesrc → decodebin → videoconvert → videoscale → alpha ─┐
                                                                 ├─→ compositor → videosink (fullscreen)
Layer B: filesrc → decodebin → videoconvert → videoscale → alpha ─┘
```

(A third, always-below **background layer** carries the optional show poster —
see the Poster note under Network Protocol. It is not a cue layer.)

Two cue layers is the entire scope. That gives us:
- **Fade from black** — start layer A at alpha 0, ramp to 1.
- **Fade to black** — ramp layer A alpha to 0.
- **Crossfade to next cue** — preload layer B, ramp A down and B up simultaneously.

No picture-in-picture, no wipes, no arbitrary N layers.

### Video Sink

- Linux (Pi included): prefer `glimagesink` where GL is available, fall back to `xvimagesink` or `kmssink`.
- Windows: `d3d11videosink`.
- macOS: `glimagesink` or `osxvideosink`.

The engine picks the sink at startup based on what GStreamer's registry reports available.

### Fade Model

Every cue has a **single `fade_in_ms`**. When nothing is on air it is the
fade-from-black duration; when a cue is already playing it is the crossfade
duration *into* this cue (i.e. the incoming cue's fade-in drives the
crossfade). There is no separate `fade_out_ms` or `crossfade_to_next_ms`.
To **fade to black** (or any colour), add a `color`-type cue and GO into it —
its `fade_in_ms` is the fade duration.

### Auto-Preload Behaviour

The controller speculatively **preloads (STANDBY) the selected cue onto the
idle layer** once the selection settles, so GO starts instantly instead of
stalling on a cold decode. The controller can also issue `LOAD_CUE` explicitly
(e.g. if the operator jumps out of order), which overrides any speculative
preload.

### Playback Control API (what the client calls into)

- `load(layer, path)` — preroll a file on a given layer, do not play.
- `play_at(utc_ms)` — start playback at a synchronized wall-clock time.
- `pause()`, `stop()`, `seek_to(ms)`.
- `set_rate(rate)` — clamped to `[0.95, 1.05]` for drift correction (wider than the old system's 0.98–1.02 because Pi 3 clocks are less stable).
- `set_alpha(layer, value)` — for fades and crossfades. Alpha ramps are driven by the client's UI thread, not GStreamer, so we keep them predictable.
- `set_volume(layer, value)`.
- Position/EOF callbacks via async channels.

### Supported Media

- **Video containers/codecs:** anything GStreamer supports out of the box. Recommend **H.264 in MP4** for maximum hardware-acceleration coverage, or WebM (VP9 + Opus) where CPU headroom allows.
- **Images:** treat as a "video" of arbitrary duration by looping a single decoded frame — keeps the pipeline uniform.
- **Colour cues:** a solid-colour `videotestsrc` producer (`load_color`), used for fades to black/white; no media file.
- **Ceiling:** 1080p30 on x86 and Pi 4; 720p30 on Pi 3 (H.264 only, see below).

### Hardware Decode on Pi 3 (armv7)

On armv7 the media engine **tries `v4l2h264dec` first** for H.264 sources and **falls back to software decode with a startup log warning** for other codecs (VP9, AV1, etc.). This isn't a hard requirement — non-H.264 files still play, they're just CPU-bound and may drop frames above ~480p. The recommended encoding preset for Pi 3 deployments is therefore H.264 baseline/main profile in an MP4 container.

---

## Network Protocol (`cuemesh2-shared::protocol`)

JSON envelopes over WebSocket on **port 9420**:

```json
{
  "type": "<MSG_TYPE>",
  "ts_utc_ms": 1234567890123,
  "payload": { ... }
}
```

### Message Set (initial cut — refine as we build)

**Controller → Client**
- `HELLO_ACK`
- `LOAD_CUE` — layer, kind (video/image/color), file path (or color), start/end ms, fade_in_ms
- `PLAY_AT` — `master_start_utc_ms`
- `SEEK_TO`, `SET_RATE`, `SET_VOLUME`
- `CROSSFADE` — operator-triggered manual crossfade to a specific cue; duration_ms
- **`PAUSE`** — freeze playback in place on all layers; alphas held; no fades.
- **`FADE`** — fade all layers to black at the fixed `DEFAULT_FADE_MS` (1500ms) rate, then stop.
- **`STOP`** — cut all layers to black instantly, stop pipelines.
- `SHOW_TESTSCREEN`
- `REQUEST_STATUS`, `SYNC`, `READY_CHECK`

Note: per-cue `fade_in_ms` is a payload field on `LOAD_CUE`/`PLAY_AT` and is executed automatically by the client — it is not a separate wire message. `PAUSE` / `FADE` / `STOP` are the three operator-facing override commands.

**Client → Controller**
- `HELLO` (client_id, human name), `READY`
- `STATUS` (state, position, rate, volume, layer alphas)
- `DRIFT` (measured drift ms)
- `HEARTBEAT`, `LOG`, `SYNC_REPLY`

### Trust Model

The deployment is a single theatre's LAN, so **any client that connects is automatically accepted**. No manual approval step. The controller optionally reads a **blacklist** of `client_id`s from its config; blacklisted clients get rejected at `HELLO` and never enter the roster. There is no `ACCEPT` / `REJECT` / `AUTH` handshake — clients join, appear in the roster, and start receiving sync immediately.

Client state machine: `idle → loading → ready → playing → paused → error → black`.

Since this is a clean-slate protocol, we're free to iterate on the exact payload shapes as implementation proceeds — pin them down in `cuemesh2-shared::protocol` with `serde`-derived structs and version the envelope from day one.

---

## Clock Sync (`cuemesh2-shared::clock_sync`)

Standard NTP-style four-way handshake, driven from the controller every 1000ms while a client is accepted:

1. Controller sends `SYNC` with `t1` (UTC ms).
2. Client records `t2` on receive, `t3` on send, replies with both.
3. Controller records `t4` on receive.
4. `offset = ((t2 - t1) + (t3 - t4)) / 2`.

Drift correction (client-side):
- Rolling median over the last 8 samples to reject RTT outliers.
- Proportional rate adjustment within `[0.95, 1.05]`.
- Hard seek when drift exceeds a configurable threshold (default 300ms).

---

## Resilience Requirements

This is a hard requirement, not a nice-to-have.

- If a client loses network mid-cue, it **must keep playing** the current cue to its natural end (or until told otherwise) without stuttering, crashing, or dropping frames.
- On disconnect, the client applies its configured **dropout policy**: `continue`, `freeze`, or `black`.
- The client should **automatically attempt to reconnect** in the background while playback continues.
- On reconnect, the controller re-establishes clock sync and asks for status before issuing new commands.

Practically: the WebSocket task and the media engine task are independent. The media engine never waits on the network, and the network task never blocks the media engine.

---

## Show File Format

TOML, extension `.cuemesh.toml`. Rough shape (final field names get pinned when we write the `Show` struct):

```toml
[show]
title = "..."
version = 1
media_root = "~/cuemesh_media"
dropout_policy = "continue"  # continue | freeze | black

[show.sync]
max_drift_ms = 150
start_lead_ms = 250

[show.sync.correction]
rate_min = 0.95
rate_max = 1.05
hard_seek_threshold_ms = 300
sync_interval_ms = 1000

# Optional idle poster: image or looping video shown on connect / between cues.
[show.poster]
type = "image"                 # image | video (video loops)
file = "poster.jpg"

[[cues]]
id = "cue-001"                 # auto-generated; hidden in the editor UI
name = "Opening"
type = "video"                 # video | image | color
file = "opening.webm"          # relative to media_root (omit for color cues)
fade_in_ms = 500               # fade-from-black, or crossfade-in when a cue is on air
in_ms = 2500                   # start 2.5s into the file (video only)
out_ms = 15000                 # end at 15s; omit to play to natural end (video only)
loop = false                   # loop between in/out until replaced (video only)
on_end = "cut"                 # cut | freeze | fade — what happens at the out-point

[[cues]]
id = "cue-002"
name = "Fade to black"
type = "color"
color = "#000000"              # solid colour; file is unused
fade_in_ms = 1500
```

`default_fade_ms` and the `[show.settings]` table were removed; the operator
BLACKOUT/FADE command uses a fixed `DEFAULT_FADE_MS` constant.

**In/out/loop** are implemented as GStreamer segment seeks stored per layer
(`MediaEngine::set_bounds`) and preserved across drift hard-seeks; looping
re-seeks on `SEGMENT_DONE` for a seamless seam. The client's drift corrector
maps master wall-clock onto the `[in, out]` window (modulo the loop length for
looped cues). **`on_end`** is applied by the client when the layer EOSes at the
out-point: `cut` stops it, `freeze` holds the last frame, `fade` fades out over
the cue's `fade_in_ms`.

**Poster** is a **dedicated background layer** in the media engine, composited
*below* cue layers A and B (`MediaEngine::load_poster` / `stop_poster`, fed by
its own `intervideosrc` at zorder 0). The client (re)loads it purely from
`ShowSync` — on connect and on every show update — and then leaves it alone: it
shows through automatically whenever both cue layers are transparent (nothing
playing, or an armed-but-not-yet-played cue at alpha 0), with no idle
bookkeeping. Video posters loop seamlessly; images hold.

Loaded and validated with `serde` + `toml`. Validation: unique cue IDs, file existence relative to `media_root`, enum variants, sensible ranges.

---

## Discovery, Preflight, Diagnostics

- **Discovery** — controller advertises `_cuemesh._tcp.local.` via `mdns-sd`; client browses and offers a manual IP fallback.
- **Preflight** — before running a show, controller collects SHA-256 hashes from each client and reports `ok`/`missing`/`mismatch` per file.
- **Support bundle** — a ZIP of system info, active show, and rotating logs for troubleshooting. Rolled by `tracing-appender`, aggregated on request.

---

## UI

Minimal, functional, `egui`.

**Controller** — show manager (open/save/save-as via a pure-egui file dialog), a cue **table editor** (name/type/source/fade-in with per-row actions; ids are auto-generated and hidden), cue list with GO/NEXT/PREV/BLACKOUT, client roster with status, diagnostics/log view. Toolbar/table icons use the `egui-phosphor` icon font (egui's default fonts lack symbol glyphs).

**Client** — a chromeless, resizable box showing *just* the video canvas. Text appears only (1) on startup / when disconnected: a small grey status line at the bottom; and (2) on testscreen: the client's name + id centred over the pattern. Controller selection is automatic (first mDNS-discovered controller adopted while offline; manual override via `CUEMESH_CONTROLLER`).

The GUI runs on the main thread. Everything I/O-heavy is on `tokio` tasks communicating with the GUI via channels.

---

## Coding Standards

- `clippy` on default lints; no `#[allow(...)]` without an inline comment explaining why.
- No `unwrap()` or `expect()` in library code — use `?` and typed errors.
- `thiserror` for library errors, `anyhow` at binary entry points.
- Doc comments on all public items in `cuemesh2-shared` and `cuemesh2-media`.
- Tests: unit tests in `#[cfg(test)]` modules alongside code; integration tests in each crate's `tests/`.
- Format with `rustfmt` (default settings).
- Every commit should build and pass tests on Linux x86_64 at minimum.

---

## Out of Scope

- Audio-only cues
- More than 2 simultaneous video layers
- Wipes, PiP, or effects beyond alpha crossfade
- Multi-monitor
- Remote media transfer (media is pre-staged on each client)
- Internet features (updates, telemetry, cloud sync)
- Timecode / MTC / OSC integration (maybe later; not now)
- Mobile clients (Android/iOS)
