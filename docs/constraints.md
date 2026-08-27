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
| **One-shot *committed* decode** at key release, over the whole buffer | growing-buffer re-decode dropped long utterances | no (functional, silent) |
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

## One-shot committed decode, never streaming

Handy streamed audio into the model continuously, re-decoding a growing
buffer as more audio arrived. Measured real-time factor on this project's
hardware: **1.37x real-time** — barely faster than the utterance itself —
and it silently dropped audio past a 30 s cap, with no error surfaced to the
user (STATUS.md). A 37 s dictation was discarded outright.

**Rule:** the text voice-kb injects is produced by **exactly one decode of
the complete captured buffer**, run once after `KeyUp`. This is the `Decode`
command in the [state machine](architecture.md#statestep-a-total-function) —
it only ever fires on the `Recording → Transcribing` transition, never
mid-recording. Because the committed decode happens once per utterance, at a
known correct decode speed (9.7x real-time, see [asr.md](asr.md)), there is
no growing-buffer cost and no silent length cap.

### The one relaxation: cosmetic previews

The overlay shows a live transcript preview while the key is held
(`OverlayConfig.live_preview`). That is an extra decode, so it is worth being
precise about what this constraint does and does not forbid. What broke Handy
was not "more than one decode" — it was that the *result the user got* came
from a streaming pipeline whose cost grew with the utterance and which
silently truncated it. Three invariants keep previews on the safe side of
that line, and they are asserted in `tests/test_e2e.py`:

1. **The committed text is still one shot over the whole buffer.** Preview
   output is never injected, never merged into the final text, and never
   influences it. `AudioCapture.snapshot_capture` is non-destructive: the
   capture keeps accumulating and `stop_capture` still returns everything.
2. **Preview cost is bounded and independent of utterance length.** A preview
   decodes only a fixed-length trailing window
   (`OverlayConfig.preview_window_s`, 10 s by default), walking just the
   chunks that window needs. A five-minute dictation costs the same per
   preview as a ten-second one — there is no growing buffer anywhere.
3. **A preview can never make the user wait.** Previews are *abandoned* the
   instant a committed decode (or a cancellation) is due: `Daemon` sets the
   worker's abandon flag before it requests the decode, so a preview already
   queued on the single worker thread is dropped rather than run ahead of the
   text the user is waiting for.

There is still no time cap, no incremental re-decode of a growing buffer, and
nothing the user sees mid-recording can reach the clipboard. See
[decisions.md](decisions.md#live-transcript-preview-as-a-cosmetic-second-decode).

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
