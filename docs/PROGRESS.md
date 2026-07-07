# CueMesh2 — Autonomous Feature Session Progress

> Living document for the feature-building session started 2026-07-06.
> Updated after every completed task so work can resume in a fresh session.
> Read `CLAUDE.md` first for the design brief; this file records what has
> actually been built, what's in flight, and every decision that deviates
> from or refines the brief.

## Decisions made this session (differ from / refine CLAUDE.md)

1. **Audio is out of scope entirely** (user said so mid-session). Decoded
   audio pads are drained into `fakesink`; there is no volume anywhere in
   protocol or show file. Show files with old `volume =` keys still parse
   (serde ignores unknown fields).
2. **Media engine architecture changed** from "two decoders → compositor"
   to three pipelines: a persistent *display* pipeline (2× `intervideosrc`
   → `compositor`(I420-pinned, 2-frame latency budget) → `glimagesink`,
   never stops) plus one disposable *producer* pipeline per layer
   (`uridecodebin` → convert/scale/rate → canvas caps → `intervideosink`).
   Rationale: compositor is an aggregator that stalls the whole output if
   any linked pad starves; inter channels isolate layers completely, so
   loading/seeking/killing one layer can never blink the screen.
   All frames conform to one canvas (default 1920×1080@30, I420).
3. **Media paths on the wire are relative to a media root.** Controller
   resolves against the show's `media_root`; each client resolves against
   its own root (`CUEMESH_MEDIA_ROOT`, default `~/cuemesh_media`).
   Absolute paths only ever worked single-box.
4. **Media transfer controller→client is IN scope** (user overrode the
   brief): WS binary frames `[u64 BE transfer_id][chunk]` between
   `MEDIA_PUSH_BEGIN` / `MEDIA_PUSH_END` envelopes, 256 KiB chunks,
   client verifies SHA-256 before moving into place, replies
   `MEDIA_PUSH_RESULT` (+ `MEDIA_PUSH_PROGRESS` for the UI).
5. **Protocol v2** (`PROTOCOL_VERSION = 2`): removed `SET_VOLUME` and
   `CROSSFADE` (crossfade is now expressed by `PLAY_AT{crossfade_ms}`),
   added `SHOW_SYNC`, `RESUME`, `HIDE_TESTSCREEN`, `MEDIA_CHECK`/`REPORT`,
   `MEDIA_PUSH_*`. `LOAD_CUE` carries `kind` (video|image) and no volume.
   `PLAY_AT` carries `fade_in_ms` and `crossfade_ms: Option<u32>` — when
   set, client ramps the incoming layer 0→1 and the other layer →0 over
   the same duration, then stops the old layer.
6. **AV1 media is a trap** on typical machines (no hw decode, libaom is
   too slow, VLC hides this by shipping dav1d). H.264 MP4 is the format
   to recommend; hw decode (`vah264dec`/`nvh264dec`) is picked up
   automatically. A future preflight warning about codecs would be nice
   (not built yet).

## Task list & status

| # | Task | Status |
|---|------|--------|
| 1 | Media engine rearchitecture (intervideo, per-layer producers, images, testscreen) | ✅ done, committed `b4ecb05` |
| 2 | Protocol v2 (relative paths, SHOW_SYNC, crossfade PLAY_AT, media check/push, transfer framing) | ✅ done, tested |
| 3 | Client: media root, crossfade handling, dropout policy, drift correction, testscreen, media check/receive | ✅ done, tested |
| 4 | Controller: cue sequencing (layer alternation), GO/NEXT/PREV/BLACKOUT + spacebar, preflight+push UI, heartbeat staleness | ✅ done, tested |
| 5 | Show editor UI (cue CRUD, reorder, save TOML, rfd file dialogs) | 🔨 next |
| 6 | mDNS discovery (controller advertises, client browses + manual IP) | ✅ done, tested |
| 7 | Tests + docs | 🔨 integration handshake + example show done; `docs/OPERATION.md` pending |

### Verification pass (after tasks 2–6, before committing)
`cargo test --workspace` is green (36 tests) and `cargo clippy --workspace
--all-targets` is clean. Two **server bugs** were found while getting the
integration test to pass and are now fixed in `server.rs`:

1. **Blacklist self-deadlock (severe).** `handle_conn` held the `state`
   mutex guard and then called `log(&state, …)`, which re-locks the same
   non-reentrant `std::sync::Mutex` → the task blocked forever *and never
   released the guard*, which would freeze the entire controller (every
   task that touches `state`). Fixed by computing the blacklist verdict,
   dropping the guard, then logging.
2. **Writer task leak on disconnect.** `tokio::join!(reader, writer)` never
   completed because the outbound `mpsc` sender was still held by both the
   local var and the roster entry, so `out_rx.recv()` never returned `None`.
   The client row was therefore never removed on disconnect. Fixed by
   awaiting the reader, deregistering the client (drops the roster's
   sender), dropping the local sender, then awaiting the writer.

## What's done in detail

### Task 1 — media engine (`crates/cuemesh2-media`) ✅
- `pipeline.rs` fully rewritten around the display/producer split.
  Public API: `new()/with_canvas()`, `load(layer, path, MediaKind)`,
  `load_testscreen(layer)`, `play/pause/stop(layer)`, `pause_all`,
  `stop_all`, `seek_ms`, `set_rate`, `position_ms`, `duration_ms`,
  `set_alpha/alpha`, `is_loaded`, `subscribe()` → per-layer `Eos`/`Error`.
- `fades.rs` unchanged (alpha ramp animator, `fade`/`crossfade`).
- Examples: `play_file.rs` (single file or image), `crossfade.rs`
  (A plays → B preloads without disturbing A → alpha crossfade → B).
  Both verified on real media; steady-state 0 dropped frames at 1080p30.
- Unit tests: engine builds black, alpha set, transport errors without
  producer, testscreen load/stop. `cargo test -p cuemesh2-media` green.

### Task 2 — protocol v2 (`crates/cuemesh2-shared`) ✅
- `transfer.rs` (new): chunk framing `encode_chunk`/`decode_chunk` + tests.
- `protocol.rs`: new message set as per decision 5, round-trip tests
  for LOAD_CUE, PLAY_AT (incl. missing-fields leniency), SHOW_SYNC,
  MEDIA_CHECK/REPORT, MEDIA_PUSH begin/result. `Layer` derives `Hash`.
- `show.rs`: `volume` removed from `Cue`; `ShowFile::new_empty()`,
  `save()`, `parse_str()`/`FromStr` added; validation checks empty ids and
  empty/absolute file paths (must be media-root-relative).
- `lib.rs`: `pub mod transfer;` added.

### Task 3 — client (`crates/cuemesh2-client`) ✅
Built as planned below: split into `lib.rs` + thin `main.rs`; media root
resolution, PLAY_AT with clock-offset-corrected deadline + crossfade,
dropout policy on disconnect, drift correction with hysteresis, testscreen,
MEDIA_CHECK/receive with path sanitization + sha256 verify + atomic rename,
mDNS browse + manual-URL connect UI. Unit tests for `sanitize_rel_path`
and `check_file`.

### Task 4 — controller (`crates/cuemesh2-controller`) ✅
Built as planned below: `RunState` with layer alternation, GO/NEXT/PREV/
BLACKOUT/STOP + spacebar/arrows, `Outgoing::{Msg,Chunk}` outbound queue,
preflight (`preflight.rs`) with hash + push-missing, roster with staleness/
offset/drift/per-file status/push progress, `discovery.rs` advertise.
Integration test `tests/handshake.rs` (HELLO→ACK→SHOW_SYNC ordering, roster
folding, disconnect removal, blacklist rejection). See the verification pass
above for the two server bugs fixed while landing this test.

### Task 6 — discovery ✅
`cuemesh2-shared::protocol::MDNS_SERVICE_TYPE = "_cuemesh._tcp.local."`.
Controller `discovery::advertise(port)` registers the service; client
`discovery::spawn_browser` mirrors resolved instances into
`state.discovered` and the connect UI. Manual URL entry always available.

## How to verify things while developing

- Engine smoke tests (need a GUI session + H.264 files in ~/cuemesh_media):
  - `cargo run -p cuemesh2-media --example play_file -- ~/cuemesh_media/countdown.mp4 6`
  - `cargo run -p cuemesh2-media --example crossfade -- ~/cuemesh_media/countdown.mp4 ~/cuemesh_media/test1.mp4 2000`
- Full stack: `cargo run -p cuemesh2-controller` and
  `cargo run -p cuemesh2-client` (env `CUEMESH_CONTROLLER`, `CUEMESH_NAME`,
  `CUEMESH_MEDIA_ROOT`), controller "Open show…" uses `CUEMESH_SHOW` or
  `examples/example_show.cuemesh.toml`.
- `cargo test --workspace` must stay green; commit per completed task.

## Planned design details for remaining tasks

### Task 3 — client
- `CUEMESH_MEDIA_ROOT` env (default `~/cuemesh_media`), tilde-expanded;
  `LoadCue.file` joined onto it. `LoadCue.kind` → `MediaKind`.
- Keep per-layer bookkeeping in client state: which cue is loaded/playing
  on each layer (replaces the `PENDING_FADE_IN` static hack; store
  pending fade per layer instead).
- PLAY_AT: sleep until `master_start_utc_ms` **corrected by the filtered
  clock offset** (client may be seconds off UTC); then `play(layer)`;
  apply `crossfade_ms` semantics (ramp both, stop old layer after ramp).
- Dropout policy from SHOW_SYNC stored in state; on WS disconnect apply:
  continue (nothing), freeze (`pause_all`), black (fade to black over
  default_fade_ms then `stop_all`).
- Drift loop: on each SYNC compute offset t2/t3 vs t1 (already replies);
  ALSO keep a client-side `OffsetFilter` (median-of-8) so client knows
  controller time. Playback drift = (controller_now - master_start) -
  position_ms → `correction_for()` → `set_rate`/`seek_ms` on the playing
  layer. Send DRIFT message for the roster UI.
- MEDIA_CHECK: hash each rel_path under media root (spawn_blocking),
  reply MEDIA_REPORT. MEDIA_PUSH: write `<root>/.incoming-<id>`, hash on
  END, rename into place, MEDIA_PUSH_RESULT; progress every ~1 MB.
- EOS handling: on `MediaEvent::Eos(layer)` if that cue has
  `fade_out_ms` > 0 the fade should have been *started before* EOS
  (see auto-fade-out below) — MVP: on EOS just stop(layer). Auto
  fade-out timer: when playing a cue with fade_out_ms and known
  duration, schedule fade at duration−fade_out. Auto-preload of next
  cue (crossfade_to_next) can be client-driven from ShowSync later —
  controller-driven GO is the MVP path.

### Task 4 — controller
- `RunState { current_cue_idx: Option<usize>, active_layer: Layer }` in
  AppState. GO: target layer = other(active); LOAD_CUE(rel path, kind)
  → PLAY_AT(start = now+start_lead_ms, crossfade_ms = outgoing cue's
  crossfade_to_next_ms if something is playing, else fade_in_ms);
  advance selection; active_layer flips. NEXT/PREV move selection only.
  BLACKOUT = Fade(default_fade_ms). STOP panic-cuts. Spacebar = GO
  (egui `ctx.input` when no text field focused).
- SHOW_SYNC broadcast on show load and sent 1:1 on client join (needs
  per-client sender — already have `outbound` in ClientRow).
- Preflight: hash all show files locally (spawn_blocking, cache by
  mtime+size), MEDIA_CHECK to all; collect MEDIA_REPORT into
  `preflight: HashMap<client_id, HashMap<rel_path, MediaFileStatus>>`;
  UI table; "Push missing to client X" iterates missing/mismatch files,
  streams chunks through the outbound queue (needs the outbound channel
  to carry binary frames too — change ClientRow.outbound to an enum
  `Outgoing::Msg(ControllerMsg) | Outgoing::Chunk(Vec<u8>)`).
- Roster: mark stale if now - last_heartbeat > 3s (red dot).

### Task 5 — editor
- `rfd` workspace dep (file dialogs). Editor panel toggle. Cue table
  with per-row edit fields + ↑/↓/dup/del; media file picker constrained
  to files under media_root (store rel path); show settings form;
  Save / Save As via `ShowFile::save`. Validation errors shown inline.
  Round-trip unit test: new_empty → add cues → save → load → equal.

### Task 6 — discovery
- Controller: `mdns-sd` register `_cuemesh._tcp.local.` instance
  "CueMesh2 Controller @ <host>", port 9420, on startup.
- Client: browse continuously; discovered list in UI; clicking sets
  controller URL and reconnects (needs the connection loop to watch a
  `controller_url` slot in shared state instead of a fixed cfg field).

### Task 7 — tests/docs
- Make controller & client `lib.rs + main.rs` so integration tests can
  drive `server::run` + a fake WS client (tokio-tungstenite client).
  Test: HELLO → HELLO_ACK + SHOW_SYNC; MEDIA_CHECK → MEDIA_REPORT.
- Pure-logic tests: preflight comparison, push reassembly+hash.
- Update `examples/example_show.cuemesh.toml` to existing H.264 files
  (countdown.mp4, test1.mp4, pic1.jpg) without volume keys.
- `docs/OPERATION.md`: operator quickstart (start controller, start
  clients, preflight, push media, run show, panic buttons).

## Known gaps / future work (beyond this session)

- Fullscreen on the client: `glimagesink` opens a plain window; real
  deployments want borderless fullscreen (GstVideoOverlay embedding or
  appsink→egui texture). Documented, not built.
- Codec preflight warning (flag AV1/HEVC on clients without hw decode).
- Client-driven auto-preload/auto-crossfade at cue end (brief's
  "auto-preload" behaviour) — controller-driven GO covers the MVP.
- `start_ms`/`end_ms` cue trimming is in the protocol but not implemented
  in the engine (needs a segment seek after preroll).
- Support bundle ZIP; JSONL file logging via tracing-appender.
