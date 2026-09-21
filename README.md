# spokenpad

Offline push-to-talk dictation for Linux. Hold a key, speak, and release: the
text appears in a Neovim window that never takes focus. Speech recognition runs
locally on the CPU, and nothing is ever pasted or typed into other
applications.

## Features

- **Push-to-talk, or latch.** Hold the hotkey while you speak. Hold Shift with
  it and recording keeps running after you let go, until you press the hotkey
  again.
- **Text appears while you speak.** Each finished sentence lands in the file
  during the recording (progressive commit). A live preview of the current
  sentence is shown below it. Releasing the key only decodes the last second
  or so, however long you talked.
- **Nothing is lost.** Every recording is also saved as a WAV. If a decode
  fails, `spokenpad transcribe` recovers the text from it.
- **Works on any Linux desktop.** The hotkey is read from `/dev/input`, so it
  works on X11 and Wayland alike. You open the dictation editor in any
  terminal. On i3 and sway, spokenpad can open a floating window for you.
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
- Membership in the `input` group, to read the hotkey. **Security note:** the
  `input` group can read every keystroke from every keyboard, so any program
  you run can then log keys. spokenpad opens the devices read-only and only
  acts on its hotkey, the latch modifier and the cancel key.

## Install

1. Build and install the binary and the systemd user unit:
   ```sh
   git clone https://github.com/membranepotential/spokenpad
   cd spokenpad
   scripts/install.sh
   ```
   This puts `spokenpad` in `~/.local/bin` (make sure it is on your `PATH`)
   and the unit in `~/.config/systemd/user`.
2. Download the models (about 670 MB) to `~/.local/share/spokenpad/models`.
   Every file is checked against a pinned sha256.
   ```sh
   scripts/fetch-models.sh
   ```
3. Check that the models load: `spokenpad check`. It prints
   `Configuration valid; CPU recognizer ready; VAD ready.`
4. Add yourself to the `input` group, then log out and back in:
   ```sh
   sudo usermod -aG input "$USER"
   ```
5. Enable the service:
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
6. Open the dictation editor in any terminal: `spokenpad editor`. Then hold
   the hotkey and speak.

To update, pull and run `scripts/install.sh` again, then
`systemctl --user restart spokenpad`. To remove everything the script
installed, run `scripts/install.sh --uninstall`. It leaves your models and
settings in place.

Watch the service with `journalctl --user -u spokenpad -f`. The full debug
log is `~/.local/state/spokenpad/spokenpad.log`.

## Choose your hotkey

The default hotkey is **F16** (evdev code 186). No application binds F16, so
holding it collides with nothing. Most keyboards have no F16 key, but a
programmable keyboard (QMK, VIA) or a remapper such as keyd can send F13 to
F24 from any key.

To use another key, set its **evdev key code** in
`~/.config/spokenpad/config.toml`:

```toml
[hotkey]
key_code = 186
```

Find the code of a key with one of these:

- `sudo evtest`: pick the keyboard, press the key, and read `code NNN`. Works
  in any session.
- `sudo libinput debug-events --show-keycodes`: press the key and read the
  number in brackets, as in `KEY_F16 (186)`. Works in any session.
- `xev` on X11: it prints the X11 `keycode`. Subtract 8 to get the evdev code.

spokenpad does not grab the key. The focused application still receives it,
which is why a key no application uses is the best choice.

## Use it

Hold the hotkey, speak, and release. The text is appended to the dictation
file as its own paragraph and saved after every utterance.

- **Latch:** hold Shift with the hotkey, then let go and keep talking. Press
  the hotkey again to stop. Set `hotkey.latch_modifier` to change or disable
  it.
- **Cancel:** press Escape while recording. Text that already reached the file
  stays, and the WAV is kept. After release, Escape does nothing.
- **The editor:** `spokenpad editor` runs Neovim in the current terminal on
  the dictation socket. Close it to end the passage: the next editor starts a
  new file. If you dictate with no editor open, the text goes to a file, one
  desktop notification (`notify-send`) says where, and the next
  `spokenpad editor` opens that file.
- **Clipboard:** after every release, the dictation Neovim copies the whole
  buffer to its `+` register, ready to paste wherever you want. This needs a
  Neovim clipboard provider (`wl-copy`, `xclip` or `xsel`).
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
  Pressing the key again ends that wait at once and starts the next capture.
- **A tap shorter than 120 ms is discarded**, with "held too briefly — hold the
  key while speaking" in the winbar. The WAV is still written.
- **Losing the hotkey keyboard ends the recording by decoding it.** Unplugging
  it mid-sentence, held or latched, keeps what you said.
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
  and resume by themselves. Without a VAD model there is no preview, and the
  capture is decoded at release.
- **Past the 60-minute in-memory limit** the capture is decoded and the winbar
  names the recovery WAV, which keeps recording.

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
3. Import the session environment into systemd (install step 5) and restart
   the service.

Details, the terminal table and placement: [docs/nvim-window.md](docs/nvim-window.md).

## Configuration

spokenpad reads `~/.config/spokenpad/config.toml`. Every key is optional.
[`config.example.toml`](config.example.toml) documents every key at its
default; copy only what you change. An unknown key is an error, so a typo
cannot pass silently. The most useful keys:

| key | default | what it does |
|---|---|---|
| `hotkey.key_code` | `186` (F16) | evdev code of the push-to-talk key |
| `hotkey.latch_modifier` | `"shift"` | modifier that latches a recording; `"none"` disables it |
| `asr.family` | `"parakeet"` | model family: `parakeet`, `whisper` or `sense_voice` ([docs/asr.md](docs/asr.md)) |
| `asr.vocabulary` | `[]` | words to bias Parakeet towards, such as `["kubectl", "nginx"]` |
| `nvim.mode` | `"attach"` | `attach`: you run `spokenpad editor`; `managed`: the daemon opens a window on i3 or sway |
| `nvim.terminal` | `"alacritty"` | managed mode only: the terminal for that window |
| `nvim.init` | your Neovim config | `"bundled"` opens the window about 3× faster; pair it with `nvim.colorscheme` |

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
spokenpad editor                  open the dictation editor in this terminal
spokenpad transcribe WAV [--out PATH]
                                  decode a recording
spokenpad check                   validate the config and load the models
```

Global options: `-c/--config PATH`, `--model-dir DIR`, `-v/--verbose` (debug
log to stderr), `--log-file PATH` (`none` for no file). The daemon also takes
`--dump-audio DIR`, which saves each capture as decoded.

Exit codes: `2` model files missing, `3` no readable input device has the
hotkey (usually: not in the `input` group), `4` the WAV given to `transcribe`
is unreadable or not 16 kHz, `1` anything else. systemd does not restart the
service after `2` or `3`.

## Accuracy and speed

On five local test clips of one speaker, the default model scores 17.6% word
error rate through the same VAD path the daemon uses. On one laptop CPU
(i7-9850H, 6 threads) it decodes clips of 5 s and longer 13–17× faster than
real time. Five clips are
a regression check, not a benchmark; see [docs/evaluation.md](docs/evaluation.md)
and [docs/asr.md](docs/asr.md).

## Development

```sh
cargo build --locked --release
cargo test --locked --all-targets                  # no keyboard, microphone or display needed
cargo test --locked --test e2e -- --ignored        # loads the real models (fetch-models.sh first)
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
