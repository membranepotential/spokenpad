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
  it needs no special permissions and works on X11 and Wayland alike. By
  default spokenpad opens its own dictation window at the mouse pointer, one
  that never takes the focus (X11, and Wayland through Xwayland); you can
  instead open the editor yourself in any terminal.
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
- For the window spokenpad draws itself (`nvim.mode = "pane"`, the default):
  an X display (Wayland works through Xwayland), `fontconfig` for `fc-match`,
  and `libxcb`, `libxkbcommon` and `libxkbcommon-x11`. These are opened when a
  pane opens rather than linked, so the daemon starts and dictates without
  them — the text then goes to a file — and attach mode never needs them;
  `spokenpad check` reports whether this machine has them.

## Install

On Arch Linux, build and install the package in
[`packaging/aur`](packaging/aur/PKGBUILD):

```sh
git clone https://github.com/membranepotential/spokenpad
cd spokenpad/packaging/aur
makepkg -si
```

It installs `/usr/bin/spokenpad` and two systemd user units, and enables
`spokenpad.socket` for every user. There is no service to enable: the socket
listens from login on, and the first `spokenpad start` starts the daemon,
which answers it at once and records while it loads its model. Then:

1. Download the speech model, about 670 MB, to
   `~/.local/share/spokenpad/models`, verified against pinned sha256 sums:
   ```sh
   spokenpad fetch-models
   ```
   If you skip this, the daemon downloads it on your first press, records
   what you dictate meanwhile, and shows the progress in the dictation
   window. `spokenpad check` loads the model and reports whether this
   machine can open a pane window.
2. [Bind your keys](#bind-your-keys).
3. Log out and back in, or start the socket for this session:
   `systemctl --user start spokenpad.socket`.
4. Hold your push-to-talk key and speak: the dictation window opens by
   itself at the mouse pointer, without taking the focus. On Wayland without
   Xwayland there is no X display for it; set `nvim.mode = "attach"` and open
   the editor in any terminal with `spokenpad editor` instead.

The daemon runs under your systemd user manager, not inside your session, so
it sees only the environment the manager has. That matters only for
[a window that opens by itself](#a-window-that-opens-by-itself): GNOME, KDE
Plasma and most display managers import `DISPLAY` for you. On a plain i3 or
sway session, add this to the window manager's config:
```
exec systemctl --user import-environment DISPLAY XAUTHORITY
```
On sway, import `SWAYSOCK WAYLAND_DISPLAY DISPLAY` instead.

To update, rebuild the package; a running daemon keeps the old binary until
`systemctl --user restart spokenpad` or your next login. Watch it with
`journalctl --user -u spokenpad -f`. The full debug log is
`~/.local/state/spokenpad/spokenpad.log`.

**Upgrading from `scripts/install.sh`.** That script is gone. Remove what it
installed, and the line that started the old service, before you install
the package:
```sh
systemctl --user disable --now spokenpad.service
rm ~/.config/systemd/user/spokenpad.service ~/.local/bin/spokenpad
systemctl --user daemon-reload
```
and delete `systemctl --user start spokenpad` from your window manager's
config. Models and settings stay where they are.

**Other distributions.** Build with Cargo (see [Requirements](#requirements))
and install the units from `packaging/systemd` as your own:
```sh
cargo install --locked --path . --root ~/.local
cp packaging/systemd/spokenpad.{socket,service} ~/.config/systemd/user/
mkdir -p ~/.config/systemd/user/spokenpad.service.d
cp packaging/systemd/dev.conf.example ~/.config/systemd/user/spokenpad.service.d/dev.conf
systemctl --user daemon-reload
systemctl --user enable --now spokenpad.socket
```
The drop-in points the service at `~/.local/bin/spokenpad`.

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
`/usr/bin/spokenpad`.

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
  keep talking. Press the key again, with or without Shift, to stop. One you
  forget stops by itself after five minutes without speech, keeping
  everything you said. **After any stop, automatic or not, the next press
  starts a new recording** — it does not resume the old one, and it clears
  the notice in the winbar, so read that before you press.
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
  `~/.local/state/spokenpad/dictation/`. It is a scratch pad you never save:
  every change you make in it is written at once, and `:q` always writes and
  quits.

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
incomplete > microphone unavailable > capture not kept > recording lost >
recording shortened > recording partly transcribed > no speech model > microphone gap > reached the time limit > stopped after
silence > config not reloaded > nearly silent > held too briefly >
downloading the speech model > loading the speech model > transcribing
recordings > preview paused. The speech model's
notices are not about one capture: they stay until the model is ready and has
caught up. The log always has the full sentence and paths.

- **Dictation works from the first press, before the speech model is ready.**
  The daemon answers presses as soon as it starts and loads the model
  afterwards (a few seconds; the first time, after downloading it). A capture
  made meanwhile goes to its recording only, holding nothing in memory, and
  the winbar says "loading the speech model" or "downloading the speech
  model" with the percentage and how many recordings wait. Once the model is
  ready, each recording is transcribed into the window in order, as it would
  have been live ("transcribing recordings"), just without a preview while
  you speak. Such a capture ends by itself after 60 minutes, the most a live
  one can hold, and no recording waiting to be transcribed is pruned. One
  that is gone anyway is reported as "recording lost". Recordings not
  transcribed yet are listed for the next start, which transcribes the rest
  from where each one's text reached. One that fails partway says "recording
  partly transcribed": the next start tries the rest again, or, if it failed
  at the same point before, the notice gives the `spokenpad transcribe --from
  SECONDS` that recovers it. One cut short since it was recorded says
  "recording shortened".
- **A model that cannot be had does not stop dictation.** Offline on the
  first run, a failed download, or a missing configured `asr.model_dir`: the
  winbar says "no speech model" and why, the recordings are kept, and the
  next press tries again. A default model that is there but does not load is
  checked against its pinned sha256 and downloaded again once. Only the
  default models are ever downloaded; for a model you configured, fix the
  path and press again.

- **What you type is saved as you type it.** Every change in the dictation
  file is written at once, keystroke by keystroke in Insert mode, with no
  `:w` and without your format-on-save; `:q` writes and quits.
- **You can type into the window while you dictate.** In Insert mode your
  cursor stays where you are typing: dictated text lands at the end of the
  text, and what you type stays in one piece beside it. In Normal mode, a
  command you have only half typed (a count, `g`, `"`, `f`) makes Neovim hold
  back everything spokenpad sends until you finish or cancel it: the preview,
  the level meter and the text stop moving. The waiting keys show in the
  bottom-right corner with the bundled init (`showcmd`); press <kbd>Esc</kbd>
  and everything catches up. Text dictated meanwhile waits and lands once, in
  the window, and the dictation window says "waiting for the editor: finish
  or <Esc> the pending command" in its last row. After two minutes (a prompt
  nobody sees) spokenpad stops waiting: that text and what follows it go to
  a separate dictation file, as if no editor were open, and a desktop
  notification names the file. Closing the
  window with a command half typed cancels it, and what you typed is written
  as usual.
- **Recording continues for a quarter second after you let go**
  (`audio.postroll_ms`), so a word still sounding at key-up is not cut off.
  Pressing the key again within the first 150 ms continues the same
  recording; pressing it later ends the wait at once and starts the next
  capture.
- **A tap shorter than 120 ms is discarded**, with "held too briefly — hold the
  key while speaking" in the winbar. The WAV is still written.
- **A microphone that stops delivering audio is reopened**, and the capture it
  interrupted is marked as having a gap. While idle the same repair runs
  quietly, so the pre-roll is ready at the next press. A microphone that is
  not there when the daemon starts (unplugged, or a sound server not up yet
  at login) is looked for the same way; a press meanwhile says "microphone
  unavailable". It is looked for again after a wait that doubles up to a
  minute; a press always tries at once.
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
- **A latch you forget stops by itself** after five minutes without speech
  (`capture.silence_timeout_s`). It is an ordinary stop: the tail is decoded,
  everything spoken is kept, and the winbar says "stopped after silence" until
  your next press, which starts a new recording. The timeout runs from the
  last text the recogniser produced, so a pause while you think is not
  silence to it — but music or a conversation in the room is speech to the
  detector, and only the limits below end such a capture. A key that is still
  down is not stopped this way, whether it is the push-to-talk key or the
  Shift+key that latched: a held key re-fires its binding every few tens of
  milliseconds, so stopping it would only start the next capture. The
  four-hour limit bounds those.
- **No capture runs longer than four hours**, whatever is being said into it.
  This is not a setting: it is what keeps the recovery WAV readable, since a
  WAV's own size field overflows at about 37 hours. The winbar says "reached
  the time limit".
- **Past the 60-minute in-memory limit** the capture ends, its tail is
  decoded, and the winbar names the recovery WAV — which is finished and
  closed there, like any other capture's, not carried on past the limit. Text
  that lands while you speak is what makes the audio droppable, so the limit
  is reachable only where nothing lands: with no VAD model, with
  `vad.enabled = false`, with `preview.enabled = false` (which turns the whole
  progressive tick off, not just the visible preview), or with a
  `preview.interval_ms` long enough that few ticks fire.
- **The silence timeout is off where nothing can report speech**: no VAD
  model, `vad.enabled = false`, `preview.enabled = false`. There is no
  progressive decode in those cases, so there is nothing to measure, and the
  two limits above are what end a forgotten capture. A `preview.interval_ms`
  long enough to starve the timeout is not silently ignored: the daemon
  refuses to start on a configuration whose timeout is under two ticks, and
  says which two keys disagree.

## A window that opens by itself

By default the daemon opens the dictation window itself, on the first
key-down: a window it draws, at the mouse pointer (`mode = "pane"`). It needs
no rule in your window manager's config: the window carries the properties
that make a window manager refuse it focus, and on sway the daemon adds a
`no_focus` rule over sway's IPC before each window. It works on X11, and on
Wayland through Xwayland; verified headless on i3, sway, Openbox and KWin
(Wayland and X11) — **new, try it before you rely on it**. It does not survive
a daemon restart: the window belongs to the daemon, which writes everything
in it to its file before it goes. On sway it will not open on an empty
workspace, since sway focuses the first window on one whatever the rules say.

`mode = "attach"` has you open the editor yourself (`spokenpad editor`, in any
terminal), and the daemon writes into it; it works on any desktop, Wayland
without Xwayland included. No window may take focus, and none does.

`mode = "managed"`, which opened your terminal on i3 or sway and needed a
`no_focus` rule in your config, was removed on 2026-09-22: the pane does the
same with no rule. A config that still sets it, or `terminal`,
`window_instance` or `window_fraction`, is refused with a message that says
so; delete those lines. The window rules the package used to ship in
`/usr/share/spokenpad/i3` and `sway` are gone too, and so is any need for
them.

**Setting up the pane:**

1. Nothing to set: it is the default. What you may want to change:
   ```toml
   [nvim]
   # font_family = "monospace"   # whatever `fc-match monospace` gives
   # font_size = 11.25           # points, as Alacritty's font.size
   # pane_dimensions = { columns = 72, lines = 20 }
   # pane_layout = "tiled"       # i3 and sway tile it instead of floating it
   ```
2. Import `DISPLAY` (and `XAUTHORITY`) into systemd, as in install step 4 —
   the window is X11 even on Wayland. On sway, import `SWAYSOCK` too: sway's
   own `/etc/sway/config.d/50-systemd-user.conf` does, if your config
   includes `/etc/sway/config.d/*`. Without it the daemon looks for sway's
   socket in `$XDG_RUNTIME_DIR` by the process that runs the display, and
   refuses to open the pane when it cannot reach sway.
3. `spokenpad check` says whether the libraries, the font and the display are
   all there, and names what is missing. Then restart the service.

With no display, or with a library missing, dictation still works: the text
goes to a dictation file and the next editor opens on it. Nothing is lost and
the daemon does not fail.

Details and placement: [docs/nvim-window.md](docs/nvim-window.md).

## Configuration

spokenpad reads `~/.config/spokenpad/config.toml`. Every key is optional.
[`config.example.toml`](config.example.toml) documents every key at its
default; copy only what you change. An unknown key is an error, so a typo
cannot pass silently. Keys are not configured here; see
[Bind your keys](#bind-your-keys). The most useful keys:

| key | default | what it does |
|---|---|---|
| `capture.silence_timeout_s` | `300` | seconds without speech after which a latched capture ends by itself; `0` turns it off, and the four-hour limit still applies. Must be at least twice `preview.interval_ms` |
| `asr.family` | `"parakeet"` | model family: `parakeet`, `whisper` or `sense_voice` ([docs/asr.md](docs/asr.md)) |
| `asr.vocabulary` | `[]` | words to bias Parakeet towards, such as `["kubectl", "nginx"]`; switches to beam search, which sometimes drops a sentence ([docs/asr.md](docs/asr.md)) |
| `nvim.mode` | `"pane"` | `pane`: the daemon opens a window it draws itself; `attach`: you run `spokenpad editor` |
| `nvim.font_family`, `nvim.font_size` | `"monospace"`, `11.25` | pane mode only: the font it draws with, sized in points exactly as Alacritty's `font.size` (scaled by the X resource `Xft.dpi`), so the same numbers give the same cells — provided `Xft.dpi` is set (`xrdb` or `~/.Xresources`): the pane does not read an XSETTINGS daemon's `Xft/DPI` or RandR's physical screen size, which Alacritty falls back to, and uses 96 dpi instead |
| `nvim.pane_layout` | `"floating"` | pane mode only: `"tiled"` has i3 and sway tile it beside your window instead; allowed only where proven unfocused (i3, sway, Openbox, KWin), floating elsewhere |
| `nvim.pane_dimensions` | `{ columns = 72, lines = 20 }` | pane mode only: its size in cells, as Alacritty's `window.dimensions`, cut to what fits on the monitor; about a third of a 1080p screen at the default font. The window adds a fixed margin of 4 pixels at 96 dpi on every side |
| `nvim.init` | your Neovim config | `"bundled"` opens the window about 3× faster; pair it with `nvim.colorscheme` |
| `nvim.copy_to_clipboard` | `false` | copy the whole buffer to `+` after every release |

The running daemon reads the file again each time it opens a dictation
window, or attaches to one you opened: `[nvim]` changes apply to that
window. Every other section takes effect after
`systemctl --user restart spokenpad`, which the log says when it sees one
changed. A file that no longer loads leaves the settings in use, and the
window says "config not reloaded" and why. A file that does not load when
the daemon starts gives it the defaults — the pane among them, whatever mode
the file asked for — because the pane is where that notice is seen: it
opens on the next press and says "config not reloaded" until the file is
fixed.

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
spokenpad transcribe WAV [--out PATH] [--from SECONDS]
                                  decode a recording, from SECONDS in
spokenpad check                   validate the config and load the models
spokenpad fetch-models [--dir DIR]
                                  download the default models ahead of time
```

Global options: `-c/--config PATH`, `--model-dir DIR`, `-v/--verbose` (debug
log to stderr), `--log-file PATH` (`none` for no file). The daemon also takes
`--dump-audio DIR`, which saves each capture as decoded.

Exit codes: `2` model files missing (`check`, `transcribe`), `3` another
spokenpad daemon is already running, `4` the WAV given to `transcribe` is
unreadable or not 16 kHz, `1` anything else, including `start`, `stop`,
`toggle` or `cancel` finding no daemon listening. The daemon itself does not
exit over a missing model, a microphone it cannot open, or a config file
that does not load (it runs on the defaults): it says so in the winbar and
tries again at the next press or window.

## Accuracy and speed

Over 75 minutes of the author's own dictation — 181 recordings, replayed
through the same path the daemon uses — the default model scores **9.6% word
error rate on the English recordings and 17.3% on the German ones**. On one
laptop CPU (i7-9850H, 6 threads) it decodes clips of 5 s and longer 13–17×
faster than real time.

Read those figures with two caveats. They are one speaker on one microphone,
so they say what this setup does, not what the model does. And the reference
transcripts come from another speech recogniser rather than from a person: on
the five hand-checked clips that reference is itself 17.0% wrong, almost
entirely on technical words (`udev` as `udef`, `rm -rf` as one word), so the
true error rate on such material is lower than the number above and the number
flatters any system that writes what that reference writes.

The five committed clips in `eval-samples/` are a regression check for changes,
not a benchmark: `cargo run --release --example=eval` scores 18.7% there. See
[docs/evaluation.md](docs/evaluation.md) and [docs/asr.md](docs/asr.md).

## Development

```sh
cargo build --locked --release
cargo test --locked --all-targets                  # no keyboard, microphone or display needed
cargo test --locked --test e2e -- --ignored        # loads the real models (spokenpad fetch-models first)
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
cargo run --release --example=eval                 # WER on local clips in eval-samples/
cargo install --locked --path . --root ~/.local    # run your build: see packaging/systemd/dev.conf.example
```

The Neovim tests start real headless editors and fail if `nvim` is missing;
set `SPOKENPAD_ALLOW_MISSING_NVIM=1` to skip them.

Technical documentation is in [docs/](docs/README.md): the
[architecture](docs/architecture.md), the [hard constraints](docs/constraints.md),
the [progressive commit](docs/progressive-commit.md) design, the
[dictation window](docs/nvim-window.md), and the [decision log](docs/decisions.md).

## License

MIT, see [LICENSE](LICENSE).
