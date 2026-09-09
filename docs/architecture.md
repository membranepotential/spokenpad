# Architecture

← [docs index](README.md) | See also [constraints](constraints.md),
[progressive commit](progressive-commit.md), and [Rust implementation](rust.md).

Spokenpad's production path is entirely Rust. Python is an offline reference
for ASR/VAD parity and evaluation; the daemon never imports or starts it.

## Production components

| Module | Responsibility | External boundary |
|---|---|---|
| `config.rs` | Parse and validate the shared TOML | filesystem |
| `state.rs` | Total session-state transition function | none |
| `hotkey.rs` | Observe evdev keys read-only | `/dev/input` |
| `audio.rs` | Capture pre-roll and immutable audio chunks | PortAudio |
| `recorder.rs` | Persist every capture independently of decode | filesystem |
| `inference.rs` | CPU-only models plus VAD merge/pad/settlement | ONNX Runtime |
| `decode.rs` | Own progressive commit offsets and release tails | worker messages |
| `session.rs` | Utterance identity, cancellation, preview cadence | internal channels |
| `nvim.rs` | Owned editor lifecycle and transactional RPC appends | Unix socket, i3 |
| `daemon.rs` | Event loop and component orchestration | all of the above |

The Neovim presentation code is embedded from `src/lua/nvim_indicator.lua`
and `src/lua/dictation_init.lua`. Preview text is extmark virtual text, never
buffer content.

## Event and data flow

```text
read-only evdev event
        │
        ▼
state transition ──► capture command ──► PortAudio chunks
                                           │
                        ┌──────────────────┴──────────────────┐
                        ▼                                     ▼
                 recording writer                     decode worker
                                                              │
                                             settled commits + open preview
                                                              │
                                                              ▼
                                                owned Neovim RPC buffer
```

The state transition is pure. Blocking model and editor work runs off the
event loop. Audio recording receives chunks before the in-memory capture cap,
so recovery does not depend on decoding succeeding.

## Decode invariants

- A committed sample range is decoded once.
- Long silence can close a pending VAD chunk before the speech-size target;
  ordinary pauses still merge for recognizer context.
- Release decodes only the range after the committed offset.
- Preview may re-decode only the bounded open tail and cannot reach the file.
- If every segmented decode is empty, one whole-buffer retry is allowed as a
  recovery exception.

## Concurrency and ownership

The main loop owns session state. Capture callbacks do bounded work and hand
immutable chunks to recording/decode consumers. One inference worker owns the
recognizer and VAD instances, keeping their non-thread-safe state serialized.

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
