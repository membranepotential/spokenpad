# Decisions

← [docs index](README.md) | ADR-style log: what was chosen, what was
rejected, and why. Each entry cross-links to the constraint or architecture
doc that depends on it.

## Rejected Handy as a baseline

[Handy](https://github.com/cjpais/Handy) 0.9.6 was evaluated first as the
closest existing tool. Rejected after seven defects found in an afternoon,
four of which damaged the running system: a uinput keyboard clone that
destroyed per-device XKB layout, character-synthesis typing that rewrote the
core X keymap, streaming decode that dropped long utterances, and a
focus-stealing overlay that aborted transcription (README.md, STATUS.md).
Every hard constraint in [constraints.md](constraints.md) traces back to one
of these seven. Handy was run end-to-end, every change it made was reverted,
and it was uninstalled (STATUS.md).

## Python + `uv` over Rust (superseded)

Originally chosen for the implementation language and toolchain. Python 3.13,
strict mypy, and `uv` remain for offline setup/evaluation helpers only; the
production runtime is Rust. At the time, no performance case demanded Rust:
the ASR bottleneck is the ONNX Runtime, not Python, and int8 CPU decode
already runs at 9.7x real-time (see [asr.md](asr.md)) — well past the
one-shot-per-utterance requirement.

## PySide6 over GTK4

**Superseded 2026-09-08.** The overlay is deleted: off by default since the
nvim sink landed, with no user, it was the second-sink trap below in UI form.
The later Rust runtime removed the remaining Qt event-loop/thread plumbing and
the PySide6 dependency. The original UI reasoning is kept below for the record.

The overlay must be positioned on a specific output and must never take
keyboard focus (see
[constraints.md](constraints.md#no-window-spokenpad-opens-may-take-focus)). This ruled
out GTK4: it has no `Window.move()` or `set_type_hint()` on X11 — verified —
so it cannot position or hint a non-focusable overlay window on this
platform (STATUS.md: "GTK4 has no `move()`/`set_type_hint()` on X11
(verified)"). PySide6 supported both and was used by the now-deleted Python
runtime; it is no longer a project dependency.

## TOML config over a settings GUI

There is no settings application. Configuration is a TOML file parsed and
validated once at startup, before any thread, device or model —
`Config::load` in [`config.rs`](../src/config.rs) is the production path;
`config.py` retains a mirror of it for the offline evaluation tools. Unknown
sections and keys are a hard error rather than a silent default. The only
runtime UI is the
dictation window's winbar. This keeps the UI surface to exactly the one
window that must exist for user feedback during recording, rather than
building a second, larger UI surface purely for configuration.

## Hotwords-only cleanup for v1

Vocabulary correction for v1 is decode-time hotword biasing only (see
[asr.md](asr.md#hotwords-biasing-the-beam-not-rewriting-the-output)) — no LLM
cleanup pass. `[text].replacements` in the TOML
([`config.example.toml`](../config.example.toml)) exists as an escape hatch for
exact, whole-word substitutions, but hotword biasing is the primary mechanism.

An LLM pass is deferred, not ruled out. STATUS.md records this as an open
question: "Does hotword biasing alone close the technical-vocabulary gap
(`dir`→`there`), or is an LLM cleanup pass still needed? Answer with the eval
harness once references exist." That harness needs
`eval-samples/transcripts.json` hand-corrected to ground truth first — it
currently holds Handy's raw (error-including) output, not references
(STATUS.md).

## Progressive commit: settled chunks land while speaking

**2026-09-08.** The committed decode no longer waits for the key. Each chunk
the VAD has closed is decoded once and appended the moment no later audio can
change it; releasing the key decodes only the open tail. Design, settle rule
and latency budget: [progressive-commit.md](progressive-commit.md).

Why now: a 379 s passage took 31.5 s to land and an 821 s one 54.5 s, and the
preview meanwhile had to hold the whole transcript as virtual text, cropping
its beginning at eight lines. The preview was *already* decoding settled
chunks exactly once — to show them, then throwing the work away — so this
promotes that work to the commit rather than adding a decode.

The constraint is reworded from "one-shot decode at key release" to "every
committed sample decoded exactly once, never from a growing buffer", which is
the property the original wording was protecting
([constraints.md](constraints.md#every-committed-sample-is-decoded-exactly-once-never-streamed)).

Rejected:

- **Committing on every VAD span** rather than on merged chunks: four WER
  points, measured; the merge target stays.
- **Committing the last chunk as soon as it closes**, without waiting for 1 s
  of silence: the span that closed it may be the one `flush()` cut mid-word,
  which moves on the next tick. One second is the cheapest proof it did not.
- **Letting the daemon own the committed offset**: a preview mid-flight can
  commit a chunk after the daemon has snapshotted for the release decode, and
  that chunk would be decoded twice. The offset lives on the worker, the one
  thread that serialises decodes; the daemon holds a lagging hint used only to
  keep snapshots small.

## Live transcript preview as a cosmetic second decode

**Superseded 2026-09-08** by progressive commit above: the preview is now
only the open tail, and the settled chunks it used to carry are committed
instead. Kept for the record.

The overlay now shows a rolling preview of what is being said while the key
is held. This touches the project's third hard constraint, so the change was
a deliberate *rewording* of that constraint rather than a quiet exception to
it (see [constraints.md](constraints.md#every-committed-sample-is-decoded-exactly-once-never-streamed)).

The constraint used to read "one-shot decode at key release; no streaming, no
incremental re-decode, no time cap." It exists because Handy's streaming
model ran at 1.37x real-time and **silently discarded a 37 s dictation** at
its 30 s finalize cap. Read literally, "one decode, ever" also forbids showing
the user anything at all until they let go of the key — which is the single
biggest usability gap against Wispr Flow, and costs nothing that the original
failure was about.

What actually failed in Handy was that the *committed result* came out of a
growing-buffer pipeline. So the constraint is now scoped to the committed
decode: it is the text that gets injected that must come from exactly one
decode of the complete buffer. Previews are permitted only under three
invariants that make the old failure mode unreachable:

- previews are never injected, merged, or otherwise allowed to influence the
  committed text (the Rust session exposes them only to nvim's virtual-text
  renderer);
- a preview decodes a fixed-length trailing window, never a growing buffer,
  so its cost cannot scale with utterance length;
- previews are abandoned before a committed decode is requested, so a
  cosmetic decode can never sit in front of the user's real one on the single
  worker thread.

Rejected alternatives:

- **A real streaming recognizer.** Reintroduces exactly the architecture that
  was rejected, for a cosmetic feature.
- **Re-decoding the whole buffer for each preview.** Simpler to write, and
  precisely the growing-buffer cost the constraint was written against.
- **Previewing on a second recognizer instance.** Doubles the resident model
  (CPU-only, and the decode is already 9.7x real-time) and removes the
  serialization that keeps the non-thread-safe `Transcriber` safe. Sharing
  the one worker is what guarantees a preview and the committed decode never
  overlap.

## The sink is neovim, not the clipboard

**2026-09-07.** The transcript is appended to a floating neovim window over
that editor's msgpack-RPC socket. Nothing is pasted anywhere, and
`inject.py` — the clipboard round trip and its window-class-aware paste combo
— is deleted.

The driving use case changed: not "text lands at the cursor while I work",
but "record long passages quickly, for example while reading through a
generated document". Under that use, pasting into the focused window is not a
feature to preserve, it is the problem — the focused window is the document
being read.

What this buys, beyond matching the use case:

* **No race with focus.** The old sink wrote wherever focus happened to be
  when a decode landed, which is a second or more after the key was released.
* **No dependence on the target application.** No per-window-class paste
  combo, no clipboard-restore race (`Injected.confirmed` existed precisely
  because that race could not be closed), no terminal swallowing `ctrl+v`.
* **A stronger version of the no-synthesis rule.** The text crosses no shell
  and no argv boundary — it is a msgpack string argument. A dictated
  `$(rm -rf ~)` is just text. See [constraints.md](constraints.md).
* **The preview invariant becomes physical.** In nvim the live preview is an
  extmark's virtual text, not buffer content: it *cannot* be saved, yanked,
  or undone into the file. What was previously a discipline enforced by code
  review is now enforced by the data model.

**Rejected: keeping the paste path as a second, selectable sink.** It would
have cost nothing to leave in, and that is the trap — a second sink with no
caller is a code path that rots untested while looking maintained. It is one
`git revert` away if the need returns.

**Historical consequences.** The Qt overlay was disabled; the same indicator (phase,
level meter, live preview) is rendered in the nvim window's winbar, where the
text is about to land. (That code lived in `src/lua/nvim_indicator.lua` and
`src/lua/nvim_rust.lua` at the time; both were merged into
`src/lua/spokenpad.lua` on 2026-09-11.) Preview settings moved
out of `[overlay]` into their own `[preview]` section, because they are no
longer an overlay concern. A fourth thread was added for the RPC connection
(see [architecture.md](architecture.md)). The Rust runtime later replaced this
adapter and removed `pynvim`.

**Measured on this machine, 2026-09-07:** cold open of the dictation window
1.0-1.2 s (the first ever open took 13.5 s while the user's plugin manager
did one-time work); reattach to a running nvim 430 ms; append 19-62 ms. All
of it off the user's latency path — the window is opened on key-down, on its
own thread, while the utterance is still being spoken.

## What a capture tells the user, and what a cancel means

**2026-09-11.** Everything the user has to know about the capture they just
made is one `Notice` owned by `Session`, shown in the winbar — in every phase,
beside the phase label, never in place of the preview — until the next key
press: held too briefly, microphone gap, microphone
unavailable, capture incomplete, nearly silent, preview paused, memory cap. One
at a time, set once by the event that caused it. The daemon no longer keeps its
own re-warning latches, and nothing re-raises a warning on a timer — a message
that reappears every second is noise, and one that never appears at all is how
the memory-cap incident went unnoticed.

Five behaviours were settled with it:

- **Cancel is ignored once the key is released.** `(Transcribing, Cancel)` is a
  no-op. Escape is read from every keyboard regardless of focus, and the audio
  is already captured, so honouring it there destroys finished dictations. It
  still discards while recording.
- **Committed text is never dropped.** Cancelling stops *decoding*. An append
  queued before the cancel is still written, and shutdown drains the editor
  queue with a bounded deadline, logging anything undelivered at error level.
- **A tap under 120 ms is discarded with a notice** rather than silently. The
  threshold is a named constant in `state.rs`, not a setting: it is a property
  of how a key feels, not of a deployment. The recovery WAV is kept anyway.
- **Losing the hotkey keyboard ends the recording by decoding**, never by
  cancelling: `Event::HotkeyLost` fires when the keyboard holding the key
  disappears, and when the last hotkey-capable keyboard disappears during a
  latched recording (which no remaining key could end otherwise).
- **Silence is not decoded.** With a VAD model loaded, a capture — or release
  remainder, or preview snapshot — it finds no speech in produces no segments
  and no recognizer call at all. The old rule handed the whole buffer over
  instead, on the reasoning that "the VAD heard nothing" is not "there is
  nothing to hear" and a silent decode only costs time. It costs more than
  that: observed live, a 0.5 s near-silent press (peak 0.013) was decoded
  whole and Parakeet returned "Thank you.", which landed in the file. A model
  asked to transcribe silence invents speech. The whole-buffer retry stays for
  the different failure it was written for — every chunk of a segmented
  capture decoding to nothing — and now fires only when there was more than
  one chunk. Trade-off: speech the VAD misses entirely is lost, where it used
  to get a second chance at the whole buffer; `vad.threshold` is the knob.
  Without a VAD model nothing changes, because nothing else knows better.

Rejected: a configurable minimum hold; deleting the WAVs of too-short taps; a
periodic "still recording" reminder; keeping the whole-buffer fallback behind a
loudness gate, which would be a second, worse detector beside the one already
loaded.

## Cloud ASR (Gladia) captured as issue #1, rejected as the default

[Issue #1](https://github.com/membranepotential/spokenpad/issues/1) proposes
[Gladia](https://www.gladia.io/) as an alternative or pluggable second ASR
backend, citing per-term weighted custom vocabulary (versus this project's
single global `hotwords_score`), sub-300ms streaming finals, and claimed WER
improvements from its Ursa 2 model.

Rejected as the default for three reasons, per the issue:

- **Conflicts with local processing.** Dictation here is mostly Claude
  prompts — code context, file paths, what's being built — and cloud ASR
  means that content leaves the machine. The project's opening requirement
  was local voice processing.
- **The local path is already fast enough.** The bottleneck that motivated
  leaving Handy was its *streaming architecture* (1.37x real-time, dropped
  audio past 30s), not local ASR generally. One-shot Parakeet already hits
  9.7x real-time with no VRAM cost (see [asr.md](asr.md)).
- **Cost.** $0.61/hr async, $0.75/hr real-time on Gladia's self-serve tier —
  small per-session but ongoing, against a local path that costs nothing
  per-use once the model is downloaded.

The issue leaves open whether a hybrid (local by default, cloud opt-in per
context) is worth it later — noting that reintroduces the privacy question
rather than settling it, so it needs a deliberate decision rather than a
default.

## A notice is a headline plus a detail, and notices are ranked

**2026-09-11.** Two defects with one root: a notice was a single sentence the
editor drew whole, and the last code path to call `Session::notify` won.

At 40 columns the winbar was truncated from the left, so the memory-cap notice
read `<aining audio is in /tmp/capture-example.wav — recover …` — the phase
label and the reason both gone, the part that survived the least useful. A
notice is now a short headline (`memory limit reached`) and a detail sentence,
sent as two fields; the editor always draws the phase and the headline and
appends the detail only when the window has room for all of it, giving up the
meter first. The memory-cap detail names the recovery WAV by file name; the
full path goes to the log, which is not 40 columns wide.

The precedence is `Notice::priority`, applied only in `Session::notify`, which
replaces a notice when the new one ranks at least as high: memory cap > capture
incomplete > microphone unavailable > microphone gap > nearly silent > held too
briefly > preview paused. Before that, a paused preview overwrote a microphone
gap, and a microphone event after the in-memory ceiling overwrote the one
notice that says where the audio went — which a special case in `daemon.rs`
half-repaired at release time and which the ranking now makes structural.

## Hotword biasing built, then deferred: v1 ships plain transcription

`asr.vocabulary` is empty by default and stays that way. The machinery works
and is kept — `modified_beam_search`, the reconstructed `bpe.vocab`, the
`hotwords_score` sweep in `scripts/eval.py` — but no vocabulary is configured,
so nothing biases the decode.

The reason is measured, not theoretical. Hotwords are a thumb on the scale for
words the model *might* have heard, and this project's own eval set contains
the case where that goes wrong. `handy-1787827757.wav` has the speaker saying
"set S E T you corrected to **sed** S E D" and "reset R E S E T you fix to
**Rust** R U S T" — both members of each pair, in one sentence, about each
other. Biasing toward `set` there actively damages the transcript. The tool
this project replaced shipped exactly that kind of correction (as fuzzy
post-replacement rather than decode-time biasing) and it is what produced
`set` → `sed` and `reset` → `rust` in the first place.

Measured on the five verified references with an empty vocabulary: **13.9%**
aggregate WER whole-buffer and **17.6%** through the VAD path the daemon uses,
against Handy's **48.7%** on the same five. Most of that margin is the 37 s
clip Handy discarded outright, which is now scored rather than excluded; on the
four clips Handy completed the two tools are close, some of Handy's showing
coming from the very replacement map that broke set/reset. So the gap hotwords
would close is real but small, and the failure mode they introduce is the one
this project exists to avoid. Current figures live in
[rust.md](rust.md#verification) — re-run `uv run scripts/eval.py` rather than
quoting these.

Deferred rather than dropped: the `--sweep` harness stays, and a vocabulary
can be added later against measurements rather than intuition. Known costs of
shipping without it, all visible in `uv run scripts/eval.py`:

- `mkdir` → `mkir`, `udev` → `Udev` / `U Dev`, `rm -rf` → `RMRF`
- `cd home` → `C D home.` — short commands get spelled out, 100% WER on two
  words, and the one sample where Handy clearly beats us
- `commands` → `comments`, which biasing might fix; `a dir` → `there`, which
  it cannot, since that one is a context error rather than a rare word
