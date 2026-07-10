# MultiPlex — Operator Quickstart

This is the hands-on guide for running a show. For the design and internals,
see `CLAUDE.md`; for what has been built and why, see `docs/PROGRESS.md`.

MultiPlex has two programs:

- **Controller** — the operator's machine. Runs the show, holds the cue list,
  and is the hub every client connects to.
- **Client** — a playback machine wired to a projector or screen. Plays media
  in sync on command. You can run as many as you like; a client can also run
  on the controller machine itself (single-box mode).

Everything is LAN-only and offline. No internet, no accounts, no cloud.

---

## 1. Stage the media

Each client plays files from its own **media root** (default
`~/multiplex_media`, override with the `MULTIPLEX_MEDIA_ROOT` environment
variable). Cues in the show file reference files **relative to that root**, so
the same show works on every machine regardless of where its media lives.

Recommended format: **H.264 video in an MP4 container** (and JPG/PNG for
stills). H.264 gets hardware decoding automatically on almost every machine.
Avoid AV1 — most machines have no AV1 hardware decoder and the software
decoder GStreamer ships is too slow for smooth playback, even though VLC
(which bundles its own fast decoder) plays the same file fine.

You can copy media to each client by hand, or let the controller push it — see
step 4.

---

## 2. Start everything

On the controller machine:

```sh
multiplex-controller
```

Optionally point it at a show on startup:

```sh
MULTIPLEX_SHOW=~/shows/mynight.multiplex.toml multiplex-controller
```

On each client machine:

```sh
multiplex-client
```

Useful client environment variables:

- `MULTIPLEX_CONTROLLER` — controller URL, e.g. `ws://192.168.1.10:9420`
  (default `ws://127.0.0.1:9420` for single-box mode).
- `MULTIPLEX_NAME` — the name shown in the controller's client roster.
- `MULTIPLEX_MEDIA_ROOT` — where this client's media lives.

Clients find the controller automatically over the LAN (mDNS): open the
client window and pick the controller from the discovered list, or type its
`ws://…:9420` URL manually. The controller listens on **port 9420** — make
sure the LAN/firewall allows it. Any client that connects is accepted
immediately; there is no approval step.

---

## 3. Load or build a show

- **Open an existing show:** *Open show…* in the controller's top bar.
- **Create or edit one in-app:** *New show* or *Edit show*. The editor lets
  you set the show settings and add/reorder/duplicate/delete cues, choosing
  each cue's media file from the files found under the media root. *Save to
  file* writes the `.multiplex.toml`; *Apply to running show* pushes it live to
  every connected client without saving.

A cue's fade and crossfade timings live on the cue itself:

- `fade_in_ms` / `fade_out_ms` — fade this cue up from / down to black.
- `crossfade_to_next_ms` — when you GO to the next cue, crossfade into it over
  this many milliseconds instead of cutting.

---

## 4. Preflight and push media

Before the show, click **Preflight**. The controller checks every file the
show needs (name + size) and asks each client to do the same, then the
roster shows, per client, how many files are present and flags anything
**missing** or **mismatched** (present but the wrong size).

For any client with missing/mismatched files, click **Push missing media**.
The controller streams those files over the same connection; the client
verifies each one (SHA-256) before putting it in place, and the roster shows a
progress bar. Re-run Preflight afterwards to confirm all-green.

---

## 5. Run the show

- **GO** (or the **spacebar**) — fire the selected cue. It loads on the idle
  layer and starts in sync across all clients, crossfading from whatever is on
  air if the outgoing cue asked for it.
- **↑ / ↓** (or PREV/NEXT) — move the selection without firing.
- The selected cue is highlighted; the on-air cue is marked **▶**; cues that
  crossfade into the next are marked **⤳**.

Clients keep their clocks aligned with the controller continuously and nudge
playback rate slightly (or hard-seek if far off) to stay within the show's
drift target. The roster shows each client's measured clock offset and drift.

---

## 6. Panic buttons

- **PAUSE / RESUME** — freeze playback in place on all clients, then resume.
- **BLACKOUT (fade)** — fade everything to black over the show's default fade
  time, then stop.
- **STOP (cut)** — cut everything to black instantly.
- **Testscreen** — show a test pattern on every client (for focusing and
  aligning projectors). Toggle it off to return to black.

---

## 7. If a client drops off the network

Clients are built to survive network loss mid-cue. What happens is set by the
show's **dropout policy**:

- `continue` — keep playing the current cue to its natural end (default).
- `freeze` — freeze on the current frame.
- `black` — fade to black and stop.

Either way the client keeps running, reconnects on its own in the background,
and re-syncs its clock when the controller comes back. You do not need to
restart anything.
