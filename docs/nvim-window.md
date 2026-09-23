# The dictation window

← [docs index](README.md) | Implemented by
[`shell/nvim/mod.rs`](../src/shell/nvim/mod.rs), its transport
[`shell/nvim/rpc.rs`](../src/shell/nvim/rpc.rs), and
[`spokenpad.lua`](../src/lua/spokenpad.lua). The reasoning for
using nvim at all is in
[decisions.md](decisions.md#the-sink-is-neovim-not-the-clipboard); the rules
it must not break are in [constraints.md](constraints.md).

The transcript goes to a dedicated neovim, over its msgpack-RPC socket.
No transcript is pasted anywhere, and no window that was not opened for
this purpose is ever written to.

## Two modes: who opens the editor

`nvim.mode` says who opens that neovim. It is explicit configuration, not
detection.

| `nvim.mode` | who opens it | where it works | focus |
|---|---|---|---|
| `pane` (default) | the daemon, on the first key-down, in a window it draws itself | X11, and Wayland through Xwayland | the window's own properties; on sway, a `no_focus` rule the daemon adds over IPC |
| `attach` | you, with `spokenpad editor` | any terminal, any desktop: X11 or Wayland, any window manager | the daemon opens no window, so it cannot take focus |

`pane` is the default since its focus guarantee was proven on i3, sway,
Openbox and KWin (Wayland and X11): a window that appears by itself beside
what you are reading, with nothing to install in your window manager. What
it gives up is the window's independence from the daemon: spokenpad owns it,
so a daemon restart closes it. `attach` works everywhere else — any
terminal, any desktop, also Wayland without Xwayland, where there is no X
display for a pane; a pane that cannot open says so in the log, and names
`attach`.

Until 2026-09-22 there was a third mode, `managed`: the daemon opened the
user's terminal on i3 or sway, and only after proving a `no_focus` rule for it
in the running window manager's configuration. The pane is that same window
without the rule and without the terminal, proven on more window managers, so
managed mode was removed
([decisions.md](decisions.md#managed-mode-is-removed-the-pane-replaces-it-2026-09-22)).
A configuration that still sets `mode = "managed"`, or one of the keys only it
read (`terminal`, `window_instance`, `window_fraction`), is refused with a
message that names the key and says the pane replaced it.

Everything below the mode is shared: one file per editor, the winbar
indicator and notices, the live preview, the opt-in clipboard copy after a
release, and reattaching to a live socket after a daemon restart.

## Attach mode

```
spokenpad editor
```

runs, in the terminal you typed it in, and replacing that process:

```
nvim [-u <init>] --cmd "let g:spokenpad_owner = '<marker>'" \
  --listen <socket> <dictation file>
```

with the same `nvim.editor`, `nvim.init`, `nvim.colorscheme` and ownership
marker a pane's editor gets. The daemon finds it on the next key-down: the
marker file beside the socket proves spokenpad started it, and it is adopted
on the file it was started on. From then on it is the dictation window,
exactly as if the daemon had opened it. `spokenpad editor` refuses to start a
second editor while one is listening on the socket.

### Dictating with no editor open

The key works whether or not an editor is open, and what is said is never
lost:

- **The text goes to the file.** Each commit is appended to a dictation file
  directly, with the same paragraph rule the editor applies (one blank line
  between utterances, a continued utterance extends its paragraph), and the
  file is on disk before the next commit is taken. The rule exists twice —
  `transactional_append` in Lua and `passage::append_paragraph` in Rust — and
  a test runs both on the same inputs.
- **The next editor shows it.** The file is the *pending passage*, recorded
  in `<socket>.pending`. Every later dictation with no editor continues it,
  and the next editor — `spokenpad editor`, or a pane — opens on it
  rather than on a new file. `spokenpad editor` *takes* the passage: it
  removes the pointer before the editor reads the file, under a lock
  (`<socket>.pending.lock`) that every direct write also holds, so nothing
  is ever written behind an open editor's back; until the daemon reaches
  that editor, text goes to a new pending passage. An editor the daemon opens
  itself settles the pointer once it holds the file. A pointer that names anything but a regular file inside
  `nvim.dictation_dir` is ignored.
- **An editor that exits mid-dictation loses nothing.** A request that
  could not be sent to the editor certainly did not land, so its text goes to
  the pending passage. An append that stays unconfirmed even after its
  repeat goes there too, and the log says it may be in both places:
  see [Reconnection](#reconnection).
- **Decoding is unchanged.** The file is only a different sink for the same
  commits; each is still decoded exactly once.
- **The log says where.** When a pending passage is started, the daemon logs
  a warning that says where the text is, why no window took it, and to run
  `spokenpad editor`; every write is logged too. The winbar cannot say it —
  there is no winbar — and spokenpad sends no desktop notifications. The
  next window opens on the passage.

The pointer lives beside the socket, in `$XDG_RUNTIME_DIR` by default, so a
reboot forgets it; the file stays in `nvim.dictation_dir`.

The same path catches a pane that could not be opened (no X display, a
library missing, sway unreachable): the text goes to the pending passage
instead of only to the log.

## Pane mode: the window spokenpad draws

```
nvim --embed --listen <socket> -u <init> <dated file>
```

No terminal. The daemon creates an X11 window, starts that nvim as a child
with its stdin and stdout as the channel, and attaches to it as a UI with
`nvim_ui_attach(columns, rows, { ext_linegrid = true, rgb = true })`. Neovim
then describes its display as redraw events instead of drawing to a terminal,
and spokenpad turns them into pixels. The attach is the only call the pane
waits for before the window is mapped. Neovim sources your configuration
after the attach, and a message there (an `echoerr`, a deprecation notice)
raises a hit-enter prompt that holds every later call until you press Enter —
in the window, which has to be showing for you to see it. So the pane's own
setup in Neovim ([the quit notice](#what-happens-when)) is sent without
waiting and runs once the prompt is answered; the daemon's calls over the
`--listen` socket wait for it within the 30 s startup limit, as for any
editor it starts. The editor is otherwise the same one
every mode gets — same argv, same init, same ownership marker, same
`--listen` socket — so the daemon appends to it exactly as it does to an
editor you opened with `spokenpad editor`. **Committed text does not travel over the drawing channel**:
the pane draws, and never writes to the buffer.

### Why it needs no rule

The window carries five properties, all set before it is first mapped, which
is when a window manager reads them:

| property | what it does |
|---|---|
| `_NET_WM_USER_TIME = 0` | "do not focus this window when it is mapped" (EWMH). On i3 this is the whole guarantee, and unlike `no_focus` it holds for the first window on an empty workspace. |
| `_NET_WM_WINDOW_TYPE_UTILITY` | floats it on tiling window managers, and is what refuses focus on those that ignore user time (bspwm, Hyprland's Xwayland). |
| `_NET_WM_STATE_ABOVE` | keeps it above the window you are typing in. KWin, on Wayland and on X11, stacks a window it refused focus *below* the active one otherwise. |
| `WM_HINTS input = True` | the ICCCM "passive input" model, so the window manager may give it focus *later*, when you click it — which is how you click in and type. |
| `WM_CLASS = spokenpad-pane` | the name the one rule spokenpad adds itself, on sway, matches (below). |

`_NET_WM_USER_TIME` is written **once, as zero, and never again**. The EWMH
contract is that a toolkit updates it to the timestamp of the last user
interaction, and a window manager re-reads it at every map, so a window that
kept it current would steal focus the next time it appeared. That is measured,
along with everything in the table:
[2026-09-21-own-window-p0-properties.md](experiments/2026-09-21-own-window-p0-properties.md).
`WM_TAKE_FOCUS` is deliberately absent: with `input = True` and a user time of
zero it changes nothing, and announcing it would oblige spokenpad to answer
it.

**sway reads none of these.** It focuses every window it maps on the focused
workspace unless a `no_focus` rule matches it (`should_focus` in
`sway/tree/view.c`). So before the window exists the daemon asks the display
which window manager runs it: the name on the root's
`_NET_SUPPORTING_WM_CHECK` window, which is `wlroots wm` under sway's
Xwayland (and `i3`, `Openbox`, `KWin` elsewhere). The environment does not
decide this, since a `$SWAYSOCK` that was never imported, or is left from
another session, would decide it wrongly. On `wlroots wm` the X server
names the process that owns that check window (the X-Resource extension),
and it must be `sway` (`/proc/<pid>/comm`). The daemon then looks for sway's
IPC socket where that sway puts it — `sway-ipc.<uid>.<pid>.sock` in
`$XDG_RUNTIME_DIR` — and then at `$SWAYSOCK`, and counts a socket only if the
process listening on it (`SO_PEERCRED`) is that same sway. To the first one
that does, it sends

```
no_focus [instance="^spokenpad-pane$" class="^spokenpad-pane$"]
```

The pane opens only if sway answers that it took the rule. sway accepts
`no_focus` at runtime and ignores a rule it already holds, so the rule is sent
before every pane; a `swaymsg reload` drops it, and the next pane sends it
again. Nothing is written to your sway configuration. The pane does not open,
and the text goes to the pending passage with a log line that says why,
when:

- the display does not name its window manager's process, so nothing can
  tell whether it is sway;
- the process is not sway — another wlroots compositor (labwc, river,
  Wayfire) looks the same from the display, and spokenpad has no rule it can
  add there and has not verified how it focuses windows; the log line
  names the compositor and says to use `nvim.mode = "attach"`;
- no socket of this sway is found — `$SWAYSOCK` unset, dead, or another
  sway's, and nothing in the runtime directory;
- the focused workspace is empty, since sway focuses the first window on a
  workspace whatever the rules say.

**Where it is verified.** Each of these runs headless in the test suite,
with the pane opened the way the daemon opens it and the focus sampled every
5 ms while it opens and redraws. "Your click" is a real click through the
window manager, except on KWin Wayland, which takes no synthetic input
headless: there it is the activation KWin performs on a click.

| window manager | the pane takes focus on its own | shown above the focused window | your click focuses it | test |
|---|---|---|---|---|
| i3 4.25 | never, also as the first window on an empty workspace | yes (floating) | yes | `tests/pane_window.rs` |
| sway 1.12 (Xwayland) | never, through the rule above; on an empty workspace the pane does not open | yes (floating) | yes | `tests/pane_focus_wms.rs` |
| Openbox 3.6 | never, also on an empty desktop | yes | yes | `tests/pane_focus_wms.rs` |
| KWin 6.7 (Wayland, Xwayland) | never, with focus stealing prevention at its default or off, also on an empty desktop | yes, through `_NET_WM_STATE_ABOVE` | yes | `tests/pane_focus_wms.rs` |
| KWin 6.7 (X11, `kwin_x11`) | never, with focus stealing prevention at its default or off, also on an empty desktop | yes, through `_NET_WM_STATE_ABOVE` | yes | `tests/pane_focus_wms.rs` |

Measured in
[2026-09-22-pane-focus-other-wms.md](experiments/2026-09-22-pane-focus-other-wms.md)
and
[2026-09-22-pane-stacking-and-sway.md](experiments/2026-09-22-pane-stacking-and-sway.md).
Mutter, xfwm4, bspwm and Hyprland are only read from their source, in the
[P0 experiment](experiments/2026-09-21-own-window-p0-properties.md), and
awesome is known to need more. Try pane mode before you rely on it there, and
say what you find.

**Under focus-follows-mouse**, moving the pointer into the pane focuses it,
as it would any window; that and your click are the only ways it gets the
focus. It opens beside the pointer, never under it ([Where it
opens](#where-it-opens)), so a pointer that rests or jiggles where it was
does not enter it. `tests/pane_hover.rs` proves both halves:

| window manager | focused at the map, by a 6-pixel jiggle, or by its own resize | focused when the pointer moves in |
|---|---|---|
| i3 4.25, `focus_follows_mouse yes`, floating at 96 and 192 dpi, tiled, and too large to fit beside the pointer | never | yes |
| Openbox 3.6, `followMouse yes`, `underMouse` no and yes | never | yes |

Openbox with `underMouse yes` (not its default) focuses whatever window
comes to be under the pointer, so a pane too large to open beside the
pointer, which opens around it, is focused there at the map
([investigation](experiments/2026-09-22-pane-hover-focus.md)). KWin's
focus-follows-mouse policies and sway's are not run.

### What it costs to run

| | |
|---|---|
| processor time while the window is open and nothing happens | none: the loop blocks on one channel fed by two threads, with no timer and nothing to poll — a close or a shutdown is not waited for either, because whoever asks also knocks on the window |
| resident memory the window adds | about 5.5 MB, for the framebuffer, the rasterised glyphs and the editor's client |
| key-up to the transcript in the file, opening the window on the way | about 300 ms |
| system libraries | `libxcb`, `libxkbcommon`, `libxkbcommon-x11`, opened when a pane opens rather than linked, so the binary starts without them in the other modes |
| programs | `fc-match`, from fontconfig: four times at startup, and at most once per frame after that |

`spokenpad check` reports all of those before anyone dictates, and exits
non-zero when one is missing.

### The font

`nvim.font_family` is a fontconfig family name and defaults to `monospace`,
the alias your desktop already points at the font you want. It is a *name*,
checked once where the configuration is read, so the pane cannot be handed
something fontconfig would read as pattern syntax.

`nvim.font_size` is in **points** and means exactly what Alacritty's
`font.size` means: the same family and size give the same text and the same
cells in both, on any display. `core/font.rs` repeats Alacritty 0.17's
arithmetic step by step:

- **Scale.** The X resource `Xft.dpi` divided by 96 is winit's scale
  factor. The pane looks it up when it opens exactly where winit does, in
  x11rb's default resource database: the first screen's `RESOURCE_MANAGER`
  (also for a display such as `:0.1`), else `~/.Xresources`, else
  `~/.Xdefaults`, queried as `Xft.dpi`, so `*dpi` matches and the last of
  equal entries wins. A missing or unusable value means 96 dpi, and the
  debug log and `spokenpad check` say which it was. The size in pixels is
  `points × dpi / 72`, kept in
  crossfont's own `f32` steps and asked of the rasteriser in 64ths of a
  pixel. 12 pt is 16 px at 96 dpi and 32 px at 192.
- **Metrics.** FreeType's size metrics at that size: ascender rounded up,
  descender down, line height to the nearest pixel, from the table FreeType
  trusts (OS/2's typographic values when the font sets `USE_TYPO_METRICS`,
  `hhea` otherwise). The advance of `0` is rounded as the font's hinting
  asks: fontconfig's `hintslight` rounds it at the requested size, full
  hinting of a TrueType font that sets `head.flags` bit 3 at a whole-pixel
  em, and no hinting keeps the fraction.
- **Cell.** The width is that advance and the height the larger of the line
  height and ascender minus descender, both floored. The baseline sits the
  descent above the cell's bottom; underline and strikethrough are placed
  where Alacritty's `create_rect` puts them, and the unfocused cursor's
  outline is 0.15 of a cell wide, Alacritty's default `cursor.thickness`.
  The underline styles are measured in the underline's thickness, so they
  grow with the font too.

The match is exact rather than close: 304 cells measured from Alacritty's
own window on an Xvfb, over four fonts, five resolutions and all three
hinting modes, equal the pane's, and `tests/pane_hidpi.rs` checks it again
against a live Alacritty at 96, 144 and 192 dpi, with `*dpi`, and with
`Xft.dpi` only in `~/.Xresources`, under a private `HOME` so the user's
fontconfig cannot tilt it
([experiment](experiments/2026-09-22-pane-hidpi.md)). **The precondition is
that `Xft.dpi` is set.** winit asks an XSETTINGS daemon's `Xft/DPI` before the
resource, and RandR's physical screen size when neither exists; the pane
reads neither, so a desktop that sets the resolution only through XSETTINGS,
or not at all, gets 96 here where Alacritty may use another value.

Bold is thickened by hand where fontconfig has no bold face — which is what
`monospace` resolves to on some machines — and italic falls back to the plain
face, because a synthetic slant looks worse than none. A character the family
does not cover is fetched from whichever font fontconfig names for it, so a
transcript that contains `漢` or `✓` shows them rather than blank cells.

Asking fontconfig means spawning `fc-match`, which costs about 60 ms, and the
pane draws in the thread that owns the window — so a page of a script the
family lacks must not become one spawn per character. Three things keep that
off the drawing path:

- a fallback face already loaded for an earlier character is used when it
  covers this one, so a page of Japanese asks once instead of five hundred
  times (the first face that has the character wins, which is what a terminal
  does, and can differ from what fontconfig would have picked);
- fontconfig's answer stands for the whole 256-character page the character
  is on, verified in process — if the best font for that page does not have
  this character either, nothing does;
- a frame that has spent 50 ms asking stops, paints the rest of that page
  blank for one frame and repaints straight away, so the window keeps
  answering. A single `fc-match` that does not come back is killed after
  250 ms.

The family's own four faces are looked up once, when the pane opens, and
may take up to 5 s each: nothing is drawn yet, and on a busy machine
`fc-match` alone was measured at 280 ms on average, which under the drawing
path's 250 ms made the pane fail to open.

Two hundred ideographs the family does not cover take one frame and one
`fc-match`; the same page drawn again costs no lookup at all.

The pane plays the terminal's part, so Ctrl+V does what it does in a
terminal set up to paste with it. In Insert mode (and Replace mode) and on
the command line it pastes the clipboard (`+`); in Normal, Visual and
Operator-pending mode it is Neovim's own `<C-v>`, Visual block.
Ctrl+Shift+V pastes in every mode, as most terminals bind it. Which of the
two a Ctrl+V is follows the mode Neovim last named in a `mode_change`
redraw event (`core::grid::Mode`); `core::keys::press` decides. The paste
is a terminal's bracketed paste: one `nvim_paste` of `getreg('+')`, run
inside the embedded nvim, so its own clipboard provider reads the
clipboard, and no mapping, abbreviation or auto-indent touches the text. It
is one undo step and repeats with `.`. On the command line only the first
line goes in, as `vim.paste` does it. Attach mode is unaffected: there the
terminal decides.

The paste is a queued request, as Neovim's own TUI sends a paste
(`nvim_paste`) beside keys (`nvim_input`). Neovim queues a key the moment
it arrives and reads every queued key before it runs a queued request, so
the paste lands after every key typed before it. Two limits follow from the
same rule. A key typed after Ctrl+V can still go first, if it reaches
Neovim before the paste has started, which takes a key within milliseconds
of it. And a paste into a command that waits for a key (`<C-r>` in Insert
mode, a count or `"` in Normal mode) runs once that command has its key; it
is never read as that key. The mode is the one Neovim last drew, so a
Ctrl+V pressed before Neovim has redrawn after `i` or `<Esc>` goes by the
mode before it.

There is no input method: dead keys and Compose work, because they are
xkbcommon's and spokenpad reads the layout the X server has loaded, but IBus
and Fcitx are not clients of this window. For German and English dictation
that is not a gap; for CJK input it is, and `attach` is the answer there.

### What happens when

- **No `DISPLAY`, or a library missing.** The daemon says which, and the text
  goes to the pending passage. It does not fail. `$DISPLAY` is read once, when the
  configuration is loaded, and passed from there to both the window and the
  editor inside it — so the two always agree about which server they are on,
  which is what the editor's clipboard provider needs.
- **You close the window.** The editor inside it is asked to write every
  modified buffer and quit, its socket goes, and the next dictation opens a
  new window on a new file. **A recording running at that moment is
  cancelled**, as `spokenpad cancel` would: committed text stays in the
  file, the tail is not decoded, the WAV is kept, and the log says so and
  names the file. Until the next key press no window opens again: text still
  arriving from before the close — a chunk that was being decoded, or the
  tail of a recording already released — goes to the pending passage, which
  the next pane opens on. The pane tells the daemon (`PaneHost::take_closed_by_user`,
  asked by the editor thread after every piece of work and every 66 ms), and
  the state machine cancels only a capture that started before the close
  (`Event::WindowClosed`), so a key pressed right after closing starts a
  recording that goes on. The close is stamped when the window manager's
  request, or Neovim's word that it is quitting, arrives, and a close
  recorded for one pane is cleared when the next is asked for, so it is never
  taken for the next pane's.
- **You `:q` in it.** The same, from the other end, a running recording
  cancelled included. When the pane attaches, it registers a `VimLeavePre`
  autocommand in its Neovim that, with `v:dying` at 0 and `v:exitreason` at
  `quit` — a quit Neovim was told to do: `:q`, `:q!`, `:wq`, `:qa`, `:cq` —
  sends a notification on the pane's own channel before any channel closes. That notification is what makes an
  exit your close, not the exit status or its timing: Neovim closes its
  channels and then waits up to two seconds for its jobs (a language server
  that ignores `SIGTERM`), and the pane does not wait for the process at all.
  `:q` always writes and quits: the dictation file is saved on every change,
  and before `:quit`, `:wq` or `:qall` looks at what is unsaved
  ([The file](#the-file)).
- **The editor dies.** Killed, crashed, or taken down by a deadly signal
  (which sets `v:dying`): it says nothing before its channel closes, so it
  is not your close. The pane goes, a recording goes on, and its next text
  opens a new pane on the pending passage — nothing is lost to a crash.
  `:restart` (Neovim 0.12) counts as the editor dying too: it runs
  `VimLeavePre` with `v:exitreason` at `restart`, and then the process
  exits. Neovim starts a new server for a UI that handles its `restart`
  event, which the pane does not, so that server is left running without a
  window, on the pane's `--listen` socket. It has not run its `--cmd`, so it
  does not carry the marker yet; the next key-down finds spokenpad's `--cmd`
  on its command line (`v:argv`), gives it a second to show a UI, stops it
  and opens a new pane. Before 0.12 there is no
  `v:exitreason` and no `:restart`, and the notice needs `v:dying` alone. In attach mode the daemon cannot tell `:q` in an editor it did not
  start from that editor dying, so there quitting the editor never cancels
  a recording.
- **The daemon restarts.** The window goes with it — it is a thread of that
  process and the editor is its child. An editor you opened in attach mode
  outlives the daemon and is reattached to; a pane cannot. Nothing is lost: dictated text is written to the file after every utterance,
  and the pane writes every modified buffer before it quits, so a restart
  costs the window and not the transcript. The next dictation opens a new one.
  [What closing a pane guarantees](#what-closing-a-pane-guarantees) says what
  "nothing is lost" covers exactly.
- **You run `spokenpad editor` anyway.** It opens an editor in your terminal
  and the daemon adopts it, rather than opening a pane — the socket is the
  contract, not the mode. Close it and the next dictation opens a pane again.

### What closing a pane guarantees

Dictated text is already on disk before the window closes: the Lua side
writes the file after every append, and after every change you make in it
([The file](#the-file)). What can still be only in the buffer is a change
whose write failed, or another file you opened in the same editor, and pane
mode is the one mode where closing the window ends the editor — an editor
opened in attach mode outlives the daemon.

So the pane writes every modified buffer first, and the whole teardown has a
budget, because `shell::daemon::SHUTDOWN_GRACE` (3 s) is what the daemon
gives its editor thread before it exits and stops that thread wherever it had
got to. The pane divides that budget: 1.2 s for the writes, then 0.5 s for
the editor to quit before it is killed. Before either, a command left half
typed in the window is cancelled with `<Esc>`, at most three, each followed
by a call that runs only once Neovim has read it (0.1 s each): Neovim runs no
call while one is pending, the write included, so closing the window after a
stray `g` or `2` used to lose everything typed since the last utterance.

That call also says what the `<Esc>` typed, because in Insert mode it is not
always a cancel: after `<C-v>` it goes into the text as a literal ESC, and
after `<C-v>` and digits (`<C-v>u12`) it ends the number, whose control
character goes in. The first is taken back with `<BS>`, which in Replace mode
also puts back the character it replaced, and the second is deleted, so
neither reaches the file. Both rest on facts, not on the text, which may
hold an ESC or a control character of the user's own: before the `<Esc>`,
the pane reads from its own grid what Neovim shows at the cursor (`^` while
`<C-v>` waits, `"` for `<C-r>`, `?` for `<C-k>`) and where the cursor is. An
ESC is taken back only if `^` was showing and Insert mode goes on; a control
character is deleted only if Insert mode ended with the cursor on it at the
very screen cell it was on, which leaving Insert mode never does unless the
`<Esc>` inserted it. At the start of a line, where the cursor cannot move
left, a `<C-v>` number typed right before a control character of the user's
is left in the file: there the two cannot be told apart (`tests/pane_render.rs` checks both, in Insert and
Replace mode, against a real Neovim). `nvim_get_mode` cannot be that check:
Neovim answers it on arrival, which can be before the `<Esc>` ahead of it is
read. The budgets are checked against the grace at compile time, so a change
to one of them cannot quietly break the guarantee. The systemd unit's `TimeoutStopSec=10` sits well above all of it,
so a stop that goes wrong ends in seconds and never in a SIGKILL during a
write.

If Neovim refuses to write a buffer — a file that turned read-only, a
directory that went away, a full disk — the text comes back with the failure
and spokenpad writes it to `<the file>.unsaved` next to it, or, when that
exists already, to the first free `<the file>.unsaved-1`, `-2` and on, so
an earlier rescue is never overwritten. Each is created new, `0600` like the
dictation files, and never through a symlink. A buffer that never had a file
goes to `unsaved-<timestamp>.md` in the state directory. Either way the log
says where, at error level. The only case that loses anything is an editor
that stops answering entirely: then there is no way to ask it what is in the
buffer, and the log says that too.

### Floating or tiled

`nvim.pane_layout = "floating"`, the default, opens the pane above your
windows beside the pointer. `"tiled"` makes it an ordinary window
(`_NET_WM_WINDOW_TYPE_NORMAL` instead of `_UTILITY`): i3 and sway tile it
beside the window you are typing in, at the size of its tile, and the pane
follows the tile's size. Openbox and KWin have no tiles; there it is an
ordinary window beside the pointer, kept above like the floating one. A
tiled pane goes where the window manager tiles it, which may be under the
pointer.

A tiled pane gives up `_UTILITY`, which is what refuses focus on window
managers that ignore the user time (bspwm, Hyprland's Xwayland). So tiled is
allowed only where it is proven never to take the focus — i3, sway, Openbox
and KWin (Wayland and X11), recognised by the name on the display's EWMH
check window ([experiment](experiments/2026-09-22-tiled-pane-focus.md)).
Under any other window manager, or none, the pane opens floating; the log
says why every time. On
sway an empty workspace refuses both layouts alike.

Every pane logs one line when it opens, with the layout asked for and the one
applied, the window manager, and the grid in cells at the map (a tiled pane
is resized to its tile afterwards):

```
opened the pane tiled under i3, 72x20 cells
opened the pane floating (asked tiled: awesome is not proven to keep a tiled pane unfocused) under awesome, 72x20 cells
```

A pane logged as tiled that still floats is floated by the window manager's
configuration. i3 and sway read a criterion's value as a pattern that may
match anywhere in the name, so a rule written for a window named `spokenpad`
— such as the `for_window [instance="spokenpad"] floating enable` that the
package shipped for managed mode until 2026-09-22 — matches `spokenpad-pane`
too, and floats a tiled pane on i3 4.25.1
([experiment](experiments/2026-09-22-tiled-pane-floats-under-managed-rule.md)).
Delete such a rule, or anchor it (`instance="^spokenpad$"`).

### Where it opens

`nvim.pane_dimensions` cells — `{ columns = 72, lines = 20 }` by default, as
Alacritty's `window.dimensions` — on the monitor under the pointer, beside
the pointer and never under it. A grid larger than the monitor is cut to as
many whole cells as fit (`Dimensions::fit`), and Neovim is told the grid the
window has.

The window's outer frame — the window with the border and title bar the
window manager draws around it — opens a **gap** from the pointer: 20 pixels
at 96 dpi, scaled by `Xft.dpi` / 96 and rounded up (`Gap::at`), 40 at 192
dpi. Each axis is placed on its own (`Side` in `core/geometry.rs`):

- right of the pointer, or below it, when the frame fits there;
- else left of it, or above it, its last pixel the gap short of the pointer;
- else centred on it, as far as the monitor allows.

One axis with a side keeps the pointer outside the frame, the gap away. A
pane too large for a side on either axis opens **around** the pointer: the
window itself, not its frame, is centred on it and kept on the monitor, so
the pointer is at least the gap inside every edge the monitor does not hold
back. On a 1280x800 screen the default pane has no room beside a pointer in
the middle of the screen; on 1920x1080 it has room everywhere.

The gap is what keeps focus-follows-mouse from focusing the pane by
accident. i3 focuses a window when the pointer crosses onto its frame, from
outside or from inside the window, and the pane used to open with its
window's corner on the pointer: a nudge of one to three pixels up or left
focused it ([investigation](experiments/2026-09-22-pane-hover-focus.md)).
Twenty pixels is well beyond a hand's jiggle, and scaled with the display
because a desktop at 192 dpi draws its frames and its pointer twice as
large.

Around the grid the window keeps a **margin** of 4 pixels at 96 dpi on every
side, scaled by `Xft.dpi` / 96 and rounded down as Alacritty scales its
`window.padding` (`Dpi::padding`): 8 at 192 dpi. It is painted in Neovim's
default background and repainted when that changes, it is not part of the
grid, and a click in it goes to the nearest cell. The window is the grid plus
the margin, and the margin comes off the monitor before the grid is fitted to
it. It is fixed, not a setting: the grid running into the window's edge looked
cramped, and this is the small margin that was asked for.
`tests/pane_hidpi.rs` checks the window against Alacritty's with
`padding = { x = 4, y = 4 }` at 96, 144 and 192 dpi.

The default, 72x20 cells at 12 pt and 96 dpi, is about 730x410 pixels with
a monospace font's cells of about 10x20, a little over a third of a
1920x1080 screen each way; at 192 dpi it is the same share of a 3840x2160
screen. Pure
functions in `core/geometry.rs` decide the monitor and the corner:
`pick_output` takes the monitor under the pointer, else the primary, else the
first, and `placement` puts the frame beside the pointer on it, as above. The monitors come from RandR and the
pointer from the X server itself, rather than from a window manager's IPC
socket, so this works under a window manager spokenpad has never heard of.
The pane is placed **once**, when it opens, and never again: a window that
jumped back under the pointer on every keypress would fight anyone who had
put it somewhere they wanted it.

Under Xwayland the pointer is **not** asked for: `QueryPointer` there answers
with wherever the pointer last was over an X window, which is not where it is,
and a window at a stale position looks deliberate in a way a corner does not.
With `WAYLAND_DISPLAY` set the frame goes to the bottom-right corner instead;
sway centres an Xwayland window whatever it asks for.

Nobody knows the frame before the window is mapped. So the position asked
for in `WM_NORMAL_HINTS` leaves 48 pixels (scaled like the gap) for a frame on
every side (`Extents::assumed`): i3 puts its frame at that position and
Openbox and KWin the window, and the largest frame measured is KWin's
36-pixel title bar, so the gap holds either way. Once the window manager has
mapped the window (`Pane::show` waits up to 0.5 s), the pane reads the frame
— the window's top-level ancestor if the window manager reparented it into
one, else `_NET_FRAME_EXTENTS`, else none — and moves the window once, so
that the frame sits exactly where `placement` puts it. The window's gravity
is static, which makes every window manager measured take that move as the
window's own position
([frame extents](experiments/2026-09-22-pane-frame-extents.md)). A window
manager that sized the window itself (a tile) placed it too, and is left to.

## Focus, and why there is no focus code

The window must never take focus — it appears while you are reading something
else, and you keep reading. Your click, or your pointer moving into it under
focus-follows-mouse, may focus it; nothing else may. The pane's own properties do that everywhere but
sway, and on sway a rule spokenpad adds over sway's IPC
([above](#why-it-needs-no-rule)). Either way spokenpad contains **no focus
call at all**: not `xdotool windowfocus`, and not a "remember the focused
window and restore it afterwards" dance either, since restoring focus is
itself a focus change and would race anything the user did in between.

The pane is also opened lazily, on the **first key-down**, not at daemon
start: until you dictate there is no reason for a window to be sitting on
your desktop. It is opened on the editor thread while the utterance is still
being spoken, so the wait is paid in parallel with the recording rather than
added to it, and the append that follows simply queues behind it.

## Latched recording

`spokenpad toggle` (bound to Shift and the push-to-talk key) starts a
recording that outlives the key release: let go, keep talking, press the key
again to stop. Push-to-talk is unchanged without it.

The mode lives in the state machine — `Recording { hold: Hold::Latched }` —
rather than being read off whichever request ends the recording, because the
ending request differs between the modes: a `stop` for push-to-talk, a
`start` or `toggle` for a latch. The stopping press does *not* need Shift, so
there is nothing to remember about which hand started it. Every `stop` is
ignored while latched, because Shift and the key come up in either order and
the window manager may or may not send one. A `toggle` while the key is held
latches the running recording. The winbar shows a lock while latched.

The full transition table, including how auto-repeat is told apart from a
second press, is in [`core/state.rs`](../src/core/state.rs) and
[decisions.md](decisions.md#control-by-socket-not-by-reading-the-keyboard).

## The file

One markdown file per **editor** under `nvim.dictation_dir`, named by
`nvim.file_template` (`dictation-%Y-%m-%d-%H%M%S.md`), written after **every**
utterance with `noautocmd write` — or, with no editor open, by the daemon
itself into the pending passage (see [attach mode](#dictating-with-no-editor-open)).

The file is a scratch pad you never save by hand. **Every change you make in
it is written promptly**, in every mode: `spokenpad.lua` writes the
dictation buffer at once on `TextChanged` (a change in Normal mode),
`InsertLeave` and `BufLeave`, and in Insert mode 300 ms after the last
change (`TextChangedI` and `TextChangedP` restart a timer). Every write
fsyncs, since `'fsync'` is on by default, and on slow storage a write per
keystroke would stall Neovim's main loop, the daemon's appends included;
saving only on leaving Insert mode would lose a paragraph typed into an
editor that then died. The write is the same one an append makes,
`silent lockmarks noautocmd write`: no autocommand of yours runs, so a
format-on-save cannot reflow a transcript, and the `'[` and `']` marks stay
where your change put them. It writes only the dictation buffer, only when
it has unsaved changes (an append has written its own), and a write that
fails says nothing — a message could raise a prompt, which would hold every
call the daemon sends — and leaves the buffer modified for the next change,
the next append or the pane's close to try again.

**`:q` always writes and quits.** A `QuitPre` autocommand writes the
dictation buffer the same way before `:quit`, `:wq` or `:qall` looks at what
is unsaved, and `VimLeavePre` before any other way out. That covers a change
whose own write has not run: a `TextChanged` waits while keys are still
queued, so `dd:q` typed in one go, or run by a mapping or a macro, reaches
the `:quit` with the buffer modified. `BufLeave` covers `:edit` and
`:bnext` the same way, also with your `set nohidden`: the dictation buffer's
own `'bufhidden'` is `hide`, since Neovim refuses to abandon a modified
buffer (E37) before `BufLeave` runs. `'autowriteall'` is not set: it would write the
dictation buffer with your autocommands, format-on-save included, on `:edit`,
`:bnext`, `:!` and `:make` as well. Another file you open in the dictation
editor is yours to save.

**`:q!` does not discard.** Neither does `:qa!` or `:cq`: `VimLeavePre`
runs for every quit, and `QuitPre` before `:q!` and `:qa!` as before `:q`,
and each writes the dictation buffer, so a change you made is in the file
however you quit. There is no way to throw away an
edit by quitting; undo it (`u`) and let the next write save that instead.

A window is a passage. Closing it ends the passage, and the next dictation
opens a new window on a new file rather than appending under everything said
an hour ago — dictation started as a day-long log and that was wrong in use.
While the window stays open, every utterance goes into it, across a daemon
restart included: reattaching *adopts* the buffer already on screen (matched
on the dictation directory, since the running window may carry an earlier
timestamp) rather than opening a second file underneath it. The previous
file is never touched — a fresh page is not the same as discarding what came
before.

* **A real file, not a scratch buffer**, because a transcript that disappears
  because a buffer was closed is the same class of failure as one dropped by
  a streaming decoder, which is the failure this project exists to eliminate.
* **`noautocmd`**, because a format-on-save autocommand in the user's own
  config would reflow dictated prose behind their back.

Each utterance is appended as its own paragraph — one blank line between,
none at the top of a fresh file. The cursor follows the end of the buffer
only for a reader who was already at the end; someone who scrolled up to
re-read or edit an earlier passage keeps their place. Someone typing in the
window keeps their cursor as well: in Insert or Replace mode the view still
follows the text, but the cursor stays where the next key lands. It used to
be moved onto the last character like a reader's, and the next key typed
went into the middle of the last dictated word.

## What runs inside nvim

`spokenpad.lua` — one file since 2026-09-11, previously split in two — is
loaded over RPC on every connection (so a reattach re-applies it) and defines
`_G.Spokenpad`. The daemon calls exactly four things: `setup`,
`append_once`, `push`, and, only when `nvim.copy_to_clipboard` is true,
`copy_buffer`, which sets `+` to the whole buffer after every release (see
[decisions.md](decisions.md#the-whole-buffer-is-copied-to-the-clipboard-after-a-release)
and [decisions.md](decisions.md#the-clipboard-copy-becomes-opt-in-off-by-default-2026-09-21)
for why it is now opt-in). Reloading is state-preserving by construction: the
chunk keeps the previous module's pinned buffer, indicator state, meter history
and append de-duplication cache, so restarting the daemon neither blanks the
indicator nor replays an append whose reply was lost.

The whole indicator is pushed at once — phase, level, preview,
preview_placement, notice, notice_detail, latched, previewing — and
`Spokenpad.push` replaces the editor's copy with it whole, so that copy is
a function of daemon state rather than of the history of updates that
reached it. Only the meter's history is the editor's own. `preview` and `notice` are separate fields and are
drawn in separate places; an absent
notice travels as the empty string, because nvim turns a msgpack nil inside a
map into `vim.NIL`, which Lua cannot tell from a field the daemon meant to set.

A notice travels as **two** fields, `notice` (the headline) and
`notice_detail`, rather than as one sentence the editor would have to take
apart: `Notice::headline` and `Notice::detail` in `core/session.rs` are where
the wording lives, and the editor decides only whether the window has room for
the second half.

**The winbar** carries the indicator: a phase dot, and while recording a level
meter on a perceptual curve (`level ^ 0.6`), 24 cells wide where there is room
for them. It is built per window and fitted to its width: the phase label and,
when there is one, the notice headline are drawn at any width, and what is left
over goes first to the meter, then to "no live preview, still recording", then
to the notice detail. It is set
window-locally on every window showing the dictation buffer, so a global winbar
from the user's own config is overridden for this buffer only and left alone
everywhere else — and a window that stops showing the buffer has its winbar
cleared, so a split the user moved off the transcript does not keep a frozen
"REC" over someone else's file.

The rest of the chrome — line numbers, sign and fold columns, cursorline, list —
is window-local too. Only the **global** half (`laststatus`, `showtabline`,
`ruler`) is applied, and only in an editor spokenpad opened for the purpose,
where the dictation buffer is the whole instance. In an adopted editor those
globals are the user's own and are left alone. Levels are sent at ~10 Hz as
notifications, not requests — a round trip per sample would put nvim's event
loop on the dictation latency path for something purely cosmetic.

**The live preview** is an extmark's inline virtual text on the last text
line, drawn exactly where its text will land: when the text is appended, every
word stays in its cell and only the highlight changes. Virtual text, not
buffer content: it cannot be written to the file, yanked, or undone into the
buffer even deliberately. That makes "a preview is never committed"
([constraints.md](constraints.md#the-one-relaxation-a-cosmetic-preview-of-the-open-tail))
a property of the data model rather than a discipline.

Since 2026-09-08 the preview is only the **open tail** — the sentence being
spoken now, at most one chunk. Everything before it has already been
committed to the buffer above, so the preview restarts from nothing each time
a chunk lands and the transcript itself is never cropped
([progressive-commit.md](progressive-commit.md)).

Where the text lands depends on whether this capture already wrote text into
the buffer, which only the daemon knows. It sends that as
`preview_placement` (`PreviewPlacement` in `shell/nvim/mod.rs`): the editor
thread decides it by the rule it applies to the append's `continued`, from
where it wrote the capture's last text, so the two cannot disagree; the Lua
side never guesses it from the buffer. `landing` in `spokenpad.lua` then
places both the append and the preview:

- `continuation`: after the last text line, behind the space the append will
  put there (none when the line already ends in whitespace).
- `new_paragraph`: after the last text line, the rest of its last row and one
  row of blanks — the blank line the append puts between paragraphs — then
  the preview from the first cell of the next row.
- An empty buffer (only blank lines): on line 1 from its first cell, with no
  blank row, since the append replaces the buffer from line 1.

Inline virtual text wraps with its line (nvim ≥ 0.10), but per cell: it
ignores `linebreak`, so a preview drawn as it is would split words where the
committed text will not. `lay_out` in `spokenpad.lua` therefore writes every
wrap `linebreak` would make out as spaces to the end of the row, by nvim's own
rule (`charsize_regular`): at a `breakat` character, the word after it and the
blanks after that word must fit on the row, or the row ends there; a
double-width character that would start in a row's last cell starts on the next
row after a filler cell, which nvim draws in virtual text as it does in the
buffer. `apply_chrome` sets `showbreak` to `NONE` in the dictation window, so
every row is as wide as the window. The layout is for the first window showing
the buffer, and it is redone when that window is resized (`WinResized`), since
the daemon sends a preview again only when it changes.

One case cannot match: when the last text line's final word ends in the
window's last column, `linebreak` moves that word to the next row as soon as
text follows it, and a preview cannot move buffer text. The preview is then
drawn from the next row's first cell, behind its separator, and the landing
text moves the word. The probes behind all of this are in
[experiments/2026-09-23-inline-preview-layout.md](experiments/2026-09-23-inline-preview-layout.md).

The preview is part of the last text line on screen, and
`nvim_win_text_height` counts inline virtual text, so following it is plain
arithmetic. A reader at the end is kept there by computing the view directly:
walk up from the last line until the text fills the window, then hide the
surplus rows of that top line with smoothscroll's `skipcol`. The window height
is `winheight()`, the rows text is drawn in: `nvim_win_get_height` counts the
winbar too. The window's `scrolloff` is set to 0, because a `scrolloff` would
scroll the view straight back. A reader's cursor rests on the text's last
character, before the preview, and nvim scrolls to a cursor it cannot show, so
a preview taller than the window gives up its oldest words, marked by `…`.

Whether a window still follows is decided by comparing its view with the one
the preview left there. A reader who scrolled away keeps their place for the
rest of the preview. A window whose size changed has had its view moved by
nvim, not by the reader, so across a resize only the cursor counts. Before
2026-09-23 a resize read as the reader moving away, and the preview stopped
following with its newest words off the screen. The same happens to a view
computed for one row more than the window shows, which nvim scrolls at the
next redraw: the earlier code left a row free below the preview in the belief
that nvim reserves it at the end of the buffer, and that row was the winbar.

When previews stop, the winbar says so instead of freezing. Past
`preview.max_seconds` (30 s of uncommitted tail) they **pause and resume by
themselves** once the tail settles — the tick keeps committing meanwhile, which
is what settles it; past the in-memory ceiling they stop for
good and the notice names the WAV that keeps recording. Without that message, a frozen preview during a long passage
reads as lost audio rather than as a cost control — which is exactly how it was
first reported.

Every other **notice** about the capture just made — held too briefly,
recording cancelled, microphone gap, microphone unavailable, capture
incomplete, nearly silent, stopped after silence, reached the time limit,
memory cap — is appended to the
winbar in the same way, in its own
`SpokenpadNotice` highlight (the theme's `WarningMsg` foreground, or
`DiagnosticWarn` where that is unset). One at a time, until the next key press.

Which one, when a capture collects two, is decided by `Notice::priority`, the
declaration order of `session::Priority`, and applied in exactly one place,
`Session::notify`: memory cap > capture incomplete > microphone unavailable >
microphone gap > reached the time limit > stopped after silence > nearly
silent > held too briefly > recording cancelled > preview paused, ties going
to the newer report ([usage.md](usage.md#the-winbar-and-its-notices) has the
whole list, the speech model's notices included). The two auto-stops sit there because nothing is lost
when a capture ends by itself; what the user reads instead is the microphone
trouble that may have caused the silence. A paused preview can
therefore never take the winbar from a microphone that dropped audio, and the
microphone events that follow the in-memory ceiling cannot take it from the
sentence saying where the audio went.

Each notice is a short **headline** and a **detail**. The headline is drawn at
any window width; the detail is appended only when the rest of the bar leaves
room for all of it, with a `%<` truncation marker between them as the backstop
so that nvim, if it ever has to truncate, takes the detail and not the phase.
Half a sentence is worse than none: at 40 columns the memory-cap notice used to
render as `<aining audio is in /tmp/capture-example.wav — recover …`, having
lost the phase, the warning sign and the reason. That is also why the
memory-cap detail names the recovery WAV by file name — the directory goes to
the log, which has a terminal's width.

It is drawn in **every phase**, not only while recording: a tap too short to
record and a notice that outlives the decode are both read while the window is
idle, which is precisely when the phase-conditional preview showed nothing.
The notice is in the winbar rather than in the preview for the second half of
the same reason — the preview is the live tail, so a notice standing in its
place is indistinguishable from dictated text. Its text is `%`-escaped before
it reaches the winbar: a winbar is a statusline expression and the memory-cap
notice carries a file path.

**Which nvim runs it is a choice, and the trade is measured.** `nvim.init`
selects between three, and the numbers below are key-down to a placed window
answering RPC, median of three, on a full LazyVim setup:

| `nvim.init` | opens in | theme | yank flash |
|---|---|---|---|
| unset (your `~/.config/nvim`) | **0.73 s** | yours | yours |
| `"bundled"` | **0.21 s** | none | built-in |
| `"bundled"` + `nvim.colorscheme` | **0.24 s** | yours | built-in |

The default is unset, because a dictation window is still an editor and people
want their editor -- a bundled config was the default for one afternoon and
never looked like the user's neovim. But the third row is what that argument
was actually reaching for: the window is for *watching a transcript land*, not
for editing, so a whole plugin set is startup cost with little to show for it,
and the two things it did show for it both survive without it. The yank flash
is built into nvim (`vim.hl.on_yank`), and `nvim.colorscheme` puts only the
one directory providing the named scheme onto the runtimepath -- the colours,
without the configuration they normally live in.

**A theme loaded this way is the theme's defaults, not the theme as its owner
configured it**, and that difference is visible. With tokyonight configured
as `transparent = true`, the real editor shows the terminal through; loading
the plugin raw takes tokyonight's own `#222436` instead and paints a
grey-blue block inside the terminal's black border -- close to right, and
clearly wrong. `nvim.transparent` (on by default) clears the background groups after
any colourscheme loads, which reproduces that setting and is the right answer
for a window floating over a terminal in any case. Verified against the full
config: `Normal`, `NormalFloat`, `EndOfBuffer`, `SignColumn` and `WinBar` all
come back with no background either way. Set it `false` to take the
colourscheme's own.

Neither of the fast rows is a default this project can pick, because the
colourscheme name is the user's. `config.example.toml` gives the recipe
(`init = "bundled"` with a `colorscheme`).

The bundled config also maps `j`/`k` and the `<Down>`/`<Up>` arrows to move
by **screen** line. An utterance
is one buffer line wrapped over many screen rows, so plain `j` leaps a whole
paragraph and reading a transcript by keyboard is unusable. It is an
expression mapping that checks `v:count`, so `5j` still means five buffer
lines and `gg`/`G` are untouched.

What spokenpad enforces itself, so it holds under any of them:

- **The chrome comes off** -- status line, tab line, line numbers, sign
  column, fold column, cursorline -- applied over RPC once attached and
  re-applied on `BufWinEnter`/`WinNew`/`FileType`, because a plugin reacting
  to those events would otherwise put it back.
- **Prose wrapping** (`wrap`, `linebreak`) per window, so a long utterance
  reads as a paragraph and never splits mid-word.
- **Committed text is written with `noautocmd`**, so a format-on-save cannot
  reflow dictated prose behind your back on the write after every utterance.

One visible cost belongs to the slow row only: with a full config the window
paints a normal editor for a moment before the chrome comes off, because that
happens after nvim answers RPC rather than before it draws. `"bundled"` has
the chrome off in its first frame.

## Startup cost

About **300 ms** from key-up to the transcript in the file with a pane opened
on the way (`tests/pane_daemon.rs` prints it), and off the latency path
either way: the window is opened on the key-down, on the editor thread, while
the user is still speaking.

The editor's own start is the larger part, and a configuration can make it
much larger: a first-ever open took 13.5 s while a plugin manager did
one-time work, so an editor has a generous 30 s to answer (`STARTUP_TIMEOUT`,
not a setting). Readiness
is a raw msgpack-RPC round trip on the socket, under one absolute deadline,
so an editor that is still starting cannot block the editor thread with no
way out, and a pane whose editor exited is noticed at once rather than
waited out.

Until 2026-09-22 the managed terminal measured **244 ms** from key-down to a
window placed and answering RPC, and **92 ms** to reattach to one already
open; the three fixes that got it there (a raw RPC probe instead of an
`nvim --remote-expr` spawn per poll, the window manager's IPC socket instead
of `xrandr` and `i3-msg`, and the terminal told where to open) are in
[decisions.md](decisions.md).

`init` is passed straight to `nvim -u`, so nvim's own special value works
there too — for a window with no configuration at all:

```toml
[nvim]
init = "NONE"
```

## Reconnection

A live socket is reattached to rather than opened again, so an editor you
opened in attach mode survives a daemon restart and is written into again.
Quitting nvim simply means the next dictation opens a fresh pane — in pane
mode; in attach mode it goes to a new pending passage until you run
`spokenpad editor` again.

Liveness is checked on every key-down, and a plain `connect` is not enough: a
socket answers exactly as before the user `:bdelete`s the dictation buffer,
and every append of that utterance then fails against a buffer that is gone.
So the check is a real RPC round trip that asks the editor which buffer it
still has pinned, and it passes only if that is the buffer this session owns.
It is bounded by an absolute two-second deadline — every call in
[`shell/nvim/rpc.rs`](../src/shell/nvim/rpc.rs) carries one, because a peer
that dribbles bytes must not extend a call indefinitely by staying inside a
per-read timeout. The connect waits for the same deadline while the
editor's accept queue is full, and then counts as a timeout, never as a
stale socket to remove. An editor that does not answer in time is dropped and
reattached to (or respawned) rather than waited on, since the recording is
already running.

**Except an editor that is waiting for its user.** Neovim runs no RPC call
while a command is half typed in its window — a count, `g`, `"`, `f`, `r`, a
prompt — and runs every one it held back the moment the command is finished
or cancelled. Measured with a stand-in Neovim on 0.12.5: after `g`, `"`, `f`,
`2`, `gr`, `z`, `r` or `d2`, an `nvim_eval` sent over the socket was still
unanswered after 3 s; after `d` alone, or in Insert mode, it came back in
20 ms. On an editor it is already attached to, the daemon therefore asks why a
reply is late (`Patience::WhileTyping` in `rpc.rs`): `nvim_get_mode` is one of
the few calls Neovim answers at once even then, and while it says `blocking`
the call is waited for, asking again every half second. This covers the
liveness check, the append and its retry, and the clipboard copy. While a call
is held:

- **the log says so every 10 s**, with how long it has been held;
- **the pane says so** in its last row ("waiting for the editor: finish or
  <Esc> the pending command", shorter in a narrow window), drawn by the pane
  itself because Neovim is the one not running; `showcmd` keeps the right
  end. An editor you opened in attach mode gets the log only;
- **the wait ends after 2 minutes** (`HELD_AT_MOST`). A person mid-command
  finishes within seconds; what outlasts that is a hit-enter prompt or a
  plugin's `input()` in an editor nobody is looking at, which would hold the
  call for hours while every later utterance queued behind it. The call then
  takes the timeout path of an editor that stopped answering: one repeat on a
  fresh connection with the same operation id, then "append not
  confirmed", the text to the pending passage (below), and the next utterance
  there too until the editor answers again;
- **a daemon stop ends it at once.** The daemon sets the session's
  `quitting` flag before its last message to the editor thread. A patient
  call wakes every half second, before its deadline too, and gives up when
  it sees the flag; one sent after the flag gets a quarter second instead of
  two (`QUITTING_TIMEOUT`); and an append given up on while stopping is not
  reconnected and repeated, which on an editor that stopped answering took
  seconds more. Its text goes to the pending passage. Measured in
  `src/shell/nvim/tests.rs`, with the flag set before the append or while it
  waits, on an editor held by a count or frozen with `SIGSTOP`: given up
  200–510 ms after the flag. In `tests/e2e.rs` the whole daemon stopped
  161 ms after it was told to with an append held, and within 1.6 s (1 s of
  it the last decode) with the editor frozen, the text in the pending passage
  both times. Once the flag is set, the thread neither reattaches nor opens
  an editor: what is still queued goes to the pending passage. Before, the append gave up after 2 s, the reconnection's
probe after 2 more, and the utterance was reported as failed while its
request still sat in Neovim's queue; the next one went to a pending passage
instead of the open window. Startup, attach and the last idle push on detach
keep their plain deadlines: an editor stuck in a startup prompt is not one to
wait for.

An append that times out is the ambiguous case: the request may have completed
after its reply was lost. It is retried once, on a fresh connection, with the
**same operation id** — the Lua side returns its cached result instead of
appending twice. A retry that answers means the text is in the window
exactly once. If the retry fails too, the session disconnects, so the next
utterance reattaches rather than writing into a client whose reply stream is out
of step, and the text goes to the **pending passage**, with a warning in
the log ("text saved outside the window") that names the file and says
it may be in the window too. Kept only in the log it would be lost to the
user in the usual case, since Neovim 0.12.5 drops a request whose connection
closed before it ran (measured: after `<Esc>` ended the command it was held
behind, the text was not in the buffer). It is in both places only if the
editor ran the request after all: one whose reply alone was lost on a live
connection, or an editor that was not reading at all (stopped, swapped out)
when the connection closed. Measured: continued after `SIGSTOP`, Neovim reads
the request and the close together and usually runs the request first; on a
loaded machine it was also seen to drop it (`src/shell/nvim/tests.rs` accepts
either, never twice). A second copy
the user can delete is the price of never losing the text.

When the daemon exits it **detaches without closing nvim**: the user may still
be editing what they dictated, and killing their editor because a daemon
exited would lose exactly the text this window exists to keep. It pushes one
last idle indicator on the way out — a window left showing "REC", a half-lit
meter and a preview that will never land says the daemon is still listening when
it has exited. That one push is sent as a *request*, unlike every other
indicator update, because nvim discards input it has not parsed when a channel
reaches EOF and a notification written immediately before the socket closes is
regularly lost; it is still best-effort, with a quarter of a second allowed for
cosmetics.

A spawn that fails leaves nothing behind: the dictation file is created before
the editor starts, so its name is settled and its permissions are ours from the
first byte, and it is removed again unless a session pins it — but only if it is
still zero bytes. Anything with content in it is the user's transcript and is
left alone.
