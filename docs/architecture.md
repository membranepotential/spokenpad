# Architecture

← [docs index](README.md) | See also [constraints](constraints.md),
[progressive commit](progressive-commit.md), and [Rust implementation](rust.md).

spokenpad is entirely Rust: one statically linked binary runs the daemon,
`spokenpad editor`, `transcribe` and `check`. The WER harness is the
`examples/eval.rs` example ([evaluation.md](evaluation.md)).

## Production components

`src/` is split in two: `core/` is the functional core, `shell/` is the
imperative shell, and `config.rs` sits at the root because both sides read it.

| Module | Responsibility | External boundary |
|---|---|---|
| `config.rs` | Parse and validate the TOML once, before any thread or model | filesystem |
| `core/control.rs` | The control protocol: `Request`, `Reply`, their one-line codec, the stamped `Received` | none |
| `core/state.rs` | Total session-state transition function over requests and the clock; the minimum hold, the repeat window, and the limits that end a capture nobody ends | none |
| `core/frames.rs` | `Frames`: capture-absolute sample offsets, distinct from slice indices | none |
| `core/geometry.rs` | Which output the pointer is on, and the clamped window rect | none |
| `core/font.rs` | The pane's font size in points, `Xft.dpi`, and the cell a face makes: Alacritty's and FreeType's arithmetic, rounding included | none |
| `core/text.rs` | Filler stripping, exact replacements, whitespace repair | none |
| `core/wm.rs` | i3/sway IPC framing; parsing outputs, tree, config and `no_focus` rules; the pane's runtime `no_focus` command for sway | none |
| `core/terminal.rs` | The known terminals: how each names its window, where it opens, its argv | none |
| `core/session.rs` | Utterance lifecycle, preview cadence, and the one user-visible notice | internal channels |
| `core/decode.rs` | Committed sample offset, settled commits, release tails, preview isolation | worker messages |
| `core/segments.rs` | VAD merge/pad/settlement: spans in, decode windows out | none |
| `shell/inference.rs` | CPU-only models; the sherpa recognizer and the Silero detector | ONNX Runtime |
| `shell/control.rs` | Bind the control socket, or take over the one systemd passed (socket activation); stamp and forward each request; the client the CLI uses | Unix socket |
| `shell/audio.rs` | Pre-roll, immutable capture chunks, dropping committed audio, the memory ceiling, stream repair (backed off while the microphone stays missing) | PortAudio (behind `InputBackend`) |
| `shell/recorder.rs` | Persist every capture independently of decode, prune the directory, and keep the list of recordings not transcribed yet for the next start (`waiting.tsv`) | filesystem |
| `core/grid.rs` | Neovim's `ext_linegrid` redraw events, typed, and the screen they fold into; which rows each one changed | none |
| `core/keys.rs` | A keysym, its modifiers and the text a layout produced, as the notation `nvim_input` reads | none |
| `shell/nvim/mod.rs` | Editor lifecycle in all three modes (attach, managed spawn, pane), ownership proof, transactional appends, indicator, `spokenpad editor` | Unix socket, window manager |
| `shell/pane/mod.rs` | The pane: its loop, the renderer, and what `spokenpad check` looks for | X11 |
| `shell/pane/host.rs` | The pane's thread: the two things the daemon tells it, whether a pane is open, and restarting after a panic | internal channels |
| `shell/pane/x11.rs` | The window, the properties that keep a window manager from focusing it, `PutImage`, and `Xft.dpi` from x11rb's resource database, as winit reads it | X11, `~/.Xresources` |
| `shell/pane/ui.rs` | `nvim --embed` over stdio, `nvim_ui_attach`, and the thread that decodes its redraw stream | a child process |
| `shell/pane/font.rs` | `fc-match` for the face, swash for hinted glyphs, per-grapheme caching, and a character fallback kept off the drawing path: loaded faces first, one answer per Unicode page, a budget per frame and a timeout per process | fontconfig, filesystem |
| `shell/pane/keyboard.rs` | The layout the X server has loaded, dead keys and Compose | X11, libxkbcommon |
| `shell/pane/place.rs` | The monitors from RandR and the pointer from X, fed to `core/geometry.rs` | X11 |
| `shell/pane/xkb.rs` | libxcb and libxkbcommon, opened with `dlopen` when a pane opens | shared libraries |
| `shell/nvim/passage.rs` | With no editor open: append to the pending dictation file, and the pointer the next editor opens | filesystem |
| `shell/nvim/rpc.rs` | msgpack-RPC transport with absolute deadlines; pure codec | Unix socket |
| `shell/wm.rs` | i3/sway IPC requests under a deadline, once per spawn, and the pane's `no_focus` rule sent to sway before each pane; `xdotool` for the pointer on i3 | IPC socket, one subprocess |
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
`libc`, PortAudio, sherpa, `std::fs`, `std::process`, `std::net` or
`std::thread`; code that needs one of those belongs in `shell/`.

- `core/state.rs` — `step(State, Event, silence) -> (State, Command)`, total:
  which request starts, continues, latches or ends a capture; when the clock
  closes a release's repeat window; and when the clock ends a capture nobody
  is ending. Every ending carries a `Cause`, and the three that are not
  `KeyPress` are what the user reads in the winbar.
- `core/control.rs` — the control protocol as values.
- `core/session.rs` — what a capture means: which utterance is current, whether
  a preview is due, which notice is showing.
- `core/decode.rs` — offsets, settlement, and what a release still owes.
- `core/segments.rs` — VAD spans merged into padded, settled decode windows.
- `core/frames.rs`, `core/geometry.rs`, `core/text.rs` — values and arithmetic.
- `core/font.rs` — points and `Xft.dpi` to pixels, and a face's tables to a
  cell, the way Alacritty and FreeType compute them.
- `core/wm.rs` — the i3 IPC protocol, which sway shares, as values: frames,
  replies, `no_focus` proof, `include` resolution, placement commands.
- `core/terminal.rs` — the terminal table: window names, focus criteria per
  window manager, argv.
- one pure half still lives inside a shell module: in `shell/nvim`, the RPC
  codec, the spawn argv, ownership parsing, and `passage::append_paragraph`.

The shell owns everything that can fail for reasons outside the program:
`shell::daemon::run` and `shell::daemon::serve`, the audio backends,
`shell::recorder`, the control socket, the nvim session, `shell::wm`, and
`shell::logging`.

`shell::daemon::run` is the imperative shell proper — it takes the per-user
lock, listens on the control socket (its own, or the one systemd passed in),
registers signal handlers and opens PortAudio, in that order, so that the
press that socket-activated the daemon is read and stamped at once. The
models come last and off the main thread: `run` hands `serve` a loader, which
the inference thread runs — downloading the default models if they are
missing, then building the recognizer and the segmenter — while the loop
already takes presses. `shell::daemon::serve` is the event loop over whatever
devices it is handed: it is generic over the audio backend, the recognizer and
the segmenter, takes a `Receiver<Received>` of control requests, a pipeline
that is either built already or a loader, and a stop flag, and touches no
process-global state.

Until the pipeline is built, a capture is recorded to its recovery WAV only:
the loop drops its in-memory audio as it arrives, and at the release queues
the WAV. Once the inference thread reports `Ready`, each queued WAV goes to it
as `Work::Recording` and is decoded a `preview.max_seconds` window at a time
through the same `Worker::tick` and `Worker::finish` a live capture uses. The
speech model's state (`core::session::Recognition`) becomes a winbar notice
that shows when it outranks the capture's own. A failed load is reported and
retried at the next press; the daemon does not exit over it.

`serve` is also handed a `Reload`, which reads the config file again. The
editor thread calls it before each window it opens or editor it attaches,
and gives that window the file's `[nvim]`; every other section is fixed at
start, and a change to one is logged as needing a restart. A file that does
not load keeps the settings in use and raises a notice. That is the seam `tests/e2e.rs` drives
headlessly, directly and through a real control socket.

## Event and data flow

```text
       control socket: spokenpad start | stop | toggle | cancel
                                      │
                                      ▼
                  clock ──►  state::step ──► Command
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
                         ┌────────────┴────────────┐
                         ▼                         ▼
          dictation Neovim RPC buffer     pending dictation file
                                          (no editor open)
```

Everything the event loop learns about the microphone arrives as a typed
`CaptureEvent` from `AudioCapture::poll`, drained on a fixed interval and again
at every release; there is no separate watchdog, health or notice query. Each
event is delivered exactly once.

The session owns the single user-visible notice — held too briefly, microphone
gap, microphone unavailable, capture incomplete, nearly silent, preview paused,
stopped after silence, reached the time limit, memory cap. It is shown in the winbar in every phase, beside the phase label
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

What a running capture holds in memory is the open tail, not the capture: the
loop drops every device buffer the worker has committed past, so a latched
capture costs the same at an hour as at ten seconds
([progressive-commit.md](progressive-commit.md#what-is-kept-in-memory)). The
recovery WAV is written from the callback before the drop and still holds all
of it.

Which is why the table ends a capture nobody is ending. Three rules, all of
them producing the same `Command::Decode` a key release produces, so the tail
is decoded and everything spoken is kept:

| rule | when | setting |
|---|---|---|
| `Cause::Silence` | a latch with no key down has heard no speech for the timeout | `capture.silence_timeout_s`, 300 s |
| `Cause::Length` | any capture has run for `MAX_CAPTURE` | none: it keeps the WAV readable |
| `Cause::Memory` | `AudioCapture` reports the in-memory ceiling | none: it bounds this machine's RAM |

Only the shell can see the ceiling, so it arrives as `Event::Exhausted` rather
than as a rule over the clock; the other two are the clock alone. What tells
the table a capture is not forgotten is `Event::Speech`, which `Session` raises
whenever the recognizer produced text — a settled commit or a live preview.
With no VAD model, or with the progressive tick off, nothing produces text
before the release, so the silence rule is off and the other two bound the
capture. "No key down" is `last_press` older than `KEY_SETTLED`: auto-repeat
fires the *toggle* binding for a held Shift+key, so a latch being re-pressed
is a held key and the length limit is what bounds it.

## Decode invariants

- A committed sample range is decoded once, and is dropped from memory
  afterwards without any window ever reading it again: a window's lead padding
  stops at the previous chunk's speech end.
- Long silence can close a pending VAD chunk before the speech-size target,
  whether a later span follows it or it is simply the end of the slice;
  ordinary pauses still merge for recognizer context.
- Silence the VAD heard nothing in advances the committed offset without a
  decode, one split threshold behind the end of the audio.
- Release decodes only the range after the committed offset.
- A capture made before the pipeline was built is decoded from its recovery
  WAV once it is, through the same tick-then-finish path, so the same rules
  hold for it; only a window and the open tail of it are ever in memory.
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

Four owners: the main loop (session state), the control socket thread, the
inference worker, and the editor thread; `shell::recorder` owns disk I/O on a thread of
its own.

In `nvim.mode = "pane"` there is a fifth, and only then: the pane thread owns
the window, the embedded editor and everything drawn. It exists because a pane
has to be *driven* — Neovim reports its display whenever it has something to
say, and nothing is drawn until the pane applies it — and the editor thread is
busy blocking on transcripts. Two more threads sit under it, one waiting for X
events and one decoding the editor's redraw stream; both feed the pane's
single channel, so its loop blocks in one place and costs nothing while
nothing happens. It blocks with no deadline: a thread that sends it a command
also sends the window a client message, which is what makes the blocked wait
return. A panic there costs one window, not the mode — the next open starts a
new thread.

One deadline covers opening a pane and the editor answering inside it, and one
owner enforces it: a window that opens after the daemon stopped waiting is
closed by the thread rather than left standing, and every failing path closes
the pane. An abandoned one would be a live editor on the dictation socket that
no session owns, which every later key-down would refuse rather than replace.

Closing a pane ends the editor inside it, so the pane writes every modified
buffer before it quits, and that teardown fits inside the grace the daemon's
shutdown gives the thread — divided into a write budget and a quit budget,
and checked against the grace at compile time. Text Neovim will not write
comes back with the failure and is kept beside its file as `.unsaved`, rather
than discarded along with the editor.

The daemon tells that thread two things, open and stop, and is never told a
window closed. It does not need to be: the editor dies with the window, its
socket goes with it, and the next key-down finds a dead socket and asks for a
new pane — which is exactly what happens when a user closes a managed
terminal. Committed text never travels over the drawing channel; it goes over
the editor's own socket, as in every other mode.

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
