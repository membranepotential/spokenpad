# How large is a window manager's frame, and where does it go?

_2026-09-22, `f81484e` plus the placement change in progress, headless only
(Xvfb above `:50` or a compositor's own Xwayland, 1280x800). i3 4.25.1,
Openbox 3.6.1, KWin 6.7.5 (`kwin_x11`, and `kwin_wayland --virtual` with
Xwayland), sway 1.12 with Xwayland._

## Question

The pane is to open with its outer frame a gap away from the pointer
([hover focus](2026-09-22-pane-hover-focus.md)). Before the map nobody knows
the frame. So:

1. How far does each window manager's frame reach past the window, at 96 and
   at 192 dpi? That sizes the room the position asked for before the map has
   to leave.
2. Does the window manager put the frame or the window at a position the
   window asks for, before the map (`WM_NORMAL_HINTS`) and after it
   (`ConfigureWindow`)? With which `win_gravity`?
3. Does it say where the frame is (`_NET_FRAME_EXTENTS`, a reparenting frame
   window)?

## Method

A scratch integration test, not kept, on `tests/harness` (the desktops of
`tests/pane_focus_wms.rs`; i3 also with `focus_follows_mouse yes` and
`font pango:monospace 8`, at `Xft.dpi` 96 and 192). It opened
`shell::pane::x11::Window` floating at 300,200, 400x200, mapped it, waited,
and read the window's rectangle in root coordinates, the rectangle of its
top-level ancestor, and `_NET_FRAME_EXTENTS`. Then it asked for 300,200 again
with `ConfigureWindow` and read them again. Each window manager ran once with
the gravity the pane had (`NorthWest`) and once with `win_gravity = Static`
rewritten into `WM_NORMAL_HINTS` before the map. KWin on Wayland and sway ran
only with `Static`, once the pane had switched to it.

## Data

No recordings. One synthetic window per run.

## Results

Frames (`_NET_FRAME_EXTENTS` left, right, top, bottom; every reparenting
window manager's frame window agreed with it):

| window manager | extents | reparents |
|---|---|---|
| i3, default font (X core `fixed`) | 4, 4, 18, 4 | yes |
| i3, Pango monospace 8, 96 dpi | 2, 2, 18, 2 | yes |
| i3, Pango monospace 8, 192 dpi | 4, 4, 30, 4 | yes |
| Openbox 3.6.1, default theme | 1, 1, 20, 5 | yes |
| KWin 6.7.5 X11, Breeze | 0, 0, 36, 0 | yes |
| KWin 6.7.5 Wayland (Xwayland) | 0, 0, 36, 0 | no; the title bar is drawn by the compositor |
| sway 1.12 (Xwayland) | not set | no |

Where the frame's top-left corner lands, for a window asking for 300,200:

| window manager | `NorthWest`, before the map | `NorthWest`, `ConfigureWindow` | `Static`, before the map | `Static`, `ConfigureWindow` |
|---|---|---|---|---|
| i3 | frame at 300,200 | window at 300,200 | frame at 300,200 | window at 300,200 |
| Openbox | frame at 300,200 | frame at 300,200 | window at 300,200 | window at 300,200 |
| KWin X11 | frame at 300,200 | frame at 300,200 | window at 300,200 | window at 300,200 |
| KWin Wayland | not measured | not measured | window at 300,200 | window where asked¹ |
| sway | not measured | not measured | centred: 440,300 | centred |

¹ The window was already there; a move elsewhere was not tried.

## Conclusion

- The largest frame measured reaches 36 pixels past the window (KWin's title
  bar), and top plus bottom never more than 36 at 96 dpi (34 for i3 at 192
  dpi). A position asked for before the map that leaves 48 pixels (scaled by
  `Xft.dpi` / 96) on every side keeps the frame the gap from the pointer
  whichever side of that position the window manager draws it on.
- With `Static` gravity a `ConfigureWindow` places the window itself on all
  three reparenting window managers, i3 included, which reads no gravity and
  always did. So once the frame is known, one move puts it where it belongs.
  Before the map i3 still puts its frame at the position asked for; the room
  above covers that.
- Every window manager but sway says how large its frame is, through a frame
  window, `_NET_FRAME_EXTENTS`, or both. sway places Xwayland windows itself,
  centred, and ignores the position.
- Decided: [the pane opens beside the pointer](../decisions.md#the-pane-opens-beside-the-pointer-never-under-it-2026-09-22).
