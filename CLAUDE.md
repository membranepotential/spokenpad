# spokenpad — agent notes

Local push-to-talk dictation for Linux. The user binds keys in their window
manager to `spokenpad start`/`stop` (push-to-talk), `toggle` (latch) and
`cancel`, which talk to the daemon over its control socket
(`$XDG_RUNTIME_DIR/spokenpad.sock`); the transcript lands in a dictation
Neovim over msgpack-RPC.
`nvim.mode = "attach"` (default): the user runs `spokenpad editor` in any
terminal; `"managed"`: the daemon opens a floating terminal on i3 or sway;
`"pane"`: the daemon opens a window it draws itself, with `nvim --embed` in
it, needing no window-manager rule (X11 and Xwayland; verified on i3 only).
Rust only, CPU only (sherpa-onnx linked statically: Parakeet TDT by default, Whisper
or SenseVoice via `asr.family`; Silero VAD).

Read first: `STATUS.md` (live dashboard, keep it true, ≤ 60 lines),
`docs/constraints.md` (hard rules), `docs/architecture.md`, `docs/rust.md`.
Design history is `docs/decisions.md`; add an entry when you change behaviour.

## Hard constraints (never negotiate these)

- spokenpad reads no input device: nothing opens `/dev/input`, and the
  daemon is controlled only through its control socket. Never `EVIOCGRAB`,
  uinput, or any input synthesis (`xdotool type`, enigo, XTest). Nothing is
  ever pasted. The only clipboard write is the dictation nvim setting its own
  `+` register to the whole buffer after a release, and only when
  `nvim.copy_to_clipboard` is set (off by default).
- The only network access anywhere in the program is the pinned default
  model download (`spokenpad fetch-models`, or automatically on first launch;
  `core/models.rs`, `shell/models.rs`): fixed URLs, verified against a pinned
  size and sha256. A user-configured `model_dir`/`asr.family` is never
  downloaded.
- No window spokenpad opens may take focus. Attach mode opens no window;
  managed mode proves the i3/sway `no_focus` rule over IPC before it spawns a
  graphical editor; pane mode sets `_NET_WM_USER_TIME = 0` (once, never
  again), `_NET_WM_WINDOW_TYPE_UTILITY` and `WM_HINTS input = True` before the
  first map, and announces no `WM_TAKE_FOCUS`. The code contains no focus
  call.
- Committed speech is decoded exactly once; the release decodes only the tail.
  The one exception: a VAD chunk that decodes to "" is decoded once more
  without trailing silence (`TrailingSilence::Bare`).
  The preview is extmark virtual text and can never become file content.
- CPU provider only. `audio.sample_rate` is 16000.

## Layout

`src/` is the split: `src/core/` is the functional core, `src/shell/` the
imperative shell, `src/config.rs` the root both sides read, `src/main.rs` the
CLI. Nothing under `src/core/` may import `libc`, PortAudio, sherpa,
`std::fs`, `std::process`, `std::net` or `std::thread` — code that needs one of
those belongs in `src/shell/`.

- Functional core: `core/state.rs` (total transition table over control
  requests and the clock; the repeat window), `core/control.rs` (the
  one-line control protocol), `core/session.rs` (per-capture policy,
  notices), `core/decode.rs` (progressive commits over
  `Recognizer`/`Segmenter` traits), `core/segments.rs` (`merge_spans`: VAD
  spans to padded, settled windows), `core/wm.rs` (i3/sway IPC protocol,
  `no_focus` proof), `core/terminal.rs` (the terminal table), `core/models.rs` (the pinned
  default model manifest), `core/grid.rs` (nvim's `ext_linegrid` redraw
  events and the screen they fold into), `core/keys.rs` (keysym and modifiers
  to nvim key notation), `core/frames.rs`, `core/geometry.rs`,
  `core/text.rs`; plus the `shell/nvim/rpc.rs` codec, `parse_ownership` and
  `passage::append_paragraph`, still inside their modules.
- Imperative shell: `shell/daemon.rs` (`run` = lock/signals/devices, `serve` =
  the generic loop), `shell/audio.rs` (`InputBackend` seam; PortAudio impl),
  `shell/recorder.rs` (recovery WAV), `shell/control.rs` (control socket
  server and the CLI's client), `shell/inference.rs` (sherpa/Silero, model
  families), `shell/models.rs` (downloads and verifies the default models),
  `shell/nvim/mod.rs` (editor lifecycle, both modes, `spokenpad editor`),
  `shell/nvim/passage.rs` (text dictated with no editor open), `shell/wm.rs`
  (i3/sway IPC socket), `shell/logging.rs`.
- The pane (`shell/pane/`, the dictation window spokenpad draws itself,
  reached through `nvim.mode = "pane"`): `mod.rs` (the loop, the renderer,
  `requirements` for `spokenpad check`), `host.rs` (its thread, and the two
  things the daemon tells it), `x11.rs` (the window, the properties that keep
  a window manager from focusing it, `PutImage`), `ui.rs` (`nvim --embed`
  over stdio, `nvim_ui_attach`), `font.rs` (`fc-match` plus swash glyphs and
  per-character fallback), `keyboard.rs` (the user's real layout, dead keys,
  Compose), `place.rs` (RandR monitors and the X pointer, fed to
  `core/geometry.rs`), `xkb.rs` (libxcb and libxkbcommon opened with `dlopen`
  when a pane opens, so the binary starts without them in the other modes).
- Editor UI: `src/lua/spokenpad.lua` (winbar, preview extmark, transactional
  `append_once`) and `src/lua/dictation_init.lua` (bundled init).
- Tests: unit tests in-module; `src/shell/nvim/tests.rs` (real
  `nvim --headless`); `tests/cli.rs`; `tests/e2e.rs` drives
  `shell::daemon::serve` with a synthetic microphone, a scripted request
  channel or the real control socket and CLI, and a real headless nvim.
- `examples/eval.rs` (WER harness), `examples/verify_window.rs` (manual
  i3/sway window smoke check), `examples/verify_native.rs` (JSON dump of
  segments and progressive commits), `examples/pane.rs` (opens the pane on a
  given display, and can write a screenshot).
- `tests/harness/mod.rs` is the headless desktop the pane tests share (its own
  Xvfb above `:50`, its own i3, XTEST input that refuses any other display).
  `tests/pane_window.rs` proves the window never takes focus;
  `tests/pane_render.rs` runs a real embedded nvim in it and checks the
  drawing against nvim's own screen; `tests/pane_daemon.rs` drives the real
  `shell::daemon::serve` in pane mode.
- `scripts/install.sh` (binary to `~/.local/bin`, user unit; `--uninstall`).
  Models (`$XDG_DATA_HOME/spokenpad/models`, pinned sha256) come from
  `spokenpad fetch-models` or the first launch. `packaging/` holds the unit,
  and the i3/sway window rules with example key bindings.

## Commands

```sh
cargo build --locked --release
cargo test --locked --all-targets              # no mic, user display, lock or service socket is touched
cargo test --locked --test e2e -- --ignored    # real-model e2e; needs `spokenpad fetch-models` first
cargo clippy --locked --all-targets -- -D warnings && cargo fmt --check
cargo run --release --example=eval             # WER on the local eval clips (--whole: no VAD)
scripts/install.sh                             # deploy: the service runs ~/.local/bin/spokenpad
```

Nvim-dependent tests fail loudly when nvim is missing unless
`SPOKENPAD_ALLOW_MISSING_NVIM` is set; `tests/pane_window.rs` does the same for
Xvfb and i3 with `SPOKENPAD_ALLOW_MISSING_X11`. That test starts its own X
server above display `:50` and its own i3 with a generated config; nothing ever
opens on `:0` or reads the user's i3 configuration. All tests pass while the
user's service is running: they use temp dirs and never the real state dir or
the daemon lock.

## Working conventions

- Style: fully typed, illegal states unrepresentable, functional core /
  imperative shell, no derived state stored, no wrapper with one caller. Every
  `unsafe` carries an accurate `SAFETY:` comment (lint is deny).
- Docs describe the system as implemented. When you change behaviour, update
  `README.md` ("While you dictate"), the relevant `docs/*.md`, and
  `config.example.toml` comments in the same change.
- Commits: signed, on `main`, message written to a file and passed with `-F`.
  Commit each finished unit of work without being asked, once the checks
  above pass and the docs are updated: one focused commit per logical change,
  never a half-done state. Pushing still needs the user's word. Deploying a
  verified build (`scripts/install.sh`, then `systemctl --user restart
  spokenpad`) is always allowed; in managed mode it closes the user's
  dictation window, so say that you did it.
- Experiments: every experiment (a benchmark, a corpus replay, a model or
  parameter comparison, a spike) gets its own file
  `docs/experiments/YYYY-MM-DD-slug.md` in the same change: question, method,
  data, numbers, conclusion. Record negative and inconclusive results too.
  Format: `docs/experiments/README.md`. A decision that follows still gets
  its `docs/decisions.md` entry, linking the experiment.
- `.agents/` and `.codex/` are untracked directories used by other tools.
  Leave them alone even when empty.
- The models (`~/.local/share/spokenpad/models`, ~670 MB) and
  `eval-samples/*.wav` (the user's voice) are local only; never copy them
  anywhere or send them to a service. One exception, granted by the user on
  2026-09-21: recordings may be sent to Gladia (`GLADIA_API_KEY` in the
  git-ignored `.env`) to get reference transcripts for evaluation, and
  nowhere else. Transcripts of the user's own recordings are private too:
  they live in the git-ignored `eval-samples/local/`, never in a commit.
