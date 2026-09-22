# Does a tiled pane stay unfocused on every window manager?

_2026-09-22, code at 171e829 plus the change this file ships with
(`nvim.pane_layout`, the i3 desktop in `tests/harness/desktops.rs`). One
Manjaro machine, headless: i3 4.25.1 and Openbox 3.6.1 on Xvfb, sway 1.12
and KWin 6.7.5 (`kwin_wayland`) with Xwayland 24.1.13, KWin 6.7.5
(`kwin_x11`) on Xvfb. Other agents ran test suites on the machine at the
same time._

## Question

The user asked for a tiled pane beside the floating one, with one condition:
a tiled pane must never take the focus by itself on any window manager, and
where that cannot be proven the daemon must refuse the tiled layout there.
A tiled pane differs from the floating one in one property: its window type
is `_NET_WM_WINDOW_TYPE_NORMAL` instead of `_UTILITY`, which tiling window
managers tile. Everything else — user time 0, `_NET_WM_STATE_ABOVE`,
`WM_HINTS input = True`, the sway rule — is the same. Does that hold the
focus off everywhere?

## Method

`cargo test --locked --test pane_focus_wms tiled -- --nocapture
--test-threads=1`: the story of
[the stacking experiment](2026-09-22-pane-stacking-and-sway.md), with
`nvim.pane_layout = "tiled"`, on i3 (now also a `harness::desktops` desktop,
so i3 runs the daemon's own open path too), sway, Openbox, KWin on Wayland
and KWin on X11, the two KWins at focus stealing prevention None, the most
permissive level. The focus is sampled every 5 ms through the open, six
redraws, the next passage's pane and a pane on an empty workspace; the
user's click must focus the pane; the positive control must be focused.
Where the window manager tiles, the pane must be tiled (not floating);
elsewhere it must be above the focused window, as the floating pane is.
The whole file was also run in parallel, with the floating stories.

## Data

No recordings. The pane shows generated sentences.

## Results

| | i3 | sway | Openbox | KWin Wayland | KWin X11 |
|---|---|---|---|---|---|
| focused on open and six redraws | never (262 samples) | never (211) | never (275) | never (269) | never (319) |
| placed | tiled, 632x778 beside the holder | tiled, 636x773 beside the holder | at the pointer, above | at the pointer, above | at the pointer, above |
| the user's click focuses it | yes | yes | yes | yes | yes |
| the next passage's pane | never | never | never | never | never |
| alone on an empty workspace | never | refused, no window (as floating) | never | never | never |
| positive control focused | yes (no user time) | yes (not matched by the rule) | yes (no user time) | yes (no user time, normal type) | yes (no user time, normal type) |

The tiled pane takes the size the window manager gives its tile; the pane
follows a resize as it always has, and Neovim is told the new grid.

## Conclusion

- A tiled pane never took the focus on any of the five window managers, in
  any stage. The user time holds it on i3, Openbox and KWin even with a
  normal window type (as the ablations in the earlier experiments showed for
  single maps), and the runtime rule holds it on sway.
- So the tiled layout is refused nowhere. On sway the empty-workspace
  refusal applies to both layouts alike.
- Openbox and KWin do not tile: there "tiled" is an ordinary window at the
  pointer, kept above like the floating pane. That is documented, not
  refused, since it never takes the focus.
- Not run: any other tiling window manager (bspwm, Hyprland, awesome).

Decision: [decisions.md](../decisions.md#a-tiled-pane-beside-the-floating-one-2026-09-22).
