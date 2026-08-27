# Constraints

← [docs index](README.md) | Enforced by the modules described in
[architecture.md](architecture.md); the hardware specifics referenced below
are in [hardware.md](hardware.md); the rejected alternatives that led here
are in [decisions.md](decisions.md).

Every rule below traces to an observed failure in
[Handy](https://github.com/cjpais/Handy) 0.9.6, evaluated first and rejected
(see [decisions.md](decisions.md#rejected-handy-as-a-baseline)). These are
not defaults chosen for taste — they are constraints earned by breaking a
running system.

| Rule | Observed failure it prevents | Damaged the system? |
|---|---|---|
| Read `/dev/input/event*` **read-only** | uinput clone inherited the default XKB layout | yes |
| **Never** synthesise characters (`xdotool type` / enigo) | rewrote the core X keymap | yes |
| **One-shot decode** at key release, never streaming | growing-buffer re-decode dropped long utterances | no (functional, silent) |
| **CPU only** | auto-selection bound the model to the 4 GB GTX 1650 | no |
| **Bias vocabulary at decode time**, never fuzzy replacement | edit-distance rewrite turned real words into wrong ones | no |
| **Overlay must not steal focus** | focus steal aborted transcription mid-utterance | no |

## Read evdev read-only

Handy grabbed the M4 key by cloning the keyboard through a uinput virtual
device (the standard way to intercept a key system-wide on Linux: grab the
real device with `EVIOCGRAB`, then re-emit a filtered event stream through a
new virtual one). The clone device does not inherit the per-device
`setxkbmap` layout applied to the physical keyboard by
`~/.local/bin/keychron-add.sh` (see [hardware.md](hardware.md)) — it gets
whatever X11's default layout is. The result: keyboard layout for that
input path silently reverted, corrupting keystrokes system-wide until the
physical device was replugged.

**Rule:** voice-kb reads `/dev/input/event*` for the hotkey only, never calls
`EVIOCGRAB`, and never creates a uinput clone. It observes key state; it does
not intercept or re-emit it.

## Never synthesise characters

Handy typed transcribed text with synthetic keystrokes (`xdotool type` /
the equivalent enigo call). Both work by rewriting the *core* X keymap on the
fly to find keycodes for arbitrary Unicode characters, one character at a
time. This is not layout-neutral: it mutates global X server state, is why
this failure mode is not merely rare-but-possible.

It is also slow. Measured on this project's hardware: **183 ms** via
clipboard + `ctrl+v` versus **3.3 s** via character synthesis for the same
159-character string (README.md).

**Rule:** voice-kb never synthesises characters. It writes the transcript to
the clipboard and injects a paste keystroke (`inject.py`, not yet
implemented), restoring the previous clipboard contents afterward
(`PasteConfig.restore_delay_ms` in
[`config.py`](../src/voice_kb/config.py)). The paste combo is window-class
aware — terminals like Alacritty need `ctrl+shift+v`
(`PasteConfig.per_window_class`).

## One-shot decode, never streaming

Handy streamed audio into the model continuously, re-decoding a growing
buffer as more audio arrived. Measured real-time factor on this project's
hardware: **1.37x real-time** — barely faster than the utterance itself —
and it silently dropped audio past a 30 s cap, with no error surfaced to the
user (STATUS.md).

**Rule:** voice-kb captures audio to a buffer while the key is held and
decodes it exactly once, after `KeyUp`. This is the `Decode` command in the
[state machine](architecture.md#statestep-a-total-function) — it only ever
fires on the `Recording → Transcribing` transition, never mid-recording.
Because decode only happens once per utterance, at a known correct decode
speed (9.7x real-time, see [asr.md](asr.md)), there is no growing-buffer cost
and no silent length cap.

## CPU only

Handy's model backend auto-selected an execution provider and bound the
model to the discrete GPU — a GTX 1650 with 4 GB VRAM. That GPU needs to stay
free for other workloads; the project's stated goal was CPU-only local ASR
from the start (README.md, STATUS.md: "the 4 GB GTX 1650 stays free").

**Rule:** `AsrConfig.num_threads` is fixed at 6 and the ONNX Runtime provider
is pinned to `cpu` (`scripts/spike_decode.py` builds the recognizer with
`provider="cpu"` explicitly). No code path in this project selects a GPU
provider.

## Bias vocabulary at decode time, never fuzzy replacement

An earlier approach corrected ASR output with fuzzy/edit-distance string
replacement after decoding. On short technical tokens, edit distance is not
selective enough: `set` → `sed`, `reset` → `rust` (README.md,
[`config.py`](../src/voice_kb/config.py) — see the `TextConfig.replacements`
docstring, which names this failure directly).

**Rule:** vocabulary correction happens inside decoding, by biasing the beam
search toward configured hotwords (`AsrConfig.vocabulary`,
`AsrConfig.hotwords_score` — see [asr.md](asr.md) for the mechanism and the
measured tuning table). `TextConfig.replacements` still exists for exact,
whole-word substitutions only — never fuzzy/edit-distance matching — and is
explicitly a secondary tool, not the primary correction mechanism.

## Overlay must not steal focus

Handy's status overlay was a focusable window. When it appeared during
recording, it could take focus away from the application the user was
dictating into, aborting the transcription in progress.

**Rule:** `OverlayConfig` defaults the overlay to enabled, borderless, and
positioned without stealing input focus (`no_focus` — the window must never
receive keyboard focus at any point in its lifecycle). This requirement
directly shaped the UI toolkit choice; see
[decisions.md](decisions.md#pyside6-over-gtk4).
