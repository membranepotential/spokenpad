# The dictation window

← [docs index](README.md) | Implemented by
[`shell/nvim/mod.rs`](../src/shell/nvim/mod.rs), its transport
[`shell/nvim/rpc.rs`](../src/shell/nvim/rpc.rs), and
[`spokenpad.lua`](../src/lua/spokenpad.lua). The reasoning for
using nvim at all is in
[decisions.md](decisions.md#the-sink-is-neovim-not-the-clipboard); the rules
it must not break are in [constraints.md](constraints.md).

The transcript goes to a dedicated neovim, over its msgpack-RPC socket.
Nothing is pasted anywhere, and no window that was not opened for this
purpose is ever written to.

## Three modes: who opens the editor

`nvim.mode` says who opens that neovim. It is explicit configuration, not
detection.

| `nvim.mode` | who opens it | where it works | focus |
|---|---|---|---|
| `attach` (default) | you, with `spokenpad editor` | any terminal, any desktop: X11 or Wayland, any window manager | the daemon opens no window, so it cannot take focus |
| `managed` | the daemon, on the first key-down, in `nvim.terminal` | i3 or sway | proven: the running window manager's `no_focus` rule, before the window exists |
| `pane` | the daemon, on the first key-down, in a window it draws itself | X11, and Wayland through Xwayland | the window's own properties, which any window manager reads |

`attach` is the default because it is the one that works everywhere with no
window-manager rules to install: a new user gets a working setup on any
desktop, and nothing spokenpad does can move focus. `managed` is the
original behaviour, generalised from alacritty on i3 to five terminals on i3
and sway; it is worth its setup for a window that appears by itself, beside
what you are reading. `pane` is that same window without the setup and
without the terminal: spokenpad owns it, so the rule, the terminal table and
the empty-workspace gap all go away — and so does the window's independence
from the daemon.

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
marker a managed spawn uses. The daemon finds it on the next key-down: the
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
  and the next editor — `spokenpad editor`, or a managed spawn — opens on it
  rather than on a new file. `spokenpad editor` *takes* the passage: it
  removes the pointer before the editor reads the file, under a lock
  (`<socket>.pending.lock`) that every direct write also holds, so nothing
  is ever written behind an open editor's back; until the daemon reaches
  that editor, text goes to a new pending passage. An editor the daemon opens
  itself settles the pointer once it holds the file. A pointer that names anything but a regular file inside
  `nvim.dictation_dir` is ignored.
- **An editor that exits mid-dictation loses nothing.** A request that
  could not be sent to the editor certainly did not land, so its text goes to
  the pending passage. An append whose reply is lost is not redirected,
  since it may have landed; writing it again elsewhere could write it twice.
- **Decoding is unchanged.** The file is only a different sink for the same
  commits; each is still decoded exactly once.
- **You are told.** When a pending passage is started, the daemon sends one
  desktop notification (`notify-send`, off with `nvim.notify = false`) saying
  where the text is and to run `spokenpad editor`; every write is also in the
  log. The winbar cannot say it — there is no winbar.

The pointer lives beside the socket, in `$XDG_RUNTIME_DIR` by default, so a
reboot forgets it; the file stays in `nvim.dictation_dir`.

The same path catches a managed-mode editor that could not be opened (a
missing `no_focus` rule, a terminal that is not installed): the text goes to
the pending passage instead of only to the log.

## Pane mode: the window spokenpad draws

```
nvim --embed --listen <socket> -u <init> <dated file>
```

No terminal. The daemon creates an X11 window, starts that nvim as a child
with its stdin and stdout as the channel, and attaches to it as a UI with
`nvim_ui_attach(columns, rows, { ext_linegrid = true, rgb = true })`. Neovim
then describes its display as redraw events instead of drawing to a terminal,
and spokenpad turns them into pixels. The editor is otherwise the same one
every mode gets — same argv, same init, same ownership marker, same
`--listen` socket — so the daemon appends to it exactly as it does to a
terminal editor. **Committed text does not travel over the drawing channel**:
the pane draws, and never writes to the buffer.

### Why it needs no rule

The window carries four properties, all set before it is first mapped, which
is when a window manager reads them:

| property | what it does |
|---|---|
| `_NET_WM_USER_TIME = 0` | "do not focus this window when it is mapped" (EWMH). On i3 this is the whole guarantee, and unlike `no_focus` it holds for the first window on an empty workspace. |
| `_NET_WM_WINDOW_TYPE_UTILITY` | floats it on tiling window managers, and is what refuses focus on those that ignore user time (bspwm, Hyprland's Xwayland). |
| `WM_HINTS input = True` | the ICCCM "passive input" model, so the window manager may give it focus *later* — which is how you click in and type. |
| `WM_CLASS = spokenpad-pane` | a name no rule written for the managed-mode terminal can match, because this window needs none. |

`_NET_WM_USER_TIME` is written **once, as zero, and never again**. The EWMH
contract is that a toolkit updates it to the timestamp of the last user
interaction, and a window manager re-reads it at every map, so a window that
kept it current would steal focus the next time it appeared. That is measured,
along with everything in the table:
[2026-09-21-own-window-p0-properties.md](experiments/2026-09-21-own-window-p0-properties.md).
`WM_TAKE_FOCUS` is deliberately absent: with `input = True` and a user time of
zero it changes nothing, and announcing it would oblige spokenpad to answer
it.

**Verified on i3 only.** The other window managers are read from their source
(the experiment's table says which), not run. Try pane mode before you rely on
it, and say what you find.

### What it costs to run

| | |
|---|---|
| processor time while the window is open and nothing happens | none: the loop blocks on one channel fed by two threads, with no timer and no polling |
| resident memory the window adds | about 5.5 MB, for the framebuffer, the rasterised glyphs and the editor's client |
| key-up to the transcript in the file, opening the window on the way | about 300 ms |
| system libraries | `libxcb`, `libxkbcommon`, `libxkbcommon-x11`, opened when a pane opens rather than linked, so the binary starts without them in the other modes |
| programs | `fc-match`, from fontconfig, once per font at startup |

`spokenpad check` reports all of those before anyone dictates, and exits
non-zero when one is missing.

### The font

`nvim.font_family` is a fontconfig family name and defaults to `monospace`,
the alias your desktop already points at the font you want. `nvim.font_size`
is in **pixels**: the pane rasterises at a pixel size and has no display
resolution to convert a point size from, so a point size here would be a
number that means something on paper and nothing on the screen.

Bold is thickened by hand where fontconfig has no bold face — which is what
`monospace` resolves to on some machines — and italic falls back to the plain
face, because a synthetic slant looks worse than none. A character the family
does not cover is fetched from whichever font fontconfig names for it, so a
transcript that contains `漢` or `✓` shows them rather than blank cells.

There is no input method: dead keys and Compose work, because they are
xkbcommon's and spokenpad reads the layout the X server has loaded, but IBus
and Fcitx are not clients of this window. For German and English dictation
that is not a gap; for CJK input it is, and `attach` or `managed` is the
answer there.

### What happens when

- **No `DISPLAY`, or a library missing.** The daemon says which, and the text
  goes to the pending passage, exactly as it does when a terminal is not
  installed. It does not fail.
- **You close the window.** The editor inside it is asked to write every
  modified buffer and quit, its socket goes, and the next dictation opens a
  new window on a new file. Same as closing a managed terminal.
- **You `:q` in it.** The same, from the other end.
- **The daemon restarts.** The window goes with it — it is a thread of that
  process and the editor is its child. This is the one real difference from
  managed mode, where your terminal outlives the daemon and is reattached to.
  Nothing is lost: dictated text is written to the file after every utterance,
  and the pane writes every modified buffer before it quits, so a restart
  costs the window and not the transcript. The next dictation opens a new one.
- **You run `spokenpad editor` anyway.** It opens an editor in your terminal
  and the daemon adopts it, rather than opening a pane — the socket is the
  contract, not the mode. Close it and the next dictation opens a pane again.

### Where it opens

`nvim.window_fraction` of the monitor under the pointer, with its top-left
corner at the pointer, clamped fully on-screen — the same rule managed mode
uses and the same pure functions in `core/geometry.rs` decide it. The
monitors come from RandR and the pointer from the X server itself, rather
than from a window manager's IPC socket, so this works under a window manager
spokenpad has never heard of.

Under Xwayland the pointer is **not** asked for: `QueryPointer` there answers
with wherever the pointer last was over an X window, which is not where it is,
and a window at a stale position looks deliberate in a way a corner does not.
With `WAYLAND_DISPLAY` set the window goes to the corner instead.

The position is asked for in `WM_NORMAL_HINTS` before the window is mapped, so
it appears in about the right place, and corrected once afterwards — a window
manager that draws a frame places the frame rather than the window inside it,
which on i3 is four pixels across and eighteen down.

## Managed mode: what gets spawned

```
alacritty --class spokenpad \
  -o window.position.x=<x> -o window.position.y=<y> \
  -e nvim -u <bundled dictation_init.lua> --listen <socket> <dated file>
```

`nvim.terminal` names the terminal, from a table spokenpad knows rather than
as an argv it would have to trust. Each row is taken from that terminal's own
documentation, and says how its window is named for the window manager,
whether it can be told where to open, and how it takes the command:

| `nvim.terminal` | window name (`I` = `nvim.window_instance`) | initial position | source |
|---|---|---|---|
| `alacritty` | `--class I`: X11 class and instance, Wayland `app_id` | `-o window.position.x/y`, X11 only | `alacritty --help`, `alacritty(1)` |
| `kitty` | `--class I --name I`: X11 class/instance, Wayland `app_id` | `--position XxY`, X11 only, non-negative | `kitty --help` |
| `foot` | `--app-id=I`; Wayland only | none | `foot(1)` |
| `wezterm` | `start --always-new-process --class I`: both halves of `WM_CLASS`, Wayland `app_id` | `--position screen:X,Y`, X11 only, non-negative | wezterm.org/cli/start, wezterm's X11 `WM_CLASS` code |
| `ghostty` | `--class=spokenpad.I` (Wayland `app_id`; GTK needs a dotted id), `--x11-instance-name=I` | none (GTK cannot) | ghostty `Config.zig` |
| `headless` | no window: `nvim --headless` | — | — |

`wezterm` gets `--always-new-process`, and `ghostty` `--gtk-single-instance=false`:
without them the window may open inside an already running instance, and the
process spokenpad started would exit at once. A terminal outside the table
is refused, because its window name — and so the `no_focus` rule — cannot be
known before the window exists.

The **instance** is `spokenpad` by default. It is the name every
window-manager rule keys on, and it is restricted to `[A-Za-z][A-Za-z0-9_-]*`
because it is interpolated into criteria strings — a config value must not be
able to become window-manager syntax.

The daemon runs as a systemd user service, so the terminal it spawns sees the
user manager's environment, not your session's. Import what the terminal and
the IPC socket need from your window manager's startup: `DISPLAY` (and
`XAUTHORITY`) on i3; `SWAYSOCK`, `WAYLAND_DISPLAY` and `DISPLAY` on sway:

```
exec "systemctl --user import-environment DISPLAY XAUTHORITY; systemctl --user start spokenpad"
```

The window is opened lazily, on the **first key-down**, not at daemon start:
until you dictate there is no reason for a terminal to be sitting on your
desktop. It is opened on the editor thread while the utterance is still being
spoken, so the wait is paid in parallel with the recording rather than added
to it, and the append that follows simply queues behind it.

## Focus, and why there is no focus code

The window must never take focus — it appears while you are reading something
else, and you keep reading.

Who enforces that differs by mode. In `managed` it is the window manager's
job, through a rule the user installs and spokenpad proves. In `pane` it is
the window's own properties, which every window manager reads without being
told anything ([above](#why-it-needs-no-rule)). What both have in common, and
what the rest of this section is about, is that spokenpad contains **no focus
call at all**: not `xdotool windowfocus`, and not a "remember the focused window and
restore it afterwards" dance either, since restoring focus is itself a focus
change and would race anything the user did in between.

Two lines in a rules file do it properly. On i3,
[`packaging/i3/spokenpad.conf`](../packaging/i3/spokenpad.conf):

```
for_window [instance="spokenpad"] floating enable
no_focus   [instance="spokenpad"]
```

and on sway, [`packaging/sway/spokenpad.conf`](../packaging/sway/spokenpad.conf),
the same for `app_id` *and* `instance`: every supported terminal except foot
picks Wayland or Xwayland by itself, and a rule for the other would not
apply.

spokenpad refuses to **spawn** a graphical editor unless it can prove those
rules are in the *loaded* configuration for this exact name. It asks the
running window manager over its IPC socket (`$SWAYSOCK`, `$I3SOCK`, or
`i3 --get-socketpath`; `GET_VERSION` says which one answered). i3 returns its
configuration with every included file, as loaded. sway returns the main file
only, and spokenpad reads nothing from disk, since a file on disk may never
have been loaded: on sway the rules must be in the main config file itself
(`~/.config/sway/config`), not in a file it includes. A rule counts only if its criteria are exactly this one
property with a literal value.

The rules are not enough on an empty workspace: i3 and sway both give the
first window on a workspace focus, whatever `no_focus` says. So spokenpad also
reads the tree and does not spawn while the focused workspace holds no
window; the text then goes to the pending passage. The tree is read just
before the spawn, so switching to an empty workspace in the ~200 ms before
the window maps is the one remaining gap.

The proof is taken at spawn only: reattaching to an editor that is already on
screen trusts the rule that was proven when it was opened, because the window
is already mapped and unfocused and a later reload cannot un-steal focus that
was never stolen.

Verified live on i3 on 2026-09-07: the focused window was unchanged across an
open, and i3 reported the new window as `focused: false`. sway is implemented
against `sway-ipc(7)`, `sway(5)` and sway's source, and tested against a fake
IPC server; it has not been run live.

## Placement

Size and position are spokenpad's, not the window manager's, so the rule file stays to the two
things only a window manager can do.

Both window modes follow the same rule with the same code; where they differ
is only who is asked. Managed mode asks the window manager over its IPC
socket, because it has to talk to i3 or sway anyway to prove the rule; pane
mode asks X directly ([above](#where-it-opens)).

The window is **a third of the screen on each axis** (`nvim.window_fraction`,
0.33) with its **top-left corner at the mouse pointer**, on whichever monitor
the pointer is on (on i3, read with `xdotool` in managed mode and with
`QueryPointer` in pane mode; sway gives a client no way to ask, and Xwayland
answers with a stale position, so on Wayland the window opens in a corner of
the chosen output) — it opens beside what you are reading rather than in a
fixed corner you have to look away to find. `geometry::pick_output` chooses the
output and `geometry::placement` clamps the rect fully on-screen — both pure
functions over output rectangles — so a pointer near an edge tucks the window
flush against it, and a pointer in the bottom-right corner (or one that cannot
be read at all) puts it in the bottom-right corner.

Placement happens **once, on spawn**, and never again: a window that jumped back
under the pointer on every keypress would fight anyone who had put it somewhere
they wanted it. Reattaching to a running editor moves nothing.

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
re-read or edit an earlier passage keeps their place.

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

The whole indicator is pushed at once — phase, level, preview, notice,
notice_detail, latched, previewing — so the editor's copy is a function of
daemon state rather than of the history of updates that reached it. `preview`
and `notice` are separate fields and are drawn in separate places; an absent
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

**The live preview** is an extmark's `virt_lines` hanging below the end of the
buffer, exactly where the committed text will land. Virtual text, not buffer
content: it cannot be written to the file, yanked, or undone into the buffer
even deliberately. That makes "a preview is never committed"
([constraints.md](constraints.md#the-one-relaxation-a-cosmetic-preview-of-the-open-tail))
a property of the data model rather than a discipline.

Since 2026-09-08 the preview is only the **open tail** — the sentence being
spoken now, at most one chunk. Everything before it has already been
committed to the buffer above, so the preview restarts from nothing each time
a chunk lands and the transcript itself is never cropped
([progressive-commit.md](progressive-commit.md)).

Virtual text does **not** wrap — nvim truncates a chunk at the window edge —
so the preview is word-wrapped here by hand, measured in display columns with
`strdisplaywidth` rather than bytes, since dictation is routinely German and
byte counting would break `längeren` several columns early. It keeps the last
eight lines; the newest words are the ones being checked against what was
just said.

The preview hangs below EOF, where nvim will not scroll by itself: `zb`,
`zz` and CTRL-E all stop with the last real line at the bottom. So a reader
at the end is kept there by computing the view directly: walk up from the
last line until the text plus the preview rows fill the window, then hide the
surplus rows of that top line with smoothscroll's `skipcol`. The window's
`scrolloff` is set to 0, because a `scrolloff` would scroll the view straight
back.

When previews stop, the winbar says so instead of freezing. Past
`preview.max_seconds` (30 s of uncommitted tail) they **pause and resume by
themselves** once the tail settles — the tick keeps committing meanwhile, which
is what settles it; past the in-memory ceiling they stop for
good and the notice names the WAV that keeps recording. A daemon running
without a VAD model issues no preview at all, and says so the same way for the
whole capture. Without that message, a frozen preview during a long passage
reads as lost audio rather than as a cost control — which is exactly how it was
first reported.

Every other **notice** about the capture just made — held too briefly,
microphone gap, microphone unavailable, capture incomplete, nearly silent,
memory cap — is appended to the winbar in the same way, in its own
`SpokenpadNotice` highlight (the theme's `WarningMsg` foreground, or
`DiagnosticWarn` where that is unset). One at a time, until the next key press.

Which one, when a capture collects two, is decided by `Notice::priority` and
applied in exactly one place, `Session::notify`: memory cap > capture
incomplete > microphone unavailable > microphone gap > nearly silent > held too
briefly > preview paused, ties going to the newer report. A paused preview can
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
colourscheme name is the user's. `config.example.toml` documents the recipe.

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

**244 ms** from key-down to a window that is placed, chrome-free and
answering RPC; **92 ms** to reattach to one that is already open. Both are off
the latency path — the window is opened on every key-down, on the editor
thread, while the user is still speaking.

Neither number is about nvim. Measured time-to-RPC-ready is 0.26 s under a
full LazyVim config and 0.28 s under the bundled one, so the editor was never
the cost. Three other things were, and all three are fixed:

- **The readiness probe spawned a whole nvim per poll.** `nvim --server
  <socket> --remote-expr 1` every 100 ms, at ~200 ms a spawn. It is now a raw
  msgpack-RPC round trip on the socket: one connection, one request, and the
  reply arrives when nvim's event loop reaches it. It still has a hard
  timeout, which is why the Rust client enforces one absolute deadline; an
  editor that is still starting must not
  block the editor thread with no way out, hanging the daemon's shutdown.
  `nvim.startup_timeout_s` stays a generous 20 s: a first-ever open took
  13.5 s while a plugin manager did one-time work.
- **The first X query of the process cost ~1.9 s**, where every later one
  costs ~50 ms. That landed on the first dictation of a session — the one
  occasion with nothing on screen to hide it. The layout is queried once per
  spawn, before the editor starts, so a slow editor start costs one query,
  not one per attempt. Since 2026-09-21 the outputs, the configuration and
  the tree come from the window manager's own IPC socket (`shell/wm.rs`), one
  request per connection under a two-second deadline, instead of `xrandr` and
  `i3-msg` subprocesses; only the pointer still costs one `xdotool` call, on
  i3. On the headless path no query is made at all.
- **The window was placed after nvim answered.** It is now told where to open
  (`window.position.x`/`y`, which i3 honours for a floating window: verified,
  the window maps at exactly the requested point, floating and unfocused), so
  its first frame is already in the right place instead of appearing wherever
  the window manager chose — often the middle of the other monitor — and then
  flying across the screen. Only the size is still corrected afterwards,
  because alacritty measures its window in character cells and guessing the
  font metrics to avoid one small resize would be worse than the resize. That
  correction is issued as soon as i3 reports the window (~0.18 s), which is
  well before nvim has drawn anything.

`init` is passed straight to `nvim -u`, so nvim's own special value works
there too — for a window with no configuration at all:

```toml
[nvim]
init = "NONE"
```

## Reconnection

A live socket is reattached to rather than respawned, so restarting the daemon
does not litter the desktop with terminals. Quitting nvim simply means the
next dictation opens a fresh one — in managed mode; in attach mode it goes to
a new pending passage until you run `spokenpad editor` again.

Liveness is checked on every key-down, and a plain `connect` is not enough: a
socket answers exactly as before the user `:bdelete`s the dictation buffer,
and every append of that utterance then fails against a buffer that is gone.
So the check is a real RPC round trip that asks the editor which buffer it
still has pinned, and it passes only if that is the buffer this session owns.
It is bounded by an absolute two-second deadline — every call in
[`shell/nvim/rpc.rs`](../src/shell/nvim/rpc.rs) carries one, because a peer
that dribbles bytes must not extend a call indefinitely by staying inside a
per-read timeout. An editor that does not answer in time is dropped and
reattached to (or respawned) rather than waited on, since the recording is
already running.

An append that times out is the ambiguous case: the request may have completed
after its reply was lost. It is retried once, on a fresh connection, with the
**same operation id** — the Lua side returns its cached result instead of
appending twice. If that retry fails, the session disconnects, so the next
utterance reattaches rather than writing into a client whose reply stream is out
of step.

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
