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
| [`asr.py`](../src/voice_kb/asr.py) | shell | sherpa-onnx / CPU inference | implemented |
| [`inject.py`](../src/voice_kb/inject.py) | shell | X11 clipboard + paste | implemented |
| [`overlay.py`](../src/voice_kb/overlay.py) | shell | X11 window (PySide6) | implemented |
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
stream, run inference, write the clipboard, move a window). Keeping them thin
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
via `phase_of`, never stored — the overlay reads a phase, it does not own
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
    ├─ StartCapture   → audio.py starts accumulating from the pre-roll buffer
    ├─ Decode         → asr.py runs the one committed decode over the whole
    │                     captured buffer (hotword-biased via bpe.vocab, asr.md)
    ├─ DiscardCapture → audio thrown away, nothing decoded
    │
    ▼
text.py               -- post-process: strip fillers, apply exact replacements
    │
    ▼
inject.py             -- clipboard + paste keystroke, window-class aware
    │
    ▼
text lands at the cursor
```

`overlay.py` sits to the side of this pipeline, reading `Phase` off the
current `SessionState` to show idle/recording/transcribing, without being on
the critical path from keypress to injected text.

The live transcript preview is the one place the overlay reads audio rather
than just phase: while recording, a timer on the Qt thread snapshots a
fixed-length trailing window (non-destructively — `snapshot_capture` never
consumes the buffer) and asks the *same* worker for a throwaway decode. It is
still off the critical path by construction: previews are abandoned before the
committed decode is requested, and nothing a preview produces can reach the
clipboard. See
[constraints.md](constraints.md#the-one-relaxation-cosmetic-previews). No
event, command or state was added for it — a preview is a shell concern, so
`state.py` is untouched.

## Two coordinate systems, and the rule for keeping them apart

`xrandr` and `xdotool` report the desktop in **device pixels**. Qt's
`move()` and `resize()` take **logical pixels**, which differ whenever the
device pixel ratio is not 1 -- `Xft.dpi: 192` gives a ratio of 2.0.

There is no single factor to convert between them, because Qt reports screen
*origins* in device pixels but screen *sizes* in logical ones. On this machine
a 3840x2160 panel at x=3840 comes back from Qt as `(3840, 0, 1920, 1080)`.

The rule is therefore not "divide by the ratio" but:

> Choosing **which** output is a question about the physical desktop — answer
> it with xrandr. Placing something **on** that output is a question for Qt —
> answer it with `QScreen`'s own rect, via `overlay.screen_rect`.

Mixing them is silently wrong rather than loudly wrong. Placement computed
from xrandr and passed to `move()` put the overlay at y=3936 on a 2160px-tall
screen: mapped, `IsViewable`, painted correctly, and 1776px below the bottom
edge. The overlay was invisible for the entire life of the project and every
test passed the whole time, because offscreen render tests never involve a
screen and unit tests never involve Qt. It took `xwininfo` on the live window
to see it.

`geometry.py` stays pure and unit-testable throughout; it does the same
centre-and-clamp arithmetic either way. Only the rect handed to it changes.

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
look identical -- so a recovery also writes a message into the overlay's
preview band. That is the only case where the overlay shows text that did not
come from the recogniser.

## Why this split matters here specifically

The [constraints](constraints.md) this project runs under were all discovered
by a *shell* doing too much: a keyboard clone in the shell corrupted layout
state, character synthesis in the shell corrupted the X keymap, streaming
decode in the shell silently dropped audio. Keeping the state machine and text
processing pure means those classes of bug cannot originate in `state.py` or
`text.py` — they can only come from the shell modules, which is exactly where
this project now concentrates its defensive code and its manual testing.


## Threading

Three threads, and the boundaries matter:

| Thread | Owns | Must never |
|---|---|---|
| evdev watcher | `/dev/input/event*` reads | block; it is the hotkey's latency budget |
| Qt main | the state machine, the overlay | do slow work -- see below |
| worker | the `Transcriber`, and injection | be touched from the Qt thread |

Hotkey callbacks are emitted as Qt Signals from the watcher thread and Qt
queues them onto the Qt thread, so `SessionState` is only ever mutated in one
place and needs no lock.

The Qt-thread rule is not theoretical. `_sync_overlay` once shelled out to
xrandr and xdotool on *every* dispatched event; evdev auto-repeat fires around
30 times a second while a key is held, so holding the hotkey spawned roughly 60
subprocesses per second. That stalled the event loop badly enough to delay the
key-up (capture kept running ~22s past the physical release) and to batch three
decode results into the same millisecond. Auto-repeat is now dropped in
`hotkey.py`, the overlay repositions only on a real phase change, and the
monitor layout is cached. Anything added to the Qt thread's hot path deserves
the same scrutiny.
