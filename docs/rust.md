# Rust implementation

The Rust runtime implements the daemon and the `transcribe` command.
It keeps the same Parakeet TDT checkpoint, CPU provider, six inference threads,
Silero segmentation, TOML sections, read-only evdev input, and nvim Lua UI.
The Python package retains only ASR/VAD/decode reference functions and shared
evaluation helpers; it is not an executable daemon and is never called by the
Rust runtime. Setup/evaluation scripts remain Python.

## Ownership and ordering

`src/` is split in two: `core/` holds the pure modules, `shell/` everything
that touches a device, a file, a process or a thread, and `config.rs` sits at
the root because both sides read it. Nothing under `core/` may import `evdev`,
`libc`, PortAudio, sherpa, `std::fs`, `std::process`, `std::net` or
`std::thread`.

- `core/state.rs` contains the pure state machine, including the 120 ms minimum
  hold and the rule that a cancel after release is a no-op; `core/session.rs`
  handles utterance identity, cancellation, preview scheduling, and the single
  user-visible notice, which the next key press clears — `Notice::priority`
  ranks them and `notify` replaces only upwards, so the worst thing that
  happened to a capture is the thing the user reads. `preview()` and
  `notice()` are separate: the editor draws the preview below the transcript
  and the notice in the winbar, in every phase, as a headline plus a detail it
  appends only when the window is wide enough.
- `shell/daemon.rs` splits into `run` and `serve`. `run` is the shell: the
  per-user lock, signal handlers, PortAudio, evdev, and the models. `serve` is
  the event loop, generic over the audio backend, recognizer and segmenter and
  taking its key events from a plain channel, which is what the headless
  end-to-end tests drive. It drains input before inference results and moves
  blocking inference and editor RPC to separate threads; the editor thread runs
  until its own channel says to stop, so text queued before a cancel or a
  shutdown is still written (bounded at three seconds, with anything
  undelivered logged at error level).
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
  lives in `core/segments.rs`, which is what the offline differential check
  compares against the Python splitter.
- `shell/audio.rs` maintains pre-roll and immutable callback chunks behind an
  `InputBackend` seam, so the capture arithmetic is driven deterministically in
  tests; PortAudio is one implementation of it. Snapshots copy sample data
  outside the callback lock. Everything the loop learns about the device arrives
  as a typed `CaptureEvent` from `poll`, delivered once each: stream restarted
  (with the gap, and whether a capture was running), stream unavailable, memory
  cap reached, device flags. The watchdog also repairs a dead stream while
  *idle*, so the pre-roll is full at the next press; the ring is cleared when a
  capture starts, so audio from before the previous capture cannot be spliced
  into this one. A release waits up to 100 ms for the callback already in
  flight, so the partial device buffer at that moment reaches both the capture
  and the WAV. Failure-path teardown aborts the stream rather than stopping it,
  so a wedged device cannot block the event loop; orderly shutdown still stops.
  The in-memory ceiling is `MAX_UTTERANCE_SECONDS = 3600`, a compile-time
  constant, not a config key.
- `shell/recorder.rs` writes shared chunks independently before the in-memory
  limit is applied. `start()` never joins the previous writer on the key-press
  path — it detaches it, because that path must not wait on a sick filesystem.
- `core/frames.rs` gives capture-absolute sample offsets their own type,
  `Frames`, distinct from indices into a snapshot, which may begin after the
  capture did.
- `shell/hotkey.rs` opens every evdev descriptor with `File::open`
  (read-only), scans, polls and reads; it owns no policy. `core/hotkey.rs` has
  the other half: `WatcherState` is a pure fold over the event stream — which
  devices hold the hotkey, and which latch modifiers each reports — and
  auto-repeat is discarded there. Kernel state seeds a device when it is
  registered, and the latch is evaluated in event order thereafter. Devices are
  identified by `(rdev, ino)`, read off the node by the shell and judged by the
  pure `verdict`, so a replug that reuses a path is seen. Hotplug uses a 500 ms
  scan. Losing the keyboard that holds the hotkey, or the last hotkey-capable
  keyboard while a latched recording runs, sends `Event::HotkeyLost`: the
  recording ends by decoding what was said, never by cancelling.
- `shell/nvim/mod.rs` owns the socket and pinned dictation buffer, and
  `shell/nvim/rpc.rs` the msgpack transport, where every call carries an
  absolute deadline. `shell/x11.rs` performs bounded placement queries, one per
  spawn. No module synthesizes
  input or calls X focus APIs.

The dictation preview uses the theme's comment foreground with an italic
distinction, without an extra virtual blank line. It remains virtual text,
never file content. Diagnostics are disabled only for the dedicated dictation
buffer, and successful saves are silent; write errors still propagate and roll
back. Ordinary editor buffers and the user's Neovim configuration are unchanged.

Live ticks and recovery use the same segmentation and decode implementation.
Padding can overlap around silence, as it did in Python; committed speech is
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
the pinned [native offline stream implementation](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.6/sherpa-onnx/csrc/offline-stream.cc#L150-L160)
finalizes feature extraction on that call.

## Configuration and deployment

All sections reject unknown fields, wrong types, non-finite durations, and
invalid ranges. Model paths are `Option<PathBuf>`: a path set in a config
resolves against that config's directory, and leaving the key out keeps the
built-in, working-directory-relative default. `audio.sample_rate` must be
**16000** — the Silero window is 512 samples at that rate and Parakeet's
features assume it, and nothing resamples in between — and the error names both
models. `vad.chunk_seconds` must be positive, `preview.max_seconds` at most
3600 (default 30), and `nvim.colorscheme` must match `[A-Za-z0-9_.-]+`, since it
becomes Lua code. Recovery rejects a WAV whose rate differs from the configured
capture rate, as in the original command.

The default config location honours `XDG_CONFIG_HOME` and may be absent, in
which case defaults are used. An explicit `--config PATH` that does not exist is
an **error**: silently running on defaults because a `--config` typo pointed
nowhere is how a user loses their settings.

Command-line flags: `-c/--config PATH`, `--model-dir PATH`, `-v/--verbose`,
`--log-file PATH` (the literal `none` disables the file), and, on the daemon
only, `--dump-audio DIR`, which writes each capture exactly as decoded for
debugging. Subcommands are `transcribe <WAV> [--out PATH]` and `check`.

Exit codes are selected by error *type*, never by matching a message, so a
reworded error cannot silently turn into a restart loop: `2` model files
missing, `3` the hotkey watcher could not start
(`shell::daemon::HotkeyUnavailable`), `4` an unreadable or wrong-rate WAV
handed to `transcribe`, `1` everything else.
The systemd unit refuses to retry 2 and 3 and does retry 1 — which is what a
second daemon losing the race for
`$XDG_STATE_HOME/spokenpad/daemon.lock` (an `flock`, held for the process
lifetime, never unlinked) reports.

Run `cargo build --locked --release`. The official sherpa build script obtains
the pinned native libraries, or uses `SHERPA_ONNX_LIB_DIR`. They are copied next
to the executable and located using an origin-relative runpath. No build-machine
Python path is needed at runtime. The systemd unit executes
`target/release/spokenpad`; the per-user lock above rejects a second Rust
daemon. The Python reference predates that lock and must be stopped before
switching.

The installed i3 `no_focus` rule remains required. The Rust adapter checks
the active configuration, including loaded include files, before opening a
graphical terminal. Reattaching requires evidence that the editor belongs to
spokenpad; an arbitrary nvim socket is refused.
Fresh editors must also finish the startup handlers registered before our final
one-shot `VimEnter` callback. Both ownership and readiness require the generated
session nonce; an early RPC response alone is not proof of startup completion.
The graphical launcher accepts an Alacritty command with an explicit instance
setting. Unverifiable terminal commands are refused; `terminal = []` is only
allowed for explicitly headless editors. This intentionally tightens the
former Python adapter's permissive launcher to enforce the no-focus constraint.

## Verification

`cargo test --locked --all-targets` covers pure transitions and worker
ordering, capture arithmetic against a synthetic input backend, recording
recovery and pruning with temporary files, and real headless nvim RPC tests. No
test uses the actual keyboard or microphone, takes the daemon lock, or touches
the real state directory, so the suite runs while the user's own service does.

`tests/e2e.rs` drives `shell::daemon::serve` end to end with three
substitutions and nothing else — a synthetic microphone, a channel in place of
evdev, and a
counting recognizer — against a real `nvim --headless` over msgpack-RPC. It
asserts the things only the whole loop can show: text landing in the file,
progressive commits appending before release, a too-short tap discarded with a
notice the idle winbar really renders, a latched recording ending on the second
press, a cancel keeping
committed text and the WAV, a second press during transcription starting a new
paragraph, the preview staying virtual text, and a microphone restart marking
the gap while keeping the audio. One further test loads the real CPU models and
is `#[ignore]`d; it needs `models/`:

```sh
cargo test --locked --test e2e -- --ignored
```

The nvim-dependent tests **fail** when nvim is missing rather than reporting a
green suite; `SPOKENPAD_ALLOW_MISSING_NVIM=1` skips them deliberately.

`scripts/verify_rust.py` compares Rust with the Python reference using the
locally held WAVs. It checks exact segment start/end/padding/settlement,
transcripts, progressive commit offsets, and the final release remainder.
The native executable is `examples/verify_native.rs`. Audio and text stay local.

Initial-port verification on 2026-09-09, before the endpoint-padding fix: all
five evaluation WAVs produced exactly the same segments and raw transcripts in
Rust and Python. A 102.5s simulated live passage also matched all six
progressive commits, their offsets, and the final transcript. Its final
remainder was 1.8s of audio, decoded by Rust in 0.43s on that run. These figures
are historical evidence, not validation of the later endpoint-padding change.

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

**Current accuracy figures**, re-measured 2026-09-11 with `uv run
scripts/eval.py` on the five verified references: **17.6%** aggregate WER
through the VAD path the daemon uses, **13.9%** whole-buffer, against Handy
0.9.6's **48.7%** on the same five, all five scored. Five clips of one speaker
are a regression proxy, not a general accuracy guarantee — see
[evaluation.md](evaluation.md).

Final post-fix differential verification on 2026-09-09 passed all five
recordings with exact segment and transcript parity. Re-run on 2026-09-11,
the short `cd home` clip (`handy-1787827474.wav`) decodes to an empty string in
Rust while the Python reference still yields `C D home.`; the VAD segments are
identical and the build at commit b1159ec reproduces the same empty result, so
this is a native-runtime difference on an edge case, not a regression of the
2026-09-11 changes. It is tracked in STATUS.md. The 102.5-second progressive passage matched
all six commits and its final transcript; its 1.8-second release remainder
decoded in 0.53 seconds in that run.

The native suite and the focused offline Python reference suite are both part
of verification. As of 2026-09-11, `cargo test --locked --all-targets` passes
**123 library, 1 binary, 4 CLI and 10 end-to-end tests**, with the one real-model
e2e test ignored by default, and the retained Python suite collects and passes
**68** tests. Strict all-target clippy, rustfmt, Ruff, and mypy checks apply.
The optional `cargo run --example verify_window` smoke harness opened a real
dedicated editor, saved multiline Unicode exactly, confirmed unchanged X11
focus after opening and appending, and closed its temporary editor. Run this
manual check only when no other dictation editor is open.

The user service was switched to `target/release/spokenpad` on 2026-09-09.
Startup completed on the actual microphone/keyboards without a callback-timeout
warning; live evdev descriptor flags were read-only, with no service restarts
or watchdog warnings during the post-switch check. The executable and its
native library resolution were checked independently of Python.

The first live Rust dictation captured 89.3s of audio for an 89.2s hold;
release decoded the remaining 2.6s in 0.27s, followed by a 33ms editor append.
This is a live latency observation, not just the saved-recording simulation.

Historical design and latency measurements in the other documents describe
the Python implementation unless identified as Rust measurements. Tests and
saved recordings cannot establish live microphone reliability; that requires
dictation through the running Rust service.

Native API sources: [sherpa Rust wrapper](https://docs.rs/sherpa-onnx/1.13.6/sherpa_onnx/),
[evdev](https://docs.rs/evdev/0.13.2/evdev/struct.Device.html),
[i3 IPC](https://i3wm.org/docs/ipc.html).
