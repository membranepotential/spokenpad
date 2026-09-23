# Using spokenpad

What spokenpad does while you dictate, in detail. The README has the short
version; this page is for when something surprised you.

## The winbar and its notices

The winbar of the dictation window is the whole interface: a phase dot, a
level meter while recording, and a lock while latched. The live preview of
the sentence you are in is drawn where that text will land. When something
happened to a capture, the winbar shows a **notice** beside the phase label
until the next key press. It is shown once per capture and never replaces
the preview.

A notice has two parts: a **headline** (`⚠ microphone gap`), always drawn,
and a sentence explaining it, added only when the window is wide enough for
all of it. Two headlines carry their figure themselves, because the
explanation seldom fits beside them: the download's percentage
("downloading the speech model, 37%"), and why the config did not load
("config not reloaded: unknown field `pane_dimension` (line 3)";
`spokenpad check` prints the whole report). A narrow window gives up the
level meter first, then the explanation, but never the phase label or the
headline.

When two things happen to the same capture, the more serious one is shown:
memory limit reached > capture incomplete > microphone unavailable > capture
not kept > recording lost > recording shortened > recording partly
transcribed > no speech model > microphone gap > reached the time limit >
stopped after silence > config not reloaded > nearly silent > held too
briefly > downloading the speech model > loading the speech model >
transcribing recordings > recording cancelled > preview paused. The speech
model's notices are not about one capture: they stay until the model is
ready and has caught up. The log always has the full sentence and paths.

## Starting, stopping, cancelling

- **Latch.** Press Shift with the key (`spokenpad toggle`), then let go and
  keep talking. Press the key again, with or without Shift, to stop. After
  any stop, automatic or not, the next press starts a new recording: it
  does not resume the old one, and it clears the notice in the winbar.
- **Cancel.** The cancel key (`spokenpad cancel`) while recording keeps the
  text that already reached the file and the WAV; the winbar says "recording
  cancelled" until the next press. After release, cancel does nothing.
- **Closing the window cancels the recording.** Close the pane while a
  recording runs, through your window manager or with `:q`, and the
  recording is cancelled as if you had pressed the cancel key. No window
  opens again until you press the key. An editor that crashes is different:
  the recording goes on, and its next text opens a new window.
- **Recording continues for a quarter second after you let go**
  (`audio.postroll_seconds`), so a word still sounding at key-up is not cut
  off. Pressing the key again within the first 150 ms continues the same
  recording; pressing it later ends the wait at once and starts the next
  capture.
- **A key that repeats while held.** A `start` while recording is ignored,
  and a `start` within 150 ms of a `stop` means the key never came up, so
  the recording continues. A `stop` during a latched recording is ignored,
  because Shift and the key come up in either order.
- **A tap shorter than 120 ms is discarded**, with "held too briefly — hold
  the key while speaking" in the winbar. The WAV is still written.

## Limits

- **A latch you forget stops by itself** after five minutes without speech
  (`capture.silence_timeout_seconds`). It is an ordinary stop: the tail is
  decoded, everything spoken is kept, and the winbar says "stopped after
  silence". The timeout runs from the last speech the detector heard or text
  the recogniser produced, so a pause while you think is not silence to it;
  music or a conversation in the room is speech to the detector, and only
  the limits below end such a capture. A key that is still down is not
  stopped this way: a held key re-fires its binding every few tens of
  milliseconds, so stopping it would only start the next capture.
- **No capture runs longer than four hours.** This is not a setting: it
  keeps the recovery WAV readable, since a WAV's own size field overflows at
  about 37 hours. The winbar says "reached the time limit".
- **Past the 60-minute in-memory limit** the capture ends, its tail is
  decoded, and the winbar names the recovery WAV. Text that lands while you
  speak is what makes audio droppable, so this limit is reachable only where
  nothing lands: with a `preview.interval_seconds` long enough that few ticks
  fire. The daemon refuses a configuration whose silence timeout is under two
  ticks, and says which two keys disagree.
- **A long recording costs no more memory than a short one.** Audio that has
  been transcribed is dropped as you speak; what is held is the sentence you
  are still in. The recovery WAV keeps the whole recording.
- **Previews pause on a long unsettled tail** (`preview.max_seconds`, 30 s)
  and resume by themselves; text keeps landing while they are paused.

## What is transcribed

- **A capture with no speech in it is not transcribed.** A recogniser given
  silence invents words ("Thank you."), so when the voice activity detector
  (VAD) hears nothing, nothing is appended. The detector is always on.
- **Speech the VAD heard is not dropped by the recogniser.** Parakeet
  sometimes returns nothing for a short sentence; such a chunk is decoded
  once more without its trailing silence.
- **A capture much shorter than the hold, or nearly silent,** says so in the
  winbar. "Nearly silent" is the usual sign of the wrong microphone.

## Before the model is ready, and after a restart

- **Dictation works from the first press.** The daemon answers presses as
  soon as it starts and loads the model afterwards (a few seconds; the first
  time, after downloading it). A capture made meanwhile goes to its
  recording only, and the winbar says "loading the speech model" or
  "downloading the speech model" with the percentage and how many recordings
  wait. Once the model is ready, each recording is transcribed into the
  window in order ("transcribing recordings"), just without a preview. Such
  a capture ends by itself after 60 minutes. There is no silence timeout
  before the model is ready: nothing decodes, so there is nothing to measure.
- **Stopping the daemon does not lose a capture.** A capture still recording
  when the daemon stops (a restart, an upgrade, a logout), or whose last
  text is not written yet, is listed for the next start, which transcribes
  the rest from where its text reached. A capture you cancel is not.
- **A recording that fails partway** says "recording partly transcribed":
  the next start tries the rest again, or, if it failed at the same point
  before, the notice gives the `spokenpad transcribe --from SECONDS` that
  recovers it. One cut short since it was recorded says "recording
  shortened"; one that is gone says "recording lost".
- **A model that cannot be had does not stop dictation.** Offline on the
  first run, a failed download, or a missing configured `asr.model_dir`: the
  winbar says "no speech model" and why, the recordings are kept, and the
  next press tries again. A default model that is there but does not load is
  checked against its pinned sha256 and downloaded again once. Only the
  default models are ever downloaded.

## The microphone

spokenpad records from the default input device, the one your sound server
(PipeWire, PulseAudio) uses as its default source. `spokenpad check` lists
every input device with its host API and marks the one the daemon opens;
listing opens none of them. To use another, set `audio.device` to words from
its name, such as `device = "USB"`: they are matched, case-insensitive and in
order, against PortAudio's device name and host API. The daemon picks it up
after `systemctl --user restart spokenpad`.

A microphone that stops delivering audio is reopened, and the capture it
interrupted is marked as having a gap. While idle the same repair runs
quietly, so the pre-roll is ready at the next press. A microphone that is not
there when the daemon starts is looked for the same way, after a wait that
doubles up to a minute; a press meanwhile says "microphone unavailable" and
always tries at once.

## The dictation window

`nvim.mode` chooses who opens the editor.

- **`"pane"` (default).** The daemon opens a window it draws itself, with
  Neovim embedded, on the first key-down. It needs no rule in your window
  manager's config: the window carries the properties that make a window
  manager refuse it focus, and on sway the daemon adds a `no_focus` rule over
  sway's IPC before each window. It works on X11, and on Wayland through
  Xwayland. That it never takes the focus is verified, headless, on i3,
  sway, Openbox and KWin (Wayland and X11); on other desktops, try it before
  you rely on it. On sway it will not open on an empty workspace, since sway
  focuses the first window on one whatever the rules say. It does not
  survive a daemon restart: the daemon writes everything in it to its file
  before it goes.
- **`"attach"`.** spokenpad opens no window. You run `spokenpad editor` in
  any terminal, and the daemon writes into that Neovim. It works on any
  desktop, Wayland without Xwayland included. Close the editor to end the
  passage: the next editor starts a new file. Text dictated with no editor
  open goes to a file, the log says where, and the next `spokenpad editor`
  opens that file.

With no display, or with a library the pane needs missing, dictation still
works: the text goes to a dictation file and the next editor opens on it.
`spokenpad check` says whether the libraries, the font and the display are
all there, and names what is missing.

**Where the pane opens.** Its frame keeps 20 pixels (scaled by `Xft.dpi` /
96) right of and below the pointer, or left of or above it where the monitor
has no room. So under focus-follows-mouse a nudge of the mouse does not focus
it, and moving the pointer into it does, as with any window: that and your
click are the only ways it gets the focus. A window too large to fit beside
the pointer either way opens centred around it. Under Wayland (Xwayland) the
pointer cannot be read: the window asks for the monitor's bottom-right corner
instead, and sway centres it. `nvim.pane_layout = "tiled"` has i3 and sway
tile it beside your window instead.

**Typing while you dictate.** In Insert mode your cursor stays where you are
typing: dictated text lands at the end of the text, and what you type stays
in one piece beside it. In Normal mode, a command you have only half typed (a
count, `g`, `"`, `f`) makes Neovim hold back everything spokenpad sends until
you finish or cancel it: the preview, the level meter and the text stop
moving, and the last row says "waiting for the editor: finish or <Esc> the
pending command". Press <kbd>Esc</kbd> and everything catches up. After two
minutes spokenpad stops waiting: that text and what follows go to a separate
dictation file, as if no editor were open, and the log names the file.

**What you type is saved as you type it.** A change in the dictation file is
written at once in Normal mode, and 300 ms after you stop typing in Insert
mode (at once when you leave it), with no `:w` and without your
format-on-save. `:q` writes and quits, and so does `:q!`: quitting never
discards an edit. The files are in `~/.local/state/spokenpad/dictation/`,
one Markdown file per editor.

**Copying text out.** spokenpad never pastes into other programs. In the
window, `"+y` copies to the system clipboard (`ggVG"+y` copies everything); a
plain `y` stays in Neovim unless your own Neovim config sends it to the
clipboard. The pane uses `xclip` for this, even on Wayland, where it is an
Xwayland window; `spokenpad editor` in a Wayland terminal uses `wl-copy` from
`wl-clipboard`. With `nvim.copy_to_clipboard = true`, the dictation Neovim
copies the whole buffer to `+` after every release.

**Pasting in.** In the pane, Ctrl+V pastes the clipboard in Insert mode and
on the command line, as a terminal does; in Normal mode it is Visual block.
Ctrl+Shift+V pastes in any mode.

## Display and the systemd user manager

The daemon runs under your systemd user manager, not inside your session, and
takes the X display from the manager before each window it opens. Most
sessions give the manager one: the startx and display manager session scripts
on Arch (`/etc/X11/xinit/xinitrc.d/50-systemd-user.sh`), GNOME, KDE Plasma,
and sway's `/etc/sway/config.d/50-systemd-user.conf`. If the window does not
open and the log says there is no display (a custom `~/.xinitrc`, or a sway
config that does not include `/etc/sway/config.d/*`), add this to your window
manager's config, and press again:

```
exec systemctl --user import-environment DISPLAY XAUTHORITY
```

On sway, import `DISPLAY SWAYSOCK` instead; without `SWAYSOCK` the daemon
finds sway's socket in `$XDG_RUNTIME_DIR` by the process that runs the
display. An X server whose cookie is not in `~/.Xauthority` also needs
`XAUTHORITY` in the daemon's own environment: if you imported it after the
daemon started, run `systemctl --user restart spokenpad` once.

## Exit codes

`2` a command line spokenpad cannot parse (bare `spokenpad` prints the help
and exits with it), `3` another spokenpad daemon is
already running, `4` the WAV given to `transcribe` is unreadable or not
16 kHz, `5` `check` found that the pane cannot open on this machine, `6`
model files missing (`check`, `transcribe`), `1` anything else, including
`start`, `stop`, `toggle` or `cancel` finding no daemon listening.

On the socket `spokenpad.socket` listens on, `start` and `toggle` first try to
start that unit once. `stop` and `cancel` start nothing: with no daemon there
is nothing to end. The daemon itself does not exit over a missing model, a
microphone it cannot open, or a config file that does not load (it runs on
the defaults): it says so in the winbar and tries again at the next press or
window.
