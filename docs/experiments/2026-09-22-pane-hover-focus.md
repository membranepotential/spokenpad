# Pane focus under focus-follows-mouse

_2026-09-22, main at `1b21933`, headless only (Xvfb above `:50`, 1280x800).
i3 4.25.1, Openbox 3.6.1. Focus sampled every 5 ms by
`tests/harness/desktops.rs::FocusSampler` (i3: IPC tree and `GetInputFocus`;
Openbox: `_NET_ACTIVE_WINDOW` and `GetInputFocus`)._

## Question

The user runs i3 with its default `focus_follows_mouse yes`. Moving the
mouse over the pane does not focus it, so closing it with i3's `kill` takes
an extra click. Every earlier focus test ran with `focus_follows_mouse no`.

1. Why does hover not focus the pane? Which property or behaviour stops it?
2. With `focus_follows_mouse yes`, can the pane get the focus without a
   deliberate pointer move: at the map, with the pointer at its corner, on a
   redraw, `place`, unmap and remap, a workspace switch, or when another
   window closes?
3. If hover focus can be allowed, what is the smallest change that allows it
   only for a deliberate move?

## Method

A scratch integration test was used and not kept: `tests/scratch_hover_focus.rs`,
built on `tests/harness`. It starts i3 with a generated config that holds
only `focus_follows_mouse yes|no` (plus `default_floating_border pixel 2` in
one run) and `ipc-socket`. A plain window fills the workspace as the window
being typed into. The pane is `shell::pane::x11::Window`, created the way
`Pane::open` and `Pane::show` create it: at `geometry::placement(monitor,
pointer, 648x360)`, mapped, then `place`d at the same rectangle. The pointer
moves with XTEST on the test's own Xvfb. Before each stage the test gives the
focus back to the plain window (i3: `[id=…] focus` over IPC; Openbox:
`_NET_ACTIVE_WINDOW`), and leaves the pointer where the stage needs it.
"Focused" means that at least one sample during the stage, or 500 ms after
it, put the i3 focus, the EWMH active window or the X input focus on the pane
or one of its frames.

For the cause, the same pane was also mapped with one property changed before
the map: `_NET_WM_USER_TIME` removed, `_NET_WM_STATE_ABOVE` removed, the type
set to `_NET_WM_WINDOW_TYPE_NORMAL`, or `WM_HINTS input = False`. Each was
then entered from outside.

Sources read: i3 4.25.1 `src/handlers.c` (`handle_enter_notify`,
`handle_motion_notify`), `src/x.c` (`x_push_node`, `x_push_changes`),
`src/manage.c`, `include/xcb.h`; sway 1.12
`sway/input/seatop_default.c`, `sway/commands/seat/cursor.c`; Openbox git
master at `3a2fbc6` (the binary run is 3.6.1) `openbox/event.c`, `openbox/client.c`.

## Data

No recordings. Synthetic windows and XTEST pointer motion only.

## Results

### Cause (i3)

i3 focuses on hover in two cases only. Both need the pointer to reach i3's
frame window:

- an `EnterNotify` on the frame with mode `Normal` (`handle_enter_notify`).
  i3 does not select `EnterWindow` on the client window (`CHILD_EVENT_MASK`).
  It removes `EnterWindow` from every frame while it maps, moves and stacks
  windows (`x_push_changes`), and ignores enters that share a sequence with a
  map or unmap request.
- a `MotionNotify` over the title bar (`handle_motion_notify`).

None of the pane's properties plays a part. Each pane below was mapped away
from the pointer, and the pointer then moved in from outside:

| pane (i3, ffm yes) | focused on map | focused when entered from outside |
|---|---|---|
| as shipped | no | yes |
| no `_NET_WM_USER_TIME` | yes | yes |
| no `_NET_WM_STATE_ABOVE` | no | yes |
| `_NET_WM_WINDOW_TYPE_NORMAL` | no | yes |
| `WM_HINTS input = False` | no | yes |

The cause is where the pane opens. `placement` puts the client's top-left
pixel on the pointer, and `show` moves it back there after the map. i3's
floating frame then reaches 4 px left and 18 px up of the pointer (normal
border), so after the map the pointer is inside the client. Moving right or
down into the text crosses no frame edge, and i3 gets no event it acts on.
Moving 1 to 3 px up or left crosses onto the border or the title bar, and the
pane takes the focus.

### Safety with `focus_follows_mouse yes` (i3)

| stage | ffm yes, floating | ffm yes, pixel border | ffm yes, tiled | ffm no, floating |
|---|---|---|---|---|
| A map, pointer on the client's corner (today's placement) | no | no | no¹ | no |
| B nudge up 1 or 3 px from there | **focused** | **focused** | no¹ | no |
| B nudge up 10 px (still on the title bar) | **focused** | no (off the frame) | no¹ | no |
| B nudge left 1 or 3 px (border) | **focused** | **focused** | no¹ | no |
| B nudge left 5 px, or (-6,-6), (-30,-30) (off the frame) | no | no | no | no |
| B nudge right and down (+5,+5), into the text | no | no | no | no |
| C1 20 redraws (`PutImage`), pointer inside | no | no | no | no |
| C2 pointer glides inside the client only | no | no | n/a² | no |
| C3 `place`: move and resize, pointer stays inside | no | no | n/a² | no |
| D unmap and remap, pointer inside | no | no | n/a² | no |
| E `place` moves the pane under a resting pointer | no | no | no | no |
| F workspace away and back, pointer over the pane | no | no | no | no |
| G the focused tiled window closes, pointer over the pane | no | no | no | no |
| H a focused floating window over the pane and the pointer closes | no | no | no | no |
| I map with the pointer resting mid-pane | no | no | no | no |
| J pointer glides in from outside (the deliberate move) | **focused** | **focused** | **focused** | no |
| K the pane was focused once; another focused floating window closes | no | no | no | no |

¹ A tiled pane gets the right half of the screen, so the pointer at (300,200)
stays in the other window. ² The glide into the tile crossed its edge, which
is J; the pane was still focused at C3 and D, so those rows prove nothing.

Pointer-to-frame gap: the client placed right of and below the pointer by a
margin, then the pointer moved over every point within 6 px of where it
rested (169 positions):

| client offset from pointer | outer frame starts at (i3 normal border) | focused (normal / pixel border) |
|---|---|---|
| 0 px (today) | (-4, -18): pointer inside | **yes / yes** |
| 8 px | (4, -10) | **yes / yes** |
| 24 px | (20, 6) | no / no |
| 48 px | (44, 30) | no / no |

### Openbox 3.6.1, `followMouse yes`, `focusDelay 0`

Openbox puts the *frame* at the requested position, so today the pointer is
on the frame's top-left pixel.

| stage | `underMouse no` (default) | `underMouse yes` |
|---|---|---|
| A map, pointer on the frame's corner | no | **focused** |
| B nudges up/left 1–3 px, (+5,+5), (-30,-30) | no | no |
| C2 glide inside, E `place` under a resting pointer | no | no |
| D unmap and remap, pointer inside | no | **focused** |
| I map with the pointer resting mid-pane | no | **focused** |
| entered from outside, as shipped (3 runs) | focused 3/3 | not run |
| entered from outside, no `_NET_WM_STATE_ABOVE` (3 runs) | 0/3 (probably stacked below the full-screen window, so never under the pointer; stacking not read) | not run |
| client 24 or 48 px right of/below the pointer: map, then 6 px jiggle | no | no |

After the unmap and remap (D), a later entry from outside (J) did not focus
the pane on Openbox. The likely cause is that the remapped pane lost
`ABOVE`, as in the row above; this was not checked. The daemon never unmaps
a pane (each passage opens a new window), so no real sequence reaches this.

### sway 1.12 (source only, not run)

sway focuses a view when the node under the cursor differs from the node it
was over at the last motion (`check_focus_follows_mouse`, `previous_node`).
`handle_rebase`, which runs after a map or a transaction, sets `previous_node`
to the node under the cursor. So a pane that appears under a resting cursor
is not focused, and a later move into it from another node is. The
`no_focus` rule the pane adds acts only at the map. The border belongs to the
same node, so the i3 nudge problem does not arise there. Under Xwayland the
pane opens in a corner, not at the pointer. The headless harness cannot drive
this: `seat - cursor set/move` runs `cursor_rebase`, not the motion handler,
and no virtual-pointer tool (wlrctl, ydotool) is installed.

## Conclusion

- **The cause is placement, not a property.** i3 focuses on hover only when
  the pointer crosses into its frame. The pane opens with the pointer already
  inside it, so moving into the text never focuses it. Every property variant
  was focused when entered from outside.
- **Today's placement is a safety gap under focus-follows-mouse.** On i3,
  moving the pointer 1 to 3 px up or left from where the pane opened puts the
  focus on the pane (every such nudge in both i3 runs). That is not a deliberate move
  into the window. Openbox with `underMouse yes` (not its default) focuses the
  pane at the map whenever the pointer is inside it. Nothing else measured
  gave it the focus: redraws, `place`, remaps, workspace switches and closing
  windows all left the focus alone on i3.
- **Proposed change:** open the pane with its *outer frame* at least 16 to
  24 px away from the pointer on both axes: right of and below it, or left
  of and above it when it does not fit. Pass that gap to
  `geometry::placement`. Ask for the pre-map position with a generous
  allowance for the frame (for example 48 px). After the map, `show` should
  place the outer frame (the top-level ancestor, or `_NET_FRAME_EXTENTS`)
  rather than the client. With a gap of at least 20 px from the frame, no
  point within 6 px of the pointer focused it on i3 or Openbox, and a real
  move into it did. No window property changes, so every map-time proof in
  `pane_window.rs` and `pane_focus_wms.rs` still holds. The change also
  closes the Openbox `underMouse` gap. When the pane is too large to leave
  the pointer outside on either axis, the fallback needs a decision:
  centred on the pointer (safe, but hover does nothing) or clamped as today
  (unsafe near the frame edge).
- **Decision needed:** `docs/constraints.md` allows the user's own click to
  focus the pane. Hover focus extends that to the user's own pointer entering
  it, where they configured focus-follows-mouse. The pane is large and above
  other windows, so a pointer crossing it on the way elsewhere also focuses
  it. That is ordinary focus-follows-mouse behaviour, but it is new for this
  window.
- Not verified: sway (source only), KWin's "focus (strictly) under mouse"
  policies (expected to behave like Openbox `underMouse`), a real pane with
  nvim inside (the bare X11 window was used; the pane adds no event mask that
  i3 reads), and HiDPI title bars taller than 18 px.
