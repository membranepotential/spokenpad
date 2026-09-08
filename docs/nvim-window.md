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
alacritty --class 'Floating,voice-kb' \
  -o window.position.x=<x> -o window.position.y=<y> \
  -e nvim -u <bundled dictation_init.lua> --listen <socket> <dated file>
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

The window is **a third of the screen on each axis** (`nvim.window_fraction`,
0.33) with its **top-left corner at the mouse pointer**, on whichever monitor
the pointer is on — it opens beside what you are reading rather than in a
fixed corner you have to look away to find. The rect is clamped fully
on-screen by `geometry.dictation_rect`, so a
pointer near an edge tucks the window flush against it; a pointer in the
bottom-right corner (or a pointer that cannot be read at all) puts it in the
bottom-right corner.

Placement happens **once, on spawn**. On later dictations the window is only
moved to the current workspace — a window that jumped back under the pointer
on every keypress would fight anyone who had put it somewhere they wanted it.

## Latched recording

Holding the latch modifier (`hotkey.latch_modifier`, shift by default) with
the hotkey starts a recording that outlives the key release: let go, keep
talking, press the hotkey again to stop. Push-to-talk is unchanged without it.

The mode lives in the state machine — `Recording(latched=True)` — rather than
being read off whichever event ends the recording, because the ending event
differs between the modes: a `KeyUp` for push-to-talk, a `KeyDown` for a
latch. The stopping press does *not* need the modifier, so there is nothing to
remember about which hand started it. The winbar shows a lock while latched.

The modifier is read with evdev's `active_keys()` at the moment of the press —
a query of the kernel's current key state — rather than by tracking modifier
events. Tracking would need the watcher to have seen every modifier event on
every device since it started, and it has not: devices come and go through
udev, so a modifier already held when a keyboard is plugged in would be
invisible forever after.

## The file

One markdown file per **window** under `nvim.dictation_dir`, named by
`nvim.file_template` (`dictation-%Y-%m-%d-%H%M%S.md`), written after **every**
utterance with `noautocmd write`.

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

`nvim_indicator.lua` is loaded over RPC on every connection (so a reattach
re-applies it), and defines `_G.VoiceKb`. The daemon calls exactly three
things: `setup`, `append`, and `set_state`.

**The winbar** carries the indicator: a phase dot, and while recording a
24-cell level meter on a perceptual curve (`level ^ 0.6`). It is set window-locally on every window showing the dictation
buffer, so a global winbar from the user's own config is overridden for this
buffer only and left alone everywhere else. Levels are sent at ~10 Hz as
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

When previews stop — past `preview.max_seconds`, which only a daemon running
without a VAD model can reach, or past the in-memory ceiling — the winbar
says `preview paused, still recording`. Without that, a frozen preview during
a long passage reads as lost audio rather than as a cost control — which is
exactly how it was first reported.

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
configured it**, and that difference is visible. This user runs tokyonight
with `transparent = true`, so their real editor shows the terminal through;
loading the plugin raw took tokyonight's own `#222436` instead and painted a
grey-blue block inside alacritty's black border -- close to right, and clearly
wrong. `nvim.transparent` (on by default) clears the background groups after
any colourscheme loads, which reproduces that setting and is the right answer
for a window floating over a terminal in any case. Verified against the full
config: `Normal`, `NormalFloat`, `EndOfBuffer`, `SignColumn` and `WinBar` all
come back with no background either way. Set it `false` to take the
colourscheme's own.

Neither of the fast rows is a default this project can pick, because the
colourscheme name is the user's. `config.example.toml` documents the recipe.

The bundled config also maps `j`/`k` to move by **screen** line. An utterance
is one buffer line wrapped over many screen rows, so plain `j` leaps a whole
paragraph and reading a transcript by keyboard is unusable. It is an
expression mapping that checks `v:count`, so `5j` still means five buffer
lines and `gg`/`G` are untouched.

What voice-kb enforces itself, so it holds under any of them:

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
the latency path — the window is opened on every key-down, on the bridge
thread, while the user is still speaking.

Neither number is about nvim. Measured time-to-RPC-ready is 0.26 s under a
full LazyVim config and 0.28 s under the bundled one, so the editor was never
the cost. Three other things were, and all three are fixed:

- **The readiness probe spawned a whole nvim per poll.** `nvim --server
  <socket> --remote-expr 1` every 100 ms, at ~200 ms a spawn. It is now a raw
  msgpack-RPC round trip on the socket: one connection, one request, and the
  reply arrives when nvim's event loop reaches it. It still has a hard
  timeout, which is the reason it was out of process to begin with — pynvim
  requests have none, and attaching to an editor that is still starting would
  block the bridge thread with no way out, hanging the daemon's shutdown.
  `nvim.startup_timeout_s` stays a generous 20 s: a first-ever open took
  13.5 s while a plugin manager did one-time work.
- **The first X query of the process cost ~1.9 s**, where every later one
  costs ~50 ms. That landed on the first dictation of a session — the one
  occasion with nothing on screen to hide it. The bridge now warms it up when
  its thread starts, and caches the monitor layout for 5 s besides.
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
next dictation opens a fresh one.

Liveness is checked on every key-down with a plain `connect` to the socket —
not an RPC round trip, which has no timeout and would block the bridge on
what is meant to be a cheap check. A socket that is connectable but not
answering (an editor still starting, or one wedged) is left alone rather than
attached to.

When the daemon exits it **detaches without closing nvim**: the user may still
be editing what they dictated, and killing their editor because a daemon
exited would lose exactly the text this window exists to keep.
