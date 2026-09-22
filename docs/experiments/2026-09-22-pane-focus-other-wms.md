# Does the pane stay unfocused on sway, Openbox and KWin?

_2026-09-22, code at 775fabd plus the tests added with this file
(`tests/pane_focus_wms.rs`, `tests/harness/desktops.rs`); no change to the
pane itself. sway 1.12 with Xwayland 24.1.13, Openbox 3.6.1 on Xvfb, KWin
6.7.5 (`kwin_wayland`) with Xwayland 24.1.13, all headless on one Manjaro
machine._

## Question

`nvim.mode = "pane"` is to become the default. Its one hard requirement is
that the window never takes the focus
([constraints.md](../constraints.md#no-window-spokenpad-opens-may-take-focus)).
Until now that was run only on i3
([P0](2026-09-21-own-window-p0-properties.md)); every other window manager was
read from its source. Which of the window managers now installed leave the
pane unfocused, and where one does not, is there a fix that keeps the rule
of no focus call?

KWin 6 no longer ships `kwin_x11` in the `kwin` package (`pacman -Ql kwin`
lists only `kwin_wayland` and `kwin_wayland_wrapper`), so KWin was run as a
Wayland compositor with Xwayland, which is how KWin users run it today.

## Method

`cargo test --locked --test pane_focus_wms -- --nocapture --test-threads=1`.
Each test starts its window manager in a headless session of its own
(`harness::desktops`): an empty environment, a temporary `HOME`,
`XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR`, and a generated configuration.

| window manager | how it runs | configuration | focus read from |
|---|---|---|---|
| sway | `WLR_BACKENDS=headless`, `WLR_LIBINPUT_NO_DEVICES=1`, `WLR_RENDERER=pixman`, Xwayland on | `xwayland enable`, `focus_follows_mouse no`, one 1280x800 output | sway's tree over its own IPC socket, and `GetInputFocus` on its Xwayland |
| Openbox | on an Xvfb above `:50`, `--sm-disable --config-file` | `focusNew yes` (its default, and the setting under which it focuses new windows), `followMouse no`, two desktops | `_NET_ACTIVE_WINDOW` and `GetInputFocus` |
| KWin | `kwin_wayland --virtual --xwayland`, on a private `dbus-daemon` | `FocusStealingPreventionLevel` 1 (Low, the default) or 0 (None), click to focus, two desktops | `_NET_ACTIVE_WINDOW` and `GetInputFocus` on its Xwayland |

Every test runs the same story:

1. A plain window of the test's own (`focus-holder`, `WM_HINTS input = True`)
   holds the focus.
2. The pane opens exactly as the daemon opens it: `NvimSession::ensure` with
   `nvim.mode = "pane"` — screen measured, pane thread started, window
   mapped, the embedded nvim attached — and then six appends, each followed
   by a key typed at the holder.
3. A sampler thread with its own X connection reads the X input focus and
   the window manager's focused window every 5 ms, from before the open to
   after the last redraw. A stage fails if any sample names the pane or its
   frame.
4. Placement: viewable, wholly on screen, and whether
   `_NET_CLIENT_LIST_STACKING` puts the pane above the holder.
5. The user selects the pane, then the holder again.
6. The user closes the pane; the next passage's pane opens and is sampled
   the same way; then a pane opens on an empty workspace.
7. Positive control: the shipped window with `_NET_WM_USER_TIME` deleted
   before the map, and if that is not focused, also typed
   `_NET_WM_WINDOW_TYPE_NORMAL`. At least one must be focused, or the run
   cannot tell focus from no focus.
8. Ablations, recorded and not asserted: `WM_HINTS input = False`; user time
   0 with `_NET_WM_WINDOW_TYPE_NORMAL`; the shipped properties plus
   `_NET_WM_STATE_ABOVE`.

How input reaches each window manager:

- On Openbox, clicks and keys are XTEST on the test's own Xvfb.
- On sway, a click is `seat - cursor set/press/release` over sway's IPC, and
  a key is XTEST inside Xwayland. That key reaches the X focus without going
  through sway.
- KWin cannot be given input headless. XTEST on its Xwayland goes to KWin
  over libei, and KWin refuses that without a user's approval
  (`kwin_eis_prompter` connects and disconnects in KWin's debug log). The
  attempt also deactivates the focused window, so the KWin tests type
  nothing. "The user selects" is a KWin script,
  `workspace.activeWindow = window`, loaded over the private bus. That
  script is what KWin's click-to-focus does on a click.

## Data

No recordings. The pane shows six generated sentences.

## Results

Two full serial runs and one parallel run gave the same verdict on every
row, except the two marked "varied".

### Was the pane focused?

| stage | sway | Openbox | KWin, Low | KWin, None |
|---|---|---|---|---|
| open, six redraws, keys typed at the holder | **focused** (tree and X focus) | never (266 samples) | never (268) | never (263) |
| after the redraws, the window manager focuses | **the pane** | the holder | the holder | the holder |
| keys typed that reached the holder | 0 of 6 (they went to the pane) | 6 of 6 | not typed | not typed |
| the next passage's pane, after the user selected the first | **focused** | never | never | never |
| a pane alone on an empty workspace | **focused** | never | never | never |
| the user selects the pane | focused | focused | focused | focused |
| the user selects the holder again | the holder | the holder | the holder | the holder |

### Placement

| | sway | Openbox | KWin, Low | KWin, None |
|---|---|---|---|---|
| size, position | 414x252 at 433,274 | 414x252 at 641,420 | 414x252 at 640,436 | 414x252 at 640,436 |
| viewable, wholly on the 1280x800 screen | yes | yes | yes | yes |
| floating | yes | stacking WM | stacking WM | stacking WM |
| above the holder in `_NET_CLIENT_LIST_STACKING` | yes | yes | **no** | **no** |

The test process has no `WAYLAND_DISPLAY`, so `place.rs` asked Xwayland for
the pointer even on sway and KWin; in a real Wayland session the pane opens
in a corner instead ([nvim-window.md](../nvim-window.md#placement)).

### Controls and ablations

"Focused" means the window manager and the X input focus both named the
window.

| window | sway | Openbox | KWin (both levels) |
|---|---|---|---|
| control: no `_NET_WM_USER_TIME` | focused | focused | not focused |
| control: no user time, `_NET_WM_WINDOW_TYPE_NORMAL` | — | — | focused |
| user time 0, `_NET_WM_WINDOW_TYPE_NORMAL` | **focused** | not focused | not focused, and stacked **below** the holder |
| `WM_HINTS input = False`, user time 0: on map | not focused | not focused | not focused |
| the same, after the user selects it | sway focuses it, X input focus `PointerRoot`, an XTEST key reached it | varied between runs | varied between runs |
| shipped plus `_NET_WM_STATE_ABOVE` | focused | not focused, above | not focused, **above** |

## Conclusion

- **Openbox 3.6.1: supported.** Openbox never focused the pane in any stage,
  with `focusNew` on. The control shows that user time 0 is what holds, and
  a normal window type with user time 0 holds as well.
- **KWin 6.7.5 on Wayland (with Xwayland): supported for focus.** KWin never
  focused the pane, at the default level and with focus stealing prevention
  off. Each of the two properties holds by itself: without a user time the
  utility type still blocks, and with a normal type user time 0 still
  blocks. One cost is new: **KWin stacks the pane below the active window**.
  A pane that overlaps the focused window is partly hidden behind it.
  `_NET_WM_STATE_ABOVE` set before the map fixes the stacking without giving
  focus, on KWin and on Openbox. That is a stacking change, not a focus fix, so it is not shipped
  here and is left open.
- **sway 1.12: unsupported.** sway focuses the pane when it maps, including
  the next passage's pane and a pane on an empty workspace. Keys the user
  types go to the pane. sway's `view_map`
  ([sway/tree/view.c at 1.12](https://github.com/swaywm/sway/blob/1.12/sway/tree/view.c))
  focuses every new window on the focused workspace unless a user's
  `no_focus` rule matches it or its ICCCM input model is "No Input". It reads
  neither `_NET_WM_USER_TIME` nor the window type. So no property the pane
  ships can prevent it.
  - The only property that does is `WM_HINTS input = False`, measured above.
    sway then leaves the window unfocused. A later click focuses it in
    sway's tree, the X input focus stays `PointerRoot`, and an XTEST key
    injected inside Xwayland reached it.
  - Whether a real key through sway's seat would reach it is **not
    measured**: no Wayland input tool (`wtype`) is installed.
  - The same property would have to be sway-only. On Openbox and KWin the
    rows after a select varied between runs, and on i3 `input = False` is
    the ICCCM model that asks never to be given the keyboard. Choosing it is
    a design decision with a usability cost. It is not made here.
  - Pane mode on sway is therefore unsupported. Attach mode, and managed mode
    with its proven `no_focus` rule, remain the sway options.
  - `sway_focuses_the_pane_so_pane_mode_is_unsupported_there` pins this: it
    fails if sway ever stops focusing the pane, so the docs get updated.
- Every test leaves no process behind. `pgrep` for Xvfb, sway, openbox,
  kwin, Xwayland, a session `dbus-daemon` and `nvim --embed` finds nothing
  after the runs. Xwayland runs with `-terminate`, so it outlived a stopped
  sway by up to 10 s until the harness stopped it explicitly.

Decision: [decisions.md](../decisions.md#pane-mode-verified-on-openbox-and-kwin-unsupported-on-sway-2026-09-22).
