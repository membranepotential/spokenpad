# Rust implementation

The Rust runtime implements the daemon and the `transcribe` command.
It keeps the same Parakeet TDT checkpoint, CPU provider, six inference threads,
Silero segmentation, TOML sections, read-only evdev input, and nvim Lua UI.
The Python package retains only ASR/VAD/decode reference functions and shared
evaluation helpers; it is not an executable daemon and is never called by the
Rust runtime. Setup/evaluation scripts remain Python.

## Ownership and ordering

- `state.rs` contains the pure state machine; `session.rs` handles utterance
  identity, cancellation, preview scheduling, and warnings.
- `daemon.rs` owns the main event loop. It drains input before inference
  results and moves blocking inference and editor RPC to separate threads.
- `decode.rs` owns the committed sample offset on one worker. Each capture
  has its own cancellation/tick token; starting a later capture cannot revive
  a cancelled or released one. Preview output never enters the commit path.
- `inference.rs` wraps the official sherpa Rust API. The safe wrapper, FFI
  bindings, and native runtime are pinned together; startup checks the native
  version. Both models explicitly use the CPU provider.
- `audio.rs` maintains pre-roll and immutable callback chunks. Snapshots copy
  sample data outside the callback lock. `recorder.rs` writes shared chunks
  independently before the in-memory limit is applied.
- `hotkey.rs` opens every evdev descriptor with `File::open` (read-only).
  It queries kernel modifier state across keyboards and ignores repeat events.
  Hotplug uses a 500ms scan; loss of the hotkey device cancels capture.
- `nvim.rs` owns the socket and pinned dictation buffer. `x11.rs` performs
  bounded placement queries. No module synthesizes input or calls X focus APIs.

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
invalid ranges. Model paths explicitly set in a config resolve against that
config's directory. Defaults retain working-directory-relative model paths.
The default sample rate remains 16kHz; recovery rejects a WAV whose rate differs
from the configured capture rate, as in the original command.
The default config honors `XDG_CONFIG_HOME`.

Run `cargo build --locked --release`. The official sherpa build script obtains
the pinned native libraries, or uses `SHERPA_ONNX_LIB_DIR`. They are copied next
to the executable and located using an origin-relative runpath. No build-machine
Python path is needed at runtime. The systemd unit executes
`target/release/spokenpad`; a per-user lock rejects a second Rust daemon.
The Python reference predates that lock and must be stopped before switching.

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

`cargo test --all-targets` includes pure transitions and worker ordering,
recording recovery and pruning with temporary files, and real headless nvim
RPC tests. No unit test uses the actual keyboard or microphone.

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
while one previously matching recording gained a word error (3.4%). Without
VAD, WER changed from 13.4% to 13.9%. These small, partly reconstructed
references are a regression proxy, not a general accuracy guarantee. A shorter
0.5-second pad was rejected because it made the short command decode empty.

Final post-fix differential verification passed all five recordings with exact
segment and transcript parity. The 102.5-second progressive passage matched
all six commits and its final transcript; its 1.8-second release remainder
decoded in 0.53 seconds in that run.

The native suite and focused offline Python reference suite are both part of
verification: 71 Rust tests and 67 retained Python tests pass. Strict
all-target clippy, rustfmt, Ruff, and mypy checks apply.
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
