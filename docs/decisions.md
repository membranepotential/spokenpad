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

## Python + `uv` over Rust

Chosen for the implementation language and toolchain. `pyproject.toml`
targets Python 3.13, `mypy --strict`, and `uv` for dependency management
(STATUS.md: "Decided — Python + `uv`"). No performance case demanded Rust:
the ASR bottleneck is the ONNX Runtime, not Python, and int8 CPU decode
already runs at 9.7x real-time (see [asr.md](asr.md)) — well past the
one-shot-per-utterance requirement.

## PySide6 over GTK4

The overlay must be positioned on a specific output and must never take
keyboard focus (see
[constraints.md](constraints.md#overlay-must-not-steal-focus)). This ruled
out GTK4: it has no `Window.move()` or `set_type_hint()` on X11 — verified —
so it cannot position or hint a non-focusable overlay window on this
platform (STATUS.md: "GTK4 has no `move()`/`set_type_hint()` on X11
(verified)"). PySide6 (Qt) supports both, so `overlay.py` (not yet
implemented) will be built on it; `pyside6>=6.11.2` is already a project
dependency (`pyproject.toml`).

## Overlay-only UI with TOML config over a settings GUI

There is no settings application. Configuration is a TOML file parsed once
at startup into frozen dataclasses (`Config.load` in
[`config.py`](../src/voice_kb/config.py)), and the only runtime UI is the
status overlay (`OverlayConfig`). This keeps the UI surface to exactly the
one window that must exist for user feedback during recording, rather than
building a second, larger UI surface purely for configuration.

## Hotwords-only cleanup for v1

Vocabulary correction for v1 is decode-time hotword biasing only (see
[asr.md](asr.md#hotwords-biasing-the-beam-not-rewriting-the-output)) — no LLM
cleanup pass. `TextConfig.replacements` in
[`config.py`](../src/voice_kb/config.py) exists as an escape hatch for exact,
whole-word substitutions, but hotword biasing is the primary mechanism.

An LLM pass is deferred, not ruled out. STATUS.md records this as an open
question: "Does hotword biasing alone close the technical-vocabulary gap
(`dir`→`there`), or is an LLM cleanup pass still needed? Answer with the eval
harness once references exist." That harness needs
`eval-samples/transcripts.json` hand-corrected to ground truth first — it
currently holds Handy's raw (error-including) output, not references
(STATUS.md).

## Live transcript preview as a cosmetic second decode

The overlay now shows a rolling preview of what is being said while the key
is held. This touches the project's third hard constraint, so the change was
a deliberate *rewording* of that constraint rather than a quiet exception to
it (see [constraints.md](constraints.md#one-shot-committed-decode-never-streaming)).

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
  committed text (`_Worker.run_preview` emits to the overlay and nowhere
  else);
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

**Consequences.** The Qt overlay is off by default; the same indicator (phase,
level meter, live preview) is rendered in the nvim window's winbar by
`nvim_indicator.lua`, where the text is about to land. Preview settings moved
out of `[overlay]` into their own `[preview]` section, because they are no
longer an overlay concern. A fourth thread was added for the RPC connection
(see [architecture.md](architecture.md)). `pynvim` is a new dependency.

**Measured on this machine, 2026-09-07:** cold open of the dictation window
1.0-1.2 s (the first ever open took 13.5 s while the user's plugin manager
did one-time work); reattach to a running nvim 430 ms; append 19-62 ms. All
of it off the user's latency path — the window is opened on key-down, on its
own thread, while the utterance is still being spoken.

## Cloud ASR (Gladia) captured as issue #1, rejected as the default

[Issue #1](https://github.com/membranepotential/voice-kb/issues/1) proposes
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

Measured on the five verified references with an empty vocabulary: 13.4% WER
against Handy's 48.7%, though that margin is entirely the 37s clip Handy
discarded. On the four Handy completed it is 18.4% ours to 15.8% theirs — some
of Handy's edge coming from the very replacement map that broke set/reset. So
the gap hotwords would close is real but small, and the failure mode they
introduce is the one this project exists to avoid.

Deferred rather than dropped: the `--sweep` harness stays, and a vocabulary
can be added later against measurements rather than intuition. Known costs of
shipping without it, all visible in `uv run scripts/eval.py`:

- `mkdir` → `mkir`, `udev` → `Udev` / `U Dev`, `rm -rf` → `RMRF`
- `cd home` → `C D home.` — short commands get spelled out, 100% WER on two
  words, and the one sample where Handy clearly beats us
- `commands` → `comments`, which biasing might fix; `a dir` → `there`, which
  it cannot, since that one is a context error rather than a rare word
