# Architecture

← [docs index](README.md) | See also [constraints](constraints.md),
[progressive commit](progressive-commit.md), and [Rust implementation](rust.md).

Spokenpad's production path is entirely Rust. Python is an offline reference
for ASR/VAD parity and evaluation; the daemon never imports or starts it.

## Production components

`src/` is split in two: `core/` is the functional core, `shell/` is the
imperative shell, and `config.rs` sits at the root because both sides read it.

| Module | Responsibility | External boundary |
|---|---|---|
| `config.rs` | Parse and validate the TOML once, before any thread or model | filesystem |
| `core/state.rs` | Total session-state transition function, and the minimum hold | none |
| `core/frames.rs` | `Frames`: capture-absolute sample offsets, distinct from slice indices | none |
| `core/geometry.rs` | Which output the pointer is on, and the clamped window rect | none |
| `core/text.rs` | Filler stripping, exact replacements, whitespace repair | none |
| `core/wm.rs` | i3 IPC framing; parsing outputs, tree, config and `no_focus` rules | none |
| `core/session.rs` | Utterance lifecycle, preview cadence, and the one user-visible notice | internal channels |
| `core/decode.rs` | Committed sample offset, settled commits, release tails, preview isolation | worker messages |
| `core/segments.rs` | VAD merge/pad/settlement: spans in, decode windows out | none |
| `core/hotkey.rs` | `WatcherState`: which keyboards hold the hotkey and which latch modifiers they report | none |
| `shell/inference.rs` | CPU-only models; the sherpa recognizer and the Silero detector | ONNX Runtime |
| `shell/hotkey.rs` | Observe evdev keys read-only: scan, open, poll, read | `/dev/input` |
| `shell/audio.rs` | Pre-roll, immutable capture chunks, the memory ceiling, stream repair | PortAudio (behind `InputBackend`) |
| `shell/recorder.rs` | Persist every capture independently of decode, and prune the directory | filesystem |
| `shell/nvim/mod.rs` | Owned editor lifecycle, ownership proof, transactional appends, indicator | Unix socket, window manager |
| `shell/nvim/rpc.rs` | msgpack-RPC transport with absolute deadlines; pure codec | Unix socket |
| `shell/wm.rs` | i3/sway IPC requests under a deadline, once per spawn; `xdotool` for the pointer on i3 | IPC socket, one subprocess |
| `shell/logging.rs` | Private 0600 diagnostic log, rotated at 1 MB | filesystem |
| `shell/daemon.rs` | `run` (the shell) and `serve` (the event loop) | all of the above |

The Neovim presentation code is embedded from `src/lua/spokenpad.lua` (one
file: the buffer, the winbar indicator, the level meter, the preview extmark,
and the transactional append) and `src/lua/dictation_init.lua` (the optional
bundled editor configuration). Preview text is extmark virtual text, never
buffer content.

## Functional core, imperative shell

The split is the directory layout: everything under `core/` is pure and
unit-tested without a device, a thread, or a process. Nothing there may import
`evdev`, `libc`, PortAudio, sherpa, `std::fs`, `std::process`, `std::net` or
`std::thread`; code that needs one of those belongs in `shell/`.

- `core/state.rs` — `step(State, Event) -> (State, Command)`, total.
- `core/session.rs` — what a capture means: which utterance is current, whether
  a preview is due, which notice is showing.
- `core/decode.rs` — offsets, settlement, and what a release still owes.
- `core/segments.rs` — VAD spans merged into padded, settled decode windows.
- `core/hotkey.rs` — which keyboards hold the hotkey and which latch modifiers
  they report, and when a rescan must reprobe a device node.
- `core/frames.rs`, `core/geometry.rs`, `core/text.rs` — values and arithmetic.
- `core/wm.rs` — the i3 IPC protocol as values: frames, replies, `no_focus`
  proof, `include` resolution, placement commands.
- one pure half still lives inside a shell module: in `shell/nvim`, the RPC
  codec, the spawn argv, and ownership parsing.

The shell owns everything that can fail for reasons outside the program:
`shell::daemon::run` and `shell::daemon::serve`, the audio backends,
`shell::recorder`, the hotkey run loop, the nvim session, `shell::wm`, and
`shell::logging`.

`shell::daemon::run` is the imperative shell proper — it takes the per-user
lock, registers signal handlers, opens PortAudio and evdev, and loads the
models. `shell::daemon::serve` is the event loop over whatever devices it is
handed: it is generic over the audio backend, the recognizer and the segmenter,
takes a `Receiver<state::Event>` and a stop flag, and touches no
process-global state. That is the seam `tests/e2e.rs` drives headlessly.

## Event and data flow

```text
read-only evdev ──► state::step ──► Command
                                      │
        ┌─────────────────────────────┼──────────────────────────┐
        ▼                             ▼                          ▼
  Start: capture                Decode: release            Discard: reason
        │                             │                          │
        ▼                             ▼                          ▼
  PortAudio chunks            Work::Finish ──► worker      Session notice
        │      │                                  │
        │      └──► recovery WAV                  │
        ▼                                         ▼
  AudioCapture::poll ──► CaptureEvent ──►  Commit / Preview
        (gap, unavailable, cap, flags)            │
                        │                         │
                        ▼                         ▼
                    Session (state, committed hint, notice)
                                      │
                                      ▼
                       EditorWork::{Ensure, Append, Indicator}
                                      │
                                      ▼
                          owned Neovim RPC buffer
```

Everything the event loop learns about the microphone arrives as a typed
`CaptureEvent` from `AudioCapture::poll`, drained on a fixed interval and again
at every release; there is no separate watchdog, health or notice query. Each
event is delivered exactly once.

The session owns the single user-visible notice — held too briefly, microphone
gap, microphone unavailable, capture incomplete, nearly silent, preview paused,
memory cap. It is shown in the winbar in every phase, beside the phase label
and never in place of the preview, and is cleared by the next key press, not by
a timer and not by the daemon re-warning. When a capture collects two, the
ranking in `Notice::priority` decides which one stands, and `Session::notify`
is the only place that applies it; each notice carries a short headline the
winbar always draws and a detail it appends when the window is wide enough
([nvim-window.md](nvim-window.md)).

The indicator travels on the editor thread's own channel as
`EditorWork::Indicator(IndicatorState)`, coalesced so only the newest pending
update is sent; there is no shared indicator state. Appends queued ahead of it
are written first, which is why cancelling or shutting down cannot lose text
that has already been produced: the editor thread runs until its channel says to
stop, and shutdown gives it a bounded three seconds to drain, logging anything
undelivered at error level.

The state transition is pure. Blocking model and editor work runs off the event
loop. Audio recording receives chunks before the in-memory capture cap, so
recovery does not depend on decoding succeeding.

## Decode invariants

- A committed sample range is decoded once.
- Long silence can close a pending VAD chunk before the speech-size target;
  ordinary pauses still merge for recognizer context.
- Release decodes only the range after the committed offset.
- Preview may re-decode only the bounded open tail and cannot reach the file.
  With no segmenter loaded, no preview tick is issued at all.
- A capture the VAD finds no speech in is not decoded at all: no chunks, no
  recognizer call, no text. With no segmenter loaded the whole capture is
  decoded as before.
- If every segmented decode is empty and there was more than one chunk, one
  whole-buffer retry is allowed as a recovery exception.
- An `Utterance` moves forwards only: `Live → Released` or `Live → Cancelled`.
  Work queued for an older capture can neither advance nor reset a newer one.

## Concurrency and ownership

Four owners: the main loop (session state), the input watcher, the inference
worker, and the editor thread; `shell::recorder` owns disk I/O on a thread of
its own.

Capture callbacks do bounded work — one allocation, one lock, no I/O — and hand
immutable chunks to the recording and decode consumers. One inference worker
owns the recognizer and VAD instances, keeping their non-thread-safe state
serialized, and owns the committed offset because it is the one thread that
serialises decodes; the main loop keeps only a lagging hint, used to avoid
copying a long capture on every tick.

Neovim appends use request/reply RPC and are transaction-like: mutate, write
with autocommands suppressed, then acknowledge; a failed write rolls the
buffer back. Reconnect retries carry append IDs so an ambiguous timeout cannot
append twice. A new editor is accepted only after its ownership nonce and the
nonce set by the final one-shot `VimEnter` handler both match.

## Offline Python reference

The package under `src/spokenpad/` retains only:

- `asr.py`, `vad.py`, and `decode.py` for native differential checks;
- `config.py` and `text.py` for evaluation configuration/post-processing;
- `audio.py`, a NumPy type definition with no capture backend.

`scripts/fetch_model.py`, `build_hotwords.py`, `eval.py`,
`verify_references.py`, and `verify_rust.py` use that package. `install.py` is
a standalone deployment helper. No Python module handles the microphone,
keyboard, editor, window manager, service loop, or GUI.
