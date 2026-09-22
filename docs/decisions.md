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

## An empty speech chunk is decoded again without trailing silence

**2026-09-19.** The user reported words lost at the head or tail of a
dictation. The daemon log showed one cause directly: a preview displayed "What's
your alternative for that?", and the release decode of the same window
returned `""`. The user said it three times before it landed. Replaying the 18
recorded captures with under 3 s of speech, 4 decoded to nothing in the live
presentation, and all 4 decoded correctly without the 1 s of zeros the
recogniser appends. The result is on a knife edge (the Rust CLI and the Python
reference disagreed on the same window), and no presentation is safe on every
clip, so the fix is a retry rather than a new default.

`Recognizer::transcribe` takes a `TrailingSilence` (`Padded` or `Bare`). A
chunk from the segmenter that decodes empty with `Padded` is decoded once more
with `Bare`, in the release decode and in settled progressive commits. Previews
are not retried (they are redrawn every tick), and neither is the no-VAD
path, where nothing claims the audio is speech. The Python reference mirrors
it; with it, the long-standing `cd home` Rust/Python mismatch is gone.

## A post-roll after the key-up

**2026-09-19.** The capture keeps recording for `audio.postroll_ms` (250 ms)
after a decode-ending key event. Replaying the user's 128 recovery WAVs, the
VAD's last speech span ended within 0.1 s of the buffer end in 22 of them:
the key was released mid-word, and the release waited only for the one device
buffer in flight. The wait blocks the event loop, which is simpler than a
draining state, and is bounded three ways: a new key press ends it (so a quick
re-press does not lose its first word into the previous capture), shutdown
ends it, and it gives up after the post-roll plus 100 ms. The key-up that
follows a latched stop does not end it. Whether 250 ms is enough is a live
question; the statistic shows the cut, not its length.

## The whole buffer is copied to the clipboard after a release

**2026-09-19.** After every release the dictation nvim sets its own `+`
register to the whole buffer: every press in the window and any edits made by
hand, trailing blank lines stripped. The daemon sends one `EditorWork::Copy`
per `Finished` result; every `Append` of that utterance is already ahead of it
in the editor queue, so the copy always includes the text just released.

The use that asked for it: a passage is dictated over several presses, read
over in the window, then pasted into another program. Copying by hand meant
focusing a window that is built never to take focus.

This relaxes the 2026-09-07 rule that the clipboard is never touched, and only
this far: the clipboard is a copy, not a delivery path. Nothing is pasted, no
key is sent, and no other window is written to, so none of the failures in
[constraints.md](constraints.md) come back. The register is set through nvim's
own clipboard provider, so the daemon spawns no clipboard process. An empty
buffer leaves the clipboard alone, and a missing provider is a log warning.

**Rejected: copying only the capture just released.** A dictation file is one
message composed over several presses; copying one paragraph would make the
user reassemble it by hand.

## The view is computed to keep the preview on screen (2026-09-21)

The preview hung below the window from the second paragraph on. The old
positioning ran `zz` and then set `skipcol` as if the last paragraph started
at the top of the window, which holds only for the first one. The next update
then mistook the displaced view for a reader who had scrolled away and
stopped following for the rest of the preview. `position_at_end` now walks up
from the last line until the text and the preview fill the window, and sets
`topline` and `skipcol` from that. The dictation window's `scrolloff` is 0,
because a user init with `scrolloff` scrolled the view back. A reader who
scrolls away on purpose still keeps their place
([nvim-window.md](nvim-window.md)).

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

## The Python reference implementation is dropped

**2026-09-21.** `src/spokenpad/` (ASR/VAD/decode/config/text reference),
its pytest suite, and the scripts that depended on it
(`scripts/eval.py`, `verify_rust.py`, `verify_references.py`,
`build_hotwords.py`) are deleted, along with the `uv` project files
(`pyproject.toml`, `uv.lock`, `.python-version`).

Nothing at runtime ever called it: the Rust daemon and `transcribe`
command reimplemented ASR/VAD/decode natively from the start (see
[rust.md](rust.md)), and the Python package's only remaining job was
serving as the other half of `scripts/verify_rust.py`'s differential
check and `scripts/eval.py`'s WER harness. For a project going public,
one language to maintain beats two with a reference/differential
relationship that exists only to validate a port that has been the sole
implementation for weeks. `examples/eval.rs` replaces `eval.py`,
decoding through the same `core::decode::Pipeline` the daemon uses
rather than a second implementation of it — closer to what the harness
is meant to answer, not further from it.

`scripts/build_hotwords.py`'s two jobs — regenerating `bpe.vocab` from
`tokens.txt`, and rendering `asr.vocabulary` to a hotwords file — are
both already covered by `generate_bpe()` and the plain `\n`-joined
vocabulary write in
[`shell/inference.rs`](../src/shell/inference.rs), which `Transcriber::new`
runs on every construction. The only thing lost is the standalone CLI
for dumping those two generated files to disk for manual inspection; it
was explicitly off the hot path and had no other caller.

Dropped along with the differential check itself: the two known,
long-standing Rust/Python mismatches this project carried as accepted,
non-regression noise (`docs/rust.md`'s `um z E T` vs `um Z S E T` on one
clip, and punctuation-only differences in some progressive commits).
There is no longer a second implementation for them to be a mismatch
against.

**The `--whole` (no-VAD) WER moved from the previously reported 13.9% to
18.7%, measured with `examples/eval.rs` on 2026-09-21.** This surfaced
while reproducing the historical numbers for this change, not from an
intentional edit to the decode path (out of scope here; `src/shell/inference.rs`
and `src/config.rs` belong to a parallel workstream). Root-caused by
running the identical audio through the actual `spokenpad transcribe`
binary, not just the new harness:

- `cd-home.wav` (formerly `handy-1787827474.wav`) now decodes to empty
  text with no VAD loaded. The retry-without-trailing-silence fix
  (`TrailingSilence::Bare`, see
  [constraints.md](constraints.md#every-committed-sample-is-decoded-exactly-once-never-streamed))
  only fires on the VAD path by design — `core/decode.rs`:
  `Pipeline::transcribe_speech` skips the retry whenever
  `self.segmenter.is_none()`, since nothing has claimed the window holds
  speech. Two reference tokens at 100% WER move the corpus-level
  aggregate by roughly two points on this five-clip set.
- `shell-commands.wav` (formerly `handy-1787827422.wav`) loses its
  opening sentence ("So, this is a test.") in whole-buffer decode. This
  is the "first words lost on long dictations" failure STATUS.md already
  lists under Known issues as unreproduced on the Rust runtime; this is
  the first time it was reproduced there, incidentally, while chasing
  this number.

Both are pre-existing behaviours of the no-VAD decode path, not artifacts
of the Rust port: the VAD-segmented number (17.6%) reproduces the
historical figure exactly, and neither behaviour is unique to
`examples/eval.rs` — the plain `transcribe` CLI shows the same text.
Whoever next touches `src/shell/inference.rs` or `src/core/decode.rs`
should treat 18.7% as the current `--whole` baseline, not 13.9%, and may
want to open an issue for the reproduced first-words-lost case above.

## One static binary, a model family per config, models under XDG (2026-09-21)

Preparing a public release meant removing three assumptions about the
author's machine.

**sherpa-onnx is linked statically.** The crates' `static` feature links
sherpa-onnx and onnxruntime into the executable, so `spokenpad` is one file
that runs from any directory; `build.rs` and its `$ORIGIN` rpath are gone.
The binary needs only system libraries (libstdc++, libc, PortAudio). Measured
on one machine: 39 MB for the binary against 5.3 MB plus 36 MB of shared
libraries before; the same transcripts on every eval clip, and no slower
(5.2 s against 5.7 s median for the 37 s clip, model load included).
**Rejected: keeping `shared`.** It saves nothing on disk, and every install
path would have to carry two libraries beside the binary.

**Models live under `$XDG_DATA_HOME/spokenpad/models`.** The defaults for
`asr.model_dir` and `vad.model` used to be relative to the working directory,
which tied the service to a checkout (`WorkingDirectory=`). They are now
absolute paths under the XDG data home (`~/.local/share` if unset), which
`scripts/fetch-models.sh` fills after checking each file's size and sha256. A
relative path in a config file still resolves against that file's directory.
`scripts/install.sh` copies the binary to `~/.local/bin` and the user unit to
`~/.config/systemd/user`; the unit is `WantedBy=graphical-session.target`
instead of the author's own `i3-session.target`. Both scripts are POSIX shell
and replace their Python predecessors.

**`asr.family` selects the model family.** `parakeet` (the default, a NeMo
transducer), `whisper` and `sense_voice` map to sherpa-onnx's offline model
configs; each file is found in `model_dir` by its role. `[asr]` stays one flat
table, so existing configs keep working, and is parsed into a typed
`Model`: hotwords live inside `Decoding::ModifiedBeamSearch`, which only
`Model::Parakeet` has, and a key another family cannot use is rejected with a
message rather than ignored. Whisper tiny.en scored 23.0% WER and SenseVoice
29.4% on the five eval clips, against Parakeet's 17.6% ([asr.md](asr.md)).
Whisper reads at most 30 s and sherpa-onnx drops the rest silently, while VAD
windows have no length limit, so the adapter cuts longer Whisper windows into
equal pieces. **Rejected: Moonshine.** sherpa-onnx 1.13.6 fails on Moonshine
v2 windows over about ten seconds and returns nothing.
**Rejected: explicit file paths per model in the config.** Every sherpa-onnx
release names its files by role, so a directory is enough.

## The dictation editor: attach mode by default, managed on i3 and sway (2026-09-21)

Until now the dictation window worked only with alacritty under i3 on X11:
the daemon spawned `alacritty --class Floating,spokenpad -e nvim`, refused
any other terminal argv, proved the `no_focus` rule by running
`i3-msg -t get_config`, and read the layout from `xrandr`. For a public
release that excluded every Wayland desktop and most X11 ones.

`nvim.mode` now selects who opens the editor:

- **`attach` (the default).** The user runs `spokenpad editor` in any
  terminal, which becomes nvim on the dictation socket with spokenpad's
  ownership marker; the daemon adopts it on the next key-down. It opens no
  window, so "no window spokenpad opens may take focus" holds by
  construction, on any desktop and with nothing to install.
- **`managed`.** Today's behaviour, generalised: five terminals from a typed
  table (alacritty, kitty, foot, wezterm, ghostty; `headless` for no window)
  instead of one trusted argv, and i3 or sway spoken to over their shared IPC
  protocol instead of `i3-msg` and `xrandr`. The rule is proven for the X11
  instance on i3, and for the `app_id` and the instance under sway, since a
  terminal there picks Wayland or Xwayland by itself. (Superseded the
  same day, see the next entry: sway `include` lines were first followed on
  disk.)

Attach is the default because it is the only mode that works for every new
user, and the one that cannot fail the focus constraint at all. The author's
own i3 setup opts into `managed` explicitly.

**Dictating with no editor open** was the new failure to design for. Holding
the text in memory until an editor appears would lose it to a daemon restart;
refusing to record would make the key dead with no explanation. So the daemon
writes each commit straight into a dictation file with the editor's own
paragraph rule, records it as the pending passage beside the socket, and the
next editor opens on that file. Decoding is untouched — the file is only a
different sink — and one desktop notification says where the text went.

Rejected: a headless nvim owned by the daemon with the user attaching a UI
through `nvim --remote-ui`. It would reuse the RPC path unchanged, but it
makes the daemon a precondition for opening the editor at all, and the
remote UI still has rough edges (clipboard, `:q` quitting the server) that a
plain nvim does not.

## Review fixes to the editor modes (2026-09-21)

A review of the new modes found four ways to break the constraints; each is
fixed with a test that failed before.

- **Empty workspace.** i3 and sway focus the first window on a workspace
  whatever `no_focus` says, so a proven rule was not proof. Managed mode now
  also reads the tree and does not spawn while the focused workspace is
  empty; the text goes to the pending passage.
- **Only loaded config counts.** sway's `GET_CONFIG` returns the main file
  only. Following `include` lines on disk could count a rule sway never
  loaded (no reload yet, or variables that differ between sway's environment
  and the daemon's). Now only the text the window manager returns counts: on
  sway the rules belong in the main config file.
- **An editor that exits mid-dictation.** An append that could not be sent
  now goes to the pending passage; an append with a lost reply still does
  not, since it may have landed. A later commit of the same utterance
  continues its line only where the previous one went, never across files.
- **No writes behind an editor's back.** `spokenpad editor` takes the pending
  passage under a lock before nvim reads it, so the daemon never replaces a
  file an editor holds (nvim would block on "file changed since reading").
  Reloading at adopt was rejected: `:checktime` resets nvim's change check,
  and unsaved edits could then silently overwrite the direct write.
- `hotwords_score` with `greedy_search` is now an error instead of ignored.

## Control by socket, not by reading the keyboard

2026-09-21. spokenpad no longer reads `/dev/input`. It watched every keyboard
through evdev, read-only, for its hotkey (F16), Shift as the latch modifier
and Escape as the cancel key. That needed the `input` group, which can read
every keystroke on the machine, and it made spokenpad look like it owned a
key it only observed.

Now the daemon listens on a control socket (`$XDG_RUNTIME_DIR/spokenpad.sock`,
mode 0600) and the CLI talks to it: `spokenpad start`, `stop`, `toggle` and
`cancel` each send one line and wait for `ok`. The user binds keys to them in
the window manager or desktop, which owns the key. Removed with it: the
`evdev` crate, `[hotkey]` (an old config naming it is rejected with a message
pointing to the bindings), device scanning and hotplug, the rule that losing
the hotkey keyboard ends a recording by decoding it, and exit code 3. Nothing
else changed: pre-roll, post-roll, progressive commit, the 120 ms tap
discard, notices, latch, and cancel only while recording.

A binding cannot tell the daemon whether the key is really held, so the state
machine infers it from the requests and their arrival times:

- **Auto-repeat.** A held key re-fires its binding. i3 enables X11's
  detectable auto-repeat, so it re-sends only `start`; other X11 clients see
  `stop`, `start` pairs every ~40 ms, and the two processes of a pair reach
  the daemon in either order. A `start` while held is ignored. A `stop` does
  not end a held capture at once: a `start` within 150 ms means the key never
  came up, and only the clock passing that window ends the capture, released
  at the `stop`. 150 ms covers three intervals at X11's default 25 Hz rate
  plus process-spawn jitter; the post-roll counts from the `stop`, so with the
  default 250 ms post-roll the window delays nothing.
- **Latch.** `toggle` starts a latched capture; the next `start` or `toggle`
  ends it, released at that press. Every `stop` is ignored while latched,
  because Shift and the key come up in either order (i3 matches
  `--release F16` only if the press matched it too, or Shift came up first).
  A press within 150 ms of the previous one is that key's auto-repeat.
  `toggle` while held latches the running capture.
- **After the end.** A capture that is decoding does not block a new one: a
  `start` or `toggle` begins the next capture and cuts the post-roll short,
  as a quick re-press did before. A `stop` or `cancel` then does nothing.

Requests are stamped by the daemon when it reads them, never by the client,
and the loop feeds the state machine the clock at each request's stamp
before the request itself, so a window that closed before a request arrived
is closed before it is read.

**Known limit.** The first auto-repeat of a held latch key comes after the
repeat delay (660 ms by default on X11) and looks exactly like a second
press, so holding Shift+F16 that long ends the latched capture; holding the
key that ends a latched capture that long starts a new push-to-talk capture.
The README therefore says to turn repeat off for the key (`xset -r 194` on
X11, `--no-repeat` on sway; Hyprland binds do not repeat) or to tap it.

Rejected: tracking the key as down from each press until its `stop`, which
would make repeats exact on i3. Desktops such as GNOME and KDE run a shortcut
only on press, so the `stop` may never come, and the next real press would be
swallowed as a repeat. Rejected: a client-supplied timestamp; the two
processes of a pair race for the CPU before they can read the clock, so it
orders them no better than arrival does. Rejected: queueing requests while
the model loads; the socket is bound last, and until then the CLI says no
daemon is listening. (Reversed on 2026-09-22 for socket activation: see
[Presses are taken before the model is ready](#presses-are-taken-before-the-model-is-ready-2026-09-22).)

## Model download moves into the binary (2026-09-21)

**Downloading the default models is now `spokenpad fetch-models`, not
`scripts/fetch-models.sh`.** The shell script duplicated logic the binary
already needed to state precisely (where a role's files live, what the
default `model_dir` is) and could drift from it silently; the pinned
manifest — six files' URLs, sizes and sha256, unchanged from the script's
own values — now lives once, as data, in `core/models.rs`, with the
download and verification in `shell/models.rs` (network and files) kept
apart from that data on purpose: the manifest and the pure comparisons
(`files_to_ensure`, `matches`) are usable, and unit-tested, without a
network. A file is written to `<name>.part` and renamed into place only
after its size and sha256 match the pin, so a killed download or a
corrupted mirror never leaves behind a file spokenpad would go on to load.

**The daemon, `check` and `transcribe` download automatically instead of
refusing with a pointer to a command.** The user asked for automatic setup
on first launch, and it can be made safe rather than merely convenient: the
only files ever downloaded this way are the default Parakeet weights and
the default Silero VAD, only when `asr`/`vad` are still pointed at their
defaults (`core::models::files_to_ensure`), from URLs that are fixed
literals never built from configuration. A user-configured `model_dir` or
another `asr.family` is never touched — its absence stays the same "missing
ASR model" error as before, exit code 2. A download that cannot complete
(offline, DNS, a corrupted mirror) is logged and falls through to that same
error rather than a new failure path: one tested message for "the model
isn't there," however it got that way. This is also now the *only* network
access anywhere in spokenpad; see
[constraints.md](constraints.md#the-only-network-access-is-the-pinned-model-download).

**HTTP client: `ureq` 3.4 with `rustls`, not `native-tls`/OpenSSL.**
Verified on crates.io/docs.rs rather than assumed: `ureq` 3.4.2 is a small,
maintained, pure-Rust client whose `rustls` feature (the default TLS
backend for its top-level convenience calls) links no system TLS library,
keeping the static binary's `ldd` output unchanged. `gzip` is off
(`default-features = false`) since the default model files are already
compressed binary weights, not compressible text. Hashing is `sha2` 0.11,
the same RustCrypto crate family already vetted for this project's
dependency tree.

**`scripts/fetch-models.sh` is deleted**, and `scripts/install.sh`'s closing
hints point at the subcommand instead.

## The clipboard copy becomes opt-in, off by default (2026-09-21)

The 2026-09-19 clipboard copy (previous entry) ran unconditionally after
every release. The author found this made every dictation write to the
system clipboard whether wanted or not, silently overwriting whatever was
there before — a side effect worth choosing, not assuming. `nvim.copy_to_clipboard`
(default `false`) now gates it: when off, the daemon never even sends the
`copy_buffer` request to the editor, rather than sending it and discarding
the result, so a missing clipboard provider is not probed on every release
either. The mechanism `copy_buffer` implements in `spokenpad.lua` is
unchanged; only whether the daemon ever asks for it moved.

## Greedy decoding by default: beam search drops speech (2026-09-21)

The user lost the end of a dictation: "…evaluate if we should change it"
never arrived. The daemon log showed the release decode of the last 10.6 s
(3.5 s of clear speech in two runs) returning `""`, and the retry without
trailing silence returning `""` too. The recovery WAV decoded whole gave the
sentence, so the audio was fine, and a live replay of the capture (a tick
every 1.1 s, then the release) reproduced the empty tail exactly.

Sweeping the window over that tail: `modified_beam_search` produced text for
1 of 9 start offsets, often an invented "Yeah."; `greedy_search` for 9 of 9
without padding and 7 of 9 with the 1 s of zeros the recogniser appends.
This is upstream [k2-fsa/sherpa-onnx#3267](https://github.com/k2-fsa/sherpa-onnx/issues/3267)
(open): beam search on NeMo TDT returns `""` or "Yeah." about one time in
five, while greedy works. Replaying all 170 recovery captures through the
VAD path, beam search left 19 speech chunks empty (17 rescued by the retry,
2 lost) and greedy 4 (all rescued), and greedy produced 121 more words.
Confirmed the same day over the whole local corpus with references (181
captures, 75 minutes, 6832 reference words): 19 empty chunks against 5, 2
lost against 0, 57 reference words lost off the ends of captures against 5,
three "Yeah." commits against none, and 114 more words — while the word error
rate cannot tell the two apart (−0.32 points, 95% bootstrap interval [−1.22,
+0.47]). See
[the experiment](experiments/2026-09-21-greedy-vs-beam-corpus.md).

So Parakeet decodes greedily unless hotwords are asked for: a non-empty
`vocabulary` (or a `hotwords_score`) with no explicit `decoding` still
selects beam search, since sherpa-onnx has hotwords only there. The
"knife edge" of 2026-09-19 (entry above) was mostly this bug; the retry stays
as a cheap guard. The 1 s of zero padding also hurt greedy on the sweep, and
the corpus replay it was left open for has since settled it the other way: the
padding stays, because without it `Padded` and `Bare` are the same input, so
the retry is the same decode and every chunk that decodes empty is lost —
3 of 3 for greedy, 15 of 15 for beam, against 0 of 5 and 2 of 19 with it
([the experiment](experiments/2026-09-21-trailing-silence-padding.md)).

The same investigation found that the `shell-commands` eval clip does not
contain its reference's first sentence ("So, this is a test."): Parakeet,
Whisper and SenseVoice all hear the clip start at "Let's try". The recording
tool of the time had cut it; the reference is corrected, and the STATUS note
about first words lost was largely this artifact.

## Constant memory while recording (2026-09-21)

A running capture kept every sample in memory: about 230 MB per hour, which is
why `MAX_UTTERANCE_SECONDS` capped one at 3600 seconds and why a 13m41s hold in
2026-09-08 lost its tail. The audio was already useless by then — a tick
decodes from the committed offset and so does the release, and a window's lead
padding is clamped to the start of the slice it was cut from, so nothing can
read a sample before that offset.

So the capture buffer now holds `committed offset .. last frame` and the event
loop drops whole device buffers behind it as the offset moves
([progressive-commit.md](progressive-commit.md#what-is-kept-in-memory)). Half
an hour of latched capture costs 1.7 MiB instead of 116.2 MiB
([experiment](experiments/2026-09-21-constant-ram-recording.md)).

Two things had to change for the offset to keep moving, and both are the same
rule applied where it was missing:

- A pause longer than the split threshold closed a pending chunk only when a
  later span arrived to close it. It now closes one that is simply at the end
  of the slice. The window is the one the release would have decoded anyway;
  only the moment of the decode moves earlier.
- Silence the VAD heard nothing in advanced nothing at all, so a forgotten
  latch grew for as long as it ran. The offset now advances to one split
  threshold before the end of such a slice and commits empty text: no decode,
  because there is no speech, and the keep-back leaves a later window its lead
  padding. The advance is a whole number of detector windows, so the audio that
  stays is covered by exactly the windows it was before — and Silero, measured
  against the real model, finds the same spans after four seconds of silence as
  after a minute.

The preview pause was a one-way door: it stopped the whole tick, and only a
tick can commit, so a tail that once passed `preview.max_seconds` stayed past
it for the rest of the capture. A paused preview now skips the cosmetic decode
of the open tail and keeps committing settled chunks, which is what makes the
documented "resume by themselves" true, and what keeps the memory bound from
depending on the recognizer keeping up.

Rejected: dropping the cap. Without a VAD model nothing settles, nothing can be
dropped, and a forgotten capture would take the machine's memory. It stays, now
bounding the *retained* window rather than the length of a capture, and is
final for a capture once it drops audio — accepting again after a hole would
splice two moments that were never spoken together. No new setting: the
keep-back is the split threshold the merge policy already computes.

Not done: a capture still has no length limit of its own. The recovery WAV's
RIFF sizes overflow at about 37 hours and `spokenpad transcribe` refuses one
over 4 hours, so a latch forgotten for a day is still a way to lose a
recording. An automatic stop belongs to the state machine, not here.

## Lead padding stops at the committed offset (2026-09-21)

A chunk's window began `pad_seconds` (or `edge_pad_seconds`) before its first
speech, with nothing stopping that reach at the previous chunk's speech end.
When the pause between two chunks was shorter than the padding, the second
window began inside speech the first chunk had already committed and appended,
and the recognizer was given those words a second time. Over a replay of 400
generated captures at three tick cadences, 8 committed windows in 7 captures
began inside committed speech, the worst by 0.370 s — long enough for a short
word to be written twice.

It needed two chunks in one slice, so it showed up wherever a chunk closes with
another one behind it: on a long gap closing it, on the speech target being
reached, on the release decoding a remainder with several chunks in it, and —
new that day — on trailing silence closing it at a tick. The trigger was new,
the defect was not.

So lead padding is now clamped to the previous chunk's `speech_end`, which is
exactly where committing that chunk leaves the offset. `merge_spans` is pure
and knows nothing about the worker, so the clamp is expressed in its own terms;
that it equals the committed offset is what makes it correct on every path, and
it makes the tick and the release cut the same window for the same chunk. The
first chunk of a slice needs no clamp: the slice starts at the committed offset
already. A chunk after a silence commit may still pad back into that silence,
which is not committed speech and is what padding is for.

This changes windows relative to the previous behaviour, not only in the case
that prompted it: any chunk that follows a close with a pause shorter than its
padding now gets a shorter lead. It removes audio from the recognizer's input,
so it can move a transcript.

**Measured on the corpus, and kept.** Reverting the clamp in a scratch build and
replaying all 181 captures gives output that is identical capture by capture —
same words, same error rate, same counts — so at the 1.1 s tick the daemon uses
it never fires at all. At a 9 s tick, chosen to provoke it, it changes one
capture of 181, and that capture scores the same either way. It costs nothing
and removes a way for a word to be written twice, so there is nothing to weigh
against it:
[the experiment](experiments/2026-09-21-lead-padding-clamp-corpus.md).

Not fixed here: the same reach exists on the trailing side, where a chunk's
`pad_seconds` can extend into the *next* chunk's speech (worst case 0.435 s over
the same 400 captures, 201 windows affected). It predates this work and is
bounded by `pad_seconds`. The corpus now measures what it costs in the file:
4 chunk seams wrote a word twice, 6 words over 333 committed chunks, and the
clamped and unclamped builds show the same four — so all of them are the
trailing side. Small, and still waiting on a measurement of what clamping it
would cost before it is changed.

## sherpa-onnx moves to 1.13.8 (2026-09-21)

The two `=1.13.6` pins and the version assert in `Transcriber::new` now read
`1.13.8`. Nothing in 1.13.7 or 1.13.8 touches the offline NeMo transducer,
the NeMo TDT decoders, Silero VAD, Whisper or SenseVoice; the only change
that could move a number is onnxruntime 1.27 → 1.28.2, and it moved none. The
five eval clips score the same per clip and in aggregate on both versions
(greedy 18.7% VAD / 14.3% `--whole`, beam 15.4%), the Rust crates' FFI struct
definitions are byte-identical outside speaker diarization, and `ldd` still
shows no sherpa or onnxruntime shared library beside the binary.

The reason to take it is Qwen3-ASR: 1.13.7 carries the fix for that model
hallucinating text on silent audio when hotwords or a language are set
(#3907), and the centered-STFT feature alignment (#3873). Both are
prerequisites for evaluating it as a candidate with working vocabulary
biasing.

It does **not** fix `modified_beam_search` on Parakeet TDT. The same clear
10.6 s tail that decoded to `""` on 1.13.6 still decodes to `""` on 1.13.8,
with and without trailing silence, while greedy returns the words. The
decision above stands: greedy by default, and no hotwords on the current
model. Measurements in
[experiments/2026-09-21-sherpa-1.13.8-upgrade.md](experiments/2026-09-21-sherpa-1.13.8-upgrade.md).

## spokenpad draws its own dictation window (`nvim.mode = "pane"`, 2026-09-21)

Managed mode buys one thing — a window that appears beside what you are
reading — and charges a lot for it. It needs i3 or sway, a `no_focus` rule the
user installs, a terminal from a table spokenpad has to know how to name a
window in, and it still cannot open on an empty workspace, because both window
managers focus the first window on one whatever the rule says. Every one of
those costs exists because the window belongs to someone else.

So spokenpad opens its own. An X11 window with four properties set before it
is first mapped, `nvim --embed` inside it, and a renderer that turns Neovim's
`ext_linegrid` redraw stream into pixels. `_NET_WM_USER_TIME = 0` is the whole
of the focus guarantee on i3, and unlike `no_focus` it holds on an empty
workspace; `_NET_WM_WINDOW_TYPE_UTILITY` floats it, and is what refuses focus
on the window managers that ignore user time. No rule, no terminal table, no
IPC proof, no empty-workspace gap — and it works under a window manager
spokenpad has never heard of, because it asks nothing of one. Measured, with
an ablation, in
[2026-09-21-own-window-p0-properties.md](experiments/2026-09-21-own-window-p0-properties.md)
and [2026-09-21-own-window-p1-renderer.md](experiments/2026-09-21-own-window-p1-renderer.md).

What it costs, stated plainly:

- **The window cannot outlive the daemon.** It is a thread of that process and
  its editor is a child; a managed terminal is neither. Nothing is lost when
  the daemon restarts — the file is written after every utterance and the pane
  writes every modified buffer before it quits — but the window goes, and the
  next dictation opens a new one. Managed mode stays for people who want the
  other trade.
- **No input method.** Dead keys and Compose work, because spokenpad reads the
  layout the X server has loaded; IBus and Fcitx are not clients of a
  hand-rolled window. Fine for German and English, a gap for CJK.
- **Text rendering is spokenpad's problem now**, and a terminal has had
  decades of work on it. Bold is thickened by hand where fontconfig has no
  bold face, italic falls back to plain, and a character the family does not
  cover is fetched from whichever font fontconfig names for it — but never
  once per character: a loaded face that covers it comes first, an answer
  stands for a whole 256-character page of Unicode, a frame stops asking
  after 50 ms and repaints, and an `fc-match` that hangs is killed after
  250 ms. A page of two hundred ideographs went from eight seconds of frozen
  window to one frame.
- **Verified on i3 only.** The rest is read from source.
- **Closing a pane may not lose typed text, and that is a budget.** The
  editor dies with the window, so the pane writes every modified buffer
  first; and because the daemon abandons that thread after its shutdown
  grace, the write and the quit each get a share of that grace, checked
  against it at compile time. A buffer Neovim refuses to write is kept as
  `<file>.unsaved` instead of being thrown away with `qall!` and a log line.

Rejected on the way:

- **winit** cannot set `WM_HINTS input` or `_NET_WM_USER_TIME`, which are the
  two properties the whole approach rests on, and has no layer-shell either.
- **GTK4** would bring Pango, IBus and layer-shell, and with them a toolkit
  main loop that wants its own thread, a large runtime dependency and an X11
  backend GTK is moving away from. It is the right answer if input methods
  ever become a requirement, and the wrong one for a pane of monospace text in
  a daemon that values a small core.
- **nvim-rs** needs tokio or async-std and a second msgpack codec beside the
  one this project already has. The redraw stream is about three hundred lines
  to decode against the existing one.
- **fontdb** answers "which file is monospace?" by parsing five thousand faces
  at every start, a question fontconfig has already answered with the user's
  own rules applied. `fc-match` is one short-lived process.
- **Linking libxcb and libxkbcommon.** The default mode opens no window, and a
  binary that listed those libraries as needed would refuse to *start* on a
  machine without them, for a feature that user never selected. They are
  opened with `dlopen` when a pane opens, and a missing one is an error naming
  the package.

The default stays `attach`. Whether `pane` should replace `managed` is a
question for after it has been used on a real desktop.

## A capture that nobody ends, ends (2026-09-21)

Dropping committed audio ([above](#constant-memory-while-recording-2026-09-21))
left a capture with no length of its own. `spokenpad toggle` with nobody to
toggle again recorded until `hound`'s 32-bit RIFF length overflowed at 4 GiB —
about 37 hours — and `read_capture` already refused a recording over 4 hours,
so the safety net was gone long before the daemon noticed. The old
`MAX_UTTERANCE_SECONDS` bounded only the retained window, and `Session::cap`
marked the utterance released without telling the state machine, so a capped
capture kept writing to disk with nothing reading it.

So `core/state.rs` ends a capture on the clock. Three rules, one shape: each
produces the same `Command::Decode` a key release produces, carrying a `Cause`,
so the tail is decoded, every sample already captured is kept, the recorder
stops with the capture, and one ranked notice says which rule it was.

- **`Cause::Silence`** — a latched capture that has heard no speech for
  `capture.silence_timeout_s` (default 300 s). "Heard speech" is text the
  recognizer produced, from a settled commit or a live preview, which
  `Session` turns into `Event::Speech`; an empty commit is settled silence and
  an empty preview is a quiet tail, so neither counts. The timeout therefore
  runs from the last word, not the last key press: a thinking pause is not
  silence, and five minutes means five minutes of nothing said.
- **`Cause::Length`** — `MAX_CAPTURE` (4 h), for every capture. Not a setting:
  it is what keeps the recovery WAV readable, so `shell/recorder.rs` derives
  its own `MAX_RECOVERY_SECONDS` from it plus the widest pre-roll and
  post-roll `config` allows, and a unit test asserts the margin covers them.
  The two cannot drift apart.
- **`Cause::Memory`** — the in-memory ceiling, which only the shell can see,
  so it arrives as `Event::Exhausted` rather than as a rule over the clock.
  This is the pre-existing gap closed: the ceiling now ends the capture
  instead of only stopping its decoding.

Decisions inside that:

- **No key down, for silence.** A held push-to-talk key re-fires its binding
  every few tens of milliseconds, so ending its capture would start the next
  one 40 ms later — an endless chain of five-minute captures rather than a
  fix. "Latched" is not the test that catches this, because a latch made by
  holding Shift and the key *also* has a key down: its auto-repeat fires the
  toggle binding, every repeat lands inside `REPEAT_WINDOW` and refreshes
  `last_press`, and such a capture would be ended and immediately replaced by
  the next repeat — exactly the chain. So the rule skips a latch whose
  `last_press` is younger than `KEY_SETTLED` (1 s, well above the 20–40 ms
  repeat interval). What is left is a latch with nothing pressing it, which
  is the forgotten capture this is for. A key left under a book is bounded by
  the length limit whichever binding it is on, and nothing can do better:
  spokenpad reads no input device, so it cannot tell a stuck key from a held
  one.
- **Two keys that only make sense together.** Only a preview tick produces
  text, so the earliest a capture can report speech is one tick after the
  press. `preview.interval_ms` (up to an hour) and `capture.silence_timeout_s`
  had their own ranges and no relation, so `interval_ms = 600000` with the
  default 300 s timeout would have ended a capture mid-sentence every five
  minutes, having given it no chance to say anything. `Config::validate` now
  requires the timeout to be at least two ticks, naming both keys, and only
  where the tick exists — with `preview.enabled = false` or
  `vad.enabled = false` the silence rule is off and the pair says nothing
  about each other.
- **The speech clock is stamped on arrival, not on the audio.** `Session::heard`
  uses the moment the result came back, not the position of the audio it
  describes. Stamping by audio position is the more accurate number and the
  more dangerous one: a worker that falls behind by more than the timeout
  would leave `last_speech` permanently in the past and end a capture the
  user is still talking into, because the words had not been decoded yet.
  Arrival time can only delay the stop, by at most one decode.
- **A stop, not a cancel.** Everything spoken is kept and appended, as after
  any release. Only a key press can be read as a tap: `end` applies the
  `MINIMUM_HOLD` rule to `Cause::KeyPress` alone, because throwing away what
  the other three end would lose dictation rather than a slip of the finger.
- **The late key is harmless.** After an auto-stop the session is
  `Transcribing` and then `Idle`, where a `stop` and a `cancel` are already
  no-ops; the notice survives both and only the next press clears it. That
  press starts a fresh capture, which is what `toggle` from `Idle` has always
  meant, and the README says so where a user reads about the latch.
- **The notice is still cleared by that press**, rather than surviving into
  the new capture until its first text. It is there for as long as the user
  is away, which is the case it was written for; keeping it past the press
  would mean a warning about a finished capture standing beside a live "REC",
  and it would break "exactly one notice per capture, cleared by the next key
  press" — an invariant `Session::notify` applies in one place and four
  documents state. Not worth a second slot and a per-notice lifetime.
- **Off where there is no silence to measure.** With no VAD model,
  `vad.enabled = false` or `preview.enabled = false`, nothing produces text
  before the release, so `Event::Speech` never arrives and every latch would
  end at the timeout. `serve` passes `None` in exactly those cases — the same
  condition that already turns the progressive tick off — and the length limit
  and the ceiling bound the capture there.
- **One setting, and only one.** Long dictation with pauses over five minutes
  is plausible, so the silence timeout is configurable (0 turns it off, and
  the limits still apply). The other two limits are not preferences: one keeps
  a file readable, the other bounds this machine's RAM.

Notice ranking: news about audio that was lost still outranks news about a
capture that ended cleanly, so `LengthLimit` and `SilenceTimeout` sit below
the microphone notices and above `NearlySilent`. Nothing is lost when they
fire, and the log line names the cause either way.

## Iterate on a 28-capture dev subset, gate on the whole corpus (2026-09-22)

Whole-corpus runs took 8–20 minutes each and were run for questions a small
set answers. `examples/corpus.rs` now takes `--subset NAME` (a list in the
corpus's `subsets/`) or `--ids FILE`, and the dataset carries `subsets/dev.txt`:
28 captures, 17.8 minutes, holding every chunk failure today's `main` shows on
the corpus, the two captures where beam search loses speech, and some
coverage. It replays in about two minutes and reproduced the recorded run of
`main` on those captures word for word
([experiment](experiments/2026-09-22-dev-subset.md)).

- **The subset is for counts, the corpus for WER.** It was picked for its
  failures, so its WER (14.2%) is not the corpus's (10.9%), and between greedy
  and beam search it points the other way. A change is judged on the full
  corpus before it ships.
- **Least private first.** The captures were rated for private content before
  they were chosen; the subset holds one capture with identifying content, kept
  because nothing else carries its failure. Ratings and reasons stay in the
  git-ignored dataset; the tracked README lists ids and counts only.
- **Mixed-language captures are flagged, not dropped.** The Gladia references
  hold one language per capture; 15 captures mix both, and their reference is
  wrong about the minority language (21.4% WER against 9.9% on the rest). The
  dataset's hand-checked `spoken` field marks them, the report prints WER per
  group, and `--spoken en --spoken de` scores without them. Dataset version
  `2026-09-21.1`: same audio and references, so older numbers still compare.

## The pane's font size is in points, measured as Alacritty measures (2026-09-22)

On a 192 dpi display the pane's text was half the size of Alacritty's at the
same setting: `nvim.font_size` was pixels (default 16) and the pane never
asked the display's resolution. The earlier reason for pixels, that the pane
"has no display resolution to convert from", was wrong: every X11 toolkit reads
`Xft.dpi`, and Alacritty does through winit.

- **`nvim.font_size` is now points and means what Alacritty's `font.size`
  means.** `core/font.rs` repeats Alacritty 0.17's arithmetic, crossfont's
  and FreeType's rounding included, and matches all 304 cells measured from
  Alacritty's own window over four fonts, five resolutions and three hinting
  modes ([experiment](experiments/2026-09-22-pane-hidpi.md)). The default is
  Alacritty's, 11.25 pt; valid sizes are 1 to 200. The key changed unit
  before any release, so there is no migration: a config with
  `font_size = 16` now asks for 16 pt.
- **The resolution is the X resource `Xft.dpi`, looked up when a pane opens
  through the same x11rb resource database winit uses** (the first screen's
  `RESOURCE_MANAGER`, else `~/.Xresources` or `~/.Xdefaults`; wildcards and
  last-entry-wins included), and 96 when unset or unusable. winit also reads
  XSETTINGS first and RandR's physical size last; those are not reproduced,
  so matching Alacritty requires `Xft.dpi` to be set, which the README and
  `config.example.toml` state. `spokenpad check` prints the resolution the
  pane would use and why.
- **Everything else in pixels follows the font rather than a scale factor**:
  baseline, underline and strikeout as Alacritty's `create_rect` places them,
  the unfocused cursor's outline at Alacritty's 0.15 of a cell, and underline
  patterns measured in the underline's thickness. The window's size already
  came from `nvim.window_fraction` and whole cells.
- **Rejected: a scale factor on top of a pixel size.** It would be close and
  never equal, because the cell width is an advance rounded at the scaled
  size, not a scaled rounded advance, and the user's question was "the same
  as Alacritty".
- `tests/pane_hidpi.rs` sets `Xft.dpi` on its Xvfb and compares the pane's
  cells with a live Alacritty's at 96, 144 and 192 dpi; every earlier pane
  test ran without `Xft.dpi`, where 16 px is exactly 12 pt, which is why none
  noticed.

## Pane mode verified on Openbox and KWin, unsupported on sway (2026-09-22)

Pane mode is meant to become the default, and until now its focus guarantee
had been run on i3 only. The same story now runs headless on the other window
managers installed here, with the pane opened as the daemon opens it and the
focus sampled every 5 ms
([experiment](experiments/2026-09-22-pane-focus-other-wms.md)).

- **Openbox 3.6.1 and KWin 6.7.5 (Wayland, with Xwayland) never focus the
  pane**, on map, while it redraws, for the next passage's pane, and on an
  empty desktop. KWin holds at its default focus stealing prevention and with
  it turned off. `tests/pane_focus_wms.rs` asserts it.
- **sway 1.12 focuses the pane every time it maps, so pane mode is
  unsupported on sway.** sway's `view_map` gives focus to any new window
  unless a user's `no_focus` rule matches it or its ICCCM input model is "No
  Input". It reads neither `_NET_WM_USER_TIME` nor the window type, so no
  property the pane sets can stop it, and the no-focus-call rule rules out
  anything else. A test pins this and fails the day it stops being true.
- **Not shipped: `WM_HINTS input = False` for sway.** It keeps sway from
  focusing the pane on map, and a click then focuses it with the X input
  focus left at `PointerRoot`. Whether real keys from sway's seat then reach
  the pane is unmeasured. Openbox and KWin did not select an `input = False`
  window the same way in every run. It would also have to apply to sway
  only. It stays an open option, not a fix.
- **Not shipped: `_NET_WM_STATE_ABOVE`.** KWin stacks the unfocused pane
  below the focused window. `_NET_WM_STATE_ABOVE` set before the map puts it
  on top without focusing it, on KWin and Openbox. That changes stacking,
  not focus, and is left for a separate decision.
- **The harness** (`tests/harness/desktops.rs`) runs each window manager in
  an empty environment with a private `HOME`, `XDG_CONFIG_HOME`,
  `XDG_RUNTIME_DIR` and, for KWin, session bus, and with a generated
  configuration. It stops each compositor's Xwayland before the compositor,
  because Xwayland runs with `-terminate` and otherwise outlived sway by
  several seconds. KWin 6 has no `kwin_x11` in the `kwin` package, so X11
  KWin is not covered.

## Presses are taken before the model is ready (2026-09-22)

The daemon now listens first and loads its models last. `run` takes the lock,
serves the control socket, registers signals and opens PortAudio; the models
are built on the inference thread by a loader, while the loop already takes
presses. This reverses "the socket is bound last" from
[Control by socket](#control-by-socket-not-by-reading-the-keyboard).

- **Why.** Under socket activation (`spokenpad.socket`) the press that starts
  the daemon is waiting on the socket. With the old order it waited for the
  model to load, about 10 s, and for a 670 MB download on the first run: past
  any sensible client timeout, and every request was stamped when it was read,
  so that late. Now it is answered 13 ms after it connected (debug build,
  `systemd-socket-activate`, `tests/cli.rs`), and the capture it starts records
  from the moment the microphone opens.
- **What a capture does before the model is ready.** It goes to its recovery
  WAV only: the loop drops its in-memory audio as it arrives, so the
  constant-RAM rule holds through a download of any length. At the release the
  WAV is queued. When the inference thread reports `Ready`, each queued WAV is
  transcribed in order, a `preview.max_seconds` window at a time through the
  same `Worker::tick` and `Worker::finish` a live capture uses, so every sample
  is decoded once and the tail at the end. Such a capture has no preview and
  no silence timeout: the settings that depend on the segmenter apply from the
  next capture after the load (`Session::configure`).
- **The winbar says what is happening.** The speech model's state
  (`core::session::Recognition`) becomes a notice — "downloading the speech
  model" with its percentage, "loading the speech model", "transcribing
  recordings", "no speech model" with the reason — with a count of the
  recordings that wait. It ranks with the capture notices, and the window
  shows whichever ranks higher.
- **A model that cannot be had does not stop the daemon.** A failed download,
  a missing configured `model_dir`, or a file that does not load is reported
  in the winbar and the log, the recordings are kept, and the next press runs
  the loader again. Exiting instead would lose the presses queued on systemd's
  socket and, repeated, trip the unit's start limit.
- **The automatic download stays**, for the default models only, as decided in
  [Model download moves into the binary](#model-download-moves-into-the-binary-2026-09-21),
  and now runs in the background. Whether anything is missing is decided by
  file size alone: hashing the set took seconds at every start. A file of the
  right size with the wrong bytes fails to load; then, and only then, the
  loader hashes the default set and downloads again what does not match its
  pin, and tries once more. Files that verify and still do not load are
  reported as such; a configured model is never touched, and its error says
  to check `asr.model_dir`.
- **With `recording.enabled = false`** a capture made before the model is
  ready has nowhere to go and is lost, with "capture not kept" in the winbar.
- **The same bounds as a live capture.** A capture kept on disk holds
  nothing in memory, so the in-memory ceiling cannot end it; its length
  does, at the same 60 minutes ("reached the time limit", `KeptTooLong`).
  Its transcription holds a `preview.max_seconds` window: when a whole
  window settles nothing (no VAD model, or speech the detector never
  breaks), it is committed whole (`Worker::commit_whole`) rather than held
  on, at the cost of a cut that may fall inside a word.
- **A waiting recording is the only copy of its capture.** The recorder's
  pruning passes over it until it is transcribed. One that cannot be read
  back all the same is reported in the window ("recording lost") and not
  counted as transcribed.
- **Recordings still waiting when the daemon stops** are named in the log for
  `spokenpad transcribe`; one that was partly transcribed is named with the
  offset its text reaches, for `spokenpad transcribe --from SECONDS`, so the
  text already in the file is not written twice.
- Rejected: holding captures made before the model is ready in memory and
  queueing their decode behind the load. A first-run download takes minutes,
  and memory would grow with every word for that long.
- Rejected: a daemon that only answers "run `spokenpad fetch-models`" while
  its model is missing. The user chose to keep the automatic download and to
  record meanwhile.

## An Arch package, started by socket activation (2026-09-22)

spokenpad ships like jumanji: `packaging/aur/PKGBUILD` builds a GitHub tag
tarball, and `scripts/install.sh` is gone. The user asked for no install
script and as few setup steps as possible.

- **The daemon starts only through its socket.** The package installs
  `spokenpad.socket` (`ListenStream=%t/spokenpad.sock`, mode 0600) and
  enables it for every user with a link in
  `/usr/lib/systemd/user/sockets.target.wants/`, as gnupg does for
  `gpg-agent.socket`. `spokenpad.service` has no `[Install]` section: the
  first press starts it, and it answers that press at once (see
  [Presses are taken before the model is ready](#presses-are-taken-before-the-model-is-ready-2026-09-22)).
  Nothing is enabled by hand, and no window manager line starts the service.
  The daemon checks that the socket it is handed is bound to the path the
  key bindings use, never probes or removes it, and still takes its own
  per-user lock.
- **Models are not packaged.** `spokenpad fetch-models` is the one explicit
  step, and optional: the daemon downloads the default models itself.
- **No exit a user can cause.** A daemon that exits leaves the presses
  queued on systemd's socket unanswered, and systemd starts it again for
  them until the unit's start limit fails the socket. So once the socket is
  adopted, nothing the user can get wrong ends the daemon: a missing model
  is downloaded or retried, a microphone that cannot be opened at start (a
  sound server not up yet at login) is retried by the watchdog and at each
  press, with PortAudio initialised afresh so it sees devices that appeared
  since, and a config file that does not load is replaced by the defaults,
  with the error in the log and in the first window. The one deliberate
  exit is `3`, another daemon holding the per-user lock (one started by
  hand): it answers the waiting presses first (`Reply::AnotherDaemon`), and
  the unit has `RestartPreventExitStatus=3`, so each press starts it at most
  once. `Restart=on-failure` stays for crashes. A unit that passes the wrong
  socket still fails loudly: serving a socket the key bindings do not
  connect to would look like a working daemon that hears nothing.
  `TimeoutStopSec=10` stays above the three-second shutdown budget of a
  pane.
- **Development** runs a build of one's own through the drop-in
  `packaging/systemd/dev.conf.example` (`ExecStart=%h/.local/bin/spokenpad`)
  after `cargo install --locked --path . --root ~/.local`.
- **The build needs no network beyond its sources.** The sherpa-onnx
  archive is in `source=()` with its sha256 (the same digest GitHub lists
  for the release asset), and `SHERPA_ONNX_ARCHIVE_DIR` makes
  sherpa-onnx-sys copy it instead of downloading. Its build script takes
  `CARGO_TARGET_DIR` literally, so the PKGBUILD sets it absolute. makepkg's
  `-flto=auto` turns the C that `ring` compiles into GCC bitcode that
  rust-lld cannot link, so `CFLAGS` gains `-ffat-lto-objects`, as jumanji's
  and Arch's `bat` do.
- **x86-64 only**, because the package's sherpa-onnx archive is.
- **Session environment.** A socket-activated daemon runs under the user
  manager and sees its environment only. Attach mode needs none; a window
  that opens by itself needs `DISPLAY`, which GNOME, KDE Plasma and most
  display managers import, and which a plain i3 or sway config imports with
  `systemctl --user import-environment`.

## `[nvim]` reloads with each new window (2026-09-22)

The daemon read its config once, at start, so an edit to `nvim.font_size`
did nothing until a restart. Now the editor thread reads the file again
(`config::Source`, with `--model-dir` applied on top, as at start) whenever
a window is about to open or an editor to be attached, and that window
takes the file's `[nvim]`: mode, font, colorscheme, init,
`copy_to_clipboard`, and the rest. A window already open keeps what it
opened with.

- **Everything else needs a restart**, because it is built into running
  state: the microphone (`[audio]`, `[recording]`), the loaded model
  (`[asr]`, `[vad]`), the text processor (`[text]`), and the session's tick
  and silence timeout (`[preview]`, `[capture]`). `Config::restart_needed`
  names the sections that differ, and the log says so once per distinct set.
  `spokenpad check` says the same.
- **A file that no longer loads** leaves the settings in use: the window
  shows the "config not reloaded" notice with the parse error, ranked just
  below "stopped after silence", until the next press.
- `nvim.copy_to_clipboard` is decided by the editor thread for the window it
  copies from, instead of by the event loop from the start-up config.
- Rejected: watching the file (inotify) and applying changes at once. A
  window open on the old font would have to be redrawn or reopened under the
  user, and a half-saved file would be read; the next window is a moment the
  user chose.

## Pane stays on top, and sway gets a runtime `no_focus` rule (2026-09-22)

The user decided three things after the
[previous measurement](experiments/2026-09-22-pane-focus-other-wms.md):
the focus rule is about the window acting by itself, the pane stacks on top,
and sway gets a rule rather than being left unsupported. Measured in
[the experiment](experiments/2026-09-22-pane-stacking-and-sway.md).

- **The rule, reworded** ([constraints.md](constraints.md#no-window-spokenpad-opens-may-take-focus)):
  no window spokenpad opens may take focus *by itself*; the user's own
  deliberate click may focus it, so they can edit in it. The tests assert
  both halves on every window manager: never focused on map, redraw, the
  next passage's pane or an empty workspace, and focused after the user's
  click. There is still no focus call anywhere.
- **`_NET_WM_STATE_ABOVE` is set before the first map.** KWin, on Wayland
  and on X11, stacks a window it refused focus below the active one; with
  the state it is on top. On i3, sway, Openbox and both KWins the pane is
  now shown above the focused window, also after the user clicks back into
  that window, and still never focused.
- **sway: the daemon adds `no_focus [instance="^spokenpad-pane$"
  class="^spokenpad-pane$"]` over sway's IPC before every pane.** sway 1.12
  accepts `no_focus` at runtime (it is in the table of commands valid in the
  configuration and over IPC) and ignores a duplicate, and with the rule the
  pane was never focused while a renamed twin was. The pane opens only on
  sway's success reply, so a refused rule means no window. sway focuses the
  first window on a workspace whatever the rules say, so on an empty focused
  workspace the pane refuses to open and the text goes to the pending
  passage, as when no window can open. The user's sway configuration is
  never written; a `swaymsg reload` drops the rule and the next pane adds it
  again.
  - The socket comes from `$SWAYSOCK`, read once in `Config::load` into
    `nvim.sway_socket` like `$DISPLAY`, so tests pass their own sway and
    never reach the user's. sway's shipped
    `/etc/sway/config.d/50-systemd-user.conf` imports `SWAYSOCK` into the
    user manager.
  - Not chosen: `WM_HINTS input = False` on sway. It would have needed a
    sway-only variant of the window and left real keys through sway's seat
    unmeasured.
- **KWin 6.7.5 on X11 is covered** (`kwin_x11`, from `kwin-x11`, on a harness
  Xvfb with a private bus): never focused at both focus stealing prevention
  levels, on top, and a real click focuses it.
- **Harness fixes:** Openbox ignored a window mapped right after it claimed
  the screen in about one parallel run in three (already at c7a7d2b), so the
  harness maps a probe window until Openbox manages it. sway's Xwayland
  window manager sometimes missed a `WM_CLASS` rewritten right after a
  window was created, so the test pauses before its own rewrites.

## The pane finds sway from the display, not from `$SWAYSOCK` (2026-09-22)

A review of the runtime sway rule found that it hung on `$SWAYSOCK`: a daemon
whose user manager never imported it (no `include /etc/sway/config.d/*`, or
an import that names only `DISPLAY`) skipped the rule, and sway focused the
pane. The hard rule cannot depend on an environment variable being right.

- **sway is recognised by the display.** Its Xwayland window manager names
  itself `wlroots wm` on the root's `_NET_SUPPORTING_WM_CHECK` window; i3,
  Openbox and KWin name themselves `i3`, `Openbox` and `KWin` (measured, see
  the [experiment's addendum](experiments/2026-09-22-pane-stacking-and-sway.md#addendum-finding-sway-from-the-display)).
- **Its socket is found by the process that runs the display.** The
  X-Resource extension names the process that owns that check window — sway
  — and sway's socket is `sway-ipc.<uid>.<pid>.sock` in `$XDG_RUNTIME_DIR`,
  now read with `$DISPLAY` and `$SWAYSOCK`. `$SWAYSOCK` is the second place
  looked at.
- **No reachable sway, no pane.** When the display says `wlroots wm` and no
  socket answers as sway — `$SWAYSOCK` unset or dead, nothing in the runtime
  directory, or another wlroots compositor — the pane refuses to open, and
  the desktop notification for the text that went to the file says why and
  names `SWAYSOCK`. A stale `$SWAYSOCK` under i3 no longer matters at all.
- **IPC connects under the deadline.** A blocking connect to a window
  manager whose accept queue is full never returned; the socket now connects
  non-blocking and retries until the deadline, in managed mode too.
- **The focus sampler must have measured.** It counts failed samples, and
  every stage fails unless it took samples and none failed.
- Rejected: `WAYLAND_DISPLAY` or `XDG_CURRENT_DESKTOP` as the test. They
  describe the session, not the X server the pane opens on, and are as easily
  missing from the user manager as `$SWAYSOCK`.

## The pane is sized in cells, the managed terminal in screen fractions (2026-09-22)

The user decided the pane's size is columns by lines, like Alacritty's
`window.dimensions`.

- **`nvim.pane_dimensions = { columns = 72, lines = 20 }`** sizes the pane.
  Both are non-zero by type (`NonZeroU16`), so a zero-cell pane cannot be
  configured. A grid larger than the monitor under the pointer is cut to as
  many whole cells as fit (`Dimensions::fit`); the corner still goes to the
  pointer, clamped so the window is wholly on the monitor. The pane measures
  its font first, so this happens on the pane's thread: the daemon now hands
  it the monitor and pointer (`place::Target`) instead of a pixel rectangle.
- **Default 72x20**: at the default 11.25 pt that is 648x360 pixels at 96 dpi,
  a third of a 1920x1080 screen each way — what the pane was at
  `window_fraction = 0.33` — and the same third of a 3840x2160 screen at
  192 dpi.
- **`nvim.window_fraction` is now managed mode's alone.** The daemon cannot
  know a terminal's cell size, so that window stays in pixels. One concept
  per mode; neither key does anything in the other mode.
- `[nvim]` is re-read for every new window, so a changed size applies to the
  next pane.

## A tiled pane beside the floating one (2026-09-22)

The user asked for a floating/tiled option, floating by default, and made
one condition: a tiled pane must never take the focus by itself on any
window manager, or be refused where that cannot be proven.

- **`nvim.pane_layout = "floating" | "tiled"`.** Tiled changes one property:
  the window type is `_NET_WM_WINDOW_TYPE_NORMAL`, which i3 and sway tile.
  The user time, `_NET_WM_STATE_ABOVE`, `WM_HINTS input = True` and the
  sway rule are the same in both layouts.
- **Allowed only where proven.** The full focus story, run tiled on i3,
  sway, Openbox, KWin Wayland and KWin X11, never saw the pane focused; the
  user's click focused it everywhere
  ([experiment](experiments/2026-09-22-tiled-pane-focus.md)). Tiled gives
  up `_UTILITY`, which refuses focus on window managers that ignore the user
  time (bspwm, Hyprland's Xwayland), so under any window manager not on that
  list (`x11::TILED_PROVEN`, by the name on the display's check window) the
  pane opens floating, with a log line every time and one notification a
  session. (The first version of this entry allowed tiled everywhere; a
  review caught that it contradicted the user's condition.)
- **Stacking window managers have no tiles.** On Openbox and KWin "tiled"
  is an ordinary window at the pointer, kept above. Documented rather than
  refused, since it never takes the focus.
- i3 is now also a `harness::desktops` desktop, so it runs the same story
  through the daemon's open path, floating and tiled.
- The layout is read with `[nvim]` for every new window, so a change applies
  to the next pane.

## The pane is the default mode (2026-09-22)

The user decided that `nvim.mode = "pane"` becomes the default, now that its
focus guarantee is proven headless on i3, sway, Openbox and KWin (Wayland
and X11), floating and tiled.

- A new user presses the key and the dictation window appears at the
  pointer, unfocused, with nothing to install in the window manager.
- **With no X display** — Wayland without Xwayland, or `DISPLAY` never
  imported into the user manager — the pane cannot open. The text goes to
  the dictation file as always, and the desktop notification now gives the
  reason, which names `nvim.mode = "attach"` and `spokenpad editor`, rather
  than only saying that no editor is open. `spokenpad check` says the same.
- `attach` remains the mode that works on any desktop; `managed` is
  unchanged.
- Tests that run the binary remove `DISPLAY`, `WAYLAND_DISPLAY` and
  `SWAYSOCK` from its environment, so the default mode cannot reach the
  user's display.

## The sway check fails closed (2026-09-22)

A second review found two ways the sway check could still guess.

- **`$SWAYSOCK` was not tied to the display.** A socket of another sway, or
  one left over and reused, answered as sway and took the rule for a display
  it does not run. A socket now counts only if the process listening on it
  (`SO_PEERCRED`) is the process the X server names for the display's window
  manager. A display that names no process is refused, not guessed at. A
  test starts a second sway and passes its live socket as `$SWAYSOCK`: the
  pane refuses.
- **Other wlroots compositors got sway's advice.** labwc, Wayfire or river
  also call their X window manager `wlroots wm`. The daemon now reads the
  display's process name (`/proc/<pid>/comm`): anything but `sway` is refused
  with its own reason — this compositor is not sway, spokenpad cannot keep
  the pane unfocused there, use `nvim.mode = "attach"`. Not run: none of
  those compositors is installed here.

## A config that does not load at startup gives the pane (2026-09-22)

With the pane the default, a file that does not load when the daemon starts
gives it `Config::default()` — the pane — even for a user whose file asked
for attach or managed mode. Kept on purpose: the file is read again before
every new window, a file that still does not load raises "config not
reloaded" with the reason, and the pane that opens is where that notice is
seen. Falling back to attach would put the notice in no window at all until
the user opens one. The README's configuration section says so.

## A missing microphone is looked for less and less often (2026-09-22)

A microphone that stays missing (unplugged, no sound server) cost the event
loop a PortAudio initialisation, and up to 500 ms of waiting for a first
callback, every two seconds for as long as the daemon ran, and wrote
"microphone recovery failed" to the log each time. Now the watchdog waits
2 s after the first failed attempt and doubles the wait after each further
one, up to 60 s (`reopen_backoff` in `shell/audio.rs`).

- **A press does not wait for the back-off.** It tries the microphone at
  once, as before; a failed press counts as one more attempt of the streak.
- **Logged once per streak.** The first failure is an error, with the reason;
  the attempts after it are at debug level; the recovery is one info line
  ("microphone available again after N failed attempts"). The event loop no
  longer logs `StreamUnavailable` itself; it only raises the notice when a
  capture is running.
- **The stall is bounded, not gone.** An attempt still runs on the event
  loop's thread. At the cap it costs at most one such stall a minute while
  idle; a press arriving during it is answered by the control thread at
  once and stamped when it arrived, and only waits to be acted on.
- Rejected: opening the stream on a thread of its own. The stream and
  PortAudio's handle belong to the loop that captures from them, and the
  cap already makes the cost rare.

## A daemon that cannot take its lock waits for it (2026-09-22)

This replaces the exit `3` of a socket-activated daemon from
[An Arch package, started by socket activation](#an-arch-package-started-by-socket-activation-2026-09-22).
That exit answered the presses already waiting, and the unit did not
restart on it, but every later press started the service again, and it
exited again. Five presses within ten seconds reach systemd's default start
limit (`DefaultStartLimitBurst=5`, `DefaultStartLimitIntervalSec=10s`,
systemd-system.conf(5)); the socket then fails with `service-start-limit-hit`
and no key works until `systemctl --user reset-failed`. A lock that could
not be created at all (the state directory not writable, the disk full)
exited `1` without answering anyone, and was restarted every three seconds
into the same limit.

- **Now a daemon that systemd started never exits over the lock.** While it
  cannot take it, it answers each press with why: "another daemon is
  running" (`Reply::AnotherDaemon`) or "cannot lock the state directory"
  (`Reply::CannotLock`, whose client message points to `journalctl`). It
  tries again before answering each press and every 100 ms, so it takes the
  lock as soon as the other daemon stops or the directory is repaired, and
  the press that finds it free is served (`shell::control::refuse_until`,
  `lock_under_activation` in `shell/daemon.rs`). The reason is logged once
  per distinct reason. SIGTERM stops it cleanly while it waits.
- **The unit loses `RestartPreventExitStatus=3`.** Under the socket there is
  no exit `3` left to prevent. A daemon started by hand still exits `3` when
  another holds the lock: nothing restarts it.
- Rejected: `StartLimitIntervalSec=0` on the service. It keeps the socket
  alive, but every press still starts a process that finds the lock taken
  and exits, and it would also lift the limit for a daemon that crashes at
  every start, which should fail loudly.
- Rejected: a higher `StartLimitBurst`. It only moves the press count at
  which the keys stop working.
- A socket that is not the one the key bindings use still exits `1`, and is
  meant to hit the limit: it is a broken unit, not something a user did.

## Recordings left at a stop are transcribed at the next start (2026-09-22)

A daemon that stopped with recordings still waiting to be transcribed named
them in the log for `spokenpad transcribe --from`. But the next start pruned
the recording directory with nothing kept, so the file the log pointed to
could be gone by the time the user read it.

- **Now the stop writes the list, and the next start transcribes it.** The
  recordings still waiting or being transcribed go to `waiting.tsv` in the
  recording directory, one line each: the frames the text already written
  reaches, and the file name. The next `CaptureRecorder::new` takes the list
  (reads, then removes it) and keeps those recordings from pruning until
  each is transcribed, and the event loop queues them as if they had just
  been released. Once the model is ready each is transcribed from where its
  text reached (`Worker::resume`), without a press; the window counts them
  like any recording made before the model was ready.
- **Exactly once.** The offset is the last commit the stopping daemon
  handed to its editor, so the text before it is not written again. The list
  is used once: removed by the start that takes it, and not used at all when
  it cannot be removed. A daemon that crashes after taking it does not
  transcribe those recordings a second time, and no longer protects them
  from pruning either.
- **The log still names the recordings** at the stop, each with how far it
  got and that the next start does the rest. If the list cannot be written,
  the stop falls back to the old lines for `spokenpad transcribe --from`.
- A line that does not name a recording in the directory (a plain file name
  `capture-*.wav` that is there) is skipped and logged: the list is read
  from disk, and nothing outside the recording directory is opened.
- Rejected: never pruning WAVs newer than the last stop. It keeps the file,
  but leaves the transcription to the user and the offset to a log line.
- Rejected: writing the list at every commit, so that a crash keeps it too.
  That is a file write on the event loop per commit, for a case the
  recovery WAVs already cover by hand.
