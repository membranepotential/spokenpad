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

## Recordings not transcribed yet are listed for the next start (2026-09-22)

A daemon that stopped with recordings still waiting to be transcribed named
them in the log for `spokenpad transcribe --from`. But the next start pruned
the recording directory with nothing kept, so the file the log pointed to
could be gone by the time the user read it.

- **The list.** `waiting.tsv` in the recording directory names every
  recording the daemon has not finished transcribing, one line each: the
  frames the text already written reaches, and the file name. The daemon
  rewrites it when one is added (a capture kept before the model is ready),
  when one is finished, and at the stop, with the offsets as they are then;
  it removes the file once nothing is left. It never writes it per commit,
  and it does not `fsync` it: this runs on the event loop, and the list only
  has to survive the process.
- **The next start** reads the list, keeps those recordings from pruning
  until each is transcribed, and queues them as if they had just been
  released. Once the model is ready each is transcribed from where its text
  reached (`Worker::resume`, `CaptureReader::skip`), without a press; the
  window counts them like any recording made before the model was ready.
- **A crash keeps the list**, since the start only reads it. The price: the
  offsets are those of the last rewrite, so after a crash the text committed
  since then is written a second time. After a clean stop every sample is
  decoded once.
- **The log still names the recordings** at the stop, each with how far it
  got and that the next start does the rest. If the list cannot be written,
  the stop falls back to the lines for `spokenpad transcribe --from`.
- A line that does not name a recording in the directory (a plain file name
  `capture-*.wav` that is there) is skipped and logged: the list is read
  from disk, and nothing outside the recording directory is opened.
- Rejected: never pruning WAVs newer than the last stop. It keeps the file,
  but leaves the transcription to the user and the offset to a log line.
- Rejected: rewriting the list at every commit, which would make a crash
  cost nothing twice. That is a file write on the event loop per commit.

## A recording that fails partway is not reported lost (2026-09-22)

A recording made before the model was ready whose transcription failed
after some of its text was written said "recording lost", although that
text was in the file and the rest was still in the WAV. Now what the window
says depends on how far it got, whether the failure came during the run or
during the stop:

- **Nothing written: "recording lost"**, as before; the recording is
  released.
- **Further than its last attempt: "recording partly transcribed … the
  next start tries the rest again"** (`Rest::NextStart`). It stays on the
  list with its offset, and the next start resumes it.
- **No further than its last attempt** (a resumed recording that fails
  where it stopped): "… failed after 12.34s again; recover the rest with
  spokenpad transcribe --from 12.34" (`Rest::ByHand`). It leaves the list,
  so it is not retried at every start, and stays kept from pruning for the
  rest of the run, since the notice sends the user to it.
- **The file ends, or stops being readable, before its offset: "recording
  shortened"**, with the file name, and no `--from`, which would fail the
  same way. It was cut or replaced since; the rest is gone, and it is
  released.
- These rank below "recording lost" and above "no speech model": shortened,
  then partly transcribed. The log says the same with the whole path.

## Typing into the window while dictating (2026-09-22)

A live check of the pane: during a latched recording the user clicked into
it and typed, and the window "got stuck" — the preview, the level meter and
the winbar stopped moving — while one append took 2085 ms instead of the
usual 20. Two causes, measured in `tests/pane_typing.rs`:

- **A half-typed Normal-mode command holds back every RPC call.** Neovim
  waits for the rest of a count, `g`, `"`, `f`, `r` or `z` without running
  its event loop, so nothing the daemon sends runs until the command is
  finished or cancelled: a stand-in Neovim 0.12.5 left an `nvim_eval`
  unanswered for over 3 s after each of those keys, and answered in 20 ms
  after `d` or in Insert mode. The 2085 ms was an append whose 2 s deadline
  expired, followed by a retry that the next key let through. Held longer
  than 4 s, the append was reported as failed although it was still queued,
  and the next utterance went to a pending passage instead of the window.
  - Chosen: calls on an editor already attached to — the liveness check, the
    append and its retry, the clipboard copy — ask `nvim_get_mode`, which
    Neovim answers at once even then, when their reply is late, and wait for
    as long as it says `blocking` (`Patience::WhileTyping`). The text lands
    once, in the window, the moment the command ends.
  - Chosen: the bundled init turns `showcmd` on, so the waiting keys show in
    the corner and say why the window stopped moving.
  - Chosen: closing the pane cancels a pending command with `<Esc>` before
    it writes the buffers. The write waited behind it too and gave up after
    1.2 s, losing what was typed by hand.
  - Rejected: cancelling a pending command with `<Esc>` while the window
    stays open, to keep the preview moving. It changes what the user's next
    key means (`r` then `x` replaces a character; with the `r` cancelled, `x`
    deletes one), and a window that rewrites what you type is worse than one
    that waits for you.
  - Rejected: waiting everywhere. An editor stuck in a startup prompt is not
    one to wait for, so startup, attach and the last idle push keep their
    deadlines.
- **The preview moved the typing cursor.** Following the text scrolls the
  window and put the cursor on the last character, as for a reader. In
  Insert mode that is where the next key lands, so the user's text went into
  the middle of the last dictated word. Now, in Insert or Replace mode in
  that window, the view still follows and the cursor stays where it was.

A consequence to know: while a command is half typed, the editor thread
waits, and a daemon stopped then runs out its 3 s grace with the text still
queued, and the pane goes with the process. The 4 s of deadlines before this
ran it out the same way.

## The pane keeps a margin around its grid (2026-09-22)

In the live check the grid ran into the window's edges and the user found
it cramped, asking for "half an em or maybe four pixels". The pane now keeps
4 pixels at 96 dpi on every side, scaled by `Xft.dpi` / 96 and rounded down
(8 at the user's 192 dpi), in Neovim's default background, outside the grid.
The window is the grid plus the margin; the margin comes off the monitor
before the grid is fitted to it; clicks are mapped through it.

- Chosen: 4 logical pixels rather than half an em. At the default font half
  an em is 3 to 4 pixels at 96 dpi anyway, and a margin in pixels is how
  Alacritty's `window.padding` works, so the pane and a terminal with
  `padding = { x = 4, y = 4 }` stay the same size (`tests/pane_hidpi.rs`).
- Chosen: the rounding Alacritty uses, down (`floor`), so the two agree at
  every resolution, not only at whole multiples of 96 dpi.
- Rejected: a setting. One fixed value was asked for, and a setting with one
  user is a choice nobody has to make.

## A call held behind a half-typed command is waited for two minutes at most (2026-09-22)

Follow-up to [typing into the window while
dictating](#typing-into-the-window-while-dictating-2026-09-22). `nvim_get_mode`
says `blocking` for a hit-enter prompt or a plugin's `input()` as much as for
a count or `g`, so an attach-mode editor nobody looks at could hold one
append for hours, with every later utterance queued behind it and a daemon
stop never reaching its report of undelivered text.

- Chosen: a cap of 2 minutes (`HELD_AT_MOST`), then the path of an editor
  that stopped answering, where the operation id keeps the append from
  landing twice. Twenty times any pause a person takes mid-command, and short
  enough that a prompt nobody sees costs one piece of text's place in the
  window, not a whole session's.
- Chosen: a `quitting` flag on the session, set by the daemon before its
  last message to the editor thread, which ends the wait within half a
  second (measured in `tests/pane_typing.rs`: 514 ms after the flag, with an
  append held for 2.6 s). Once it is set, nothing reattaches or opens an
  editor, so what is still queued goes to the pending passage within the
  shutdown grace.
- Chosen: the log repeats every 10 s while a call is held, and the pane
  draws a notice in its last row itself.
- Measured: Neovim 0.12.5 dropped the held request when its connection
  closed; after `<Esc>` the abandoned text was not in the buffer. Given up
  means reported, never written twice.
- Rejected: a shorter cap. Someone who typed `"` and went to read something
  would lose the text of the utterance they were dictating from the window.

## The cancelling `<Esc>` at pane teardown leaves no trace (2026-09-22)

Closing the pane cancels a half-typed command with `<Esc>` so that the
buffer can be written. In Insert mode after `<C-v>` that `<Esc>` went into
the file as a literal ESC, and after `<C-v>u12` it ended the number and
`\x12` went in.

- Chosen: keep the `<Esc>` and take back what it typed. Measured on Neovim
  0.12.5: after `<C-v>` the ESC is inserted and Insert mode goes on; after
  `<C-v>` and digits the number's character is inserted and Normal mode
  follows; after `<C-k>`, `<C-r>` or `<C-o>` nothing is inserted. A literal
  ESC before the cursor is taken back with `<BS>`, which in Replace mode also
  restores the character it replaced; a control character under the cursor
  after leaving Insert mode is deleted.
- Chosen: after each `<Esc>`, a call Neovim runs only once it has read that
  key, instead of `nvim_get_mode`. `nvim_get_mode` is answered on arrival,
  and in the first version that raced ahead of the `<Esc>`: the pane sent a
  second one, which deleted the ESC in Replace mode instead of restoring the
  character under it. The check only looks, so one left queued behind a
  command that is still pending changes nothing when it runs.
- Rejected: a different cancelling key. After `<C-v>` every key is literal.

## Unconfirmed text goes to the pending passage too (2026-09-22)

An append that stayed unconfirmed after its repeat with the same operation
id (the two-minute cap, a daemon stop while Neovim held it, a reply lost on
a live connection) was kept only in the log and the recording WAV. After a
held append the text is almost never in the window either: Neovim 0.12.5
drops a request whose connection closed before it ran. So the user lost it
unless they read the log.

- Chosen: write it to the pending passage, with a desktop notification that
  names the file and says the text may also be in the window. It is in both
  only if Neovim ran the request after all, which needs a reply lost on a
  live connection whose repeat also failed.
- The operation id still decides first: a repeat that answers means the text
  is in the window once, and nothing is written elsewhere.
- Rejected: writing it elsewhere only when it is known not to have landed.
  No answer can say that, since the editor that could is the one not
  answering, so the rule would keep losing the text in the usual case to
  avoid a rare duplicate.
- Rejected: marking the text itself as possibly duplicated. A marker in a
  transcript is text the user has to find and delete in every case; the
  notification says it once.
- Measured in `tests/e2e.rs`: a stop 4 s into a held append wrote the text to
  the pending passage and nowhere else, and the daemon stopped 161 ms after
  it was told to.

## A stopping daemon gives up on an append within its grace (2026-09-22)

Review of the two-minute cap: the `quitting` flag was looked at only once
Neovim said it was holding the call. An append sent after the flag still
waited its whole 2 s deadline first, and on an editor that had stopped
answering altogether (not held: frozen) the timeout led to a reconnect and a
repeat, about 4 s more, so the process exited before the pending-passage
write and the text was only in the log.

- Chosen: while stopping, an append is not reconnected and repeated; it is
  unconfirmed at once and its text goes to the pending passage.
- Chosen: a patient call wakes every half second, before its deadline too,
  and its caller ends it when the flag is set (`Waiting::Unanswered`).
- Chosen: a patient call sent after the flag gets a quarter second
  (`QUITTING_TIMEOUT`) instead of two. A healthy editor appends in tens of
  milliseconds; a slower one is given up on and the text goes to the pending
  passage.
- Measured (`src/shell/nvim/tests.rs`): given up 200–510 ms after the flag,
  on an editor held by a count or frozen with `SIGSTOP`, with the flag set
  before or during the append; 2.6 s without the first change. The frozen
  editor, continued, ran the request as well: the documented case of text in
  both places.

## The teardown `<Esc>` is taken back on facts, not on the text (2026-09-22)

Review of the take-back: it guessed from the text after the `<Esc>`. After a
pending `<C-r>` or `<C-k>` the `<Esc>` only cancels and Insert mode goes on,
so an ESC of the user's before the cursor was taken back with `<BS>`
(reproduced: `ab\x1b`, then `A<C-r>`); and after `<C-x>` Insert mode ends
with the cursor on the character before it, so a control character of the
user's there was deleted.

- Chosen: read what Neovim shows at the cursor, from the pane's own grid,
  before sending the `<Esc>`. Measured on 0.12.5: `^` while `<C-v>` waits,
  `"` for `<C-r>`, `?` for `<C-k>`, and after `<C-v>` and digits the cell's
  own text again. `<BS>` only if `^` was showing and Insert mode goes on.
- Chosen: delete only if Insert mode ended with the cursor on a control
  character at the screen cell it was on before. Leaving Insert mode moves
  the cursor left, so only a character the `<Esc>` inserted puts it back.
- Accepted: at the start of a line, where the cursor cannot move left, a
  `<C-v>` number typed right before a control character of the user's stays
  in the file. The alternative there is deleting the user's own character.
- The test closes 23 panes, each with a pending sequence, 12 of them next
  to an ESC or a `^L` already in the file; the previous version failed on
  the first of those.

## Managed mode is removed; the pane replaces it (2026-09-22)

The user decided that `nvim.mode = "managed"` goes, with every key only it
read. `nvim.mode` keeps two values: `pane` (the default) and `attach`, which
Wayland without Xwayland still needs.

- Why: the pane is the same window at the pointer, never focused, without
  what managed mode cost. It needs no `no_focus` rule in the user's window
  manager configuration and no terminal from a table, and it is proven
  headless on i3, sway, Openbox and KWin (Wayland and X11), where managed
  mode ran on i3 and sway only. Keeping managed mode kept a second window
  path, the `no_focus` proof over IPC, the terminal table and their tests
  alive to duplicate what the pane does.
- Given up: a window that outlives the daemon. A pane closes with a daemon
  restart; it writes every buffer first, so no text is lost, and attach mode
  remains for an editor that must outlive the daemon.
- Removed: `core/terminal.rs`, the `no_focus` proof of the user's
  configuration and the placement commands (`shell/wm.rs`, `core/wm.rs`;
  the pane's own runtime rule on sway stays), `examples/verify_window.rs`,
  the keys `terminal`, `window_instance` and `window_fraction`, the window
  rules in `packaging/i3/` and `packaging/sway/` (their example key bindings
  stay), and the terminal and window-manager optional dependencies of the
  package. `nvim.editor` and `nvim.startup_timeout_s` stay: the pane reads
  both.
- Compatibility: unknown keys were already an error. A config that still
  sets `mode = "managed"` or one of the removed keys is refused with a
  message that names the key and says the pane replaced it, the way a
  leftover `[hotkey]` table is. The daemon then runs on the defaults, the
  pane among them, and the pane says "config not reloaded" until the file is
  fixed.
- Tests: the editor tests and `tests/e2e.rs` spawned a headless managed
  editor as their stand-in window. They now open the editor the way
  `spokenpad editor` does, in attach mode; the pane's own path for waiting on
  a fresh editor is tested by starting one the way a pane does, without the
  window. On the way, the frozen-editor case of the stopping-daemon test
  failed about one run in twelve on a loaded machine, at the base commit
  too: continued after `SIGSTOP`, Neovim sometimes drops a request whose
  connection closed rather than running it. It now accepts either outcome,
  and still never a second copy.

## The dictation file is saved on every change, and `:q` always writes (2026-09-22)

The user: the dictation file is a scratch pad, `:q` should always write and
quit, and every edit should be saved at once. `:q` on a buffer the user had
typed into failed with E37; only the pane's own close wrote first.

- Chosen: `spokenpad.lua`, which every editor loads over RPC in both modes,
  writes the dictation buffer on `TextChanged`, `TextChangedI` and
  `TextChangedP`. In Insert mode that is every keystroke. A file this small
  writes in a millisecond or two, and saving on `InsertLeave` instead would
  lose a paragraph typed into an editor that then died, which the pane's
  close cannot rescue.
- Chosen: the write is an append's write plus `lockmarks`: `silent lockmarks
  noautocmd write`, only of the pinned buffer, only when it is modified (an
  append has already written its own, so its later `TextChanged` writes
  nothing). No autocommand of the user's runs, so a format-on-save cannot
  reflow a transcript. A failed write stays silent and leaves the buffer
  modified: a message could raise a prompt, which would hold every call the
  daemon sends.
- Chosen: `QuitPre` writes the dictation buffer the same way. `:help
  'autowriteall'` (0.12.5) covers `:quit`, `:qall`, `:exit` and `:xit`, and
  it is set too, in editors spokenpad opened, for any other file opened
  there; but it writes with autocommands. A test that ignores the change
  events and quits proves the difference: without `QuitPre`, `:q` still
  wrote and quit, and the user's `BufWritePre` ran on the transcript.
- Rejected: `'autowriteall'` in an editor spokenpad only adopted, where the
  globals are the user's, as with the chrome.

## Closing the pane cancels the capture it showed (2026-09-22)

Reported live: during a latched capture the user closed the pane; the log
said "the dictation pane closed", the capture went on for another seventy
seconds, and its next commit opened a new window on a new file. The user
wants closing the window to cancel the recording.

- Chosen: a close by the user is a new input to the transition table,
  `state::Event::WindowClosed { at }`, with the same outcome as a cancel
  request (`DiscardReason::WindowClosed`): committed text stays in its file,
  the tail is not decoded, the recovery WAV is kept. It cancels only a
  capture that started before `at`, so a key pressed right after the close,
  before the close is reported, starts a capture that goes on. Released and
  decoding, it changes nothing, as a cancel does not.
- A close by the user is the window manager's `WM_DELETE_WINDOW` (i3's
  `kill`, a title-bar button), or Neovim exiting with status 0 (`:q`, `:wq`,
  `:qa`). The pane reads the exit status once Neovim's channel closes
  (`Ending` in `shell/pane`), waiting up to 0.5 s for the process to go.
  Only a pane the daemon had attached to counts: an editor that quits with
  status 0 while it starts (a configuration that runs `qall!`) is a pane
  that failed to open, and its capture's text goes to the pending passage
  as before.
- Kept: a crash is not a close. Neovim killed, dying, or exiting with
  another status (`:cq` included), or the X connection failing, leaves the
  capture running, and its next text opens a new pane on the pending
  passage, as before. Nothing may be lost to a crash.
- Chosen: after the user's close, text opens no pane until the next key
  press (`nvim::Want::Text`); it goes to the pending passage, which the next
  pane opens on. Without this, a chunk already being decoded at the close,
  or the tail of a capture released just before it, would open a window the
  user had just closed. Text also opens no pane while the last one is still
  closing, since that may be a close not yet recorded.
- Chosen: one desktop notification per such close ("recording cancelled"):
  the winbar that shows every other notice is gone with the window, and the
  next key press would clear a notice before any window could show it.
- Attach mode is left as it is: the daemon did not start that editor, so it
  cannot read its exit status, and a socket that closes looks the same for
  `:q` and for a crash. Quitting an attached editor never cancels a capture.
- Tests: the transition table's sweep and a test of the rule; in
  `tests/pane_daemon.rs`, a latched capture on a real daemon and pane,
  closed once through the window manager and once with `:q` typed into it
  (the recording stops growing, speech after the close is never written
  anywhere, no file or window opens), and once with its editor killed (the
  capture goes on and a new pane opens with its text).

## The pane logs the layout it opened with (2026-09-22)

The user set `nvim.pane_layout = "tiled"` on i3 4.25.1 and saw a floating
pane; the log said nothing about the layout, and the headless i3 test tiles
it.

- Chosen: one INFO line per pane that opens: the layout asked for and the
  one applied, why they differ when they do, the window manager, and the
  grid in cells at the map.
- Found: the managed-mode rule the package shipped,
  `for_window [instance="spokenpad"] floating enable`, matches the pane's
  instance `spokenpad-pane`, because i3 does not anchor criteria, and floats
  a tiled pane on i3 4.25.1
  ([experiment](experiments/2026-09-22-tiled-pane-floats-under-managed-rule.md)).
  The rule is gone from the package with managed mode; nothing in the pane
  changes. That the user's configuration carries it is not verified; the
  docs say how to check and to delete or anchor it.
- `examples/pane.rs` takes `--tiled`, which the experiment used.

## The pane's editor says when it was told to quit (2026-09-22)

Review of the close that cancels a capture found two faults.

- The exit status decided whether a `:q` was the user's close, read within
  0.5 s of Neovim's channel closing. Neovim closes its channels and then
  waits up to two seconds for its jobs: with a job that ignores `SIGTERM` (a
  language server may), a `:q` exits with status 0 two seconds after the
  channel closed, was read as a crash, and the capture went on. It also held
  the pane's thread for half a second. Chosen instead: when the pane
  attaches, it registers a `VimLeavePre` autocommand that, with `v:dying`
  at 0, notifies the pane's own channel before any channel closes. The
  notice, not the status or the timing, makes the exit the user's close,
  and nothing waits for the process. `:cq` now counts as a close, being one
  the user typed; a kill, a crash or a deadly signal (which sets `v:dying`)
  sends nothing and stays a crash.
- The recorded close was stamped after that wait and was not tied to a
  pane: a key pressed while the old pane was still exiting could open a new
  pane, and the old close, stamped later than the new capture's start, then
  cancelled the new capture and dropped the new pane's connection. Chosen:
  the close is stamped when the window manager's request or the notice
  arrives, and cleared when the next pane is asked for. Text that finds the
  last pane gone also checks for a close not yet reported, so it cannot open
  a pane in that moment.
- Tests: the pane-close test in `tests/pane_daemon.rs` runs with an editor
  whose job ignores `SIGTERM`, and adds `:q` followed at once by a key
  press: the new capture keeps recording in a new pane that stays open. With
  the previous detection it fails at the `:q`.

## The dictation file is saved on leaving it, not through 'autowriteall', and Insert mode is debounced (2026-09-22)

Review of the save-on-every-change entry above found two faults.

- `'autowriteall'` wrote the dictation buffer *with* the user's
  autocommands, and not only on `:q`: on `:edit`, `:bnext`, `:!` and
  `:make` too. A change whose own write had not run yet — `TextChanged`
  waits while keys are queued, as in a mapping or a macro like
  `dd:bnext<CR>` — was then written through a format-on-save. Chosen
  instead: `'autowriteall'` is not set, and the dictation buffer is written
  with spokenpad's own write (`silent lockmarks noautocmd write`) on
  `BufLeave`, `QuitPre` and `VimLeavePre`. Measured on 0.12.5: `QuitPre`
  runs before `:quit` refuses a modified buffer, so `:q` still writes and
  quits; a test with every change event and `InsertLeave` ignored fails
  when the `QuitPre` write is removed. Another file opened in the dictation
  editor is the user's to save.
- Every Insert-mode keystroke ran a full write, and `'fsync'` is on by
  default, so every keystroke fsynced; on slow storage that stalls Neovim's
  main loop and the daemon's appends with it. Chosen: Insert-mode changes
  are written 300 ms after the last one (a `vim.uv` timer that each change
  restarts); `TextChanged`, `InsertLeave`, `BufLeave`, `QuitPre` and
  `VimLeavePre` still write at once. The claim that a write costs "a
  millisecond or two" is gone from the docs.
- Tests: leaving an edited dictation buffer with `:edit` in one go with the
  change writes it without the user's `BufWritePre` (fails without the
  `BufLeave` write); `:q` with `InsertLeave` ignored too still writes and
  quits; a dedicated editor leaves `'autowriteall'` off. The debounce itself
  has no timing test: one would race the machine's load.

## A startup prompt no longer keeps the pane from opening (2026-09-22)

A second review of the quit notice above found a regression. The pane
waited for the notice's registration (`nvim_get_chan_info`, then
`nvim_exec_lua`) before the window was mapped. With `--embed`, Neovim sources
the user's configuration after `nvim_ui_attach`, and a message there (an
`echoerr`, a deprecation notice) raises a hit-enter prompt that holds every
later call. Measured on 0.12.5: the attach was answered at once, the mode
read `blocking`, and the next request stayed unanswered until `<CR>` was
input, then was answered. So the window never appeared, nobody could answer
the prompt, every open failed after `nvim.startup_timeout_s`, and every key
press repeated it.

- Chosen: the registration is sent and not waited for; its answer is
  handled whenever it arrives, and a failure is logged. The script finds the
  pane's channel itself (the one `stdio` RPC channel `--embed` made), so
  nothing has to be asked first. The attach stays the only call the pane
  waits for, as before the quit notice. A registration that never runs
  leaves a `:q` read as a crash, the safe direction.
- Test: `tests/pane_render.rs` opens a pane whose init runs `echoerr`: the
  window is mapped while Neovim reports `blocking`, the notice is registered
  once `<CR>` answers the prompt, and `:q` is then the user's close. With the
  registration awaited, the pane does not open.

## `:restart` is not the user's close (2026-09-22)

The same review: the quit notice keyed on `v:dying` alone, and the review
held that `:restart` (Neovim 0.12) runs `VimLeavePre` with `v:dying` at 0 and
then keeps the process, so a later crash would read as the user's close.
Measured on 0.12.5 over `--embed`, with the notice reporting `v:dying` and
`v:exitreason` from `VimLeavePre`: `:q`, `:q!`, `:wq`, `:qa`, `:cq` and `ZZ`
report 0 and `quit`; `:restart` and `:restart!` report 0 and `restart` or
`restart!`. The process does not stay: after `:restart` the old one exits
(its channel closes), and the new server it starts, with the same argv, waits
for a UI that handles the `restart` event, answering on the old `--listen`
socket with no window. So the flag set by the notice is never followed by
another life of the same process, but `:restart` itself read as the user's
close and cancelled the capture.

- Chosen: on 0.12 and later the notice also needs `v:exitreason` at `quit`,
  so `:restart` ends the pane as the editor dying: the capture goes on. The
  version is checked with `has("nvim-0.12")`, which added both `:restart`
  and `v:exitreason` (`news.txt` of 0.12 lists both as new); an unknown
  `v:` variable reads as `nil` on 0.12.5 rather than failing, so testing the
  value alone would read every quit on 0.11 as a crash. The flag is never
  cleared: `VimLeavePre` runs once the exit can no longer be cancelled.
- Rejected: clearing the flag. No process outlives a `VimLeavePre` with
  `v:exitreason` at `quit`, so there is nothing to clear it for.
- Not done: the server `:restart` leaves on the socket. Handling the
  `restart` UI event, or ending that server, is a separate change.
- Test: `tests/pane_render.rs` runs `:restart` in a pane and expects
  `Ending::EditorDied`; it fails with the `v:dying` check alone. It skips on
  a Neovim without `:restart`, and kills the server `:restart` started.

## The dictation buffer hides itself when left (2026-09-22)

The same review: with the user's `set nohidden`, `:edit other` or `:bnext`
on a change whose own write had not run yet stopped at E37. Neovim checks
for an unsaved buffer it would abandon before `BufLeave`, so the `BufLeave`
write above never ran. Measured on 0.12.5 in a headless Neovim with
`nohidden` and a `BufLeave` write: `:edit` after `dd` fails with E37 and
the file keeps the line; with the buffer's `'bufhidden'` at `hide` it moves
to the other file and the file lost the line.

- Chosen: `spokenpad.lua` sets the dictation buffer's own `'bufhidden'` to
  `hide`. That is what the default `'hidden'` does for every buffer, so
  nothing changes with the default; the user's global `'hidden'` is left
  alone.
- Test: the `BufLeave` test in `src/shell/nvim/tests.rs` runs with
  `set nohidden`; without the option it fails.

## The pane opens beside the pointer, never under it (2026-09-22)

The user runs i3 with its default `focus_follows_mouse yes`. Moving the
mouse into the pane did not focus it, and a nudge of one to three pixels up
or left did: the pane opened with its window's corner on the pointer, so the
pointer started inside i3's frame and only a move back across its edge
counted ([investigation](experiments/2026-09-22-pane-hover-focus.md)). The
user decided on 2026-09-22 that the pane may be focused by hover, and by
nothing else the pointer does not do on purpose.

- Changed rule: the user's own click, or their pointer moving into the pane
  under focus-follows-mouse, may focus it; nothing else may. Still no focus at
  the map, a redraw, the pane's own move or resize, a window closing under
  it, or a jiggle where the pointer rested when it opened.
- Chosen: the pane's outer frame opens a gap away from the pointer, right of
  and below it; on each axis where that does not fit, left of or above it;
  where neither fits, centred on it (`geometry::placement`, `Side`). One axis
  with a side keeps the pointer outside the frame. With neither, the pane
  opens around the pointer: the window itself, not its frame, is centred on
  it and kept on the monitor. Kept on the monitor by its frame instead, a
  screen-sized pane near a corner left its border one pixel beside the
  pointer, and i3 focused it when a jiggle moved the pointer from the window
  onto its own border: `tests/pane_hover.rs` caught that.
- Chosen: the gap is 20 pixels at 96 dpi, scaled by `Xft.dpi` / 96, rounded
  up and never below 20 (`Gap::at`): 40 at 192 dpi. The investigation found
  no focus from any point within 6 pixels of the pointer with the frame 20 or
  more away, at 96 dpi. At 192 dpi a desktop draws frames, text and the
  pointer twice as large, and 20 device pixels would be 10 at 96 dpi;
  scaling keeps the distance the user sees, and costs only a pane that opens
  a little further from the pointer.
- Chosen: before the map, the position asked for leaves 48 pixels (scaled
  the same way) for a frame on every side (`Extents::assumed`). i3 puts the
  frame at a position asked for before the map, Openbox and KWin with static
  gravity the window, and the largest frame measured reaches 36 pixels
  (KWin's title bar): 48 keeps the gap either way
  ([frame extents](experiments/2026-09-22-pane-frame-extents.md)).
- Chosen: `win_gravity = Static`, and after the map one `ConfigureWindow`
  that places the frame the window manager drew: its size from the window's
  top-level ancestor when the window manager reparented it, else
  `_NET_FRAME_EXTENTS`, else no frame. With static gravity every window
  manager measured reads that request as the window's own position, i3
  included, so one move is exact. `Pane::show` waits at most 0.5 s for the
  window to be viewable first, and leaves a window the window manager sized
  itself (a tile) where the window manager put it. The window's focus
  properties are unchanged, and every focus test runs as before.
- Kept: under Xwayland the pointer is not read, and the pane asks for the
  bottom-right corner, now with its frame (KWin on Wayland reports a 36-pixel
  title bar) inside the monitor. sway centres Xwayland windows whatever they
  ask for.
- Tests: unit tests of `placement` (every pointer position on a monitor,
  for several sizes and frames: the pointer is outside the frame by the gap,
  or inside the window by the gap from every edge the monitor does not hold
  back); `tests/pane_hover.rs` on i3 with `focus_follows_mouse yes` (floating
  at 96 and 192 dpi, tiled beside the pointer and under it, and the centred
  cases near the screen's edge) and Openbox with `followMouse yes`, with and
  without `underMouse`: no focus at the map, on a jiggle over the 169 points
  within 6 pixels of the resting pointer, or on the pane's own resize; focus
  when the pointer moves in from outside. With a gap of 0 the jiggle
  focused the pane on both, as in the investigation.
- Not covered: a pane too large to open beside the pointer on Openbox with
  `underMouse yes` (not its default) is focused at the map, since it opens
  under the pointer (investigation, row I). A tiled pane goes where the
  window manager tiles it, possibly under the pointer. KWin's
  focus-follows-mouse policies and sway were not run. On several monitors a
  pane opened around the pointer may leave its border on the neighbouring
  monitor, within reach.

## Two development examples removed, the Gladia script untracked (2026-09-22)

The user asked what `examples/` is for before the repository goes public.
It holds development tools, not usage examples; `examples/README.md` now
says so. Two were stale: `verify_native.rs` printed segments and commits for
inspection by hand, which `corpus.rs` now counts, and `decode_probe.rs`
served the beam-search experiment
([2026-09-21](experiments/2026-09-21-beam-search-upstream-fix.md)), which
shipped nothing; its experiment names the commit that holds it. Both are
deleted. `scripts/gladia-references.sh`, which uploads the author's
recordings to Gladia, is useful to the author alone: it is git-ignored and
kept only in the author's checkout; the docs call it local.

## The model download is bounded and takes a lock (2026-09-22)

The 2026-09-22 audit (P2-014) found the download on ureq's defaults: no
timeout at all, redirects to plain HTTP followed, and the body read to its
end however long. A stalled connection held the daemon's loader forever, and
a misbehaving mirror could fill the disk before the sha256 check rejected the
file. The fresh-user walkthrough (P9) found that the daemon's own download
and `fetch-models` or `check` wrote the same `<file>.part` without a lock.

- Chosen: one `ureq::Agent` with `https_only` (ureq checks it on every
  redirect hop), at most 5 redirects, 30 s each for resolving, connecting,
  sending and receiving the response headers, and a body budget of 30 s
  plus the pinned size at 64 KiB/s. The body is never read past the pinned
  size, and the `.part` file is removed on every failure, not only on a hash
  mismatch. Every download into a models directory holds an `flock` on
  `.fetch.lock` there; a second process says it waits, waits, and then finds
  the files in place.
- Rejected: a per-read timeout. ureq 3 bounds the whole body, not each read,
  so a stall mid-body ends only when the budget does (about 2.8 h for the
  encoder). Range requests in chunks would detect it sooner, at the cost of
  a resume protocol and one redirect round trip per chunk.
- Rejected: unique `.part` names per process. Two processes would still
  download the same 670 MB twice, and a killed one would leave its part
  behind under a name nobody reuses.
- Kept: `fetch-models` still downloads `test_en.wav`, which the daemon's own
  download skips (walkthrough P6): the real-model tests in `tests/e2e.rs`
  read it.
- Added for C9: `shell::models::terminal_progress` draws one progress line
  on stderr when it is a terminal, and `report_on_stderr` is
  `fetch-models`' reporter; `main.rs` wires them in separately.
- Tests: `shell::models` — a body longer than the pin is cut off at the pin,
  a connection cut short leaves no `.part`, plain HTTP is refused before any
  connection, a second download waits for the lock and then downloads
  nothing; the ignored `a_real_download_…` fetches one file from each real
  host through its redirects.

## The editor's socket must belong to this user (2026-09-22)

The 2026-09-22 audit (P3-013): `attach_existing` trusted any listener at
`nvim.socket_path` that showed the marker or pinned a buffer inside the
dictation directory, and never asked who listened. With the default
`$XDG_RUNTIME_DIR` (0700) nobody else can bind there; with a `socket_path`
in a shared directory another user could bind it first and receive every
transcript.

- Chosen: every connection to an editor's socket reads `SO_PEERCRED` and is
  refused unless the listener runs as the daemon's effective user.
- Rejected: refusing a `socket_path` whose directory is not the user's own
  and 0700. It would refuse setups that are safe (a private directory owned
  by a group) and say nothing about who listens.
- Test: the decision is unit-tested (`same_user`); a test cannot listen as
  another user without root, so the refusal itself is not run.

## A pane's rescue file is always a new private file (2026-09-22)

The 2026-09-22 audit (P3-011): when Neovim refused to write a buffer, the pane
wrote its text to `<file>.unsaved` with `truncate`, so an earlier rescue was
overwritten, a symlink at that name was followed, and a file that existed
kept its old mode although the docs promise 0600.

- Chosen: the rescue takes the first free name of `<file>.unsaved`,
  `<file>.unsaved-1`, … (for a buffer with no file, `unsaved-<time>.md`,
  `unsaved-<time>-1.md`, … in the state directory), created with `O_EXCL`,
  which refuses any file or symlink already at the name, and mode 0600.
- Test: `a_rescue_takes_a_new_private_file` plants a 0644 rescue and a
  symlink at the next name; both stay untouched and the text lands, 0600, in
  the name after them. It fails on the old code.

## The pane's shared state survives a panicked thread (2026-09-22)

The 2026-09-22 audit (P2-008): `audio.rs` and `recorder.rs` recover the guard
of a poisoned mutex, and `pane/host.rs` panicked on one with `expect`. A panic
on the pane thread while it held the waker or the close record would then
also panic the daemon's editor thread at its next question.

- Chosen: `host.rs` recovers the guard too, and logs it. That is sound here:
  every critical section on its `Shared` is one assignment, `take` or read
  of an `Option`, so a panic inside one cannot leave the value half written.
- Test: `a_poisoned_close_record_is_still_read`.

## The control socket bounds a whole request and survives a failed accept (2026-09-22)

The 2026-09-22 audit found three weak spots in `shell/control.rs`:

- P3-014: the 500 ms limit on a client's request was a read timeout, which
  restarts with every byte, so a client sending one byte every 499 ms held
  the one-at-a-time server for about 32 s. Chosen: one deadline for the
  whole line, from the accept (and for the client, one for the whole reply).
  Test: a client that trickles a byte every 200 ms no longer delays the next
  press past the 500 ms limit; on the old code that press timed out.
- P3-023: any `accept` error other than `EAGAIN`/`EINTR` ended the control
  thread, and with it the daemon, under `spokenpad.socket`. Chosen: only an
  error that means the listening socket is broken (`EBADF`, `EINVAL`,
  `ENOTSOCK`, `EOPNOTSUPP`, `EFAULT`) ends it; anything else (`EMFILE`,
  `ENFILE`, `ENOBUFS`, `ECONNABORTED`, …) is logged once while it lasts and
  retried every 100 ms. The same holds while the daemon refuses presses
  before it can serve.
- P3-019: a daemon that does not know a request is almost always one left
  running across a package upgrade; the CLI now says so and names
  `systemctl --user restart spokenpad`.

## The bundled init no longer sends yanks to the clipboard (2026-09-22)

The 2026-09-22 audit (P3-015): `dictation_init.lua` set
`clipboard=unnamedplus`, so with `nvim.init = "bundled"` every `y`, `d` or
`x` in the dictation window also went to the `+` selection, where a clipboard
manager keeps it. The hard rule is that spokenpad's only clipboard write is
the opt-in whole-buffer copy (`nvim.copy_to_clipboard`); the comment above
the option still described the always-on copy of before 2026-09-21.

- Chosen: the option is left at nvim's default. Text reaches the clipboard
  when the user asks, with `"+y`.
- Test: `the_bundled_init_leaves_yanks_off_the_clipboard` starts nvim with
  the bundled init and reads `clipboard`; it read `unnamedplus` before.

## The build checks the sherpa-onnx archive everywhere; CI is pinned (2026-09-22)

The 2026-09-22 audit (P2-015, P2-016, P3-017): only the Arch package checked
the prebuilt sherpa-onnx/onnxruntime archive that is linked into the binary
against a sha256; CI and a plain `cargo build` let `sherpa-onnx-sys` download
it unchecked. CI ran third-party actions by movable tag with the default
token permissions.

- Chosen: `packaging/sherpa-archive.sh DIR` downloads the archive over HTTPS
  and checks it against the sum in `packaging/aur/PKGBUILD`, which it reads
  by sourcing the PKGBUILD, so the version and the sum stay written down in
  one place; `SHERPA_ONNX_ARCHIVE_DIR=DIR` makes the build copy it from
  there. CI runs it before the build, with a new rust-cache key so no cache
  from an unchecked download is reused (the build reuses an unpacked
  `target/sherpa-onnx-prebuilt` without looking at the archive). The README
  gives the same two lines for source builds.
- Chosen: `actions/checkout` and `Swatinem/rust-cache` pinned to the commits
  of v4.4.0 and v2.9.2 (looked up with `gh api`), tags in comments;
  `permissions: contents: read` for the workflow.

## The package pulls in a clipboard tool and notify-send (2026-09-22)

The fresh-user walkthrough ([2026-09-22](experiments/2026-09-22-fresh-user-walkthrough.md)):
without `xclip`, `xsel` or `wl-clipboard`, `"+y` in the pane copied nothing,
and copying is the only way text leaves it (B1); when no window can open, a
desktop notification is the only thing a user sees, and `libnotify` was
optional (C10).

- Chosen: `xclip` and `libnotify` are dependencies. The pane is an X11
  window everywhere (Xwayland on Wayland), so `xclip` serves it on every
  desktop; `wl-clipboard` stays optional, for `spokenpad editor` in a
  Wayland terminal. `xsel` is dropped from the list. The README says to
  copy with `"+y`.
- Chosen: the package is x86-64 only, and the README no longer claims
  aarch64. sherpa-onnx publishes an aarch64 static archive for 1.13.8, but
  no aarch64 build has been made or run.
- Chosen: `THIRD-PARTY.md`, installed beside `LICENSE`, names what the
  binary links and under which licence, read from the symbols of a release
  build: sherpa-onnx, kaldi-native-fbank, kaldi-decoder, kaldifst, OpenFst
  and SentencePiece (Apache-2.0), ONNX Runtime and piper-phonemize (MIT),
  KISS FFT (BSD-3-Clause), and eSpeak NG with ucd-tools
  (GPL-3.0-or-later), which sherpa-onnx's text-to-speech brings in although
  spokenpad never calls it; and the models' licences (Parakeet TDT 0.6B v3
  CC-BY-4.0 per NVIDIA's model card, Silero VAD MIT). The PKGBUILD's
  `license` lists them. Open: whether to accept GPL-3.0-or-later for the
  binary or build sherpa-onnx without text-to-speech is the author's call.
- Chosen: a three-line `post_install` message naming the one step left
  (bind keys) and where the examples are; pacman shows it, the README
  cannot.
- `Cargo.toml` carries `repository`, `readme`, keywords and categories, and
  `publish = false`; `rust-version` stays 1.88, the newest minimum among
  the dependencies and what `as_chunks` and let chains need.

## A quit in the pane is dated by the key that asked for it (2026-09-22)

`tests/pane_daemon.rs`'s `closing_the_pane_cancels_the_capture_it_showed`
failed about one run in five under CPU load, at "the fourth capture's text
in a new pane". The daemon's log showed why: `:q<CR>` in the pane, then a
press that started a new capture, then the pane's record of the user's
close, dated when Neovim's `VimLeavePre` notice reached the pane thread —
after the press — so the state machine took the close for the new capture
and cancelled it. A user with a Neovim config that is slow to quit, pressing
the key right after `:q`, loses that dictation the same way.

- Chosen: the X watcher stamps every event when it reads it, and a quit
  Neovim announces is dated by the last key or click the pane forwarded to
  it, which is what told it to quit; a `WM_DELETE_WINDOW` is dated when it
  arrived. Neither waits for the pane's own loop.
- Rejected: X server timestamps. They are exact, but need a round trip at
  open to map the server's clock onto the daemon's, for a gain of the few
  milliseconds between the server sending a key and the watcher reading it.
- Test: the closing test now presses the key once Neovim has closed its
  socket (it does so before it waits two seconds for its job), the order a
  user produces. Under the same CPU load, 6 of 6 runs of the whole file
  passed, against 1 failure in 5 before.

## No double space where a sentence continues (2026-09-22)

The 2026-09-22 audit (P3-020): with `text.trailing_space`, every commit ends
in a space, and the join that continues a paragraph added another, so each
seam of one utterance read "word  word". Both the editor's append
(`spokenpad.lua`) and the pending passage (`passage::append_paragraph`),
which must write the same file, now join with no space when the last line
already ends in whitespace (Lua's `%s`). Test: the shared paragraph cases
gained two seams; `paragraphs_follow_the_editors_rule` failed on them
before, and `the_detached_paragraph_rule_matches_the_editors` holds the Lua
to the same result.

## An editor no pane shows is stopped, not dictated into (2026-09-22)

Left open by the `:restart` entry above: after `:restart` in the pane, the
old Neovim exits and a new server with the same arguments waits on the
pane's socket for a UI that never comes. It carries spokenpad's marker, so
the next press adopted it, and the dictation went into an editor nobody
could see.

- Chosen: in pane mode, an editor of spokenpad's on the socket with no UI
  attached (`nvim_list_uis()` empty) is told to `qall!`, its socket is
  cleared, and a new pane opens. Every editor spokenpad starts in pane mode
  is drawn by a pane, and one the user opened with `spokenpad editor` has
  its terminal as a UI, so only an invisible one matches; it has had no
  window to be typed into, so it holds nothing to lose.
- Rejected: handling the `restart` UI event, which would re-attach the pane
  to the new server: more protocol for a command the dictation window does
  not need.
- Test: `a_pane_mode_editor_with_no_window_is_stopped_not_adopted`; before
  the change the session adopted the headless editor and returned its file.

## The pane takes the display the user manager has now (2026-09-22)

The fresh-user walkthrough (C4): the daemon, a systemd service, keeps the
environment the user manager had when it started. A user who imported
`DISPLAY` after the first press still got "the dictation window could not
open", and the message did not say that a restart was needed.

- Chosen: `shell::nvim::with_manager_session` reads `systemctl --user
  show-environment` (argument vector, 2 s limit) and takes `DISPLAY`,
  `SWAYSOCK` and `XDG_RUNTIME_DIR` from it when the manager has them; the
  socket-activated daemon applies it to the `[nvim]` settings it reloads
  before each pane. A command in a terminal or a test never asks the
  manager, which would hand it the user's own display. The message for a
  missing display says to import `DISPLAY XAUTHORITY` and press again.
- Not done: `XAUTHORITY`. The pane's X connection (libxcb) reads it from the
  daemon's own environment, which a process with threads cannot safely
  change, and x11rb offers no connect with explicit credentials; a cookie
  file imported after the daemon started still takes a restart. The common
  cookie, `~/.Xauthority`, is found without the variable.
- Rejected: the manager's D-Bus API. `systemctl` needs no new dependency,
  and one short process per pane is nothing beside starting Neovim.
- Test: `the_managers_session_replaces_the_one_the_daemon_started_with`
  (the listing's parser; a quoted or empty value is not taken). The hook is
  `reload_from` in `shell/daemon/mod.rs`, which adds it only for a control
  socket systemd passed in (`Socket::Inherited`).

## A press starts the socket it finds missing (2026-09-22)

The fresh-user walkthrough (C1): after installing, `spokenpad.socket` is
enabled for the next login only, so a press before then found no socket,
exited 1 and said why on stderr, which a window manager's `exec` throws
away: nothing visible happened.

- Chosen: when `spokenpad start|stop|toggle|cancel` finds nothing listening
  (no socket file, or a refused connection) at the path the unit listens on
  (`/run/user/<uid>/spokenpad.sock`), it runs `systemctl --user start
  spokenpad.socket` once, as an argument vector with a 2 s limit, and sends
  the request again. A press that still fails, for that reason or any other,
  also says why through `notify-send`.
- Only at that path: a daemon started by hand, or a test's socket in a
  temporary directory, is never started or reported for, so no test reaches
  the user's systemd or their notifications.
- Test: `a_press_starts_the_socket_it_finds_missing` (the start makes a
  server appear and the press is served; a failed start is reported once,
  and the error names the command) and `only_the_units_own_socket_is_started`.

## A window that settles nothing is committed whole, live too (2026-09-22)

**Superseded 2026-09-23** by [a tick that reads the whole open tail](#a-tick-reads-the-whole-open-tail-the-window-commit-is-removed-2026-09-23) for the window commit; the `Preview::heard` rule and the per-segment commit (P1-002) stand.

The audit of 2026-09-22 (finding P1-001) traced a latch that stopped itself
while the user was still talking, and `tests/e2e.rs` reproduced it: slow
dictation, short phrases with pauses too short to settle a chunk, and too
little speech to fill one. Once the open tail passed `preview.max_seconds`,
every tick read the same first window of it, nothing in it settled, and
nothing committed. A tick that only commits decodes no preview, so no text
came back, and the silence timeout ended the capture mid-sentence.

- Chosen: a `TickKind::Window` (then `Commits`) window that begins at the committed offset
  and settles nothing is committed whole, as the transcription of a
  recording made before the model was ready already did (`decode_recording`
  now goes through the same tick). A release stops it between segments.
- Chosen: speech the detector found that ends later than any before it in
  the capture counts as speech for the silence timeout (`Preview::heard`),
  beside text the recognizer produced. Rejected: the detector alone, since a
  settled commit's text is also proof and costs nothing to keep.
- Rejected at first: cutting the window at its last pause instead of its
  end. The split the worker sees has merged the pauses away, and the case
  is rare enough that the cut inside a word it may cost is paid seldom.
  Reversed on 2026-09-23, see
  [below](#a-window-that-settles-nothing-is-cut-at-its-last-pause-2026-09-23).

The same audit (P1-002) found that a final decode stamped every segment's
commit with the end of all the audio it held. After the first of several
segments, a recording being transcribed claimed to be done: a daemon stopped
then, or a recognizer error on the next segment, left the rest unlisted, and
the next start skipped it. Each segment now commits at its own speech end.

## A capture the stop cuts short is transcribed at the next start (2026-09-22)

`waiting.tsv` listed only recordings made before the speech model was ready.
A live capture whose release decode had not ended when the stop's wait for
the engine ran out (three seconds), or one still held at the stop, which the
stop cancels, kept its WAV but was on no list: its untranscribed tail was
lost without a word in the log (audit P2-019). `tests/e2e.rs` reproduced
both.

- Chosen: one list of recordings whose text is not all written,
  `Transcription` with a `Stage`, entered by every capture with a WAV at its
  start. It replaces three collections that encoded the same lifecycle by
  which one held a recording (audit P2-001, P3-006). A user's cancel takes a
  capture off it; the stop writes everything still on it, with a log line
  each, and the next start transcribes the rest.
- Rejected: listing live captures while the daemon runs. The list is not
  rewritten per commit, so after a crash the next start would write again
  everything committed since the capture began.

## No log ever holds dictated text (2026-09-22)

DEBUG always reaches `spokenpad.log`, whatever `-v` says, and every
committed chunk, every release tail and every undelivered append was logged
with its text (audit P2-012). The log therefore held a second copy of up to
about 4 MB of dictation that outlived the dictation file, and with `-v` the
journal held one too. The user decided that no log line, at any level,
carries transcript text: lines say how many characters, and where the text
was undelivered they point at the recovery WAV, which holds the audio anyway.

- Rejected: keeping the text in the last-resort "undelivered" lines as a
  rescue copy. The recovery WAV is that copy, and `spokenpad transcribe`
  turns it back into text.

## Directories spokenpad creates are private; the user's are left alone (2026-09-22)

The log, which `main` opens before anything else, created
`$XDG_STATE_HOME/spokenpad` with the umask's 0755, and the daemon lock's
0700 then found it there and changed nothing (audit P2-013). Every file in it
is 0600, but a listing of the dictation and recording directories shows
when and how much the user dictated. Meanwhile the recorder forced a
configured `recording.dir` to 0700 at every press, even one the user shares
(P3-012).

- Chosen: `shell::dirs`. Every directory spokenpad creates, and every parent
  it creates on the way, is 0700 (`create_private`). Its own state directory
  is narrowed to 0700 at each start when an older version left it open
  (`secure_own`), which also covers the default dictation and recording
  directories inside it. A directory that exists is otherwise left as it is;
  the daemon warns once at the start when a recording directory can be
  listed by others.
- `shell/nvim` and `shell/pane` create theirs the same way: the dictation
  directory, the editor socket's directory (where the pending passage's
  lock and pointer live), the bundled init's, and the state directory a
  pane's rescue file falls back to.

## One model family, VAD and preview always on (2026-09-22)

The user removed three features before the first public release.

- `asr.family` and `asr.language`: Whisper and SenseVoice. Both scored
  worse than Parakeet on the five reference clips ([asr.md](asr.md)), neither
  takes hotwords, and Whisper needed a code path of its own to cut windows
  longer than its 30 seconds. `Asr` now holds a `Decoding` instead of a
  `Model` per family; any NeMo transducer still loads from `asr.model_dir`.
- `vad.enabled`: without the detector nothing settles, so nothing commits
  before the release, the capture is held in memory whole, and silence is
  decoded, which is where Parakeet invents "Thank you.". `Pipeline` now
  holds a segmenter rather than an `Option` of one, and a detector that does
  not load leaves the speech model unavailable, as a recognizer that does
  not load does: the window says so, captures are kept, and the next press
  tries again.
- `preview.enabled`: the tick that previews is the tick that commits, so
  turning it off had the same costs as turning the detector off.

A configuration that still sets one of these keys is refused with what
became of it (`config::GONE`), never with serde's bare "unknown field".
`examples/eval.rs --whole` and `examples/corpus.rs --path whole` keep their
whole-capture decode by calling the recognizer directly.

## Every duration is in seconds, and its key says so (2026-09-22)

The configuration spelled a duration three ways: `_ms` (`audio.preroll_ms`,
`audio.postroll_ms`, `preview.interval_ms`), `_s`
(`capture.silence_timeout_s`, `nvim.startup_timeout_s`) and `_seconds`
(`vad.*_seconds`, `preview.max_seconds`), with no rule for which (audit
P2-018). The user decided on seconds, suffixed `_seconds`, everywhere:
`audio.preroll_seconds = 0.25`, `audio.postroll_seconds = 0.25`,
`capture.silence_timeout_seconds = 300`, `preview.interval_seconds = 1.1`,
`nvim.startup_timeout_seconds = 20`.

- Chosen: an old key is refused, never read on: the message names the new
  key and gives the value converted, such as "write `preroll_seconds = 0.25`
  under [audio]" (`Gone::Renamed` in `config::GONE`). Reading both would
  leave two spellings of one setting for good.
- Chosen: the bound every other duration shares is named
  (`LONGEST_SECONDS`, an hour; audit P3-005), and one helper validates a
  duration and names its key in the message.

## Exit code 5 for a pane that cannot open; each command lists its own options (2026-09-22)

`spokenpad check` exited 2, "model files missing", when the pane's
requirements were missing too (audit P2-017), so a script that answers 2
with `spokenpad fetch-models` did that for a missing X server. It now exits
5. The codes are one enum, `Exit` in `main.rs`, and `--help` lists them.

The fresh-user walkthrough found `start`, `stop`, `toggle` and `cancel`
listing `--config`, `--model-dir` and `--log-file`, which they never read,
because those were global (walkthrough P5). They are now options of the
daemon, `transcribe`, `check` and `editor` only, accepted before the command
or after it, and every argument has a description. `fetch-models` reads no
configuration at all: it fetches the pinned files wherever `--dir` says, and
a broken configuration no longer stops it (audit P3-018). `run` dispatches
to one function per command (audit P2-004).

## The download's percentage and a config's error go in the headline; a cancel says so (2026-09-22)

The fresh-user walkthrough found two notices whose point never reached the
default pane (walkthrough C2, C3): the winbar draws a notice's headline
always and its detail only when all of it fits, and at 72 columns beside the
phase and the meter that is about 17 characters. The download's percentage
and the reason a config did not load were in the detail.

- Chosen: those two headlines carry their figure: "downloading the speech
  model, 37%" and "config not reloaded: unknown field `pane_dimension`
  (line 3)". The reason is `config::summary`: the first clause of the error,
  with the TOML line, at most 48 characters; the log and `spokenpad check`
  keep the whole report. Rejected: a narrower winbar layout, which is the
  pane's (lane of `src/lua/spokenpad.lua`) and would not have made a
  multi-line TOML report fit either.
- Chosen: `spokenpad cancel` leaves "recording cancelled" in the winbar until
  the next press (walkthrough P8); before, the preview vanished and nothing
  said why. It ranks above only "preview paused". Closing the pane, which
  also cancels, sets no notice: that window is gone, and the desktop
  notification says it.

## The example config is a quickstart; the reasons live in docs/configuration.md (2026-09-22)

`config.example.toml` had grown to about 500 lines, four in five of them
comments: measurements, incident dates, and implementation detail before the
line a new user needed (audit P2-003). It is now each key at its default with
one or two lines on what it does and when to change it, about 170 lines. The
reasons and the numbers moved to [configuration.md](configuration.md), per
section and key, with links to the experiments and decisions they come from.
Paths that default to an XDG location are shown commented out, so that a
copied file keeps following `$XDG_DATA_HOME` and `$XDG_STATE_HOME`, and the
default models keep downloading.

## The daemon is a module per owner; a pass of its loop is a method per step (2026-09-22)

`shell/daemon.rs` had grown to about 2000 lines: the composition root, the
bodies of the inference and editor threads, the recording bookkeeping, and a
`serve` of about 500 lines with four levels of nesting (audit P2-009,
P2-002). It is now `shell/daemon/`: `mod.rs` keeps `run`, `serve` and the
event loop, and `engine.rs`, `editor.rs`, `capture.rs`,
`transcriptions.rs`, `requests.rs` and `lock.rs` each hold one owner or one
piece of bookkeeping. `serve` builds a `Loop` and runs it; each pass calls,
in order, `poll_device`, `take_requests`, `deliver`, `tick`,
`dispatch_recordings`, and `show`, and `shut_down` is the stop. The bounds on
how many requests and results one pass takes are named (audit P3-007).
Nothing changes in behaviour, and the public paths the tests use
(`shell::daemon::{serve, Devices, PipelineSource, Loader, Reload}`) stay.
Rejected: moving the editor thread into `shell/nvim` as the audit suggested;
it is the daemon's use of an editor session, and `shell/nvim` is another
owner's.

## `spokenpad check` lists the microphones (2026-09-22)

The fresh-user walkthrough (C8) found no way to learn which input device the
daemon would open, or which names `audio.device` could match: a wrong one
showed only as "nearly silent" after a press. `check` now lists every input
device PortAudio sees, with its host API, marks the one the daemon opens for
`audio.device` (or the default), and says why it opens none when the query
matches none or several. It initialises PortAudio to list them and opens no
stream. The marking is `shell::audio::listing`, a pure function over the
same `match_device_query` the daemon uses, so the two cannot disagree.

## A failed release decode leaves the capture to the next start (2026-09-23)

A live capture whose release decode failed, for example a recognizer error on
the second of three segments, was taken off the list of recordings whose
text is not all written. Only "decode failed" reached the log: no notice, no
line in `waiting.tsv`, and its WAV was not kept from pruning, although each
segment commits at its own speech end and the list knew exactly how far the
text reached. A failed transcription of a recording already went through
the retry path.

- Chosen: a failed release decode is treated like a failed transcription
  that got further than the attempt before, since it was the first attempt:
  the capture is kept from pruning, listed from where its text reaches, and
  the window says "recording partly transcribed; the next start tries the
  rest again". If the next start fails at the same point, the notice gives
  the `spokenpad transcribe --from` that recovers it, as for any recording.
- Test: `a_capture_whose_release_decode_fails_is_left_to_the_next_start`.

## A window that settles nothing is cut at its last pause (2026-09-23)

**Superseded 2026-09-23** by [a tick that reads the whole open tail](#a-tick-reads-the-whole-open-tail-the-window-commit-is-removed-2026-09-23).

The review of 2026-09-23 found two faults in the whole-window commit above.
It cut the window at its end, which falls inside a word about as often as
the user is speaking. Worse, a word begun less than `vad.min_speech_seconds`
before that end is not yet a detector span: it fell under the closing empty
commit and was never decoded. And a release during the commit threw away the
segment whose decode had just finished, so the release decoded it again,
up to 30 s of audio.

- Chosen: `merge_spans` also returns the slice's last pause (`Split::pause`):
  the end of the last span that has silence after it, either before the next
  span or, for the last one, for at least `vad.pad_seconds` before the slice
  ends. That rule leaves out a span the detector closed only because the
  slice ended, and one it cut at its longest span with no gap. With the
  pause come the windows that decode everything before it, merged by the
  same policy and padded by at most `vad.pad_seconds`, never into the speech
  that follows. A `TickKind::Window` window that settles nothing is
  committed through that pause; the rest stays for the next tick. Only a
  window with no pause is still committed through its end.
- Chosen: a release during that commit keeps the segment whose decode it
  waited for, as a settled chunk is kept, and stops before the next one. A
  cancel still throws it away. `decode_segments` takes separate predicates
  for "decode no further segment" and "throw away the one just decoded".
- Kept: the commit runs only when the tick settled nothing and its slice
  begins at the committed offset, so no committed audio is decoded again.
- Rejected: running the detector again over the audio up to the pause. The
  merge already knows the spans, and a second detector pass would lengthen
  the uninterruptible work a release can wait behind.
- Test: `a_window_that_settles_nothing_is_cut_at_its_last_pause` (every loud
  sample decoded once, the short word included),
  `a_release_during_a_window_committed_whole_keeps_the_decoded_segment`,
  and `the_last_pause_follows_the_last_span_with_silence_after_it`.
- Measured ([experiment](experiments/2026-09-23-decode-fixes-corpus.md)):
  at the default 30 s no capture of the corpus reaches this path, and no
  committed word changes. Forced with `preview.max_seconds = 10`, the cuts
  cost 2.2 WER points, five chunks lost after the retry and 53 words lost
  at the ends, mostly because the audio right after a cut decodes to
  nothing. Open: lead-in silence for the decode after a cut.

## Exit code 6 for a missing model; 2 is clap's (2026-09-23)

`--help` and the README said exit 2 meant a missing model file, but clap
exits 2 on every command line it cannot parse, so a script could not tell a
typo from a missing model. A missing model now exits 6, the first free code;
2 means only a command line spokenpad cannot parse. `--help`, the README and
`rust.md` list every code the same way.

- Rejected: making clap exit with another code. 2 is the convention for a
  usage error, and clap's own.
- Test: `a_usage_error_is_exit_two` and
  `invalid_config_fails_before_devices_and_missing_model_is_exit_six`.

## Only `start` and `toggle` start the socket they find missing (2026-09-23)

The entry of 2026-09-22 above had every control command start
`spokenpad.socket` when it found nothing listening. A `stop` or a `cancel`
with no daemon has nothing to end, yet it started one; and a daemon that
crashes at start was started again by every key-up as well as every press.

- Chosen: only a request that may begin a capture (`Request::may_begin`:
  `start`, `toggle`) starts the unit and notifies. `stop` and `cancel` that
  find no daemon exit 1 and say so on stderr, as at any other path, without
  a desktop notification.
- Not fixed by this: a first press so quick that its `stop` reaches the
  daemon systemd starts before its `start` still leaves a recording
  running; with the socket active, both wait on it in the order they
  connected.
- Test: `only_start_and_toggle_start_the_socket`.

## The pane takes the manager's display with a config that does not load (2026-09-23)

The entry of 2026-09-22 above applied the user manager's session only to a
config file that reloaded. When the file no longer loaded, the next window
kept the settings in use, with the display the daemon started with: after
importing `DISPLAY` it could still say that neither spokenpad nor the user
manager had one.

- Chosen: `Reload` is now two steps, the file (`load`) and the session the
  window opens in (`session`), and the editor thread applies the second to
  whichever settings the window opens with: the file's, or those in use when
  the file does not load. `session` is `with_manager_session` only for a
  control socket systemd passed in (`Socket::Inherited`), and the settings
  unchanged otherwise, as before.
- Behaviour, stated once: the next pane opens on the display the user
  manager got last, not on the one of the session that pressed the key. A
  second X login of the same user that imports its `DISPLAY` (say `:1` on
  tty2) moves the first session's next pane to `:1`. Only the same user is
  affected: the manager is per user, and so is the daemon.
- Test: `a_config_that_does_not_load_still_takes_the_managers_session`.

## A starting `spokenpad editor` is never stopped as invisible (2026-09-23)

The review of 2026-09-23 asked whether a press in pane mode can `qall!` an
editor the user has just started with `spokenpad editor`, before its
terminal UI attaches: `stop_invisible` stops an editor of spokenpad's with
no UI. Measured on Neovim 0.12.5
([experiment](experiments/2026-09-23-editor-marker-before-ui.md)): the
terminal UI attaches before Neovim runs the `--cmd` that sets the marker,
so the editor is spokenpad's only once it has a UI, and cannot be stopped
while it starts.

- Chosen: no change.
- Rejected: stopping only an editor this daemon spawned for a pane, or only
  one that has had no UI for a grace period. Neither is needed for the race
  asked about, and the first would also spare what `:restart` leaves behind.
- Open: the same measurement suggests that the server `:restart` leaves
  behind carries no marker either, since it waits for a UI before `--cmd`,
  and would then be refused as unrelated rather than stopped. Not probed
  with a real `:restart`.

## A window tick holds a full window, checked in the core (2026-09-23)

**Superseded 2026-09-23** by [a tick that reads the whole open tail](#a-tick-reads-the-whole-open-tail-the-window-commit-is-removed-2026-09-23).

The review of 2026-09-23 found that `examples/corpus.rs` without
`--previews` ticked every tail with the kind the daemon uses only for a
tail longer than `preview.max_seconds`. On a short tail that settled
nothing, the replay committed the tail through its last pause, which the
daemon never does, so its numbers were not the daemon's. The worker itself
did not check that such a tick held a full window; only the daemon's loop
made sure.

- Chosen: the worker knows its window (`Worker::new(pipeline, window)`,
  `config.preview.window(rate)`), and `Worker::tick` refuses a
  `TickKind::Window` tick that does not hold exactly that many samples,
  before it decodes anything. `TickKind::for_tail` is the one rule for the
  kind, which the daemon's loop and the replay both call.
- Chosen: a third kind, `TickKind::Settled`: a preview tick without the
  cosmetic decode. The replay uses it without `--previews`; it commits what
  a preview tick commits (`a_settled_tick_commits_what_a_preview_tick_commits`).
- Rejected: letting the worker derive the kind from the audio it is given.
  The loop truncates a snapshot to the window before it crosses the thread,
  so a truncated long tail and a tail of exactly a window look the same.
- Tests: `a_window_tick_that_is_not_the_workers_window_is_refused`,
  `only_a_tail_longer_than_a_window_is_read_a_window_at_a_time`,
  `the_replay_ticks_as_the_daemon_does`.

## What `:restart` leaves is spokenpad's by its command line (2026-09-23)

The open question of the entry above, probed with a real `:restart` in an
embedded pane on Neovim 0.12.5 (`a_restart_is_not_the_users_close` in
`tests/pane_render.rs`): the server `:restart` leaves on the pane's socket
has no UI and no `g:spokenpad_owner`, because it waits for its first UI
before it runs `--cmd`. The next press refused it as an unrelated editor
("refusing unrelated nvim socket"), so no pane opened, and the server ran
until the user killed it. "An editor no pane shows is stopped" held
only for editors that had run their `--cmd`.

- Chosen: the ownership query also returns `v:argv`, which Neovim sets
  before it runs any of it. In pane mode an editor with no UI whose command
  line carries `--cmd` with this socket's marker is stopped as an invisible
  editor of spokenpad's, once it has shown no UI for `UI_GRACE` (1 s).
- Why the grace: `spokenpad editor` runs the same `--cmd`, and its server
  listens a moment before its terminal attaches. One that gains a UI within
  the grace is refused as before and keeps running.
- Rejected: telling the two apart by their command line. Both carry
  `--embed`: the terminal UI starts its server with it.
- Tests: the extended `a_restart_is_not_the_users_close` (the server is
  stopped and a new pane opens), `a_server_waiting_for_its_first_ui_is_stopped_in_pane_mode`,
  `a_starting_editor_that_gains_a_ui_is_not_stopped`.

## A tick reads the whole open tail; the window commit is removed (2026-09-23)

The window commit (three entries above) fixed P1-001 by cutting a window of
`preview.max_seconds` that settled nothing at its last pause. On the local
corpus, forced to run with `preview.max_seconds = 10`, it cost 2.2 WER
points and 53 words lost at the ends: the audio after a cut decoded to
nothing. The cause of P1-001 was not a missing cut but the truncation
itself: a tick read only the first `preview.max_seconds` of the tail, and in
slow dictation that slice never held a chunk's speech or a settling pause.

- Chosen: every tick hands the worker the whole open tail, and the detector
  reads all of it. `merge_spans` closes chunks by its usual rules, and only
  settled chunks are decoded, however long the audio a settled chunk spans.
  Every committed boundary is one a release of the same audio would have
  cut. `preview.max_seconds` bounds only the cosmetic decode: a longer tail
  gets a `TickKind::Settled` tick, which commits and does not preview.
- Removed: `TickKind::Window`, `Worker::commit_window`, `Split::pause`, the
  worker's window and its check, and the pipeline's split between
  "decode no further segment" and "throw away the one just decoded", which
  only the window commit used. A recording made before the model was ready
  is read `preview.max_seconds` at a time, with a tick over its whole held
  tail after each read, and never cut where a read ended.
- Kept: speech the detector hears resets the silence timeout
  (`Preview::heard`), and each segment of a final decode commits at its own
  speech end.
- Rejected, for now: running the detector incrementally, keeping its state
  and spans for the part of the tail it has read. A pass costs about 4 ms
  per second of tail on an idle machine (0.12 s at 30 s, 1.1 s at 280 s,
  about the longest tail the chunk rules leave open), and the tick schedule
  already keeps the worker idle half the time. An incremental scan that
  gives exactly the spans of a fresh one needs where the detector's open
  span began, which sherpa-onnx 1.13.8 does not expose.
- Measured ([experiment](experiments/2026-09-23-decode-fixes-corpus.md)):
  the committed text of all 181 captures is the same at `max_seconds` 30, 20
  and 10, and the same as `main` and the window commit at the default. At
  10 s it is 1.7 WER points and 52 lost words better than the window
  commit, and 0.52 points worse than `main` at 10 s, whose truncated slices
  cut different windows on 52 captures; that configuration commits nothing
  in slow dictation, so it is no alternative.
- Tests: `slow_dictation_past_the_preview_bound_commits_whole_chunks_before_the_release`
  (core, the real merge policy: commits before the release, where one decode
  of the whole capture ends its chunks, every loud sample once; fails on a
  truncated tick),
  `slow_dictation_past_the_preview_bound_commits_whole_chunks_and_keeps_recording`
  (e2e, through the daemon's loop and a real nvim; fails on a truncated tick
  with "stopped after silence"),
  `a_tick_that_settles_nothing_commits_nothing_however_long_the_tail`,
  `a_recording_with_nothing_settled_is_decoded_whole_at_its_end`.

## The daemon is `spokenpad daemon`; bare `spokenpad` prints the help (2026-09-23)

`spokenpad` with no command ran the daemon, and `--dump-audio` was a
top-level option that only the daemon read. A user who typed `spokenpad` to
see what it does started a second daemon, which exited 3 against the one
systemd runs, or took the microphone when none ran.

- Chosen: `spokenpad daemon` runs the daemon and takes `--dump-audio DIR`,
  besides the options `transcribe` and `check` take. `spokenpad` alone
  prints the help to stderr and exits 2 (clap's `arg_required_else_help`
  on a required command); options without a command are the same usage
  error. `spokenpad.service` and `packaging/systemd/dev.conf.example` run
  `spokenpad daemon`.
- Rejected: `daemon start`/`daemon stop`. systemd owns the daemon's
  lifecycle (`spokenpad.socket`, `systemctl --user restart spokenpad`), and
  the key bindings' `start`, `stop`, `toggle` and `cancel` stay top-level.
- Upgrading: a copied unit or drop-in whose `ExecStart` names the bare
  binary now prints the help and fails; it needs ` daemon` appended.
- Test: `no_command_prints_the_help_and_starts_nothing`,
  `each_command_lists_only_the_options_it_reads`, and the socket-activation
  tests in `tests/cli.rs`, which start `spokenpad daemon` as the unit does.

## Ctrl+V in the pane pastes as a terminal does (2026-09-23)

In the user's terminal, Ctrl+V in Insert mode pastes the clipboard. In the
pane it was Neovim's `<C-v>`, which inserts the next key literally, so the
habit did nothing useful there. The user's decision: Ctrl+V in Insert mode
pastes; in Normal mode it stays Visual block.

- Chosen: the pane, which plays the terminal's part, decides. `mode_change`
  redraw events now carry the mode's name, folded into `core::grid::Mode`
  (`Normal`, `Insert`, `CommandLine`), and `core::keys::press` makes Ctrl+V
  a paste in `Insert` and `CommandLine`, Ctrl+Shift+V one in every mode,
  and every other key what `notation` spells. The paste runs
  `nvim_paste(getreg('+'), true, -1)` inside the embedded nvim through
  `nvim_exec_lua`, as a request nothing waits for: the clipboard is read by
  nvim's own provider, spokenpad starts no clipboard program, and the text
  arrives as a bracketed paste, untouched by mappings and auto-indent.
- Ordering: Neovim queues an `nvim_input` key as it arrives and reads
  queued keys before it runs a queued request, so keys typed before Ctrl+V
  land first. This is how Neovim's own TUI sends a paste beside keys.
- Rejected: sending the paste as `<Cmd>lua ...<CR>` through `nvim_input`.
  It keeps even the keys after the paste in order, but a command waiting
  for a key takes `<Cmd>` as that key: after `<C-r>`, `<C-k>` or `<C-q>` in
  Insert mode, or `r` in Normal mode, the Lua text went into the buffer
  (tried on nvim 0.12.5, headless).
- Rejected: reading the mode with `nvim_get_mode` at the press. It is
  answered before queued keys are read, so it is no fresher than the last
  `mode_change`, and the press would wait on a round trip.
- Not handled: a Ctrl+V within one redraw of a mode change goes by the mode
  before it, and a key typed within milliseconds after Ctrl+V can land
  before the paste.
- `docs/constraints.md` now says that the clipboard is read, only on the
  user's key, and only into the pane's own nvim.
- Tests: `ctrl_v_pastes_where_it_would_type_text_and_is_visual_block_elsewhere`,
  `a_mode_change_says_what_typing_does_by_the_modes_name` (core), and
  `ctrl_v_pastes_where_a_terminal_would_and_is_visual_block_elsewhere`
  (`tests/pane_render.rs`: a real pane on Xvfb and i3, XTEST keys, a stub
  clipboard provider).

## The pane's font is 12 pt by default (2026-09-23)

`nvim.font_size` defaulted to Alacritty's own 11.25 pt, so that an
unconfigured pane matched an unconfigured Alacritty. The user asked for 12.

- Chosen: `Points::DEFAULT` is 12 pt. Matching Alacritty's default bought
  nothing in practice: a user who changed Alacritty's size writes the same
  number here, and one who did not reads the pane at a glance beside the
  window they type in, where a slightly larger face helps.
- The default 72x20 cells grow with the font, to about 730x410 pixels at
  96 dpi instead of about 656x368; the arithmetic that turns points into
  cells is unchanged.

## The preview ticks every second (2026-09-23)

`preview.interval_seconds` defaulted to 1.1 s. The user asked for 1.0.

- Chosen: 1.0 s. The real gap between ticks is already
  max(interval − last decode, last decode), so the worker stays idle at
  least half the time whatever the setting. Not measured on the corpus:
  the change is a tenth of a second in how soon settled text and the
  preview appear.
- The default silence timeout, 300 s, stays far above the two ticks
  `capture.silence_timeout_seconds` must exceed.

## The editor's startup timeout is a constant (2026-09-23)

`nvim.startup_timeout_seconds` (20 s) was a setting for one guard: an
editor that never answers — a configuration stuck at a prompt, a plugin
manager installing on first start — must not hold the editor thread
forever. Nobody has a reason to tune it, and the user asked for one setting
fewer.

- Chosen: `shell::nvim::STARTUP_TIMEOUT`, 30 s, for the pane's window and
  its editor together. Longer than the old default because the key that
  let a slow configuration raise it is gone; a first open once took 13.5 s.
  An editor that exits is still noticed at once, so a dead one costs no
  more than before.
- The key, and `startup_timeout_s` before it, is refused with "was removed
  … Delete the line". Tests pass their own deadline where they need one
  (`await_editor` takes it); no test needed a shorter budget.

## No desktop notifications (2026-09-23)

spokenpad sent a desktop notification through `notify-send` in four cases:
text went to the pending passage (no editor, a window that could not open,
an append the editor never confirmed), closing the window cancelled a
recording, a tiled pane opened floating, and a key press reached no daemon.
The user does not want them, "even as a fallback".

- Chosen: none. `nvim.notify` is refused with "was removed … Delete the
  line", and the package no longer depends on `libnotify`. This reverses
  the `libnotify` half of
  [the package pulls in a clipboard tool and notify-send](#the-package-pulls-in-a-clipboard-tool-and-notify-send-2026-09-22).
- What each said goes to the log instead, at warning level where it was a
  notification: `NvimSession::log_detached` gives the file and the reason
  the text is not in a window; a close that cancelled a recording is logged
  with the file the window showed; the tiled-pane warning was logged
  already and now is only logged. A failed key press says why on standard
  error, as it did besides the notification; the CLI writes no log, and a
  window manager may discard its standard error.
- Removed with them: `EditorWork::ClosedMidCapture`, whose only job was the
  notification, the once-per-session flag for the tiled warning, and the
  CLI's `Recovery` pair, which is now the one closure that starts the
  socket.

