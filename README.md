# spokenpad

Offline push-to-talk dictation for Linux. Hold a key, speak, and release: the
text appears in a Neovim window that never takes focus. Speech recognition runs
locally on the CPU, and nothing is ever pasted or typed into other
applications.

![The spokenpad dictation window floating over an editor mid-dictation: committed text at the top, a grey live preview below it, and a winbar showing a latched recording with a level meter.](https://raw.githubusercontent.com/membranepotential/spokenpad/main/docs/screenshot.png)

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
- **Controlled from any Linux desktop.** The daemon is controlled by
  commands you bind to any key in your window manager or desktop. It reads
  no keyboard, so it needs no special permissions and works on X11 and
  Wayland alike. By default spokenpad opens its own dictation window beside
  the mouse pointer, one that never takes the focus (X11, and Wayland
  through Xwayland; verified on i3, sway, Openbox and KWin); anywhere else,
  you can open the editor yourself in any terminal.
- **Parakeet TDT 0.6B v3**, downloaded on first use, or another NeMo
  transducer from sherpa-onnx. It can be biased towards your own vocabulary
  (project names, commands).

## Requirements

- Linux on x86-64. The package and the checked build below are x86-64 only.
- Neovim 0.10 or newer.
- PortAudio (`portaudio` on Arch, `libportaudio2` and `portaudio19-dev` on
  Debian and Ubuntu).
- A Neovim clipboard provider to copy text out of the window: `xclip` (the
  package depends on it), or `wl-clipboard` for `spokenpad editor` in a
  Wayland terminal.
- To build: Rust 1.88 or newer, a C/C++ toolchain, and `pkg-config`. The build
  links sherpa-onnx's and onnxruntime's prebuilt static libraries, see
  [Building from source](#building-from-source).
- About 700 MB of disk for the default model, and about 1.2 GB of RAM while it
  runs.
- For the window spokenpad draws itself (`nvim.mode = "pane"`, the default):
  an X display (Wayland works through Xwayland), `fontconfig` for `fc-match`,
  and `libxcb`, `libxkbcommon` and `libxkbcommon-x11`. These are opened when a
  pane opens rather than linked, so the daemon starts and dictates without
  them — the text then goes to a file — and attach mode never needs them;
  `spokenpad check` reports whether this machine has them.

## Install

On Arch Linux and Manjaro, once spokenpad is on the AUR, install it with
your AUR helper: `yay -S spokenpad` (or `pamac build spokenpad` on Manjaro).
Until then, build the package from this repository; `makepkg` needs
`base-devel` and `git`:

```sh
sudo pacman -S --needed base-devel git
git clone https://github.com/membranepotential/spokenpad
cd spokenpad/packaging/aur
makepkg -si
```

The package installs:

- `/usr/bin/spokenpad`;
- the systemd user units `spokenpad.socket`, enabled for every user, and
  `spokenpad.service`, which the socket starts on the first key press: there
  is no service to enable;
- example key bindings in `/usr/share/spokenpad/i3/spokenpad.conf` and
  `/usr/share/spokenpad/sway/spokenpad.conf`;
- this README and `config.example.toml` in `/usr/share/doc/spokenpad/`, and
  the licences in `/usr/share/licenses/spokenpad/`.

Then:

1. [Bind your keys](#bind-your-keys). This is the one step spokenpad cannot
   take for you: it reads no keyboard.
2. Hold your push-to-talk key and speak. The dictation window opens beside
   the mouse pointer without taking the focus.
   - The first press after installing starts `spokenpad.socket` if it is not
     running yet (the package enables it from your next login on).
   - The first press on a new machine downloads the speech model, about
     670 MB, into `~/.local/share/spokenpad/models`, checked against pinned
     sha256 sums. The window shows the progress; what you say meanwhile is
     recorded and transcribed once the model is ready.
   - On Wayland without Xwayland there is no X display for the window: set
     `nvim.mode = "attach"` and open the editor in any terminal with
     `spokenpad editor` instead.

Two commands are there if you want them first: `spokenpad fetch-models`
downloads the model ahead of time, and `spokenpad check` loads it and says
whether this machine can open the dictation window.

The daemon runs under your systemd user manager, not inside your session,
and takes the X display from the manager before each window it opens. Most
sessions give the manager one: the startx and display manager session
scripts on Arch (`/etc/X11/xinit/xinitrc.d/50-systemd-user.sh`), GNOME,
KDE Plasma, and sway's `/etc/sway/config.d/50-systemd-user.conf`. If the
window does not open and a notification says there is no display — a custom
`~/.xinitrc`, or a sway config that does not include `/etc/sway/config.d/*`
— add this to your window manager's config, and press again:
```
exec systemctl --user import-environment DISPLAY XAUTHORITY
```
On sway, import `DISPLAY SWAYSOCK` instead. An X server whose cookie is not
in `~/.Xauthority` also needs `XAUTHORITY` in the daemon's own environment:
if you imported it after the daemon started, run
`systemctl --user restart spokenpad` once.

To update, rebuild or reinstall the package; a running daemon keeps the old
binary until `systemctl --user restart spokenpad` or your next login. Watch
it with `journalctl --user -u spokenpad -f`. The full debug log is
`~/.local/state/spokenpad/spokenpad.log`. Neither ever holds what you
dictated: the log says how long each piece of text is, never what it says.

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

**Other distributions.** Build with Cargo, as in
[Building from source](#building-from-source), and install the units from
`packaging/systemd` as your own:
```sh
packaging/sherpa-archive.sh ~/.cache/spokenpad-sherpa
SHERPA_ONNX_ARCHIVE_DIR=~/.cache/spokenpad-sherpa cargo install --locked --path . --root ~/.local
cp packaging/systemd/spokenpad.{socket,service} ~/.config/systemd/user/
mkdir -p ~/.config/systemd/user/spokenpad.service.d
cp packaging/systemd/dev.conf.example ~/.config/systemd/user/spokenpad.service.d/dev.conf
systemctl --user daemon-reload
systemctl --user enable --now spokenpad.socket
```
The drop-in points the service at `~/.local/bin/spokenpad`.

### Building from source

spokenpad links sherpa-onnx and onnxruntime into its binary from a prebuilt
archive the sherpa-onnx project publishes. A plain `cargo build` has the
`sherpa-onnx-sys` crate download that archive from GitHub without checking
it. `packaging/sherpa-archive.sh` downloads the same archive and checks it
against the sha256 the Arch package pins; given its directory, the build
copies the archive from there instead:

```sh
packaging/sherpa-archive.sh ~/.cache/spokenpad-sherpa
SHERPA_ONNX_ARCHIVE_DIR=~/.cache/spokenpad-sherpa cargo build --locked --release
```

The build keeps the unpacked libraries in `target/sherpa-onnx-prebuilt` and
uses them from there without looking at the archive again: after a build
that downloaded them unchecked, remove that directory once. The package and
CI build this way.

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
for nothing else. Push-to-talk wants a single key rather than a chord: a
window manager runs the release binding when the key comes up, and a chord
whose modifier comes up first may never run it. The examples use **Pause**
to talk and **Scroll Lock** to cancel: most keyboards have both (on a laptop
often behind Fn), and almost no program uses them. Other good choices are
F13 to F24, which a programmable keyboard (QMK, VIA) or a remapper such as
keyd can send from any key. Do not bind `cancel` to Escape: every
application would lose Escape. `xev` (X11) or `wev` (Wayland) shows the name
and keycode of any key. If your window manager does not find `spokenpad`,
write the full path, `/usr/bin/spokenpad`.

The same i3 and sway bindings, commented out, are installed in
`/usr/share/spokenpad/i3/spokenpad.conf` and
`/usr/share/spokenpad/sway/spokenpad.conf`, with how to include them.

**i3** (`~/.config/i3/config`): change the key in the first two lines.

```
set $spokenpad_talk Pause
set $spokenpad_cancel Scroll_Lock
bindsym $spokenpad_talk exec --no-startup-id spokenpad start
bindsym --release $spokenpad_talk exec --no-startup-id spokenpad stop
bindsym Shift+$spokenpad_talk exec --no-startup-id spokenpad toggle
bindsym $spokenpad_cancel exec --no-startup-id spokenpad cancel
exec --no-startup-id xset -r 127
```

The last line turns off auto-repeat for keycode 127, which is Pause; for
another key use its keycode from `xev`. Push-to-talk works without it, but
a Shift+Pause held past the repeat delay would end its own latched
recording.

**sway** (`~/.config/sway/config`):

```
set $spokenpad_talk Pause
set $spokenpad_cancel Scroll_Lock
bindsym --no-repeat $spokenpad_talk exec spokenpad start
bindsym --release $spokenpad_talk exec spokenpad stop
bindsym --no-repeat Shift+$spokenpad_talk exec spokenpad toggle
bindsym --no-repeat $spokenpad_cancel exec spokenpad cancel
```

**Hyprland** 0.55 and newer (Lua config):

```lua
hl.bind("Pause", hl.dsp.exec_cmd("spokenpad start"))
hl.bind("Pause", hl.dsp.exec_cmd("spokenpad stop"), { release = true })
hl.bind("SHIFT + Pause", hl.dsp.exec_cmd("spokenpad toggle"))
hl.bind("Scroll_Lock", hl.dsp.exec_cmd("spokenpad cancel"))
```

Hyprland 0.54 and older (`hyprland.conf`):

```
bind = , Pause, exec, spokenpad start
bindr = , Pause, exec, spokenpad stop
bind = SHIFT, Pause, exec, spokenpad toggle
bind = , Scroll_Lock, exec, spokenpad cancel
```

Hyprland binds do not repeat unless you ask for it.

**GNOME, KDE Plasma and other desktops** run a custom shortcut only when the
key goes down, so push-to-talk is not possible there. Add a custom shortcut
for `spokenpad toggle` instead: press it once to start, and again to stop.
Tap it rather than hold it; since it is only tapped, a chord works well
here, such as Super+Alt+D. Add a second shortcut for `spokenpad cancel`,
such as Super+Alt+X.

- GNOME: Settings → Keyboard → View and Customize Shortcuts → Custom
  Shortcuts → Add Shortcut: a name, the command, and the key.
- KDE Plasma 6: System Settings → Keyboard → Shortcuts → Add New → Command
  or Script: the command, then the key.

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
  that already reached the file stays, and the WAV is kept; the winbar says
  "recording cancelled" until the next press. After release, cancel does
  nothing.
- **Closing the window cancels the recording.** Close the pane while a
  recording runs — through your window manager, or with `:q` in it — and
  the recording is cancelled as if you had pressed the cancel key: what was
  already transcribed stays in the file, the rest is not transcribed, the
  WAV is kept, and one desktop notification says so. No window opens again
  until you press the key. An editor that crashes is different: the
  recording goes on, and its next text opens a new window.
- **The editor:** `spokenpad editor` runs Neovim in the current terminal on
  the dictation socket. Close it to end the passage: the next editor starts a
  new file. If you dictate with no editor open, the text goes to a file, one
  desktop notification (`notify-send`) says where, and the next
  `spokenpad editor` opens that file.
- **Copying text out:** spokenpad never pastes, so you copy what you want
  and paste it yourself. In the window, `"+y` copies to the system
  clipboard (`ggVG"+y` copies everything); a plain `y` stays in Neovim unless
  your own Neovim config sends it to the clipboard. The window uses `xclip` for this, even on Wayland, where
  it is an Xwayland window; `spokenpad editor` in a Wayland terminal uses
  `wl-copy` from `wl-clipboard`.
- **Clipboard (opt-in, off by default):** set `nvim.copy_to_clipboard = true`
  and, after every release, the dictation Neovim copies the whole buffer to
  its `+` register, ready to paste wherever you want, through the same
  provider.
- **The microphone:** spokenpad records from the default input device, the
  one your sound server (PipeWire, PulseAudio) uses as its default source.
  `spokenpad check` lists every input device with its host API and marks
  the one the daemon opens; listing opens none of them. To use another, set
  `audio.device` in the config to words from its name, such as
  `device = "USB"`: they are matched, case-insensitive and in order,
  against PortAudio's device name and host API. The daemon picks it up after
  `systemctl --user restart spokenpad`. A capture that says "nearly silent"
  in the winbar is the usual sign of the wrong microphone.
- **Files:** one Markdown file per editor, in
  `~/.local/state/spokenpad/dictation/`. It is a scratch pad you never save:
  every change you make in it is written promptly, and `:q` always writes
  and quits.

### While you dictate

The winbar of the dictation window is the whole interface: a phase dot, a level
meter while recording, a lock while latched, and the live preview of the
current sentence below the committed text. When something happened to a
capture, the winbar shows a **notice** beside the phase label until the next
key press. It is shown once per capture and never replaces the preview.

A notice has two parts: a **headline** (`⚠ microphone gap`), always drawn, and
a sentence explaining it, added only when the window is wide enough for all of
it. Two headlines carry their figure themselves, because the explanation
seldom fits beside them: the download's percentage ("downloading the speech
model, 37%"), and why the config did not load ("config not reloaded: unknown
field `pane_dimension` (line 3)"; `spokenpad check` prints the whole report).
A narrow window gives up the level meter first, then the explanation, but
never the phase label or the headline. When two things happen to the same
capture, the more serious one is shown: memory limit reached > capture
incomplete > microphone unavailable > capture not kept > recording lost >
recording shortened > recording partly transcribed > no speech model > microphone gap > reached the time limit > stopped after
silence > config not reloaded > nearly silent > held too briefly >
downloading the speech model > loading the speech model > transcribing
recordings > recording cancelled > preview paused. The speech model's
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
- **Stopping the daemon does not lose a capture.** A capture still recording
  when the daemon stops (a restart, an upgrade, a logout), or whose last text
  is not written yet, is listed the same way, and the next start transcribes
  the rest of it from where its text reached. A capture you cancel is not.
  Neither is one whose last decode fails: the winbar says "recording partly
  transcribed", and the next start tries the rest again.
- **A model that cannot be had does not stop dictation.** Offline on the
  first run, a failed download, or a missing configured `asr.model_dir`: the
  winbar says "no speech model" and why, the recordings are kept, and the
  next press tries again. A default model that is there but does not load is
  checked against its pinned sha256 and downloaded again once. Only the
  default models are ever downloaded; for a model you configured, fix the
  path and press again.

- **What you type is saved as you type it.** A change in the dictation file
  is written at once in Normal mode, and 300 ms after you stop typing in
  Insert mode (at once when you leave it), with no `:w` and without your
  format-on-save; `:q` writes and quits, and so does `:q!`: quitting never
  discards an edit.
- **The window opens beside the pointer, never under it.** Its frame keeps
  20 pixels (scaled by `Xft.dpi` / 96) right of and below the pointer, or
  left of or above it where the monitor has no room. So under
  focus-follows-mouse (i3's default) a nudge of the mouse does not focus it,
  and moving the pointer into it does, as with any window: that and your
  click are the only ways it gets the focus. A window too large to fit beside
  the pointer either way opens centred around it; move the pointer out and
  back in, or click, to focus it. Under Wayland (Xwayland) the pointer cannot
  be read: the window asks for the monitor's bottom-right corner instead,
  and sway centres it.
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
  (`audio.postroll_seconds`), so a word still sounding at key-up is not cut off.
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
  (VAD) hears nothing, nothing is appended. The detector is always on.
- **Speech the VAD heard is not dropped by the recogniser.** Parakeet sometimes
  returns nothing for a short sentence; such a chunk is decoded once more
  without its trailing silence.
- **Previews pause on a long unsettled tail** (`preview.max_seconds`, 30 s)
  and resume by themselves; text keeps landing while they are paused, 30 s at
  a time even when you speak so slowly that no sentence ever ends.
- **A long recording costs no more memory than a short one.** Audio that has
  been transcribed is dropped as you speak; what is held is the sentence you
  are still in. The recovery WAV keeps the whole recording.
- **A latch you forget stops by itself** after five minutes without speech
  (`capture.silence_timeout_seconds`). It is an ordinary stop: the tail is decoded,
  everything spoken is kept, and the winbar says "stopped after silence" until
  your next press, which starts a new recording. The timeout runs from the
  last speech the detector heard or text the recogniser produced, so a pause
  while you think is not silence to it — but music or a conversation in the room is speech to the
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
  is reachable only where nothing lands: with a `preview.interval_seconds` long
  enough that few ticks fire.
- **Before the speech model is ready there is no silence timeout**: nothing
  decodes while you speak, so there is nothing to measure, and the two limits
  above are what end a forgotten capture. A `preview.interval_seconds` long enough
  to starve the timeout is not silently ignored: the daemon refuses a
  configuration whose timeout is under two ticks, and says which two keys
  disagree.

## A window that opens by itself

By default the daemon opens the dictation window itself, on the first
key-down: a window it draws, beside the mouse pointer (`mode = "pane"`). It needs
no rule in your window manager's config: the window carries the properties
that make a window manager refuse it focus, and on sway the daemon adds a
`no_focus` rule over sway's IPC before each window. It works on X11, and on
Wayland through Xwayland. That it never takes the focus is verified,
headless, on i3, sway, Openbox and KWin (Wayland and X11); GNOME (Mutter),
Hyprland and other window managers are not tested — **new, try it before you
rely on it**, and on an untested desktop `mode = "attach"` is the safe
choice. It does not survive
a daemon restart: the window belongs to the daemon, which writes everything
in it to its file before it goes. On sway it will not open on an empty
workspace, since sway focuses the first window on one whatever the rules say.

`mode = "attach"` has you open the editor yourself (`spokenpad editor`, in any
terminal), and the daemon writes into it; it works on any desktop, Wayland
without Xwayland included. No window may take focus, and none does.

`mode = "managed"` (your terminal on i3 or sway, with a `no_focus` rule in
your config) was removed on 2026-09-22; a config that still sets it, or
`terminal`, `window_instance` or `window_fraction`, is refused with a message
that says so. Delete those lines, and any `for_window` rule you added for
spokenpad: the pane needs none.

**Setting up the pane:**

1. Nothing to set: it is the default. What you may want to change:
   ```toml
   [nvim]
   # font_family = "monospace"   # whatever `fc-match monospace` gives
   # font_size = 11.25           # points, as Alacritty's font.size
   # pane_dimensions = { columns = 72, lines = 20 }
   # pane_layout = "tiled"       # i3 and sway tile it instead of floating it
   ```
   `[nvim]` changes apply to the next window; no restart is needed.
2. The window is X11 even on Wayland, so the daemon needs `DISPLAY` from
   your systemd user manager, as described under [Install](#install). On
   sway it also looks for `SWAYSOCK` there; without it, it finds sway's
   socket in `$XDG_RUNTIME_DIR` by the process that runs the display, and
   refuses to open the pane when it cannot reach sway.
3. `spokenpad check` says whether the libraries, the font and the display are
   all there, and names what is missing.

With no display, or with a library missing, dictation still works: the text
goes to a dictation file and the next editor opens on it. Nothing is lost and
the daemon does not fail.

Details and placement: [docs/nvim-window.md](https://github.com/membranepotential/spokenpad/blob/main/docs/nvim-window.md).

## Configuration

spokenpad reads `~/.config/spokenpad/config.toml`. Every key is optional.
[`config.example.toml`](config.example.toml) documents every key at its
default; copy only what you change. An unknown key is an error, so a typo
cannot pass silently. Keys are not configured here; see
[Bind your keys](#bind-your-keys). The most useful keys:

| key | default | what it does |
|---|---|---|
| `capture.silence_timeout_seconds` | `300` | seconds without speech after which a latched capture ends by itself; `0` turns it off, and the four-hour limit still applies. Must be at least twice `preview.interval_seconds` |
| `asr.vocabulary` | `[]` | words to bias Parakeet towards, such as `["kubectl", "nginx"]`; switches to beam search, which sometimes drops a sentence ([docs/asr.md](docs/asr.md)) |
| `nvim.mode` | `"pane"` | `pane`: the daemon opens a window it draws itself; `attach`: you run `spokenpad editor` |
| `nvim.font_family`, `nvim.font_size` | `"monospace"`, `11.25` | pane mode only: the font it draws with, sized in points exactly as Alacritty's `font.size` (scaled by the X resource `Xft.dpi`), so the same numbers give the same cells — provided `Xft.dpi` is set (`xrdb` or `~/.Xresources`): the pane does not read an XSETTINGS daemon's `Xft/DPI` or RandR's physical screen size, which Alacritty falls back to, and uses 96 dpi instead |
| `nvim.pane_layout` | `"floating"` | pane mode only: `"tiled"` has i3 and sway tile it beside your window instead; allowed only where proven unfocused (i3, sway, Openbox, KWin), floating elsewhere; the log says which it opened with. An old i3 rule `for_window [instance="spokenpad"] floating enable` floats it: delete it |
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
spokenpad check                   validate the config, load the models and
                                  list the input devices
spokenpad fetch-models [--dir DIR]
                                  download the default models ahead of time
```

Global options: `-c/--config PATH`, `--model-dir DIR`, `-v/--verbose` (debug
log to stderr), `--log-file PATH` (`none` for no file). The daemon also takes
`--dump-audio DIR`, which saves each capture as decoded.

Exit codes: `2` a command line spokenpad cannot parse, `3` another spokenpad
daemon is already running, `4` the WAV given to `transcribe` is unreadable or
not 16 kHz, `5` `check` found that the pane cannot open on this machine, `6`
model files missing (`check`, `transcribe`), `1` anything else, including `start`, `stop`, `toggle` or `cancel`
finding no daemon listening. On the socket `spokenpad.socket` listens on,
they first try to start that unit once, and say why they failed in a desktop
notification as well, since a key binding's output goes nowhere. The daemon
itself does not
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
[docs/evaluation.md](https://github.com/membranepotential/spokenpad/blob/main/docs/evaluation.md) and [docs/asr.md](https://github.com/membranepotential/spokenpad/blob/main/docs/asr.md).

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

Technical documentation is in [docs/](https://github.com/membranepotential/spokenpad/blob/main/docs/README.md): the
[architecture](https://github.com/membranepotential/spokenpad/blob/main/docs/architecture.md), the [hard constraints](https://github.com/membranepotential/spokenpad/blob/main/docs/constraints.md),
the [progressive commit](https://github.com/membranepotential/spokenpad/blob/main/docs/progressive-commit.md) design, the
[dictation window](https://github.com/membranepotential/spokenpad/blob/main/docs/nvim-window.md), and the [decision log](https://github.com/membranepotential/spokenpad/blob/main/docs/decisions.md).

## License

spokenpad's source is MIT, see [LICENSE](https://github.com/membranepotential/spokenpad/blob/main/LICENSE). The binary also contains
sherpa-onnx (Apache-2.0), ONNX Runtime (MIT) and the other libraries of
sherpa-onnx's prebuilt archive, eSpeak NG (GPL-3.0-or-later) among them; the
default models it downloads are NVIDIA's Parakeet TDT 0.6B v3 (CC-BY-4.0)
and Silero VAD (MIT). [THIRD-PARTY.md](https://github.com/membranepotential/spokenpad/blob/main/THIRD-PARTY.md) lists every component
and its licence; the package installs it to
`/usr/share/licenses/spokenpad/`.
