# Can spokenpad draw Neovim itself, well enough to dictate into?

_2026-09-21, on the P1 commits of the own-window plan. Headless: Xvfb 21.1.24,
i3 4.25.1, Neovim 0.12.5, x11rb 0.14.0, swash 0.2.10, xkbcommon 0.9.0 against
libxkbcommon 1.13.2, rustc 1.95.0, Manjaro/Arch. The timings were taken while
other work was running on the machine, so treat them as upper bounds._

## Question

Phase P0 proved spokenpad can own a window that never takes focus. P1 asks the
next question: if that window holds an embedded Neovim that spokenpad draws
itself — no terminal — does the dictation window still do its job?

Five things had to hold, and each is a pass criterion:

- **(a)** the grid spokenpad draws equals Neovim's own screen, cell for cell,
  through arbitrary editing and scrolling;
- **(b)** a committed transcript still arrives the way it always has, over the
  editor's socket, and shows in the pane without moving the focus;
- **(c)** after a click, what the user types lands in the buffer exactly,
  German layout and all;
- **(d)** the preview is drawn and never becomes buffer content;
- **(e)** a resize leaves the pane and Neovim agreeing.

And two things that are not criteria but decide whether this is usable: what
the text looks like, and what it costs while nothing is happening.

## Method

`nvim --embed --listen <socket> -u <init> <file>`, with spokenpad attached over
the process's stdin and stdout as an `ext_linegrid` UI
(`nvim_ui_attach(columns, rows, { ext_linegrid = true, rgb = true })`). Neovim
then describes its whole display as `redraw` notifications instead of drawing
to a terminal. New code:

| where | what |
|---|---|
| `core/grid.rs` | the redraw events, typed, and the screen they fold into; every event returns the rows it changed |
| `core/keys.rs` | keysym + modifiers + typed text → the notation `nvim_input` reads |
| `shell/pane/ui.rs` | the embedded process and the UI channel, on the daemon's own msgpack-RPC framing |
| `shell/pane/font.rs` | `fc-match` for the face, swash for hinted glyphs, cached per grapheme |
| `shell/pane/keyboard.rs` | xkbcommon reading the layout from the X server, with Compose |
| `shell/pane/x11.rs` | the window from P0, plus `PutImage` |
| `shell/pane/mod.rs` | the loop, the renderer, and the command that starts the editor |

[`tests/pane_render.rs`](../../tests/pane_render.rs) runs all of it on an Xvfb
and an i3 it starts itself, with `setxkbmap de` loaded into that server. The
shared harness is [`tests/harness/mod.rs`](../../tests/harness/mod.rs); it
never touches the user's display, i3 configuration or state directory.

Two details of the method are worth writing down.

- **The comparison in (a) is made deterministic, not polled.** The test reads
  Neovim's screen with a Lua call that runs `vim.cmd("redraw")` first. That
  makes Neovim flush its UI immediately, so the redraw notifications go down
  the same channel *ahead of* the answer to the call, and the pane has applied
  them by the time the answer arrives. Without it Neovim defers the flush
  while the test keeps it busy, and the comparison races: the first version of
  the test spent about a second per step waiting for a screen the pane had not
  been told about yet, and took four minutes.
- **(b) uses the real append path.** The test builds an `NvimSession` in
  attach mode against the pane's socket and calls `append`, which is the same
  code the daemon runs. The pane is not involved: it only draws what Neovim
  then reports.

## Data

No recordings. The inputs are 200 seeded random editor operations
(`ihello<Esc>`, `dd`, `<C-d>`, `gg`, `u`, `<C-r>` and so on, seed
`0x5D0_7E57_5EED`, so a failure replays exactly), one appended sentence, one
typed string, one indicator push and one resize. Screenshots are written to
`$SPOKENPAD_PANE_SCREENSHOTS`.

## Results

**Every criterion passed.**

| criterion | result |
|---|---|
| (a) grid equals `screenstring` | matched after each of 200 random operations, and after the seeded buffer, after typing and after the resize |
| (b) append over the `--listen` socket | shown in the grid **96–109 ms** after `append` was called (four runs); the pane stayed unfocused and `GetInputFocus` did not move |
| (c) click, then type on `de` | `Grüße ßé` landed in the buffer exactly, including the `dead_acute` + `e` compose |
| (d) preview | drawn as virtual text in the grid; the buffer held the committed sentence and not one character of the preview |
| (e) resize | 72x14 → 52x10; Neovim's `columns`/`lines` and the pane's grid agreed, and the cells matched again |

Rendering and cost, at font size 16 px with `fc-match monospace`
(Noto Mono on this machine):

| measurement | value |
|---|---|
| cell size | 10 x 19 px, baseline 15 px from the top |
| processor time over 2 s with the window open and nothing happening | **0.0 ms** |
| one full 72x14 `screenstring` read (the test's own cost, not the pane's) | 0.63 ms |
| whole test, from starting Xvfb to the last assertion | 7–10 s |

Screenshots (`pane-preview.png`, `pane-typed.png`, `pane-resized.png`): the
winbar renders with its colours and bold, the committed text and the grey
preview below it are both legible, the block cursor inverts the cell it is on,
and umlauts, `ß` and `é` are all correct. Bold is synthesised when fontconfig
has no bold face — Noto Mono has none — and the winbar shows that it reads as
bold.

## Conclusion

**The approach works, and P2 can wire `nvim.mode = "pane"` into the daemon.**
What the numbers say beyond the pass/fail:

- **Idle cost is zero, by construction.** The loop blocks on one channel; a
  thread waiting for X events and a thread reading Neovim's channel feed it.
  There is no timer and no polling, so an open pane costs nothing until
  something happens.
- **An append is drawn in about a tenth of a second**, which is the whole
  round trip: the daemon's RPC call, Neovim writing the file, the redraw
  coming back, and the pixels going to the X server. Dictation latency is
  dominated by decoding, which is seconds, so this is not on the critical
  path.
- **Committed text does not touch the renderer.** The pane draws; the daemon
  still appends over the socket. That keeps the "every transcript goes through
  one path" property the constraints rely on, and it is why (b) could be
  written with the real `NvimSession` instead of a stand-in.

Two decisions changed from the plan.

- **The font comes from `fc-match`, not from `fontdb`.** A `fontdb` database
  of this machine's system fonts holds 5049 faces and has to be built at every
  start to answer one question — which file is "monospace"? — that fontconfig
  has already answered, with the user's own rules applied. `fc-match` is one
  short-lived process at startup and returns the file and the face index
  directly. The cost is a hard dependency on the `fc-match` binary, which the
  pane reports as an error naming fontconfig.
- **x11rb runs with `allow-unsafe-code`**, which is the feature that provides
  `XCBConnection`. That is the only x11rb connection type `xkbcommon`'s X11
  half accepts, and reading the layout from the server is what makes dead keys
  and a per-device `setxkbmap` work. This was already flagged in the P0
  experiment.

**The pane's system libraries are opened on demand, not linked.** The pane
needs libxcb, libxkbcommon and libxkbcommon-x11, and `fc-match` on `PATH`.
Listing those libraries as needed would have meant that a binary built with
the pane refuses to *start* on a machine without them — a minimal Wayland
install, a server — for a mode that user never selected, and the default mode
opens no window at all. So x11rb runs with `dl-libxcb` and the keyboard goes
through `xkbcommon-dl`; both `dlopen` their library the first time a pane is
opened, and a missing one is an error naming the package
(`shell/pane/xkb.rs`). Measured with `ldd`: neither the `spokenpad` binary nor
`examples/pane`, which opens a pane, lists any of them.

| what | when it is needed |
|---|---|
| `libxcb.so.1`, `libxkbcommon.so.0`, `libxkbcommon-x11.so.0` | opened when a pane opens; `spokenpad check` reports them |
| `fc-match` on `PATH` | the same |

One real bug was found by the test rather than by reading. **The thread that
waits for X events did not stop when the pane was dropped**: the pane woke it
with a client message of its own, but the thread only exited when its channel
closed, and the receiver was still alive at that moment, so `join` blocked
forever. It now checks a flag the pane sets before waking it. Nothing outside
a test had ever dropped a pane, which is exactly why the criterion "the test
must end" earns its keep.

### What this does not show

- **Only i3, only X11, only this machine's fonts.** Other window managers are
  P3 and a Wayland desktop is P4.
- **No input method.** A hand-rolled X11 window has no IBus or Fcitx client,
  so dead keys and Compose work (they are xkbcommon's) but CJK input does not.
  For German and English dictation that is not a gap; it is named here so it
  is not discovered later.
- **No font fallback.** A character the matched face does not cover is drawn
  as nothing. Emoji are a colour glyph the renderer does handle, but only when
  the whole cell is one: a colour glyph with a combining mark keeps the colour
  glyph and drops the mark.
- **Italic is not synthesised.** Fontconfig gave no italic face for Noto Mono,
  and where bold is emboldened, italic falls back to the plain face. The
  preview highlight is italic, so the preview reads as plain grey rather than
  as grey italic.
- **A colourscheme was not exercised.** The test runs the bundled init, which
  deliberately sets none, so the colours in the screenshots are Neovim's
  defaults. The pane reads `default_colors_set` and `hl_attr_define` like any
  UI, so a colourscheme should simply work — but it has not been run.

No `decisions.md` entry follows yet: P1 changes no behaviour, because nothing
in the daemon uses the pane. The decision is due when `nvim.mode = "pane"`
lands in P2.
