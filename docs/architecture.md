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
| `text.py` | core | no | not yet implemented |
| `geometry.py` | core | no | not yet implemented |
| `hotkey.py` | shell | evdev | not yet implemented |
| `audio.py` | shell | PipeWire capture | not yet implemented |
| `asr.py` | shell | sherpa-onnx / CPU inference | not yet implemented |
| `inject.py` | shell | X11 clipboard + paste | not yet implemented |
| `overlay.py` | shell | X11 window (PySide6) | not yet implemented |
| `app.py` | shell | wires everything together | not yet implemented |

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
`Nothing`, `StartCapture`, `Decode`, `DiscardCapture` — and the imperative
shell (`app.py`, not yet implemented) is solely responsible for interpreting
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
    ├─ Decode         → asr.py runs one-shot decode over the captured audio
    │                     (hotword-biased via bpe.vocab, see asr.md)
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

## Why this split matters here specifically

The [constraints](constraints.md) this project runs under were all discovered
by a *shell* doing too much: a keyboard clone in the shell corrupted layout
state, character synthesis in the shell corrupted the X keymap, streaming
decode in the shell silently dropped audio. Keeping the state machine and text
processing pure means those classes of bug cannot originate in `state.py` or
`text.py` — they can only come from the shell modules, which is exactly where
this project now concentrates its defensive code and its manual testing.
