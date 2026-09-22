# spokenpad — agent notes

Local push-to-talk dictation for Linux. The user binds keys in their window
manager to `spokenpad start`/`stop` (push-to-talk), `toggle` (latch) and
`cancel`, which talk to the daemon over its control socket
(`$XDG_RUNTIME_DIR/spokenpad.sock`); the transcript lands in a dictation
Neovim over msgpack-RPC.
`nvim.mode = "pane"` (default): the daemon opens a window it draws itself,
with `nvim --embed` in it, floating or tiled (`nvim.pane_layout`), sized in
cells (`nvim.pane_dimensions`), needing no rule in the user's config (X11 and Xwayland; verified headless
on i3, sway, Openbox, KWin Wayland and X11; on sway the daemon adds a
`no_focus` rule over IPC); `"attach"`: the user runs `spokenpad editor` in
any terminal, the choice for Wayland without Xwayland. Managed mode (a
terminal on i3 or sway) was removed on 2026-09-22; its config keys are
refused with a message that says so.
Rust only, CPU only (sherpa-onnx linked statically: Parakeet TDT, or another NeMo
transducer in `asr.model_dir`; Silero VAD, always on).

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
  size and sha256. A user-configured `model_dir` or `vad.model` is never
  downloaded.
- No window spokenpad opens may take focus. Attach mode opens no window;
  pane mode sets `_NET_WM_USER_TIME = 0` (once, never
  again), `_NET_WM_WINDOW_TYPE_UTILITY`, `_NET_WM_STATE_ABOVE` and
  `WM_HINTS input = True` before the first map, and announces no
  `WM_TAKE_FOCUS`. When the display names its window manager `wlroots wm`
  and its process is sway, the pane first adds `no_focus [instance="^spokenpad-pane$"
  class="^spokenpad-pane$"]` over the IPC socket that process listens on,
  and refuses to open otherwise: no process named, another wlroots
  compositor, no socket of that sway, or an empty focused workspace. The pane
  opens with its outer frame 20 px (scaled by `Xft.dpi` / 96) beside the
  pointer, or around it when it fits beside it on neither axis, so a pointer
  resting or jiggling where it was never enters it. The user's own click, or
  their pointer moving into the pane under focus-follows-mouse, may focus it;
  nothing else may. The code contains no focus call.
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
  the pane's runtime sway rule), `core/models.rs` (the pinned
  default model manifest), `core/grid.rs` (nvim's `ext_linegrid` redraw
  events and the screen they fold into), `core/keys.rs` (keysym and modifiers
  to nvim key notation), `core/font.rs` (the pane's point size and
  `Xft.dpi` to pixels, and cells measured as Alacritty measures them),
  `core/frames.rs`, `core/geometry.rs`, `core/text.rs`; plus the `shell/nvim/rpc.rs` codec, `parse_ownership` and
  `passage::append_paragraph`, still inside their modules.
- Imperative shell: `shell/daemon.rs` (`run` = lock/socket/signals/devices and the model loader, `serve` =
  the generic loop), `shell/audio.rs` (`InputBackend` seam; PortAudio impl),
  `shell/recorder.rs` (recovery WAV), `shell/control.rs` (control socket
  server and the CLI's client), `shell/inference.rs` (sherpa's transducer
  and Silero), `shell/dirs.rs` (private directories), `shell/models.rs`
  (downloads and verifies the default models),
  `shell/nvim/mod.rs` (editor lifecycle, both modes, `spokenpad editor`),
  `shell/nvim/passage.rs` (text dictated with no editor open), `shell/wm.rs`
  (sway's IPC socket for the pane's rule, bounded helper processes),
  `shell/logging.rs`.
- The pane (`shell/pane/`, the dictation window spokenpad draws itself,
  reached through `nvim.mode = "pane"`): `mod.rs` (the loop, the renderer,
  `requirements` for `spokenpad check`), `host.rs` (its thread, the three
  things the daemon tells it, and whether the user closed the last pane), `x11.rs` (the window, the properties that keep
  a window manager from focusing it, `PutImage`), `ui.rs` (`nvim --embed`
  over stdio, `nvim_ui_attach`), `font.rs` (`fc-match` plus swash glyphs, and a
  character fallback kept off the drawing path), `keyboard.rs` (the user's real layout, dead keys,
  Compose), `place.rs` (RandR monitors and the X pointer, fed to
  `core/geometry.rs`, which puts the pane's frame beside the pointer), `xkb.rs` (libxcb and libxkbcommon opened with `dlopen`
  when a pane opens, so the binary starts without them in the other modes).
- Editor UI: `src/lua/spokenpad.lua` (winbar, preview extmark, transactional
  `append_once`, the dictation buffer saved promptly on every change and
  before `:q`)
  and `src/lua/dictation_init.lua` (bundled init).
- Tests: unit tests in-module; `src/shell/nvim/tests.rs` (real
  `nvim --headless`); `tests/cli.rs`; `tests/e2e.rs` drives
  `shell::daemon::serve` with a synthetic microphone, a scripted request
  channel or the real control socket and CLI, and a real headless nvim.
- `examples/eval.rs` (WER on the five committed clips),
  `examples/corpus.rs` (the whole local corpus through either decode path,
  with the counts WER hides: empty chunks, lost endings, chunk seams that
  wrote a word twice), `examples/pane.rs` (opens the pane on a given
  display, floating or `--tiled`, and can write a screenshot),
  `examples/screenshot.rs` (composes `docs/screenshot.png` headless). They
  are development tools, not usage examples (`examples/README.md`).
- `tests/harness/mod.rs` is the headless desktop the pane tests share (its own
  Xvfb above `:50`, its own i3, XTEST input that refuses any other display).
  `tests/pane_window.rs` proves the window never takes focus on i3, and
  `tests/pane_focus_wms.rs` on sway, Openbox and KWin (Wayland and X11), each
  in a private headless session (`tests/harness/desktops.rs`);
  `tests/pane_hover.rs` proves that under focus-follows-mouse (i3, Openbox)
  only a deliberate move into the pane focuses it;
  `tests/pane_render.rs` runs a real embedded nvim in it and checks the
  drawing against nvim's own screen; `tests/pane_daemon.rs` drives the real
  `shell::daemon::serve` in pane mode; `tests/pane_typing.rs` types into a
  pane on a German layout while a stand-in daemon dictates into it;
  `tests/pane_hidpi.rs` sets `Xft.dpi`
  and compares the pane's cells with a live Alacritty's (llvmpipe, private
  `HOME`).
- `scripts/gladia-references.sh` is local and git-ignored: the author's
  development tool that **uploads the recordings to Gladia** to build the
  frozen dataset in `eval-samples/local/`; never run by the program, not in
  the repository. `eval-samples/README.md` is tracked and describes
  both evaluation sets; the dataset carries its own git-ignored README. Models (`$XDG_DATA_HOME/spokenpad/models`, pinned
  sha256) come from `spokenpad fetch-models` or the first launch.
- `packaging/aur/PKGBUILD` (the Arch package, built from a GitHub tag
  tarball; `.SRCINFO` from `makepkg --printsrcinfo`), `packaging/systemd/`
  (`spokenpad.socket`, enabled by the package; `spokenpad.service`, started
  only by it; `dev.conf.example`, the drop-in that runs `~/.local/bin`),
  `packaging/i3` and `packaging/sway` (example key bindings). `.github/workflows/ci.yml` runs fmt, clippy and every test in an
  Arch container, with every tool the tests drive installed and no
  `SPOKENPAD_ALLOW_MISSING_*` set.

## Commands

```sh
cargo build --locked --release
cargo test --locked --all-targets              # no mic, user display, lock or service socket is touched
cargo test --locked --test e2e -- --ignored    # real-model e2e; needs `spokenpad fetch-models` first
cargo clippy --locked --all-targets -- -D warnings && cargo fmt --check
cargo run --release --example=eval             # WER on the five eval clips (--whole: no VAD)
cargo run --release --example=corpus -- --config C.toml   # the whole local corpus, both paths: the release gate
cargo run --release --example=corpus -- --config C.toml --path live --subset dev --jobs 2   # 28 captures, for iteration
cargo install --locked --path . --root ~/.local  # deploy, with the dev drop-in (packaging/systemd/dev.conf.example)
systemctl --user restart spokenpad             # ...then restart; the socket unit stays up
```

Nvim-dependent tests fail loudly when nvim is missing unless
`SPOKENPAD_ALLOW_MISSING_NVIM` is set; `tests/pane_window.rs` does the same for
Xvfb and i3 with `SPOKENPAD_ALLOW_MISSING_X11`, and `tests/pane_hidpi.rs` for
Alacritty. That test starts its own X
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
  verified build (`cargo install --locked --path . --root ~/.local`, then
  `systemctl --user restart spokenpad`, with the dev drop-in in place) is
  always allowed; in pane mode it closes the user's
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
  nowhere else. The corpus (`eval-samples/local/`, 144 MB, git-ignored in
  full) holds copies of those recordings, their transcripts and the run files
  of every experiment: never commit or quote any of it, counts and error
  rates only.
