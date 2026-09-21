# spokenpad

Offline push-to-talk dictation for Linux. Hold a key, speak, and release: the
text appears in a Neovim window that never takes focus. Speech recognition runs
locally on the CPU, and nothing is ever pasted or typed into other
applications.

## Features

- **Push-to-talk, or latch.** Hold a key while you speak. Press Shift with
  it and recording keeps running after you let go, until you press the key
  again.
- **Text appears while you speak.** Each finished sentence lands in the file
  during the recording (progressive commit). A live preview of the current
  sentence is shown below it. Releasing the key only decodes the last second
  or so, however long you talked.
- **Nothing is lost.** Every recording is also saved as a WAV. If a decode
  fails, `spokenpad transcribe` recovers the text from it.
- **Works on any Linux desktop.** The daemon is controlled by commands you
  bind to any key in your window manager or desktop. It reads no keyboard, so
  it needs no special permissions and works on X11 and Wayland alike. You open
  the dictation editor in any terminal. On i3 and sway, spokenpad can open a
  floating window for you.
- **Choice of model.** Parakeet TDT 0.6B v3 by default, or Whisper and
  SenseVoice models from sherpa-onnx. Parakeet can be biased towards your own
  vocabulary (project names, commands).

## Requirements

- Linux on x86-64 or aarch64.
- Neovim 0.10 or newer.
- PortAudio (`portaudio` on Arch, `libportaudio2` and `portaudio19-dev` on
  Debian and Ubuntu).
- To build: Rust 1.88 or newer, a C/C++ toolchain, and `pkg-config`. The build
  downloads sherpa-onnx's prebuilt static libraries from GitHub.
- About 700 MB of disk for the default model, and about 1.2 GB of RAM while it
  runs.

## Install

1. Build and install the binary and the systemd user unit:
   ```sh
   git clone https://github.com/membranepotential/spokenpad
   cd spokenpad
   scripts/install.sh
   ```
   This puts `spokenpad` in `~/.local/bin` (make sure it is on your `PATH`)
   and the unit in `~/.config/systemd/user`.
2. Check that the models load: `spokenpad check`. The first run downloads
   the default models (about 670 MB) to `~/.local/share/spokenpad/models`,
   verifying every file against a pinned sha256, then prints
   `Configuration valid; CPU recognizer ready; VAD ready.` To fetch them
   ahead of time instead, run `spokenpad fetch-models`.
3. Enable the service:
   ```sh
   systemctl --user enable --now spokenpad
   ```
   It starts with `graphical-session.target`. GNOME, KDE Plasma and other
   systemd-managed sessions reach that target. If yours does not (plain i3
   and sway usually do not), start spokenpad from your window manager
   config instead:
   ```
   exec "systemctl --user import-environment DISPLAY XAUTHORITY; systemctl --user start spokenpad"
   ```
   On sway, import `SWAYSOCK WAYLAND_DISPLAY DISPLAY` instead. The import only
   matters for the [managed window](#managed-window-on-i3-and-sway); the
   default mode needs no environment from your session.
4. [Bind your keys](#bind-your-keys).
5. Open the dictation editor in any terminal: `spokenpad editor`. Then hold
   your push-to-talk key and speak.

To update, pull and run `scripts/install.sh` again, then
`systemctl --user restart spokenpad`. To remove everything the script
installed, run `scripts/install.sh --uninstall`. It leaves your models and
settings in place.

Watch the service with `journalctl --user -u spokenpad -f`. The full debug
log is `~/.local/state/spokenpad/spokenpad.log`.

## Bind your keys

spokenpad reads no keyboard. You bind keys in your window manager or desktop
to four commands. Each one sends a single request to the running daemon over
its socket (`$XDG_RUNTIME_DIR/spokenpad.sock`) and exits:

| command | bind it to |
|---|---|
| `spokenpad start` | the push-to-talk key going down |
| `spokenpad stop` | the same key coming up |
| `spokenpad toggle` | Shift and the same key: start a latched recording, or end the running one |
| `spokenpad cancel` | a key you can reach while holding the push-to-talk key |

A binding takes its key away from every application, so pick keys you use
for nothing else. F13 to F24 are free on most systems, and a programmable
keyboard (QMK, VIA) or a remapper such as keyd can send them from any key.
Do not bind `cancel` to Escape: every application would lose Escape. If
your window manager does not find `spokenpad`, write the full path,
`~/.local/bin/spokenpad`.

The examples use F16 to talk and F17 to cancel. The standard XKB layouts
name these keys `XF86Launch7` and `XF86Launch8`, not `F16` and `F17`; their
X11 keycodes are 194 and 195. `xev` (X11) or `wev` (Wayland) shows the name
and keycode of any key.

**i3** (`~/.config/i3/config`), by keycode:

```
bindcode 194 exec --no-startup-id spokenpad start
bindcode --release 194 exec --no-startup-id spokenpad stop
bindcode Shift+194 exec --no-startup-id spokenpad toggle
bindcode 195 exec --no-startup-id spokenpad cancel
exec --no-startup-id xset -r 194
```

The last line turns off auto-repeat for keycode 194. Push-to-talk works
without it, but a Shift+F16 held past the repeat delay would end its own
latched recording. The same bindings, commented out, are in
[`packaging/i3/spokenpad.conf`](packaging/i3/spokenpad.conf) and
[`packaging/sway/spokenpad.conf`](packaging/sway/spokenpad.conf).

**sway** (`~/.config/sway/config`):

```
bindsym --no-repeat XF86Launch7 exec spokenpad start
bindsym --release XF86Launch7 exec spokenpad stop
bindsym --no-repeat Shift+XF86Launch7 exec spokenpad toggle
bindsym --no-repeat XF86Launch8 exec spokenpad cancel
```

**Hyprland** 0.55 and newer (Lua config):

```lua
hl.bind("XF86Launch7", hl.dsp.exec_cmd("spokenpad start"))
hl.bind("XF86Launch7", hl.dsp.exec_cmd("spokenpad stop"), { release = true })
hl.bind("SHIFT + XF86Launch7", hl.dsp.exec_cmd("spokenpad toggle"))
hl.bind("XF86Launch8", hl.dsp.exec_cmd("spokenpad cancel"))
```

Hyprland 0.54 and older (`hyprland.conf`):

```
bind = , XF86Launch7, exec, spokenpad start
bindr = , XF86Launch7, exec, spokenpad stop
bind = SHIFT, XF86Launch7, exec, spokenpad toggle
bind = , XF86Launch8, exec, spokenpad cancel
```

Hyprland binds do not repeat unless you ask for it.

**GNOME, KDE Plasma and other desktops** run a custom shortcut only when the
key goes down, so push-to-talk is not possible there. Add a custom shortcut
for `spokenpad toggle` instead: press it once to start, and again to stop.
Tap it rather than hold it. Add a second shortcut for `spokenpad cancel`.

How spokenpad copes with a key that repeats while held: a `start` while
recording is ignored, and a `start` within 150 ms of a `stop` means the key
never came up, so the recording continues. A `stop` during a latched
recording is ignored, because Shift and the key come up in either order.

## Use it

Hold the push-to-talk key, speak, and release. The text is appended to the
dictation file as its own paragraph and saved after every utterance.

- **Latch:** press Shift with the key (`spokenpad toggle`), then let go and
  keep talking. Press the key again, with or without Shift, to stop.
- **Cancel:** press the cancel key (`spokenpad cancel`) while recording. Text
  that already reached the file stays, and the WAV is kept. After release,
  cancel does nothing.
- **The editor:** `spokenpad editor` runs Neovim in the current terminal on
  the dictation socket. Close it to end the passage: the next editor starts a
  new file. If you dictate with no editor open, the text goes to a file, one
  desktop notification (`notify-send`) says where, and the next
  `spokenpad editor` opens that file.
- **Clipboard (opt-in, off by default):** set `nvim.copy_to_clipboard = true`
  and, after every release, the dictation Neovim copies the whole buffer to
  its `+` register, ready to paste wherever you want. This needs a Neovim
  clipboard provider (`wl-copy`, `xclip` or `xsel`).
- **Files:** one Markdown file per editor, in
  `~/.local/state/spokenpad/dictation/`.

### While you dictate

The winbar of the dictation window is the whole interface: a phase dot, a level
meter while recording, a lock while latched, and the live preview of the
current sentence below the committed text. When something happened to a
capture, the winbar shows a **notice** beside the phase label until the next
key press. It is shown once per capture and never replaces the preview.

A notice has two parts: a **headline** (`⚠ microphone gap`), always drawn, and
a sentence explaining it, added only when the window is wide enough for all of
it. A narrow window gives up the level meter first, then the explanation, but
never the phase label or the headline. When two things happen to the same
capture, the more serious one is shown: memory limit reached > capture
incomplete > microphone unavailable > microphone gap > nearly silent > held
too briefly > preview paused. The log always has the full sentence and paths.

- **Recording continues for a quarter second after you let go**
  (`audio.postroll_ms`), so a word still sounding at key-up is not cut off.
  Pressing the key again within the first 150 ms continues the same
  recording; pressing it later ends the wait at once and starts the next
  capture.
- **A tap shorter than 120 ms is discarded**, with "held too briefly — hold the
  key while speaking" in the winbar. The WAV is still written.
- **A microphone that stops delivering audio is reopened**, and the capture it
  interrupted is marked as having a gap. While idle the same repair runs
  quietly, so the pre-roll is ready at the next press.
- **A capture much shorter than the hold, or nearly silent,** says so in the
  winbar.
- **A capture with no speech in it is not transcribed.** A recogniser given
  silence invents words ("Thank you."), so when the voice activity detector
  (VAD) hears nothing, nothing is appended. Without a VAD model the whole
  capture is decoded.
- **Speech the VAD heard is not dropped by the recogniser.** Parakeet sometimes
  returns nothing for a short sentence; such a chunk is decoded once more
  without its trailing silence.
- **Previews pause on a long unsettled tail** (`preview.max_seconds`, 30 s)
  and resume by themselves; text keeps landing while they are paused. Without
  a VAD model there is no preview, and the capture is decoded at release.
- **A long recording costs no more memory than a short one.** Audio that has
  been transcribed is dropped as you speak; what is held is the sentence you
  are still in. The recovery WAV keeps the whole recording.
- **Past the 60-minute in-memory limit** the capture is decoded and the winbar
  names the recovery WAV, which keeps recording. Text that lands while you
  speak is what makes the audio droppable, so the limit is reachable only
  where nothing lands: with no VAD model, with `vad.enabled = false`, with
  `preview.enabled = false` (which turns the whole progressive tick off, not
  just the visible preview), or with a `preview.interval_ms` long enough that
  few ticks fire.

## Managed window on i3 and sway

In managed mode the daemon opens the dictation window itself, on the first
key-down, as a floating window at the mouse pointer (on sway, in the
bottom-right corner). It refuses to open it until the running window manager's
config contains a `no_focus` rule for it, which it reads over the i3 or sway
IPC socket. It also does not open it while the focused workspace is empty,
because i3 and sway focus the first window on a workspace despite the rule;
the text then goes to a file that the next editor opens.

1. Set the mode and your terminal in `~/.config/spokenpad/config.toml`:
   ```toml
   [nvim]
   mode = "managed"
   terminal = "kitty"   # alacritty (default), kitty, foot, wezterm or ghostty
   ```
2. Add the rules to your window manager config:
   [`packaging/i3/spokenpad.conf`](packaging/i3/spokenpad.conf) (include it)
   or [`packaging/sway/spokenpad.conf`](packaging/sway/spokenpad.conf) (paste
   it into the main config; sway reports only that file). Each file's header
   says how. Then reload the window manager.
3. Import the session environment into systemd (install step 4) and restart
   the service.

Details, the terminal table and placement: [docs/nvim-window.md](docs/nvim-window.md).

## Configuration

spokenpad reads `~/.config/spokenpad/config.toml`. Every key is optional.
[`config.example.toml`](config.example.toml) documents every key at its
default; copy only what you change. An unknown key is an error, so a typo
cannot pass silently. Keys are not configured here; see
[Bind your keys](#bind-your-keys). The most useful keys:

| key | default | what it does |
|---|---|---|
| `asr.family` | `"parakeet"` | model family: `parakeet`, `whisper` or `sense_voice` ([docs/asr.md](docs/asr.md)) |
| `asr.vocabulary` | `[]` | words to bias Parakeet towards, such as `["kubectl", "nginx"]`; switches to beam search, which sometimes drops a sentence ([docs/asr.md](docs/asr.md)) |
| `nvim.mode` | `"attach"` | `attach`: you run `spokenpad editor`; `managed`: the daemon opens a window on i3 or sway |
| `nvim.terminal` | `"alacritty"` | managed mode only: the terminal for that window |
| `nvim.init` | your Neovim config | `"bundled"` opens the window about 3× faster; pair it with `nvim.colorscheme` |
| `nvim.copy_to_clipboard` | `false` | copy the whole buffer to `+` after every release |

Restart the service after a change: `systemctl --user restart spokenpad`.

## Recovering a dictation

Every capture is written to `~/.local/state/spokenpad/audio/` while you speak,
independently of decoding. The log line for each capture names its file. To
get the text back:

```sh
spokenpad transcribe ~/.local/state/spokenpad/audio/capture-2026-09-08-141530.wav
```

It decodes through the same VAD and model as the daemon and prints the
transcript; `--out PATH` writes it to a file instead. The directory is pruned
oldest-first at 5 GiB, about 46 hours of speech.

## Command line

```
spokenpad                         run the daemon (what the service runs)
spokenpad start | stop            begin or end a push-to-talk capture
spokenpad toggle                  begin a latched capture, or end the running one
spokenpad cancel                  discard the capture being recorded
spokenpad editor                  open the dictation editor in this terminal
spokenpad transcribe WAV [--out PATH]
                                  decode a recording
spokenpad check                   validate the config and load the models
spokenpad fetch-models [--dir DIR]
                                  download the default models ahead of time
```

Global options: `-c/--config PATH`, `--model-dir DIR`, `-v/--verbose` (debug
log to stderr), `--log-file PATH` (`none` for no file). The daemon also takes
`--dump-audio DIR`, which saves each capture as decoded.

Exit codes: `2` model files missing, `4` the WAV given to `transcribe` is
unreadable or not 16 kHz, `1` anything else, including `start`, `stop`,
`toggle` or `cancel` finding no daemon listening. systemd does not restart
the service after `2`.

## Accuracy and speed

On five local test clips of one speaker, the default model scores 18.7% word
error rate through the same VAD path the daemon uses. On one laptop CPU
(i7-9850H, 6 threads) it decodes clips of 5 s and longer 13–17× faster than
real time. Five clips are
a regression check, not a benchmark; see [docs/evaluation.md](docs/evaluation.md)
and [docs/asr.md](docs/asr.md).

## Development

```sh
cargo build --locked --release
cargo test --locked --all-targets                  # no keyboard, microphone or display needed
cargo test --locked --test e2e -- --ignored        # loads the real models (spokenpad fetch-models first)
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
cargo run --release --example=eval                 # WER on local clips in eval-samples/
```

The Neovim tests start real headless editors and fail if `nvim` is missing;
set `SPOKENPAD_ALLOW_MISSING_NVIM=1` to skip them.

Technical documentation is in [docs/](docs/README.md): the
[architecture](docs/architecture.md), the [hard constraints](docs/constraints.md),
the [progressive commit](docs/progressive-commit.md) design, the
[dictation window](docs/nvim-window.md), and the [decision log](docs/decisions.md).

## License

MIT, see [LICENSE](LICENSE).
