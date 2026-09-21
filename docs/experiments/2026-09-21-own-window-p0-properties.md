# Which X11 properties keep a window from taking focus when it appears?

_2026-09-21, on `51b2c72` plus this change. Headless: Xvfb 21.1.24 and i3
4.25.1 (`xorg-server-xvfb 21.1.24-1`, `i3-wm 4.25.1-1`), x11rb 0.14.0,
rustc 1.95.0, Manjaro/Arch. No timings, so the load on the machine does not
matter._

## Question

Phase P0 of the own-window plan: if spokenpad creates its own X11 window
instead of spawning a terminal, can that window satisfy the hard rule in
[constraints.md](../constraints.md) — **no window spokenpad opens may take
focus** — and still be usable?

Five things had to hold on i3, and each is a pass criterion:

- (a) not focused when it is mapped, on a workspace that already has a focused
  window **and** on an empty workspace (i3's `no_focus` rule loses the empty
  case, which is the gap [nvim-window.md](../nvim-window.md) documents today);
- (b) floating, not tiled into the user's layout;
- (c) focused after the user clicks into it;
- (d) keys typed after that click arrive in the window;
- (e) unmapping and mapping it again is also unfocused.

Beside the criteria: which single property does which job, so P1 and P2 know
what they may not drop, and how a window of this kind can be **placed**, since
P2 wants the pane near the pointer.

## Method

Two new files, neither wired into the daemon:

- [`examples/pane_spike.rs`](../../examples/pane_spike.rs) creates one X11
  window with x11rb and sets, **before the first `MapWindow`**:
  `_NET_WM_USER_TIME`, `_NET_WM_WINDOW_TYPE`, `WM_HINTS` with the `input`
  flag, `WM_CLASS` (instance and class `spokenpad-pane`, a name no existing
  user rule can match), `WM_NAME` and `_NET_WM_NAME`, `WM_PROTOCOLS` with
  `WM_DELETE_WINDOW`, and `WM_NORMAL_HINTS`. Each property can be changed or
  dropped from the command line. It then reports every `KeyPress`,
  `ButtonPress`, `FocusIn` and `FocusOut` on stdout and takes commands
  (`map`, `unmap`, `user-time`, `configure`, `net-moveresize`, `geometry`) on
  stdin.
- [`tests/pane_window.rs`](../../tests/pane_window.rs) starts **its own**
  Xvfb on the first free display number above `:50` and **its own** i3 with a
  generated two-line config (`focus_follows_mouse no`, a private
  `ipc-socket`), maps the spike's window in several property combinations, and
  reads the result from two independent places: the i3 tree over the IPC
  socket (framed by the daemon's own `core::wm` code) and the X server's
  `GetInputFocus`. It runs in `cargo test` and fails loudly when Xvfb or i3 is
  missing, unless `SPOKENPAD_ALLOW_MISSING_X11=1` is set — the same rule the
  nvim tests use for a missing `nvim`.

The click and the keystrokes are faked through the XTEST extension. spokenpad
itself must never synthesise input, for the reason in
[constraints.md](../constraints.md); this is test code and it refuses to run
against any display it did not start itself (it asserts the display number is
above `:50` and that its own Xvfb child is still alive before every faked
event). Nothing reaches the user's `:0`.

Every process the test starts is stopped by a guard type on the way out, also
when an assertion fails, with `SIGTERM` before `SIGKILL` so the X server
removes its lock file and socket.

## Data

No recordings. The inputs are window properties; the outputs are the i3 tree
and `GetInputFocus`. One run takes about 12 s; it was run seven times with
identical results.

## Results

"Focused on map" is i3's `focused: true` in the tree. "X input focus" compares
`GetInputFocus` before and after the map. The proposed set is
`_NET_WM_USER_TIME = 0`, `_NET_WM_WINDOW_TYPE = _NET_WM_WINDOW_TYPE_UTILITY`,
`WM_HINTS input = True`, `WM_CLASS = spokenpad-pane`.

| window properties | focused on map | floating | X input focus |
|---|---|---|---|
| the proposed set | no | yes | unchanged |
| the proposed set, re-mapped after a click and two keys | no | yes | unchanged |
| the proposed set, `_NET_WM_USER_TIME` rewritten to 1 before the re-map | **yes** | yes | **moved** |
| no `_NET_WM_USER_TIME` | **yes** | yes | **moved** |
| `_NET_WM_WINDOW_TYPE_NORMAL` instead of `_UTILITY` | no | **no** | unchanged |
| the proposed set plus `WM_TAKE_FOCUS` | no | yes | unchanged |
| `WM_HINTS input = False`, no user time | no | yes | unchanged |
| the proposed set, empty workspace | no | yes | unchanged |

Click and keys, with the proposed set:

| step | result |
|---|---|
| `FocusIn` events seen by the window during the map | 0 |
| after a faked click in the middle of the window | i3 `focused: true`, `GetInputFocus` = the window, 1 `ButtonPress` |
| two faked key presses after that click | 2 `KeyPress` events in the window |
| `_NET_WM_USER_TIME` read back after the click and the keys | still `0` |

Placement, with the proposed set on a 1280x800 screen:

| how the position was asked for | window ended up at (root coordinates) |
|---|---|
| nothing asked (no position in `WM_NORMAL_HINTS`) | 400,287 — i3 centres it |
| `WM_NORMAL_HINTS` position 220,140, size 400x200, before the map | 224,158 with size 400x200 |
| `ConfigureWindow` x=40 y=60 w=320 h=200, after the map | exactly 40,60 with size 320x200 |
| `_NET_MOVERESIZE_WINDOW` 700,420 300x180, after the map | exactly 700,420 with size 300x180 |

## Conclusion

**All five P0 criteria pass on i3, and the proposed property set is the right
one.** What each property does, on i3:

- **`_NET_WM_USER_TIME = 0` is the whole of the focus guarantee.** Dropping it
  is the only change that made i3 focus the window, and it is the only row
  where the X input focus moved. It also holds as the **first window on an
  empty workspace**, which the `no_focus` rule i3 offers does not. A window
  spokenpad creates itself therefore needs no window-manager rule, no IPC
  proof of that rule, and no "is the workspace empty" check on i3 — the three
  things managed mode has to do today.
- **`_NET_WM_WINDOW_TYPE_UTILITY` only decides floating** here. With the
  normal type and user time 0 the window was unfocused but **tiled**, which
  would rearrange the user's layout. The type stays for that reason, and
  because it is what blocks focus on the window managers that ignore user time
  (bspwm, Hyprland's Xwayland — see the research notes).
- **`WM_HINTS input = True` costs nothing on i3** and is what makes click-to-type
  work on Mutter and KWin. `input = False` also blocked focus on map here, but
  it is the wrong base: on Mutter and KWin such a window can never be focused,
  so the user could not click in and type.
- **`WM_TAKE_FOCUS` does not matter** next to user time 0: adding it changed
  nothing. (The research had found it decisive together with `input = False`;
  with `input = True` and a user time of 0 it is inert.) P1 should not
  announce it, since there is nothing to gain and a protocol to answer.

**The property must be re-read by i3 on every map, and it must stay 0.**
Unmapping and mapping again kept the window unfocused with no property change,
so spokenpad does *not* have to set `_NET_WM_USER_TIME` again before each map.
But writing a real timestamp into it and mapping again *did* focus the window,
which proves i3 re-reads the property at each map rather than caching its first
decision. This is the one trap for P1 and P2: the EWMH contract is that a
toolkit updates `_NET_WM_USER_TIME` to the timestamp of the last user
interaction, and a toolkit that did so after the user clicked into the pane
would focus-steal on the next map. **spokenpad's window code must never write
that property after the initial 0.** The test asserts both halves: the value is
still 0 after a click and two keystrokes, and a rewritten value focuses the
window.

**Consequences for P1.** The window part of the plan is confirmed; P1 can build
on `examples/pane_spike.rs`'s property code as-is. One finding changes the
crate list: `xkbcommon`'s `x11` feature (`xkb_x11_keymap_new_from_device`,
which is how a client gets the user's real layout, dead keys and all) takes a
connection that implements `AsRawXcbConnection`. x11rb's default pure-Rust
`RustConnection` does not; only `XCBConnection` does, which needs x11rb's
`allow-unsafe-code` feature and links the system `libxcb`. Verified by
compiling and running a probe against a private Xvfb: the XKB extension set up
at version 1.0, core keyboard device id 3, keymap read. So P1 uses
`x11rb = { version = "0.14", features = ["allow-unsafe-code", ...] }` and
`XCBConnection`, or it gives up `xkbcommon-x11` and builds the keymap from
names, which would lose the user's actual layout.

**Consequences for P2 (placement).** i3 honours a position asked for in
`WM_NORMAL_HINTS` before the map, but it places the *frame* there: asking for
220,140 put the client window at 224,158, i.e. off by i3's border and title
bar, which are theme-dependent. After the map, both `ConfigureWindow` and
`_NET_MOVERESIZE_WINDOW` put the client window exactly where they were told.
The recipe for "open at the pointer" is therefore: ask for the position in
`WM_NORMAL_HINTS` before the map so the window appears near the right place,
then correct it once with a `ConfigureWindow` — a few pixels of movement, no
focus change (the placement steps in this experiment all ran while the window
was unfocused and none of them moved the focus). With nothing asked for, i3
centres the window on the output, which is a usable fallback for Xwayland,
where the pointer position cannot be read.

**What this does not show.** Only i3 was tested. The claims about Mutter, KWin,
Openbox, xfwm4, bspwm, Hyprland, sway, awesome and niri in the research notes
are still read from source, not run; that is phase P3, and it needs packages
the machine does not have (sway, openbox, bspwm, awesome, xfwm4, mutter, kwin).
Click-to-type under a Wayland compositor cannot be checked headless at all,
because XTEST inside Xwayland does not go through the compositor's seat; that
is phase P4, by hand.

No `decisions.md` entry follows yet: P0 changes no behaviour. The decision is
due when `nvim.mode = "pane"` lands in P2.
