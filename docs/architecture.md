# Architecture

← [docs index](README.md) | See also [constraints.md](constraints.md) for why
the shell modules are shaped the way they are, and [asr.md](asr.md) for what
`asr` actually does during the decode step.

## Functional core, imperative shell

The codebase splits cleanly into modules that touch nothing outside the
process (the core) and modules that touch a device, the model, or the X
server (the shell). The core is where correctness is proven by types and unit
tests; the shell is where things are allowed to fail and must be handled
defensively.

| Module | Kind | Touches the world? | Status |
|---|---|---|---|
| [`config.py`](../src/voice_kb/config.py) | core | no | implemented |
| [`state.py`](../src/voice_kb/state.py) | core | no | implemented |
| [`text.py`](../src/voice_kb/text.py) | core | no | implemented |
| [`geometry.py`](../src/voice_kb/geometry.py) | core | no | implemented |
| [`hotkey.py`](../src/voice_kb/hotkey.py) | shell | evdev (read-only) | implemented |
| [`audio.py`](../src/voice_kb/audio.py) | shell | PipeWire capture | implemented |
| [`recorder.py`](../src/voice_kb/recorder.py) | shell | writes each capture to a wav | implemented |
| [`asr.py`](../src/voice_kb/asr.py) | shell | sherpa-onnx / CPU inference | implemented |
| [`decode.py`](../src/voice_kb/decode.py) | shell | drives the VAD + model pipeline | implemented |
| [`nvim.py`](../src/voice_kb/nvim.py) | shell | nvim RPC socket, i3 | implemented |
| [`nvim_indicator.lua`](../src/voice_kb/nvim_indicator.lua) | — | runs *inside* nvim | implemented |
| [`x11.py`](../src/voice_kb/x11.py) | shell | xrandr / xdotool queries | implemented |
| [`app.py`](../src/voice_kb/app.py) | shell | wires everything together | implemented |

The core modules are pure functions over immutable data: given the same
input they always produce the same output, and they never block, spawn a
thread, or reach for a file handle. `config.py` parses TOML into frozen
dataclasses once, at startup, so nothing downstream has to defend against a
missing key or a negative duration ([`config.py`](../src/voice_kb/config.py)
docstring). `state.py` is a closed state machine — see below.

The shell modules do the opposite job: they are thin, mostly untested-by-unit-test
adapters between a pure decision (a `Command`) and a real side effect (open a
stream, run inference, append to a buffer over a socket, move a window).
Keeping them thin
is the point — the harder the module is to reason about (evdev quirks, X11
window hints, ONNX runtime setup), the less logic should live inside it.

## `state.step`: a total function

[`state.py`](../src/voice_kb/state.py) models the session as a closed union
of three states — `Idle`, `Recording`, `Transcribing` — and a closed union of
four events — `KeyDown`, `KeyUp`, `DecodeFinished`, `Cancelled`. The single
entry point is:

```python
def step(state: SessionState, event: Event) -> tuple[SessionState, Command]
```

`step` is **total**: every `(state, event)` pair is handled, including the
ones that mean nothing in context. Auto-repeat `KeyDown` events while already
`Recording`, a duplicate `KeyUp`, or a stale `DecodeFinished` callback after a
`Cancelled` all fall through to the catch-all case and return the state
unchanged with the `Nothing` command — no exceptions, no undefined
transitions. This is what makes the state machine safe to drive from
interrupt-like sources (evdev key events, a decode thread finishing late)
without the shell needing its own guard logic.

`step` never performs an effect itself. It returns a `Command` — one of
`Nothing`, `StartCapture`, `Decode`, `DiscardCapture`, `AbortDecode` — and the
imperative shell ([`app.py`](../src/voice_kb/app.py)) is solely responsible for
interpreting
that command into a real action. This is the functional-core/imperative-shell
boundary made concrete: the *decision* of what should happen next is pure and
unit-testable; *making it happen* is not, and lives elsewhere.

`Phase` (`IDLE` / `RECORDING` / `TRANSCRIBING`) is derived from `SessionState`
via `phase_of`, never stored — the indicator reads a phase, it does not own
one, so the two can't drift apart.

## Event flow

```
keypress (evdev)
    │
    ▼
hotkey.py            -- reads /dev/input/event*, read-only (constraints.md)
    │  KeyDown / KeyUp / Cancelled
    ▼
state.step()          -- pure: (SessionState, Event) -> (SessionState, Command)
    │  Command
    ▼
app.py                -- interprets the command
    │
    ├─ StartCapture   → audio.py starts accumulating from the pre-roll buffer,
    │                     and recorder.py opens a wav that every callback
    │                     reaches before any in-memory limit applies.
    │                     Every ~1s from here: the worker splits the audio
    │                     since the committed offset (vad.py), commits each
    │                     settled chunk, previews the open tail
    │                     (progressive-commit.md)
    ├─ Decode         → decode.py decodes the remainder past the committed
    │                     offset -- the open tail -- through the same
    │                     pipeline voice-kb transcribe uses on a wav
    ├─ DiscardCapture → nothing further is decoded; what already landed stays
    │
    ▼
text.py               -- post-process: strip fillers, apply exact replacements
    │
    ▼
nvim.py               -- appended over msgpack-RPC to the dictation buffer,
    │                    which nvim then writes to a dated file
    ▼
text lands in the floating nvim window; nothing is pasted anywhere
```

Nothing in that pipeline is the only copy of the audio any more.
`recorder.py` writes every capture to
`$XDG_STATE_HOME/voice-kb/audio/capture-<timestamp>.wav` while it is being
spoken — from a writer thread, because the realtime callback may not touch a
filesystem — and `voice-kb transcribe <wav>` replays a recording through the
*same* `decode.py` pipeline. That exists because on 2026-09-08 a 821.3s hold
hit a 600s in-memory ceiling enforced inside the callback, above every other
consumer, and 3m41s of dictation existed nowhere else. The ceiling is now
3600s, it bounds only resident memory, and hitting it is a warning rather
than a loss.

The sink is a neovim the daemon opens itself, floating and never focused
(see [nvim-window.md](nvim-window.md)). Nothing is written to the window the
user is working in, so dictating neither depends on nor disturbs whatever has
focus — which retires a whole class of failure the clipboard-and-paste sink
had (a paste that raced the clipboard restore, a target that swallowed
`ctrl+v`, a window-class-specific paste combo).

Phase, level meter and the preview of the open tail are rendered in the
nvim window's winbar and as virtual text by
[`nvim_indicator.lua`](../src/voice_kb/nvim_indicator.lua), where the text is
about to land. The tick that commits settled chunks and previews the tail is
a timer on the Qt thread asking the *same* worker, so the recogniser stays
serialised; `snapshot_capture` is non-destructive, so `stop_capture` still
returns the whole capture. No event, command or state was added for it: the
tick is a shell concern, and `state.py` is untouched. The design is in
[progressive-commit.md](progressive-commit.md).

## The input stream is not trusted

PortAudio streams on Linux die quietly. PipeWire can suspend the device, the
default source can change, the server can restart -- and when it happens the
callback simply stops. Nothing raises, and `InputStream.active` keeps
reporting `True`. The symptom is a capture that returns only the frozen
pre-roll ring, decoded to nothing, over and over.

So liveness is observed through the callback, never through the handle, at
three points:

| when | what | why it is not enough on its own |
|---|---|---|
| stream open | wait up to `FIRST_CALLBACK_TIMEOUT` for the first buffer | catches a stream born dead; blind to one that dies later |
| `start_capture` | `_ensure_stream_alive` reopens if the callback has been quiet for `STALE_STREAM_SECONDS` | repairs a device that died *while idle*, before it can swallow a dictation; blind to one that dies mid-utterance |
| every 33ms while recording | `recover_if_dead`, driven by the level-meter timer | the only one that can act while the utterance is still happening |

The third exists because of a real failure: the stream died 0.2s into a hold,
the user spoke for 29.8 seconds, and 0.4s of audio came back. Nothing appeared
in the log until the *next* keypress reported "no callback for 51.3s". The
first two checks were both working correctly and neither could see it.

`recover_if_dead` preserves the in-progress capture rather than restarting it:
the dead interval is lost either way, but the words before it are not. It is
also scoped to an in-flight capture, since an idle stream is already handled at
the next `start_capture` and reopening there would discard a warm pre-roll ring
for nothing.

Silence in the level meter is ambiguous -- a quiet room and a dead microphone
look identical -- so a recovery also writes a message into the indicator's
preview field. That is the only case where it shows text that did not come
from the recogniser.

## Why this split matters here specifically

The [constraints](constraints.md) this project runs under were all discovered
by a *shell* doing too much: a keyboard clone in the shell corrupted layout
state, character synthesis in the shell corrupted the X keymap, streaming
decode in the shell silently dropped audio. Keeping the state machine and text
processing pure means those classes of bug cannot originate in `state.py` or
`text.py` — they can only come from the shell modules, which is exactly where
this project now concentrates its defensive code and its manual testing.


## Threading

Four threads, and the boundaries matter:

| Thread | Owns | Must never |
|---|---|---|
| evdev watcher | `/dev/input/event*` reads | block; it is the hotkey's latency budget |
| Qt main | the state machine, the timers | do slow work -- see below |
| worker | the `Transcriber` | be touched from the Qt thread |
| nvim bridge | the `NvimSession` | share the worker's queue -- see below |

The bridge is a separate thread from the worker rather than a second job on
it, because the two block on unrelated things and must not queue behind each
other. Opening a terminal takes ~1s (measured; a plugin manager installing on
first run takes far longer), and it happens *while* the first utterance is
still being spoken — so the window is up by the time there is text for it. Put
that wait on the worker and it would sit in front of a decode.

Hotkey callbacks are emitted as Qt Signals from the watcher thread and Qt
queues them onto the Qt thread, so `SessionState` is only ever mutated in one
place and needs no lock.

The Qt-thread rule is not theoretical. `_sync_indicators` once shelled out to
xrandr and xdotool on *every* dispatched event; evdev auto-repeat fires around
30 times a second while a key is held, so holding the hotkey spawned roughly 60
subprocesses per second. That stalled the event loop badly enough to delay the
key-up (capture kept running ~22s past the physical release) and to batch three
decode results into the same millisecond. Auto-repeat is now dropped in
`hotkey.py`, and the indicators are only pushed on a real phase change.
Anything added to the Qt thread's hot path deserves the same scrutiny.
