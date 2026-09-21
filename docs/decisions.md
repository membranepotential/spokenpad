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
daemon is listening.

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

So Parakeet decodes greedily unless hotwords are asked for: a non-empty
`vocabulary` (or a `hotwords_score`) with no explicit `decoding` still
selects beam search, since sherpa-onnx has hotwords only there. The
"knife edge" of 2026-09-19 (entry above) was mostly this bug; the retry stays
as a cheap guard. The 1 s of zero padding also hurt greedy on the sweep; it
stays until a corpus replay shows removing it loses no final words.

The same investigation found that the `shell-commands` eval clip does not
contain its reference's first sentence ("So, this is a test."): Parakeet,
Whisper and SenseVoice all hear the clip start at "Let's try". The recording
tool of the time had cut it; the reference is corrected, and the STATUS note
about first words lost was largely this artifact.
