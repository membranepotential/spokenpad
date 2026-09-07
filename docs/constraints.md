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
| **Never write to a window the user did not open for this** | text pasted into whatever had focus | no |
| **One-shot *committed* decode** at key release, over the whole buffer | growing-buffer re-decode dropped long utterances | no (functional, silent) |
| **CPU only** | auto-selection bound the model to the 4 GB GTX 1650 | no |
| **Bias vocabulary at decode time**, never fuzzy replacement | edit-distance rewrite turned real words into wrong ones | no |
| **No window voice-kb opens may take focus** | focus steal aborted transcription mid-utterance | no |

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

**Rule:** voice-kb never synthesises characters.

Since 2026-09-07 it does not touch the clipboard either. The transcript is
appended to a neovim buffer over that editor's msgpack-RPC socket
([`nvim.py`](../src/voice_kb/nvim.py)), which is strictly stronger than the
clipboard sink it replaced: no keystroke is sent anywhere, no global X state
is read or written, and the transcript never crosses a shell or an argv
boundary — it is a msgpack string argument, so a dictated `$(rm -rf ~)` is
just text. The clipboard round trip (`inject.py`) is deleted; see
[decisions.md](decisions.md#the-sink-is-neovim-not-the-clipboard).

## One-shot committed decode, never streaming

Handy streamed audio into the model continuously, re-decoding a growing
buffer as more audio arrived. Measured real-time factor on this project's
hardware: **1.37x real-time** — barely faster than the utterance itself —
and it silently dropped audio past a 30 s cap, with no error surfaced to the
user (STATUS.md). A 37 s dictation was discarded outright.

**Rule:** every captured sample is decoded **exactly once**, in a single
pass that starts after `KeyUp`. This is the `Decode` command in the
[state machine](architecture.md#statestep-a-total-function) — it only ever
fires on the `Recording → Transcribing` transition, never mid-recording.
Because the committed decode happens once per utterance, at a known correct
decode speed (9.7x real-time, see [asr.md](asr.md)), there is no
growing-buffer cost and no silent length cap.

### Segmented is not streamed

That pass is split at silence by [`voice_kb.vad`](../src/voice_kb/vad.py)
before decoding, and each speech segment is appended as it lands rather than
all of them at the end. This is not the thing the rule forbids, and the
difference is worth being exact about, because "the transcript arrives in
pieces" sounds like the failure mode by description.

What broke Handy was re-decoding a *growing* buffer: the same audio decoded
again and again as more arrived, so cost grew with the utterance and a cap had
to be bolted on, which then dropped a 37 s dictation silently. Segmentation
keeps both properties that rule protects. No region is ever decoded twice, so
cost stays linear in the audio; and nothing is capped or discarded at any
length, because a segment boundary is a pause, not a limit.

The reason it exists is **correctness**, not speed. Parakeet TDT returns an
empty string when speech is a small fraction of its window — measured, same
0.53 s of speech, varying only the padding: no padding gives `'D home.'`, 2 s
each side gives `'Did the home?'`, 5 s each side gives `''`. In use that was
pressing the key, saying two words, and getting nothing back. It is also the
other end of the preview instability described below: a preview of 4.4 s
returning `"Okay."` where 3.3 s returned a full sentence is the same collapse,
seen one preview at a time.

Throughput is unchanged — 12.3-12.6x real-time whole-buffer against 11.0-11.7x
segmented, slightly *worse*, because per-segment overhead costs about what the
skipped silence saves. What changes is when the first text appears:
0.27-0.49 s instead of 2.2-3.4 s on the same clips. Nobody should reach for
segmentation to make decoding faster; it does not.

One property genuinely weakens. A cancellation during `Transcribing` used to
mean the text never existed; now some of it may already be in the buffer, so
it means "stop adding" and what landed stays. That is the honest trade for
watching a long passage arrive instead of waiting out its decode, and the file
is the user's to edit either way.

### The one relaxation: cosmetic previews

The overlay shows a live transcript preview while the key is held
(`PreviewConfig.enabled`). That is an extra decode, so it is worth being
precise about what this constraint does and does not forbid. What broke Handy
was not "more than one decode" — it was that the *result the user got* came
from a streaming pipeline whose cost grew with the utterance and which
silently truncated it. Three invariants keep previews on the safe side of
that line, and they are asserted in `tests/test_e2e.py`:

1. **The committed text is still one shot over the whole buffer.** Preview
   output is never appended, never merged into the final text, and never
   influences it. In the nvim window this is enforced *physically* rather
   than by discipline: the preview is an extmark's virtual text, not buffer
   content, so it cannot be written to the file, yanked, or undone into the
   buffer even deliberately. `AudioCapture.snapshot_capture` is non-destructive: the
   capture keeps accumulating and `stop_capture` still returns everything.
2. **Preview cost is bounded.** Not *constant* — this is weaker than it was,
   deliberately. Previews originally decoded a fixed-length trailing window,
   which made cost independent of utterance length but physically discarded
   the start of the sentence: text the user had already watched appear
   vanished in chunks while they were still speaking. A preview whose whole
   job is to show what was heard cannot throw away what was heard, so it now
   decodes the entire utterance so far and cost grows with it.

   Two things bound that growth. The gap between previews is
   `max(interval - last_decode, last_decode)`, so the worker is idle at least
   half the time however long the utterance runs; and past
   `PreviewConfig.max_seconds` (15 s) previews stop being issued at
   all. Nothing is cleared when they stop — the last one stays on screen.

   The budget this buys: at ~14.6x real-time a 15 s utterance costs ~1.0 s,
   which is the worst a key release can wait on an in-flight preview, ~0.5 s
   expected from the duty cycle.

   One further guard, for the same reason the window was dropped: each
   preview sees strictly more audio than the last, so the transcript *should*
   only grow, but the recogniser does not guarantee it — decoding 4.4 s of a
   real sample returned `"Okay."` where 3.3 s of the same sample had returned
   `"Okay, we are now at the new model."`. A preview that comes back shorter
   is treated as instability rather than news, and the previous text stands.
   The guard resets per utterance, or a long dictation would suppress every
   shorter preview of the next one.
3. **A preview can never make the user wait.** Previews are *abandoned* the
   instant a committed decode (or a cancellation) is due: `Daemon` sets the
   worker's abandon flag before it requests the decode, so a preview already
   queued on the single worker thread is dropped rather than run ahead of the
   text the user is waiting for.

There is still no time cap, no incremental re-decode of a growing buffer, and
nothing the user sees mid-recording can reach the buffer. See
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

## Never write to a window the user did not open for this

The clipboard sink wrote into whatever happened to have focus at the moment a
decode finished. That is a race with the user: focus can move between the key
release and the decode landing a second later, and the text goes wherever
focus went. It also meant every dictation depended on the target application
cooperating — a per-window-class paste combo, a target that reads the
clipboard slowly enough to race the restore, a terminal that swallows
`ctrl+v`.

**Rule:** the transcript only ever goes to a window voice-kb opened for the
purpose. The daemon spawns its own neovim and appends to a buffer in it. No
other window is ever written to, and nothing is pasted anywhere.

## No window voice-kb opens may take focus

Handy's status overlay was a focusable window. When it appeared during
recording, it could take focus away from the application the user was
dictating into, aborting the transcription in progress.

**Rule:** no window this project opens may receive keyboard focus at any
point in its lifecycle. That covers both windows it can open:

* the **overlay** is borderless and non-focusable by construction
  (`WA_ShowWithoutActivating`, `WindowDoesNotAcceptFocus`, the `Tool` window
  type). The requirement directly shaped the UI toolkit choice; see
  [decisions.md](decisions.md#pyside6-over-gtk4);
* the **dictation window** is refused focus by the window manager, via a
  `no_focus` rule keyed on its X11 instance name. There is deliberately no
  focus call anywhere in [`nvim.py`](../src/voice_kb/nvim.py) — not even a
  "restore the previous focus" one, which would be a focus change of its own.
  See [nvim-window.md](nvim-window.md).

Verified live on 2026-09-07: opening the dictation window left the focused
window unchanged, and i3 reported the new window as `focused: false`.
