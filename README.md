# CueMesh2

**Synchronised video playback for tiny theatres, on a budget.**

Got a bunch of old PCs or Raspberry Pis lying around? Want to play video across
multiple projectors/screens in sync, with crossfades, from one operator
workstation? That's what this does.

```
Controller (your laptop)
    │
    ├── Client 1  → projector 1
    ├── Client 2  → projector 2
    └── Client 3  → projector 3 ... etc
```

No internet, no accounts, no cloud. Just your LAN and a switch.

---

## What it does

- **GO** a cue — all clients start playing in sync (~50–150ms drift).
- **Crossfade** between videos (two layers, fade the alpha between them).
- **Fade to/from solid-colour** screens for blackouts/transitions.
- **Preflight check** — the controller checks every client has the right media
  files before show time.
- **Survives network drops** — a client that loses the network keeps playing
  the current cue until reconnection.

---

## What you need

One **Controller** machine (your laptop — runs the show UI) and as many
**Client** machines as you like (each with a screen/projector).

Supported platforms:


| Machine                       | Works as                     |
| ----------------------------- | ---------------------------- |
| Windows x86_64                | Controller + Client          |
| macOS (Intel & Apple Silicon) | Controller + Client          |
| Linux x86_64 (any PC)         | Controller + Client          |
| Linux aarch64 (Pi 4/5)        | Controller + Client          |
| (pending) Linux armv7 (Pi 3)            | Client only (720p30 ceiling) |

---

## Quick start

1. **Install GStreamer** on each machine (runtime dependency — see below).
2. **Copy your media** to each client (or use the push-media feature).
3. **Create a show file** (`.cuemesh.toml`) — see `examples/example_show.cuemesh.toml`.
4. **Run the controller:**

   ```sh
   cuemesh2-controller
   ```
5. **Run each client:**

   ```sh
   CUEMESH_NAME="Stage Left" cuemesh2-client
   ```

   Clients auto-discover the controller on the LAN.

---

## GStreamer install

CueMesh2 uses GStreamer for all media playback. It's **not** statically linked,
so you need the runtime:

- **Linux (Debian/Ubuntu/Pi OS):**

  ```sh
  sudo apt install libgstreamer1.0-0 libgstreamer-plugins-base1.0-0 \
    libgstreamer-plugins-bad1.0-0 gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly
  ```
- **Windows / macOS:** Download the official GStreamer runtime from
  [gstreamer.freedesktop.org](https://gstreamer.freedesktop.org/download/)
  (or use the bundled installer if available).

---

## The show file

Cues are defined in a TOML file. Here's the gist:

```toml
[[cues]]
name = "Opening"
type = "video"
file = "opening.mp4"
fade_in_ms = 500       # fade up from black over 500ms — also the crossfade duration into this cue

[[cues]]
name = "Fade to Black"
type = "color"
color = "#000000"
fade_in_ms = 1500
```

See `examples/example_show.cuemesh.toml` for a real example.

---

## Building from source

```sh
git clone https://github.com/drhmedia/cuemesh2
cd cuemesh2
cargo build --release
```

Binaries end up in `target/release/`:

- `cuemesh2-controller`
- `cuemesh2-client`

---

## Updating

Updates are **operator-triggered from the controller** — nothing updates
itself behind your back, and clients never need internet:

1. **Update controller** (toolbar): when the controller machine has internet,
   it downloads the latest signed release, verifies it, and asks you to
   confirm the restart. It also caches the client binaries for every platform
   into an `updates/` folder next to the controller, so step 2 works with no
   internet at all. (No internet ever? Copy an `updates/` bundle in by hand.)
2. **Update fleet** (toolbar, or per-client in the roster): sends the right
   binary to each out-of-date client over the LAN. Clients verify the
   cryptographic signature and *stage* the update — playback is never
   touched. You then click **Apply** (per client or fleet-wide) when nothing
   is on stage; a client that is playing politely refuses.

The updater only swaps the CueMesh binaries. On Windows/macOS, a release that
needs a newer bundled GStreamer runtime will tell you to reinstall instead.

---

## A note on media formats

**Recommended: H.264 video in an MP4 container, 30fps.** Hardware decoding works on
almost everything. Avoid AV1 — most machines don't have hardware decoders for
it and the software decoder is too slow for smooth playback.

---

## Shout-out to the open-source libraries that make this possible

CueMesh2 is built on the shoulders of many excellent projects. Thank you!

- [**Rust**](https://www.rust-lang.org/) — the language
- [**GStreamer**](https://gstreamer.freedesktop.org/) — the media engine that does all the actual work (decoding, compositing, crossfading)
- [**Tokio**](https://tokio.rs/) — async runtime
- [**tokio-tungstenite**](https://github.com/sdroege/rust-tungstenite) — WebSocket transport
- [**egui / eframe**](https://github.com/emilk/egui) — immediate-mode GUI
- [**mdns-sd**](https://github.com/keepsimple1/mdns-sd) — service discovery (no mDNS daemon needed)
- [**serde**](https://serde.rs/) — serialisation (JSON on the wire, TOML on disk)
- [**sha2**](https://github.com/RustCrypto/hashes) — content hashing for preflight checks
- [**tracing**](https://github.com/tokio-rs/tracing) — structured logging
- [**anyhow / thiserror**](https://github.com/dtolnay/anyhow) — error handling

---

## Status

Very much a work in progress. Things will break, change, and improve. If you
try it out, I'd love to hear how it went.

**MIT licensed** — do whatever you want with it.
