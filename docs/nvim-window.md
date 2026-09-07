# The dictation window

← [docs index](README.md) | Implemented by
[`nvim.py`](../src/voice_kb/nvim.py) and
[`nvim_indicator.lua`](../src/voice_kb/nvim_indicator.lua). The reasoning for
using nvim at all is in
[decisions.md](decisions.md#the-sink-is-neovim-not-the-clipboard); the rules
it must not break are in [constraints.md](constraints.md).

The transcript goes to a neovim the daemon opens itself. Nothing is pasted
anywhere, and no window the user did not open for this purpose is ever
written to.

## What gets spawned

```
alacritty --class 'Floating,voice-kb' -e nvim --listen <socket> <dated file>
```

The X11 **class** is `Floating` and the **instance** is `voice-kb`. Both are
configurable (`nvim.terminal`, `nvim.window_instance`); the instance is the
name every window-manager rule keys on, and it is restricted to
`[A-Za-z0-9_-]` because it is interpolated into an i3 criteria string — a
config value must not be able to become i3 syntax.

The window is opened lazily, on the **first key-down**, not at daemon start:
until you dictate there is no reason for a terminal to be sitting on your
desktop. It is opened on the bridge thread while the utterance is still being
spoken, so the wait is paid in parallel with the recording rather than added
to it, and the append that follows simply queues behind it.

## Focus, and why there is no focus code

The window must never take focus — it appears while you are reading something
else, and you keep reading.

That is the window manager's job, and voice-kb contains **no focus call at
all**: not `xdotool windowfocus`, and not a "remember the focused window and
restore it afterwards" dance either, since restoring focus is itself a focus
change and would race anything the user did in between.

Two lines in `~/.config/i3/i3.d/voice-kb.conf` do it properly:

```
for_window [instance="voice-kb"] floating enable
no_focus   [instance="voice-kb"]
```

Verified live on 2026-09-07: the focused window was unchanged across an open,
and i3 reported the new window as `focused: false`.

## Placement

Size and position are voice-kb's, not i3's, so the rule file stays to the two
things only a window manager can do.

The window is **a quarter of the screen** (`nvim.window_fraction`, 0.5 of each
axis) with its **top-left corner at the mouse pointer**, on whichever monitor
the pointer is on — it opens beside what you are reading rather than in a
fixed corner you have to look away to find. The rect is clamped fully
on-screen by `geometry.dictation_rect`, the same clamp the overlay uses, so a
pointer near an edge lands the window flush against it; a pointer in the
bottom-right corner (or a pointer that cannot be read at all) gives the
bottom-right quarter.

Placement happens **once, on spawn**. On later dictations the window is only
moved to the current workspace — a window that jumped back under the pointer
on every keypress would fight anyone who had put it somewhere they wanted it.

## The file

One markdown file per day under `nvim.dictation_dir`, named by
`nvim.file_template` (`dictation-%Y-%m-%d.md`), written after **every**
utterance with `noautocmd write`.

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

`nvim_indicator.lua` is loaded over RPC on every connection (so a reattach
re-applies it), and defines `_G.VoiceKb`. The daemon calls exactly three
things: `setup`, `append`, and `set_state`.

**The winbar** carries the indicator: a phase dot, and while recording a
24-cell level meter driven by the same perceptual curve (`level ^ 0.6`) the Qt
overlay used. It is set window-locally on every window showing the dictation
buffer, so a global winbar from the user's own config is overridden for this
buffer only and left alone everywhere else. Levels are sent at ~10 Hz as
notifications, not requests — a round trip per sample would put nvim's event
loop on the dictation latency path for something purely cosmetic.

**The live preview** is an extmark's `virt_lines` hanging below the end of the
buffer, exactly where the committed text will land. Virtual text, not buffer
content: it cannot be written to the file, yanked, or undone into the buffer
even deliberately. That makes "a preview is never committed"
([constraints.md](constraints.md#the-one-relaxation-cosmetic-previews)) a
property of the data model rather than a discipline.

**The chrome is stripped** — `laststatus=0`, `showtabline=0`, no line
numbers, sign column, fold column or cursorline — and re-applied on
`BufWinEnter`/`WinNew`/`FileType`, because a plugin reacting to those events
would otherwise put it back. This is a dictation surface held in a small
floating window, not a general editing session; the user's own nvim is
untouched.

## Startup cost, and the timeout that bounds it

`nvim.editor` defaults to plain `nvim`, so the window is *their* editor, with
their keybindings. That has a cost: a full plugin configuration takes about a
second before it answers RPC (measured: 1.0–1.2 s for LazyVim here), and the
first ever open took 13.5 s while the plugin manager did one-time work.

Attaching during that window would block the bridge thread with no way out —
pynvim requests have no timeout — which would in turn hang the daemon's
shutdown. So readiness is probed **out of process**, with
`nvim --server <socket> --remote-expr 1` under a hard `subprocess` timeout,
and the attach only happens once nvim answers. `nvim.startup_timeout_s`
defaults to a generous 20 s for the same reason; waiting costs nothing the
user feels.

For an instant, config-free window instead, set:

```toml
[nvim]
editor = ["nvim", "-u", "NONE"]
```

## Reconnection

A live socket is reattached to rather than respawned, so restarting the daemon
does not litter the desktop with terminals. Quitting nvim simply means the
next dictation opens a fresh one.

Liveness is checked on every key-down with a plain `connect` to the socket —
not an RPC round trip, which has no timeout and would block the bridge on
what is meant to be a cheap check. A socket that is connectable but not
answering (an editor still starting, or one wedged) is left alone rather than
attached to.

When the daemon exits it **detaches without closing nvim**: the user may still
be editing what they dictated, and killing their editor because a daemon
exited would lose exactly the text this window exists to keep.
