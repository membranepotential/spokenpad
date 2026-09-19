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
| **Every committed sample decoded exactly once**, never from a growing buffer | growing-buffer re-decode dropped long utterances | no (functional, silent) |
| **CPU only** | auto-selection bound the model to the 4 GB GTX 1650 | no |
| **Bias vocabulary at decode time**, never fuzzy replacement | edit-distance rewrite turned real words into wrong ones | no |
| **No window spokenpad opens may take focus** | focus steal aborted transcription mid-utterance | no |

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

**Rule:** spokenpad reads `/dev/input/event*` for the hotkey only, never calls
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

**Rule:** spokenpad never synthesises characters.

Since 2026-09-07 it does not paste either. The transcript is
appended to a neovim buffer over that editor's msgpack-RPC socket
([`shell/nvim/mod.rs`](../src/shell/nvim/mod.rs)), which is strictly stronger
than the clipboard sink it replaced: no keystroke is sent anywhere, no global
X state is read or written, and the transcript never crosses a shell or an
argv boundary — it is a msgpack string argument, so a dictated `$(rm -rf ~)`
is just text. The clipboard round trip (`inject.py`) is deleted; see
[decisions.md](decisions.md#the-sink-is-neovim-not-the-clipboard).

Since 2026-09-19 the clipboard is written again, but only as a copy, never as
a delivery path: after every release the dictation nvim sets its own `+`
register to the whole buffer, through nvim's clipboard provider. Nothing is
pasted, no key is sent, and no other window is written to, so every failure
above stays impossible. See
[decisions.md](decisions.md#the-whole-buffer-is-copied-to-the-clipboard-after-a-release).

## Every committed sample is decoded exactly once, never streamed

Handy streamed audio into the model continuously, re-decoding a growing
buffer as more audio arrived. Measured real-time factor on this project's
hardware: **1.37x real-time** — barely faster than the utterance itself —
and it silently dropped audio past a 30 s cap, with no error surfaced to the
user (STATUS.md). A 37 s dictation was discarded outright.

**Rule:** every captured sample that reaches the buffer is decoded **exactly
once** (two exceptions, both only after a decode produced nothing: a speech
chunk is decoded again without trailing silence, and when every chunk of a
release is still empty the whole remainder is decoded once), and no committed text ever comes from re-decoding audio that is still
growing. Cost is therefore linear in the audio, and nothing is capped or
discarded at any length.

Until 2026-09-08 that rule was implemented as a single decode of the whole
buffer after `KeyUp`. It is now implemented *progressively*: the capture is
split at silence by [`shell/inference.rs`](../src/shell/inference.rs) under
the merge policy in [`core/segments.rs`](../src/core/segments.rs), driven by
[`core/decode.rs`](../src/core/decode.rs), into chunks of about ten seconds of
speech; a chunk is decoded and appended the moment no later audio can change
it, and releasing the key decodes only the open tail. (`spokenpad.vad` mirrors
the same splitter offline, for evaluation and the Rust/Python differential
check — it is not in the daemon's path.) The full design, the rule for
deciding that a chunk has settled, and the latency budget are in
[progressive-commit.md](progressive-commit.md). The `Decode` command in the
[state machine](../src/core/state.rs) still fires only on
`Recording → Transcribing`; what it now decodes is the remainder, not the
whole. The [decode invariants](architecture.md#decode-invariants) list what
that buys.

### Segmented is not streamed

"The transcript arrives in pieces" sounds like the failure mode by
description, so the difference is worth being exact about. What broke Handy
was re-decoding a *growing* buffer: the same audio decoded again and again as
more arrived, so cost grew with the utterance and a cap had to be bolted on,
which then dropped a 37 s dictation silently. Committing a settled chunk keeps
both properties that rule protects. No committed region is ever decoded
twice, so cost stays linear in the audio; and nothing is capped or discarded
at any length, because a chunk boundary is a pause, not a limit.

Segmentation exists for **correctness** before speed. Parakeet TDT returns an
empty string when speech is a small fraction of its window — measured, same
0.53 s of speech, varying only the padding: no padding gives `'D home.'`, 2 s
each side gives `'Did the home?'`, 5 s each side gives `''`. In use that was
pressing the key, saying two words, and getting nothing back.

That measurement has a mirror image. A window with *no* speech in it does not
come back empty — it comes back invented: a 0.5 s near-silent press (peak
0.013) was decoded whole and Parakeet returned "Thank you.", which landed in
the file. So since 2026-09-11, with a VAD model loaded, a capture it finds no
speech in is not decoded at all. Only the detector may make that call — with
no VAD model the whole capture is still decoded, because nothing else knows
better.

The same knife edge has a third face: a short sentence the VAD *did* detect
can come back empty. Replaying the user's recordings, 4 of 18 captures with
under 3 s of speech decoded to `""` in their padded window (0.5 s pad plus the
1 s of zeros the recogniser appends), and every one decoded correctly from the
same window without the appended zeros. No single presentation is safe on
every clip, so since 2026-09-19 a VAD chunk that decodes empty is decoded once
more without the trailing silence. That second decode happens only after the
first produced nothing, so no text is ever produced twice from the same audio;
previews and the no-VAD path are not retried.

Throughput is unchanged — 12.3-12.6x real-time whole-buffer against 11.0-11.7x
segmented, slightly *worse*, because per-chunk overhead costs about what the
skipped silence saves. What changes is *when* text appears.

**Accuracy is preserved by merging, and that is not optional.** Decoding every
detected run of speech separately costs about four WER points, because the
model gets no context across a boundary. Runs are therefore merged until a
chunk holds `vad.chunk_seconds` (10 s) of speech, and each chunk is padded by
`vad.pad_seconds` (0.5 s) of the real surrounding audio, since Silero's
boundaries clip word onsets and endings. Measured on the eval samples, per-run
33.7% → 37.7% WER, merged-and-padded 33.4%. Measured again after the
endpoint-padding fix, the harness scores the VAD path spokenpad actually uses at
**17.6%** aggregate WER against **13.9%** whole-buffer (`uv run
scripts/eval.py --vad` and without the flag; Handy 0.9.6 scores 48.7% on the
same five clips). Splitting is not free on this set — it is bought for
incremental delivery and for the empty-transcript failure below. Anyone
lowering `chunk_seconds` for faster text is spending more of it, and should
re-run that harness.

One property is weaker than a single decode at release, and it is stated
rather than hidden: a cancel no longer means the text never existed. While
recording it means "stop adding" — what is already in the buffer stays,
because it is the user's file. After the key is released it means nothing at
all: the cancel key is read from every keyboard regardless of focus, the audio
is already captured, and the final decode is about to land, so
`(Transcribing, Cancel)` is a no-op rather than a way to destroy a finished
dictation.

### The one relaxation: a cosmetic preview of the open tail

The audio after the last settled chunk is decoded once per tick and shown as
virtual text below the committed transcript, then thrown away. That is an
extra decode of audio that is still growing, so the invariants that keep it
on the safe side of this rule are worth stating, and they are asserted in the
Rust session/nvim tests and retained Python decode tests:

1. **Preview output never reaches the buffer.** In nvim it is an extmark's
   virtual text, not buffer content, so it cannot be written to the file,
   yanked, or undone into the buffer even deliberately. Committed decode never
   reads preview state.
2. **Preview cost is bounded by the chunk, not the utterance.** Only the open
   tail is decoded, and it is at most one unclosed chunk. A preview costs the
   same at ten minutes as at ten seconds. Without a segmenter loaded there is
   no bounded tail, so **no preview tick is issued at all** and the capture is
   decoded at release. `[preview].max_seconds` (30 s) is the remaining
   backstop for a chunk that somehow never settles: past it previews pause and
   say so, and resume by themselves once the tail is short again.
3. **A preview can never make the user wait.** Previews are abandoned the
   instant a release or a cancel is due: a preview still queued on the single
   worker is skipped, and one already running stops after its current chunk
   — which, if that chunk was settled, it has just committed rather than
   wasted.

The guard against a preview that came back *shorter* than the last one is
kept: decoding 4.4 s of a real sample returned `"Okay."` where 3.3 s of the
same sample had returned a full sentence. It is keyed on the committed
offset, since a tail legitimately restarts from nothing when the chunk above
it lands.

## CPU only

Handy's model backend auto-selected an execution provider and bound the
model to the discrete GPU — a GTX 1650 with 4 GB VRAM. That GPU needs to stay
free for other workloads; the project's stated goal was CPU-only local ASR
from the start (README.md, STATUS.md: "the 4 GB GTX 1650 stays free").

**Rule:** `Asr.num_threads` is fixed at 6 and the ONNX Runtime provider is
pinned to `cpu` — [`shell/inference.rs`](../src/shell/inference.rs) sets it
explicitly for both the recognizer and the VAD, as does the Python reference.
No code path in this project selects a GPU provider, and nothing auto-detects
one.

## Bias vocabulary at decode time, never fuzzy replacement

An earlier approach corrected ASR output with fuzzy/edit-distance string
replacement after decoding. On short technical tokens, edit distance is not
selective enough: `set` → `sed`, `reset` → `rust` (README.md; the
`[text].replacements` comment in
[`config.example.toml`](../config.example.toml) names this failure directly,
and `eval-samples/references.json` keeps it as a scored regression check).

**Rule:** vocabulary correction happens inside decoding, by biasing the beam
search toward configured hotwords (`asr.vocabulary`,
`asr.hotwords_score` — see [asr.md](asr.md) for the mechanism and the
measured tuning table). `text.replacements` still exists for exact,
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

**Rule:** the transcript only ever goes to a window spokenpad opened for the
purpose. The daemon spawns its own neovim and appends to a buffer in it. No
other window is ever written to, and nothing is pasted anywhere.

## No window spokenpad opens may take focus

Handy's status overlay was a focusable window. When it appeared during
recording, it could take focus away from the application the user was
dictating into, aborting the transcription in progress.

**Rule:** no window this project opens may receive keyboard focus at any point
in its lifecycle. Since 2026-09-08 it opens exactly one: the **dictation
window**, which is refused focus by the window manager via a `no_focus` rule
keyed on its X11 instance name. There is deliberately no focus call anywhere
in [`shell/nvim/mod.rs`](../src/shell/nvim/mod.rs) — not even a "restore the
previous focus" one, which would be a focus change of its own. See
[nvim-window.md](nvim-window.md). (The Qt status overlay that preceded it was
non-focusable by construction; it is deleted, see
[decisions.md](decisions.md#pyside6-over-gtk4).)

Verified live on 2026-09-07: opening the dictation window left the focused
window unchanged, and i3 reported the new window as `focused: false`.
