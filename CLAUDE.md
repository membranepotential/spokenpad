# spokenpad — agent notes

Local push-to-talk dictation for Linux/X11/i3. Hold M4 (evdev 186), speak, release;
the transcript lands in a floating Neovim the daemon owns, over msgpack-RPC. Rust
runtime, CPU only (Parakeet TDT via sherpa-onnx, Silero VAD). Python exists only
for model setup and offline evaluation; the daemon never runs it.

Read first: `STATUS.md` (live dashboard, keep it true, ≤ 60 lines),
`docs/constraints.md` (hard rules), `docs/architecture.md`, `docs/rust.md`.
Design history is `docs/decisions.md`; add an entry when you change behaviour.

## Hard constraints (never negotiate these)

- `/dev/input` is opened read-only: `File::open` + `Device::from_fd`. Never
  `Device::open`, `EVIOCGRAB`, uinput, or any input synthesis (`xdotool type`,
  enigo, XTest). Nothing is ever pasted. The only clipboard write is the
  dictation nvim setting its own `+` register to the whole buffer after a release.
- No window spokenpad opens may take focus. The i3 `no_focus` rule is proven
  before a graphical editor is spawned; the code contains no focus call.
- Committed speech is decoded exactly once; the release decodes only the tail.
  The one exception: a VAD chunk that decodes to "" is decoded once more
  without trailing silence (`TrailingSilence::Bare`).
  The preview is extmark virtual text and can never become file content.
- CPU provider only. `audio.sample_rate` is 16000.

## Layout

`src/` is the split: `src/core/` is the functional core, `src/shell/` the
imperative shell, `src/config.rs` the root both sides read, `src/main.rs` the
CLI. Nothing under `src/core/` may import `evdev`, `libc`, PortAudio, sherpa,
`std::fs`, `std::process`, `std::net` or `std::thread` — code that needs one of
those belongs in `src/shell/`.

- Functional core: `core/state.rs` (total transition table), `core/session.rs`
  (per-capture policy, notices), `core/decode.rs` (progressive commits over
  `Recognizer`/`Segmenter` traits), `core/segments.rs` (`merge_spans`:
  VAD spans to padded, settled windows), `core/hotkey.rs` (`WatcherState`,
  `verdict`), `core/frames.rs`, `core/geometry.rs`, `core/text.rs`; plus the
  `shell/nvim/rpc.rs` codec and `parse_ownership`, still inside their module.
- Imperative shell: `shell/daemon.rs` (`run` = lock/signals/devices, `serve` =
  the generic loop), `shell/audio.rs` (`InputBackend` seam; PortAudio impl),
  `shell/recorder.rs` (recovery WAV), `shell/hotkey.rs` (evdev scan/open/poll
  and the run loop), `shell/inference.rs` (sherpa/Silero), `shell/nvim/mod.rs`
  (editor lifecycle), `shell/x11.rs`, `shell/logging.rs`.
- Editor UI: `src/lua/spokenpad.lua` (winbar, preview extmark, transactional
  `append_once`) and `src/lua/dictation_init.lua` (bundled init).
- Tests: unit tests in-module; `src/shell/nvim/tests.rs` (real
  `nvim --headless`); `tests/cli.rs`; `tests/e2e.rs` drives
  `shell::daemon::serve` with a synthetic microphone, a scripted key channel
  and a real headless nvim.
- Python reference: `src/spokenpad/` (ASR/VAD/decode mirrors, config, text),
  `scripts/` (fetch_model, eval, verify_rust, build_hotwords, install).

## Commands

```sh
cargo build --locked --release                 # the service runs target/release/spokenpad
cargo test --locked --all-targets              # no keyboard, mic, X11 or lock is touched
cargo test --locked --test e2e -- --ignored    # real-model e2e; needs models/
cargo clippy --locked --all-targets -- -D warnings && cargo fmt --check
uv run scripts/eval.py --vad                   # WER on the local eval clips
.venv/bin/python scripts/verify_rust.py        # Rust/Python differential check
uv run pytest -q; uv run ruff check .; uv run mypy
```

Nvim-dependent tests fail loudly when nvim is missing unless
`SPOKENPAD_ALLOW_MISSING_NVIM` is set. All tests pass while the user's service is
running: they use temp dirs and never the real state dir or the daemon lock.

## Working conventions

- Style: fully typed, illegal states unrepresentable, functional core /
  imperative shell, no derived state stored, no wrapper with one caller. Every
  `unsafe` carries an accurate `SAFETY:` comment (lint is deny).
- Keep the Python reference in step with Rust decode/VAD semantics, or the
  differential check becomes meaningless. Known open mismatches: clip
  `handy-1787827757` differs by one token (`z E T` vs `Z S E T`), and some
  progressive commits differ in punctuation, with identical segments and offsets
  (see STATUS.md); do not chase them as regressions.
- Docs describe the system as implemented. When you change behaviour, update
  `README.md` ("While you dictate"), the relevant `docs/*.md`, and
  `config.example.toml` comments in the same change.
- Commits: signed, on `main`, message written to a file and passed with `-F`.
  Commit each finished unit of work without being asked, once the checks
  above pass and the docs are updated: one focused commit per logical change,
  never a half-done state. Pushing still needs the user's word. Restarting
  the service (`systemctl --user restart spokenpad`) to deploy a verified
  build is always allowed; it closes the user's dictation window, so say
  that you did it.
- `.agents/` and `.codex/` are untracked directories used by other tools.
  Leave them alone even when empty.
- `models/` (~630 MB) and `eval-samples/*.wav` (the user's voice) are local
  only; never copy them anywhere or send them to a service.
