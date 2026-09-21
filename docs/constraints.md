# Constraints

← [docs index](README.md) | Enforced by the modules described in
[architecture.md](architecture.md); the hardware specifics referenced below
are in [hardware.md](hardware.md); the rejected alternatives that led here
are in [decisions.md](decisions.md).

Every rule below traces to an observed failure of the dictation tool this
project replaced, which was evaluated first and rejected (see the first entry
in [decisions.md](decisions.md)). These are not defaults chosen for taste —
they are constraints earned by breaking a running system.

| Rule | Observed failure it prevents | Damaged the system? |
|---|---|---|
| **Read no input device**; control only through the socket | uinput clone inherited the default XKB layout | yes |
| **Never** synthesise characters (`xdotool type` / enigo) | rewrote the core X keymap | yes |
| **Never write to a window the user did not open for this** | text pasted into whatever had focus | no |
| **Every committed sample decoded exactly once**, never from a growing buffer | growing-buffer re-decode dropped long utterances | no (functional, silent) |
| **CPU only** | provider auto-selection bound the model to a small discrete GPU | no |
| **Bias vocabulary at decode time**, never fuzzy replacement | edit-distance rewrite turned real words into wrong ones | no |
| **No window spokenpad opens may take focus** | focus steal aborted transcription mid-utterance | no |

## Read no input device

The replaced tool grabbed its hotkey by cloning the keyboard through a uinput
virtual device (the standard way to intercept a key system-wide on Linux: grab
the real device with `EVIOCGRAB`, then re-emit a filtered event stream through
a new virtual one). The clone device does not inherit a per-device
`setxkbmap -device` layout applied to the physical keyboard (see
[hardware.md](hardware.md#per-device-keyboard-layouts)) — it gets whatever
X11's default layout is. The result: keyboard layout for that input path
silently reverted, corrupting keystrokes system-wide until the physical
device was replugged.

**Rule:** spokenpad opens no input device at all. It never calls
`EVIOCGRAB`, never creates a uinput clone, and never reads `/dev/input`. The
daemon is controlled only through its control socket
([`shell/control.rs`](../src/shell/control.rs)): the user binds keys in the
window manager or desktop to `spokenpad start`, `stop`, `toggle` and
`cancel`, and the window manager owns the key.

Until 2026-09-21 the rule was weaker: spokenpad read `/dev/input/event*`
read-only for its hotkey. That needed the `input` group, which can read
every keystroke on the machine, and it made spokenpad look like it owned the
key. See [decisions.md](decisions.md#control-by-socket-not-by-reading-the-keyboard).

## Never synthesise characters

The replaced tool typed transcribed text with synthetic keystrokes
(`xdotool type` / the equivalent enigo call). Both work by rewriting the
*core* X keymap on the fly to find keycodes for arbitrary Unicode characters,
one character at a time. This is not layout-neutral: it mutates global X server state, is why
this failure mode is not merely rare-but-possible.

It is also slow. Measured on one machine: **183 ms** via clipboard + `ctrl+v`
versus **3.3 s** via character synthesis for the same 159-character string.

**Rule:** spokenpad never synthesises characters.

Since 2026-09-07 it does not paste either. The transcript is
appended to a neovim buffer over that editor's msgpack-RPC socket
([`shell/nvim/mod.rs`](../src/shell/nvim/mod.rs)), which is strictly stronger
than the clipboard sink it replaced: no keystroke is sent anywhere, no global
X state is read or written, and the transcript never crosses a shell or an
argv boundary — it is a msgpack string argument, so a dictated `$(rm -rf ~)`
is just text. The clipboard round trip is deleted; see
[decisions.md](decisions.md#the-sink-is-neovim-not-the-clipboard).

Since 2026-09-19 the clipboard is written again, but only as a copy, never as
a delivery path, and since 2026-09-21 only when `nvim.copy_to_clipboard` is
set (off by default): after every release the dictation nvim sets its own
`+` register to the whole buffer, through nvim's clipboard provider. Nothing
is pasted, no key is sent, and no other window is written to, so every
failure above stays impossible. See
[decisions.md](decisions.md#the-whole-buffer-is-copied-to-the-clipboard-after-a-release)
and [decisions.md](decisions.md#the-clipboard-copy-becomes-opt-in-off-by-default-2026-09-21).

## Every committed sample is decoded exactly once, never streamed

The replaced tool streamed audio into the model continuously, re-decoding a
growing buffer as more audio arrived. Measured real-time factor on the same
machine: **1.37x real-time** — barely faster than the utterance itself — and
it silently dropped audio past a 30 s cap, with no error surfaced to the
user. A 37 s dictation was discarded outright.

**Rule:** every captured sample that reaches the buffer is decoded **exactly
once** (two exceptions, both only after a decode produced nothing: a speech
chunk is decoded again without trailing silence, and when every chunk of a
release is still empty the whole remainder is decoded once), and no
committed text ever comes from re-decoding audio that is still growing. Cost
is therefore linear in the audio, and nothing is capped or discarded at any
length.

A capture does *end* by itself when nobody ends it — after
`capture.silence_timeout_s` without speech, at `MAX_CAPTURE` (4 h), or at the
in-memory ceiling
([progressive-commit.md](progressive-commit.md#when-a-capture-ends-by-itself)).
That is not a cap on dictation: each of those endings decodes the tail and
keeps every sample already captured, exactly as a key release does. What they
bound is the file and the memory, not the transcript.

Until 2026-09-08 that rule was implemented as a single decode of the whole
buffer after `KeyUp`. It is now implemented *progressively*: the capture is
split at silence by [`shell/inference.rs`](../src/shell/inference.rs) under
the merge policy in [`core/segments.rs`](../src/core/segments.rs), driven by
[`core/decode.rs`](../src/core/decode.rs), into chunks of about ten seconds of
speech; a chunk is decoded and appended the moment no later audio can change
it, and releasing the key decodes only the open tail. The full design, the
rule for deciding that a chunk has settled, and the latency budget are in
[progressive-commit.md](progressive-commit.md). The `Decode` command in the
[state machine](../src/core/state.rs) still fires only on
`Recording → Transcribing`; what it now decodes is the remainder, not the
whole. The [decode invariants](architecture.md#decode-invariants) list what
that buys.

### Segmented is not streamed

"The transcript arrives in pieces" sounds like the failure mode by
description, so the difference is worth being exact about. What broke the
replaced tool was re-decoding a *growing* buffer: the same audio decoded
again and again as more arrived, so cost grew with the utterance and a cap had
to be bolted on, which then dropped a 37 s dictation silently. Committing a settled chunk keeps
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
boundaries clip word onsets and endings. Measured on the eval samples at the
time, per-run 33.7% → 37.7% WER, merged-and-padded 33.4%. Measured on
2026-09-21 with `cargo run --release --example=eval`, the VAD path spokenpad
actually uses scores **18.7%** aggregate WER and the whole-buffer path
(`--whole`) **14.3%** (see [evaluation.md](evaluation.md)), so splitting is
not free — it is bought
for incremental delivery and for the empty-transcript failure above. Anyone
lowering `chunk_seconds` for faster text is spending more of it, and should
re-run that harness.

One property is weaker than a single decode at release, and it is stated
rather than hidden: a cancel no longer means the text never existed. While
recording it means "stop adding" — what is already in the buffer stays,
because it is the user's file. After the key is released it means nothing at
all: the audio is already captured, and the final decode is about to land, so
`(Transcribing, Cancel)` is a no-op rather than a way to destroy a finished
dictation.

### The one relaxation: a cosmetic preview of the open tail

The audio after the last settled chunk is decoded once per tick and shown as
virtual text below the committed transcript, then thrown away. That is an
extra decode of audio that is still growing, so the invariants that keep it
on the safe side of this rule are worth stating, and they are asserted in the
session, decode and nvim tests:

1. **Preview output never reaches the buffer.** In nvim it is an extmark's
   virtual text, not buffer content, so it cannot be written to the file,
   yanked, or undone into the buffer even deliberately. Committed decode never
   reads preview state.
2. **Preview cost is bounded by the chunk, not the utterance.** Only the open
   tail is decoded, and it is at most one unclosed chunk. A preview costs the
   same at ten minutes as at ten seconds, and so does the memory: the audio
   behind the committed offset is dropped while the capture runs
   ([progressive-commit.md](progressive-commit.md#what-is-kept-in-memory)).
   Without a segmenter loaded there is no bounded tail, so **no preview tick is
   issued at all** and the capture is decoded at release. `[preview].max_seconds`
   (30 s) is the remaining backstop for a chunk that somehow never settles:
   past it the tick stops decoding the open tail but keeps committing what has
   settled, so the tail shrinks and previews resume by themselves.
3. **A tick can only make the user wait for bounded work.** Previews are
   abandoned the instant a release or a cancel is due: one still queued on the
   single worker is skipped, and one already running stops after its current
   chunk — which, if that chunk was settled, it has just committed rather than
   wasted. What a release can still queue behind is the work already inside
   the tick: one detector pass, and one settled chunk's decode. Neither is
   interruptible, so both are bounded instead — a tick is handed at most
   `[preview].max_seconds` of audio however long the tail is, which holds the
   detector pass to about 0.29 s (measured; 2.4 s over a 270 s tail, which is
   why the bound is there — see
   [the experiment](experiments/2026-09-21-constant-ram-recording.md)).

The guard against a preview that came back *shorter* than the last one is
kept: decoding 4.4 s of a real sample returned `"Okay."` where 3.3 s of the
same sample had returned a full sentence. It is keyed on the committed
offset, since a tail legitimately restarts from nothing when the chunk above
it lands.

## CPU only

The replaced tool's model backend auto-selected an execution provider and
bound the model to a small discrete GPU, whose memory was needed for other
work. The goal was CPU-only local ASR from the start, and int8 weights on a
few CPU threads decode fast enough that a GPU buys nothing
([asr.md](asr.md#speed-on-a-cpu)).

**Rule:** the ONNX Runtime provider is pinned to `cpu`, with
`asr.num_threads` threads (default 6) —
[`shell/inference.rs`](../src/shell/inference.rs) sets it explicitly for both
the recognizer and the VAD.
No code path in this project selects a GPU provider, and nothing auto-detects
one.

## The only network access is the pinned model download

Since 2026-09-21, `spokenpad fetch-models` and the daemon/`check`/
`transcribe`'s own automatic download before loading a still-default model
are the only code in this project that opens a network connection — see
[decisions.md](decisions.md#model-download-moves-into-the-binary). Every URL
fetched is a literal constant in
[`core/models.rs`](../src/core/models.rs), never built from configuration,
an argument, or anything read from a file, and every download is verified
against a pinned size and sha256 before it is written to its final path.
This is deliberately narrower than "the daemon may reach the network": it
never does, once its models are in place, and nothing else in spokenpad
ever makes a request at all.

## Bias vocabulary at decode time, never fuzzy replacement

An earlier approach corrected ASR output with fuzzy/edit-distance string
replacement after decoding. On short technical tokens, edit distance is not
selective enough: `set` → `sed`, `reset` → `rust` (the
`[text].replacements` comment in
[`config.example.toml`](../config.example.toml) names this failure directly,
and `eval-samples/references.json` names it among the wordings a clip
exercises).

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

**Rule:** the transcript only ever goes to an editor opened for the purpose,
over that editor's own socket. In managed mode the daemon spawns that neovim
itself; in attach mode the user opens it with `spokenpad editor`, which runs
nvim on the dictation socket and marks it as spokenpad's. No other window is
ever written to, and nothing is pasted anywhere. With no such editor open,
the transcript goes to a dictation file on disk — never to a window — and the
next editor opens on that file (see
[nvim-window.md](nvim-window.md#dictating-with-no-editor-open)).

## No window spokenpad opens may take focus

The replaced tool's status overlay was a focusable window. When it appeared
during recording, it could take focus away from the application the user was
dictating into, aborting the transcription in progress.

**Rule:** no window this project opens may receive keyboard focus at any point
in its lifecycle. How that holds depends on `nvim.mode`:

- **Pane mode opens exactly one: a window spokenpad draws itself**, and it
  needs no rule and no window manager's cooperation. The window carries
  `_NET_WM_USER_TIME = 0` — the EWMH value for "do not focus this window when
  it is mapped" — together with `_NET_WM_WINDOW_TYPE_UTILITY`,
  `WM_HINTS input = True` and a `WM_CLASS` of `spokenpad-pane`, all written
  before the window is first mapped, which is when a window manager reads
  them. Three rules hold this in place, and each is asserted:
  - **`_NET_WM_USER_TIME` is written once, as zero, and never again.** The
    EWMH contract is that a toolkit keeps it at the timestamp of the last user
    interaction, and a window manager re-reads it at every map — so a window
    that kept it current would steal focus the next time it appeared. The
    window type exposes no way to write it a second time, and the test checks
    both halves: the value is still zero after a click and two keystrokes, and
    a rewritten value focus-steals.
  - **`WM_TAKE_FOCUS` is not announced.** With `input = True` and a user time
    of zero it changes nothing, and announcing it would oblige spokenpad to
    answer a protocol whose whole purpose is taking focus.
  - **There is no focus call**, as everywhere else in this project.

  The proof is `tests/pane_window.rs`, which opens the shipped window on an
  i3 it starts itself and checks the i3 tree and the X input focus, on a
  workspace that already has a focused window *and* on an empty one — where
  `no_focus` fails and this does not. It runs an ablation beside it, so
  "unfocused" is known to mean something: drop the user time and the same
  window is focused. **Verified on i3 only.** Mutter, KWin, Openbox, xfwm4,
  bspwm and Hyprland are read from their source and not run; sway and awesome
  are known to need more. Pane mode is new and says so wherever it is offered.
- **Attach mode opens no window at all.** The user opens the editor in a
  terminal of their choosing, and the daemon only ever talks to its socket,
  so the rule holds by construction, on any desktop.
- **Managed mode opens exactly one: the dictation window**, and only after
  proving that the running window manager refuses it focus. The terminal is
  one of a known table ([`core/terminal.rs`](../src/core/terminal.rs)) that
  names its window before it exists — the X11 instance on i3, the Wayland
  `app_id` and the instance under sway — and the daemon reads the loaded
  configuration over the window manager's IPC socket and refuses to spawn
  unless it finds a `no_focus` rule for exactly that name
  ([`shell/wm.rs`](../src/shell/wm.rs)), and unless the focused workspace
  already holds a window, since both window managers focus the first window
  on a workspace despite `no_focus`. A terminal outside the table is
  refused, since its window cannot be named in advance.

There is deliberately no focus call anywhere in
[`shell/nvim/mod.rs`](../src/shell/nvim/mod.rs) or
[`shell/wm.rs`](../src/shell/wm.rs) — not even a "restore the previous focus"
one, which would be a focus change of its own; the only window-manager
command sent floats, sizes and moves the dictation window by criteria. See
[nvim-window.md](nvim-window.md). (The Qt status overlay that preceded it was
non-focusable by construction; it is deleted, see
[decisions.md](decisions.md#pyside6-over-gtk4).)

Verified live on 2026-09-07: opening the dictation window left the focused
window unchanged, and i3 reported the new window as `focused: false`. sway is
implemented from its documentation and source and tested against a fake IPC
server, not yet live.
