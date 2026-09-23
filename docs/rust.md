# Rust implementation

spokenpad is one Rust binary: the `daemon`, `editor`, `transcribe`,
`check` and `fetch-models` commands, and the control commands the key
bindings run. There is no other implementation — the
Python reference used during the port is retired, see
[decisions.md](decisions.md#the-python-reference-implementation-is-dropped).
It ships as an Arch package (`packaging/aur/PKGBUILD`) whose systemd user
socket starts the daemon on the first press; the binary downloads
its own default models (`fetch-models` subcommand, `core::models` and
`shell::models`, see [decisions.md](decisions.md#model-download-moves-into-the-binary-2026-09-21)).
Evaluation is `examples/eval.rs` (see [evaluation.md](evaluation.md)).

## Ownership and ordering

`src/` is split in two: `core/` holds the pure modules, `shell/` everything
that touches a device, a file, a process or a thread, and `config.rs` sits at
the root because both sides read it. Nothing under `core/` may import
`libc`, PortAudio, sherpa, `std::fs`, `std::process`, `std::net` or
`std::thread`.

- `core/state.rs` contains the pure state machine over the four control
  requests and the clock, including the 120 ms minimum hold, the 150 ms
  repeat window that tells a held key's auto-repeat from a new press, and the
  rule that a cancel after release is a no-op; closing the dictation pane
  cancels as a cancel request does. `core/control.rs` is the
  one-line wire protocol. `core/session.rs`
  handles utterance identity, cancellation, preview scheduling, and the single
  user-visible notice, which the next key press clears — `Notice::priority`
  ranks them and `notify` replaces only upwards, so the worst thing that
  happened to a capture is the thing the user reads. `preview()` and
  `notice()` are separate: the editor draws the preview where its text will
  land and the notice in the winbar, in every phase, as a headline plus a detail it
  appends only when the window is wide enough.
- `shell/daemon/mod.rs` holds `run` and `serve`; the two threads `serve`
  starts are `engine.rs` and `editor.rs` beside it, and its bookkeeping is
  `capture.rs`, `transcriptions.rs`, `requests.rs` and `lock.rs`. `run` is
  the shell: the per-user lock, first the control socket, then signal handlers and
  PortAudio, and a loader for the models that the inference thread runs
  while presses are already taken. `serve` is the event loop, generic over
  the audio backend, recognizer and segmenter, handed a pipeline that is
  built already or a loader, and taking its control requests from a plain
  channel, which is what the headless end-to-end tests drive. Until the
  pipeline is built, captures are kept as their recovery WAVs and decoded
  from them afterwards. Each pass of the loop is a sequence of `Loop`
  methods: poll the device, take the requests, deliver results, tick,
  dispatch kept recordings, drop committed audio, show the winbar. Each request
  reaches the state machine after the clock at its own stamp, so a repeat
  window that closed before the request arrived is closed first. It drains
  requests before inference results and moves
  blocking inference and editor RPC to separate threads; the editor thread runs
  until its own channel says to stop, so text queued before a cancel or a
  shutdown is still written (bounded at three seconds, with anything
  undelivered logged at error level). The editor thread also knows where it
  wrote each capture's last text, so it decides both whether an append
  continues a paragraph and where the editor draws the preview
  (`PreviewPlacement`), by the same test.
- `core/decode.rs` owns the committed sample offset on one worker. Each
  capture is an `Utterance` with one monotone lifecycle — `Live`, then either
  `Released` for its final decode or `Cancelled` — so a later capture cannot
  revive an earlier one and no pair of booleans can disagree. Preview output
  never enters the commit path, and a cancel stops decoding without unwriting
  text the recognizer already produced.
- `shell/inference.rs` wraps the official sherpa Rust API. The safe wrapper,
  FFI bindings, and native runtime are pinned together; startup checks the
  native version. Both models explicitly use the CPU provider. The merge,
  padding and settlement policy it applies to the detector's spans is pure and
  lives in `core/segments.rs`.
- `shell/audio.rs` maintains pre-roll and immutable callback chunks behind an
  `InputBackend` seam, so the capture arithmetic is driven deterministically in
  tests; PortAudio is one implementation of it. Snapshots copy sample data
  outside the callback lock. Everything the loop learns about the device arrives
  as a typed `CaptureEvent` from `poll`, delivered once each: stream restarted
  (with the gap, and whether a capture was running), stream unavailable, memory
  cap reached, device flags. The watchdog also repairs a dead stream while
  *idle*, so the pre-roll is full at the next press; the ring is cleared when a
  capture starts, so audio from before the previous capture cannot be spliced
  into this one. A decode (release, latched stop) keeps capturing for
  `audio.postroll_seconds` (0.25 s, at most 1) after the `stop` or the ending
  press, because speech was still sounding at key-up in 22 of 128 recorded
  captures; audio captured during the repeat window counts towards it. The
  wait counts device frames delivered under the state lock, so it also
  completes past the memory ceiling; it ends early on a waiting `start` or
  `toggle`, or shutdown, gives up after the rest of the post-roll plus 100 ms, and reports a stream
  that went stale as a microphone gap. A discard (tap, cancel) waits only for
  one further device buffer, up to 100 ms. Failure-path teardown aborts the stream rather than stopping it,
  so a wedged device cannot block the event loop; orderly shutdown still stops.
  `discard_before` drops whole device buffers the decoder has committed past,
  so what a capture holds is its open tail rather than its length;
  `snapshot_capture` returns a `Snapshot` that says where the audio it hands
  out begins, and `finish_capture` a `Captured` that carries the capture's
  length and its loudest sample, both of which outlive the audio that was
  dropped. `MAX_UTTERANCE_SECONDS = 3600` is a compile-time constant, not a
  config key, and bounds the *retained* window; reaching it is final for
  that capture, because accepting audio again after a hole would splice two
  moments that were never spoken together, so it ends the capture through
  `Session::cap` and `state::Event::Exhausted`.
- `shell/recorder.rs` writes shared chunks independently before the in-memory
  limit is applied. `start()` never joins the previous writer on the key-press
  path — it detaches it, because that path must not wait on a sick filesystem.
- `core/frames.rs` gives capture-absolute sample offsets their own type,
  `Frames`, distinct from indices into a snapshot, which may begin after the
  capture did.
- `shell/control.rs` listens on `$XDG_RUNTIME_DIR/spokenpad.sock` (mode
  0600), serves one connection at a time in arrival order, stamps each
  request when it is read, queues it for the loop and answers `ok`. Under
  socket activation it takes over the socket systemd passed as descriptor 3
  (`Socket::Inherited`), never probes it and never removes it. A socket it
  binds itself replaces a file nobody answers on; a live one or a non-socket
  is refused.
  The same module is the client `spokenpad start|stop|toggle|cancel` use; that
  path reads no config and loads no model.
- `shell/nvim/mod.rs` owns the socket and pinned dictation buffer in both
  `nvim.mode`s, and `shell/nvim/rpc.rs` the msgpack transport, where every
  call carries an absolute deadline. `shell/nvim/passage.rs` writes commits
  to the pending dictation file while no editor is open. `shell/wm.rs` speaks
  the i3/sway IPC protocol to add the pane's `no_focus` rule on sway, one
  request per connection under a deadline. No module synthesizes input or calls a focus API.

The dictation preview uses the theme's comment foreground with an italic
distinction, without an extra virtual blank line. It remains virtual text,
never file content. Diagnostics are disabled only for the dedicated dictation
buffer, and successful saves are silent; write errors still propagate and roll
back. Ordinary editor buffers and the user's Neovim configuration are unchanged.

Live ticks and recovery use the same segmentation and decode implementation.
Padding can overlap around silence; committed speech is
not decoded again on release. The existing whole-buffer retry after *every*
segment returns empty remains an explicit recovery exception. Preview inference
is cosmetic and may repeat the open tail; it cannot become file content.

Long pauses are independent context boundaries, as specified in
[progressive commit](progressive-commit.md#chunk-construction). Each
recognizer input also receives one second of synthetic zero PCM at its end.
This is an empirical input-context fix for omitted terminal speech, not an
extra recording interval or a second decode. The original capture, saved WAV,
VAD boundaries, and committed offsets are unchanged by these synthetic frames.
Real samples and zero PCM are concatenated before a single waveform call:
the pinned [native offline stream implementation](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.8/sherpa-onnx/csrc/offline-stream.cc#L150-L160)
finalizes feature extraction on that call.

## Configuration and deployment

All sections reject unknown fields, wrong types, non-finite durations, and
invalid ranges. A model path set in a config resolves against that config's
directory; leaving the key out keeps the default under
`$XDG_DATA_HOME/spokenpad/models` (`~/.local/share` if unset), which
`spokenpad fetch-models` fills, and which `check` and `transcribe` fill on
their own before loading a still-default Parakeet or Silero model, and the
daemon in the background while it already records (never for a configured
`model_dir`, whose absence stays the plain "missing model" error). `[asr]`
is parsed into a typed `Decoding`, and a hotword key greedy search cannot
use is rejected. A key spokenpad no longer reads (`config::GONE`) is refused
with what became of it, never as a bare unknown field.
`audio.sample_rate` must be **16000** — the Silero window is 512 samples at
that rate and Parakeet reads it, and nothing resamples in between. `vad.chunk_seconds` must be positive, `preview.max_seconds` at most
3600 (default 30), `capture.silence_timeout_seconds` either 0 (off) or in [1,3600]
(default 300), and `nvim.colorscheme` must match `[A-Za-z0-9_.-]+`, since it
becomes Lua code. Recovery rejects a WAV whose rate differs from the configured
capture rate, as in the original command.

The default config location honours `XDG_CONFIG_HOME` and may be absent, in
which case defaults are used. An explicit `--config PATH` that does not exist is
an **error**: silently running on defaults because a `--config` typo pointed
nowhere is how a user loses their settings.

Command-line flags: `-c/--config PATH` and `--model-dir PATH` for
`daemon`, `transcribe` and `check` (`editor` takes `-c` only), `-v/--verbose`
and `--log-file PATH` (the literal `none` disables the file) for the same
four, given before the command or after it, and, on `daemon` only,
`--dump-audio DIR`, which writes each capture exactly as decoded for
debugging. Subcommands are `start`, `stop`, `toggle`, `cancel` (no options:
they read no config and write no log), `daemon` (what `spokenpad.service`
runs), `editor`, `transcribe <WAV> [--out PATH] [--from SECONDS]`, `check`
and `fetch-models [--dir DIR]`, which reads no configuration either.
`spokenpad` with no command prints the help to stderr and exits 2, clap's
code for a command line without its command.

Exit codes are selected by error *type* (`Exit` in `main.rs`), never by
matching a message, so a reworded error cannot silently turn into a restart
loop: `2` a command line clap cannot parse (clap's own code, which nothing
else uses), `3` another daemon holds `$XDG_STATE_HOME/spokenpad/daemon.lock`
(an `flock`, held for the process lifetime, never unlinked), `4` an
unreadable or wrong-rate WAV handed to `transcribe`, `5` `check` found a
requirement of the pane missing, `6` model files missing (`check`,
`transcribe`), `1` everything else, including a control command that finds
no daemon. `3` is only for a daemon started by hand: once a daemon has
adopted systemd's socket, nothing a user can cause ends it, because each
exit would have the next press start it again until the unit's start limit
failed the socket. While it cannot take the lock it answers every press
with why (`Reply::AnotherDaemon`, or `Reply::CannotLock` for a state
directory it cannot write) and takes the lock as soon as it can. An
invalid config, a missing model or an unopenable microphone do not end the
daemon either: it runs on defaults, keeps the recordings, or retries the
device, and says so in the window.

Run `cargo build --locked --release`. The sherpa crates' `static` feature
downloads the pinned 1.13.8 static libraries (or uses `SHERPA_ONNX_LIB_DIR`)
and links sherpa-onnx and onnxruntime into the executable, so the binary needs
only system libraries (libc, libstdc++, PortAudio) and runs from any
directory; there is no runpath and nothing to keep beside it.
The package installs it as `/usr/bin/spokenpad`, whose `daemon` command
`spokenpad.service` runs when `spokenpad.socket` is first connected to; for development,
`cargo install --locked --path . --root ~/.local` and the drop-in
`packaging/systemd/dev.conf.example` run a build of one's own instead. The
per-user lock above rejects a second daemon. The package build sets
`SHERPA_ONNX_ARCHIVE_DIR`, so sherpa-onnx-sys copies its libraries from the
checked source list instead of downloading them, and an absolute
`CARGO_TARGET_DIR`, which that build script takes as given.

In pane mode (the default) the daemon opens a window it draws itself, with
`nvim --embed` inside it ([nvim-window.md](nvim-window.md)). In attach mode
the daemon opens no window: the user starts the editor with
`spokenpad editor`, which writes an ownership marker beside the socket and
then execs nvim. Reattaching requires evidence that the editor belongs to spokenpad; an
arbitrary nvim socket is refused. Fresh editors must also finish the startup
handlers registered before our final one-shot `VimEnter` callback. Both
ownership and readiness require the generated session nonce; an early RPC
response alone is not proof of startup completion.

## Verification

`cargo test --locked --all-targets` covers pure transitions and worker
ordering, capture arithmetic against a synthetic input backend, recording
recovery and pruning with temporary files, and real headless nvim RPC tests. No
test uses the actual microphone, takes the daemon lock, binds the service's
control socket, or touches the real state directory, so the suite runs while
the user's own service does.

`tests/e2e.rs` drives `shell::daemon::serve` end to end with two
substitutions and nothing else — a synthetic microphone and a counting
recognizer — against a real `nvim --headless` over msgpack-RPC. Requests
arrive on the channel the control socket feeds, or through a real control
socket from the real `spokenpad start|stop|toggle` commands. It
asserts the things only the whole loop can show: text landing in the file,
progressive commits appending before release, a too-short tap discarded with a
notice the idle winbar really renders, a latched recording ending on the second
press, auto-repeat stop/start pairs in either order staying one capture, a
cancel keeping
committed text and the WAV, a second press during transcription starting a new
paragraph, the preview staying virtual text, and a microphone restart marking
the gap while keeping the audio. One further test loads the real CPU models and
is `#[ignore]`d; it needs the default model directory filled by
`spokenpad fetch-models`:

```sh
cargo test --locked --test e2e -- --ignored
```

The nvim-dependent tests **fail** when nvim is missing rather than reporting a
green suite; `SPOKENPAD_ALLOW_MISSING_NVIM=1` skips them deliberately.

Until 2026-09-21 a differential check compared exact segment bounds,
settlement, transcripts, progressive commit offsets and the release remainder
against the Python reference used during the port; it validated the port on
all five evaluation WAVs and a 102.5 s simulated live passage, and is retired
with that reference. Its Rust half, `examples/verify_native.rs`, was removed
on 2026-09-22: `examples/corpus.rs` counts what it showed by hand.

The follow-up regression reproduced the invisible first preview and delayed
scrolling on an attached 40×10 Neovim grid. It now checks visible first-preview
text, the end of a long wrapped committed paragraph, growing preview text,
and preserved scrollback. The bundled/Tokyonight graphical smoke also passed
exact file saving, unchanged desktop focus, and isolated editor cleanup.

The reported 17-second recording contained the missing phrase in its WAV.
Long-pause separation plus one second of decoder-only silence recovered both
sentences in direct recovery and progressive decoding. Against the five local
reference recordings, aggregate VAD-path WER stayed at 17.6%, but this is not
per-recording equivalence: the short `cd home` case improved from 100% to 0%,
while one previously matching recording gained a word error (3.4%). Without VAD,
WER moved from 13.4% before that change to 13.9% after it. A shorter 0.5-second
pad was rejected because it made the short command decode empty.

**Current accuracy figures**, re-measured 2026-09-21 with `cargo run --release
--example=eval` on the five verified references, with the default
`greedy_search`: **18.7%** aggregate WER through the VAD path the daemon uses
and **14.3%** whole-buffer. Earlier figures used beam search, which drops
speech on real captures; see
[decisions.md](decisions.md#greedy-decoding-by-default-beam-search-drops-speech-2026-09-21). Five clips of one speaker
are a regression proxy, not a general accuracy guarantee — see
[evaluation.md](evaluation.md).

`cargo test --locked --all-targets` runs the library's unit tests, the CLI
tests, the end-to-end tests and the pane tests, with the tests that load the
real models or measure RSS ignored by default, plus the unit tests of the
examples; on 2026-09-23 that was 336 library, 14 CLI and 37 end-to-end
tests. Strict all-target clippy and rustfmt checks apply.
`cargo run --example pane` opens the pane on a given display, and can
write a screenshot of it.

The Rust daemon has been the live service since 2026-09-09. Startup completed
on the actual microphone without a callback-timeout warning, with no service
restarts or watchdog warnings during the post-switch check. (Until 2026-09-21
it also read the keyboard through evdev, read-only.)

The first live Rust dictation captured 89.3s of audio for an 89.2s hold;
release decoded the remaining 2.6s in 0.27s, followed by a 33ms editor append.
This is a live latency observation, not just the saved-recording simulation.

Historical design and latency measurements in the other documents describe
the Python implementation unless identified as Rust measurements. Tests and
saved recordings cannot establish live microphone reliability; that requires
dictation through the running Rust service.

Native API sources: [sherpa Rust wrapper](https://docs.rs/sherpa-onnx/1.13.8/sherpa_onnx/),
[i3 IPC](https://i3wm.org/docs/ipc.html).
