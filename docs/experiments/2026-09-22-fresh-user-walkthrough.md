# A new Arch user's first hour with spokenpad

_2026-09-22, the code at b10ce56 (managed mode removed), packaged with
`packaging/aur/PKGBUILD` (pkgver 0.2.0, sherpa-onnx 1.13.8). Manjaro,
makepkg 7.1.0, PipeWire 1.6.8, WirePlumber 0.5.17, i3 on Xvfb, Neovim 0.12.5.
Default model: Parakeet TDT 0.6B v3 int8, greedy search, Silero VAD._

## Question

The product goal is "install the package and it works": no install script,
at most a setup guide, ideally no setup step at all. Where does a new Arch or
Manjaro user who follows only `README.md` (and what it links) get stuck, get
confused, or have to do a setup step? What does the built package contain,
and does it hold what the README promises?

## Method

1. Read `README.md` top to bottom as a newcomer, noting every step, every
   assumed piece of knowledge, and every sentence that did not make sense
   without context.
2. Built the package with `makepkg -f` (no `-s`, no `-i`, nothing installed)
   in a scratch copy. The one change to the PKGBUILD: `v0.2.0` is not tagged,
   so the first source became a local tarball made with
   `git archive --prefix=spokenpad-0.2.0/ HEAD`
   (`source=("$pkgname-$pkgver.tar.gz" …)`). The sherpa-onnx archive was
   downloaded and checked against its pinned sum as written. Inspected the
   result with `tar tf`, `ldd` and `namcap`, and extracted it to a scratch
   root.
3. Ran first use from the extracted `/usr/bin/spokenpad` in a private
   environment:
   - Fresh `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`,
     `XDG_CACHE_HOME` under the scratchpad. `XDG_RUNTIME_DIR` was a mode-0700
     directory `/tmp/sprun.XXXX`: the scratchpad path is too long for a Unix
     socket (108 bytes), which PipeWire, dbus-daemon and i3 all refused.
   - Display `:61` (Xvfb 1920x1080) running i3 with the stock
     `/etc/i3/config` minus its `exec` lines (so `focus_follows_mouse` is at
     its default, yes), a private IPC socket, and an `xterm` as the user's
     focused window. A private `dbus-daemon`; D-Bus activated `dunst` on it,
     and `dbus-monitor` recorded every notification.
   - Microphone: every spokenpad process ran in
     `bwrap --dev-bind / / --tmpfs /dev/snd --tmpfs /run/dbus`, so no sound
     card and no system bus was reachable. ALSA's `null` PCM was tried first
     and rejected: it is not paced (`arecord -d 2` returned in 23 ms). What
     worked: a private PipeWire and WirePlumber (ALSA, Bluetooth and camera
     monitors disabled) with a `libpipewire-module-loopback` that exposes a
     sink `synthetic-speaker` and a default source `synthetic-mic`. The
     daemon's PortAudio reads the ALSA `default` PCM, which `pipewire-alsa`
     routes to that source; `pw-cat -p --target synthetic-speaker speech.wav`
     "speaks" into it. The speech is an `espeak-ng` pangram of 6.8 s, not a
     recording of anyone.
   - Service: `systemd-socket-activate -l $XDG_RUNTIME_DIR/spokenpad.sock
     /usr/bin/spokenpad`, with `-E` for `XDG_RUNTIME_DIR`, `DISPLAY`,
     `DBUS_SESSION_BUS_ADDRESS` and `LANG`. Without `-E` the tool passes only
     `PATH`, `HOME`, `USER` and `TERM`, and the daemon refused the socket
     because it expected one under `$XDG_STATE_HOME`: an artefact of the
     emulation, not something a user meets. Differences from the real units:
     the socket was mode 0644 instead of 0600, and there was no `Restart=`,
     `PartOf=` or `RemoveOnStop=`. The user's systemd manager was never
     touched.
   - Models: the first press downloaded the default models into the private
     `XDG_DATA_HOME`. They were deleted at the end, with the whole private
     `HOME`.
4. Ran the checklist: `--help` of every subcommand, `check`, a first press
   with no model, push-to-talk, toggle latch, cancel (during and after a
   capture), closing the pane with `:q` during a capture, a config edit
   (`font_size`, `pane_dimensions`), a mistyped key (in a running daemon and
   at start-up), the removed `mode = "managed"`, attach mode with
   `spokenpad editor` in an xterm, `transcribe` on a recovery WAV,
   `fetch-models` twice, bare `spokenpad` with a daemon running, the daemon
   without `DISPLAY`, copying the text out with and without a clipboard
   tool, and the README's own i3 bindings driven through the private i3
   (XTEST key events on `:61` only).

## Data

The synthetic pangram, played into the synthetic microphone about 12 times.
The daemon's recovery WAVs and dictation files of those runs, deleted
afterwards. Screenshots of `:61`, kept in the scratchpad only.

## Results

### What worked

| step | observed |
|---|---|
| `makepkg -f` | built in 66 s (warm cargo cache); `namcap` flags only the four run-time dependencies the PKGBUILD already explains |
| package contents | `/usr/bin/spokenpad` (38 MB, links libportaudio, libstdc++, libgcc_s, libc); both user units; `sockets.target.wants/spokenpad.socket -> ../spokenpad.socket`; `/usr/share/licenses/spokenpad/LICENSE`; `/usr/share/doc/spokenpad/{README.md,config.example.toml,dev.conf.example}`; `/usr/share/spokenpad/{i3,sway}/spokenpad.conf` (bindings, commented out). No man page, no shell completions, no `.install` message |
| first press, no model | the socket started the daemon; the pane opened at the pointer within a second, "⚠ downloading the speech model"; the models downloaded in about 24 s; "speech model ready after 26.7s"; the recording made meanwhile was transcribed into the pane |
| focus | the xterm kept the focus through every pane opening, also with the pointer inside the new pane and `focus_follows_mouse yes`; only a click focused the pane |
| push-to-talk, latch, cancel | all as documented; a `cancel` after release exits 0 and does nothing |
| `:q` during a capture | capture cancelled, text kept, one notification "spokenpad: recording cancelled" with the file path |
| config edit | `font_size = 16`, `pane_dimensions = { columns = 100, lines = 12 }` applied at the next window, no restart |
| typo in the config | `check`: TOML error with the list of valid keys, exit 1. Running daemon: the next window says "⚠ config not reloaded". Start-up: runs on the defaults, same notice |
| `mode = "managed"` | refused with a message saying what to do instead |
| attach mode | text dictated with no editor went to a file plus one notification; `spokenpad editor` opened that file, and later dictation landed in it |
| no `DISPLAY` | text to a file; notification "the dictation window could not open", explaining `import-environment` |
| `transcribe` | recovery WAV decoded in 3.2 s; a 22.05 kHz WAV or a missing file exits 4 with a clear message |
| `fetch-models` again | "present" for each file, 3.7 s |
| bare `spokenpad` while the daemon runs | exit 3, "another spokenpad daemon is running" |
| `spokenpad start` with no socket | exit 1, a clear message on stderr naming `systemctl --user status spokenpad.socket` |
| README i3 bindings | `bindcode 194`, `--release 194`, `Shift+194`, `195` started, stopped, latched and cancelled through a real i3 |

### Setup steps the README asks for

| step | needed? |
|---|---|
| clone and `makepkg -si` | yes, until the package is on the AUR; needs `base-devel` and `git`, which the README does not name |
| `spokenpad fetch-models` | no: the first press downloads the model and keeps what you dictate meanwhile |
| bind keys | yes, by design (spokenpad reads no keyboard) |
| log out, or `systemctl --user start spokenpad.socket` | only once, but a press before it does nothing visible (C1 below). Avoidable: see the fix for C1 |
| `import-environment DISPLAY XAUTHORITY` | usually already done: on Arch, `/etc/X11/xinit/xinitrc.d/50-systemd-user.sh` (from systemd) imports both, and LightDM's `Xsession` and the stock `xinitrc` source it; sway's `/etc/sway/config.d/50-systemd-user.conf` imports `DISPLAY` and `SWAYSOCK`. This machine's i3 session has both in the manager already. Needed only for a custom `~/.xinitrc` or a sway config without `include /etc/sway/config.d/*` |
| `xset -r 194` | only so that a held Shift+key cannot end its own latch |
| install a clipboard tool | not in the README's steps, but needed to get any text out (B1 below) |

### Friction, ranked

#### Blocks a new user

**B1. Without `xclip`, `xsel` or `wl-clipboard`, the text cannot leave the
pane.** _Where:_ README "Use it"; PKGBUILD `optdepends`. _What happened:_ with
the three tools hidden, `ggVG"+y` in the pane printed "clipboard: No provider.
Try …checkhealth" and the clipboard stayed empty; with `xclip` present the same
keys copied the text. spokenpad never pastes, so copying is the only way to
use what was dictated, and the README mentions a clipboard provider only under
the opt-in `copy_to_clipboard`. A fresh Arch install has none of the three.
_Needs:_ a working `"+y` out of the box, and one sentence on how to get text
out. _Fix:_ make `xclip` a dependency (the pane is always an X11 window, even
on Wayland), or let the pane own the X `CLIPBOARD` selection itself; add "copy
it out with `"+y`" to "Use it".

**B2. The documented install cannot run yet.** _Where:_ README "Install";
`packaging/aur/PKGBUILD`. _What happened:_ the PKGBUILD's first source is the
GitHub tarball of tag `v0.2.0`, which does not exist, so `makepkg` fails before
it builds; its sum is `SKIP`, and `packaging/aur/` has no `.SRCINFO`. The
cloned repository itself is never built. _Needs:_ one command. _Fix:_ tag
`v0.2.0`, run `updpkgsums`, generate `.SRCINFO`, publish to the AUR, and make
the README's first line `yay -S spokenpad` (or `pamac build spokenpad` on
Manjaro), with "needs `base-devel`" beside the manual route.

#### Confuses

**C1. A press before the socket runs does nothing visible.** _Where:_ README
"Install" step 3. _What happened:_ `spokenpad start` exits 1 with a good
message, but on stderr, which a window manager's `exec` discards. The package's
`sockets.target.wants` link takes effect only at the next login; the pacman
hook reloads user managers but starts nothing. _Needs:_ the first press after
install to work. _Fix:_ when the connect fails with "no such file" or
"connection refused", let the client run `systemctl --user start
spokenpad.socket` once and retry; failing that, `notify-send` the message.

**C2. The download shows no progress in the default pane.** _Where:_ first
press on a new machine; README "While you dictate" ("with the percentage and
how many recordings wait"). _What happened:_ the winbar showed only "⚠
downloading the speech model" for the whole download. The detail ("N%;
dictation is recorded and transcribed once it is ready", `kept()` in
`src/core/session.rs`) needs about 105 columns beside the headline, and the
default pane has 72, so the percentage and the promise that nothing is lost
are never drawn. Here the download took 24 s; on a slow link it is minutes of
a window that looks stuck. _Fix:_ put the percentage in the headline
("downloading the speech model, 37%"), and shorten the detail.

**C3. "config not reloaded" never says why.** _Where:_ the winbar after a typo.
_What happened:_ the reason is a multi-line TOML error; it did not fit even in
a 100-column pane, so only the headline showed. The user must know to run
`spokenpad check` or read the log. _Fix:_ a detail short enough to fit, such as
"unknown key `pane_dimension` (line 3); run spokenpad check".

**C4. Importing `DISPLAY` does not help until the daemon restarts, and nothing
says so.** _Where:_ the "could not open" notification; README "Install" and
"Setting up the pane". _What happened:_ the notification names `systemctl
--user import-environment DISPLAY` and says `$DISPLAY` "was not set when
spokenpad started", but not that the daemon has to restart. A user who imports
it and presses again gets the same failure. The README tells every "plain i3
or sway" user to add the import, although the Arch session scripts listed
above already do it. _Fix:_ let the daemon read `DISPLAY`, `XAUTHORITY` and
`SWAYSOCK` from the user manager (`systemctl --user show-environment`, or its
D-Bus equivalent) each time it opens a window, which removes both the restart
and the README step; failing that, add "then `systemctl --user restart
spokenpad`" to the message, and limit the README step to custom `.xinitrc`
files and sway configs without `/etc/sway/config.d/*`.

**C5. The README contradicts itself about setup and installed files.**
_Where:_ "A window that opens by itself". _What happened:_ "as in install step
4" points at the `DISPLAY` import, but step 4 is "Hold your push-to-talk key";
the import is the paragraph after the steps. "Then restart the service" after
`spokenpad check` has no reason: nothing runs yet, and `[nvim]` changes apply
without a restart. "The window rules the package used to ship in
`/usr/share/spokenpad/i3` and `sway` are gone" while the package ships the key
binding examples at exactly those paths. The README links the examples by
repository path (`packaging/i3/spokenpad.conf`) and never names the installed
`/usr/share/spokenpad/{i3,sway}/spokenpad.conf`; in `/usr/share/doc/spokenpad/`
its relative links are broken. _Fix:_ correct the reference, drop the restart,
name the installed paths in "Bind your keys".

**C6. Choosing a key assumes F16.** _Where:_ "Bind your keys". _What happened:_
every example uses F16/F17 (keycodes 194/195), which most keyboards lack. The
user has to pick a free key, find its keycode with `xev`, and change it in four
or five places consistently. _Fix:_ suggest a key most keyboards have and few
programs use (Pause, Scroll Lock, Menu, or a mouse side button, which i3 binds
with `bindsym --whole-window button9`), and give the i3 example with `bindsym`
and a key name, so one name changes.

**C7. The desktops a newcomer is most likely to use are the least covered.**
_Where:_ "Features" ("works on any Linux desktop"), "Bind your keys". _What
happened:_ the pane's no-focus behaviour is verified on i3, sway, Openbox and
KWin; GNOME (Mutter, X11 or Xwayland) and Hyprland are not in that list, but
the README gives Hyprland bindings and GNOME instructions without saying so.
GNOME and KDE get one sentence ("add a custom shortcut") and no menu path.
_Fix:_ list GNOME and Hyprland as untested until they are verified; name the
settings page for GNOME's custom shortcuts and Plasma's command shortcuts
(this walkthrough did not check the current menu names).

**C8. No way to see or choose the microphone from spokenpad.** _Where:_
`spokenpad check`; `audio.device` in `config.example.toml`. _What happened:_
`check` states that it opens no device, so "is it using my headset or my
webcam?" is answered only by a "nearly silent" notice after a press.
`audio.device` takes a device-name query, but nothing lists the names. _Fix:_
let `check` list the input devices and mark the one the daemon would open.

**C9. `spokenpad check` on a new machine downloads 670 MB silently.** _Where:_
README "Install" step 1 ("`spokenpad check` loads the model"). _What happened:_
two INFO log lines, then nothing until the download ends; `fetch-models` shows
a percentage for the same work. _Fix:_ the same progress line in `check` and
`transcribe`.

**C10. Notifications are optional, but sometimes they are the only channel.**
_Where:_ PKGBUILD `optdepends` (`libnotify`). _What happened:_ when the pane
cannot open (no `DISPLAY`, a missing library), the notification is the only
thing the user sees; without `libnotify` a press shows nothing at all. _Fix:_
make `libnotify` a dependency.

#### Polish

- **P1.** README sections for existing users ("Upgrading from
  `scripts/install.sh`", "`mode = "managed"` … was removed", the old i3 rule in
  the `pane_layout` row) are noise for a newcomer; move them to release notes.
- **P2.** README "Requirements" says x86-64 or aarch64; the PKGBUILD is
  `arch=('x86_64')` only.
- **P3.** No `.install` message after installation pointing to the key
  bindings and `/usr/share/spokenpad/`; no man page, no shell completions.
- **P4.** `license=('MIT')`, and only spokenpad's LICENSE is shipped, while the
  binary statically links sherpa-onnx, which is Apache-2.0 (the crate's
  `Cargo.toml`); onnxruntime's and the Rust crates' notices are not shipped
  either. The default model's licence is mentioned nowhere (NVIDIA publishes
  Parakeet TDT 0.6B v3 under CC-BY-4.0; not verified here).
- **P5.** CLI help: `start`, `stop`, `toggle` and `cancel` list `--model-dir`
  and `--log-file`, which do nothing for them; `transcribe`'s `<WAV>` and
  `--out` have no description; the top-level help does not say that no
  command runs the daemon.
- **P6.** `fetch-models` downloads `test_en.wav`, which the daemon's own
  download skips, so a machine set up by the first press gets one more
  download later.
- **P7.** The emulated journal had 81 "ALSA lib …" lines per daemon start
  (PortAudio probing ALSA devices). With no sound card visible in the sandbox
  this is an upper bound; a real machine probably shows fewer. Not verified on
  real hardware.
- **P8.** A cancel leaves no trace in the winbar (the preview just vanishes);
  only the log says "capture cancelled".
- **P9.** The daemon's automatic download and `fetch-models` or `check` write
  the same `<file>.part` without a lock. The sha256 check would catch a
  corrupt result. Not tested.

### What was not tested, and why

- A real microphone and real audio timing. The synthetic source delivered only
  about 55% of real time to the daemon: each 7.9 s hold gave 4.0–4.4 s of
  audio with the speech time-compressed, and every press logged "the device
  delivered less than the post-roll in time". `arecord` through the same path
  was paced correctly (3.08 s for `-d 3`). This may be the dummy-driven
  loopback or a real PortAudio-over-`pipewire-alsa` problem; it needs a look
  on real hardware (compare "held" and "audio" in the daemon's INFO line).
  Transcripts are therefore not quality data.
- The real systemd units: `SocketMode=0600`, `Restart=on-failure`,
  `PartOf=graphical-session.target`, activation through
  `sockets.target.wants` at login, and the pacman hooks. `makepkg -si` and
  `pacman -U` were not run.
- sway, KDE Plasma, GNOME, Hyprland, and Wayland without Xwayland; HiDPI.
- A heavy personal Neovim config in the pane (`nvim.init` defaults to the
  user's own config; the private `HOME` had none).
- A slow or failing network during the first download.

## Conclusion

Once the package is installed, the socket is running and a key is bound, the
first press works on a machine with no model and no configuration: the pane
opens without taking the focus, the model downloads, and nothing said
meanwhile is lost. The config reload, attach mode, recovery and error messages
behave as the README says.

The distance to "install it and it works" lies elsewhere:

1. getting the text out needs a clipboard tool the package does not pull in
   (B1);
2. the install route does not exist yet (B2);
3. two setup steps are avoidable in code: starting the socket (C1) and
   importing `DISPLAY` (C4, already done by Arch's session scripts);
4. the first-run feedback hides what matters: download progress (C2) and the
   reason for a config error (C3).

Binding a key stays the one setup step by design; C6 and C7 would make it
easier. No decision followed yet; each fix above is a proposal.
