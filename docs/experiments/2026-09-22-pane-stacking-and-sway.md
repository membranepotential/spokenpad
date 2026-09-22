# Does the pane stay on top, and can sway be made to leave it unfocused?

_2026-09-22, code at c7a7d2b plus the change this file ships with
(`_NET_WM_STATE_ABOVE` on the pane, the sway `no_focus` rule over IPC, the
`kwin_x11` desktop in `tests/harness/desktops.rs`). One Manjaro machine,
headless: i3 4.25.1 and Openbox 3.6.1 on Xvfb 21.1.24, sway 1.12 and KWin
6.7.5 (`kwin_wayland`) with Xwayland 24.1.13, KWin 6.7.5 (`kwin_x11`) on
Xvfb. Other agents ran test suites on the machine at the same time._

## Question

[The previous experiment](2026-09-22-pane-focus-other-wms.md) left two gaps
in pane mode (`nvim.mode = "pane"`):

- KWin on Wayland stacked the unfocused pane **below** the focused window.
  The user decided: set `_NET_WM_STATE_ABOVE` before the first map, and prove
  on every window manager that the pane is on top and still never focused.
- sway focused the pane on every map. The user decided: add a `no_focus` rule
  for the pane over sway's IPC before the map, if sway accepts one at
  runtime, and prove the pane is then never focused; otherwise refuse to open
  the pane on sway.

The focus rule was also reworded: the pane must never take focus by itself,
and the user's own click may focus it. Both halves are asserted now. KWin on
X11 (`kwin-x11` 6.7.5, newly installed) had not been run at all.

## Method

**Does sway 1.12 take `no_focus` at runtime?** Read at the `1.12` tag:

- `sway/commands.c` has three handler tables. `no_focus` is in `handlers[]`,
  the table used both while reading the configuration and for IPC commands.
  The config-only table (`config_handlers[]`) holds `include`, `xwayland`,
  `workspace_layout` and four others, not `no_focus`.
- `sway/commands/no_focus.c` parses the criteria, returns success without
  adding anything if `criteria_already_exists`, and otherwise appends the
  rule to `config->criteria`.
- `should_focus` in `sway/tree/view.c` returns true for the first window on
  the focused workspace before it looks at any rule, and otherwise false
  exactly when a `CT_NO_FOCUS` criterion matches the view.
- `split_args` keeps a bracketed criteria list as one argument, and every
  criteria value is a PCRE pattern matched anywhere in the name, so the rule
  is anchored: `no_focus [instance="^spokenpad-pane$"
  class="^spokenpad-pane$"]`.

Then on a headless sway 1.12 (private `XDG_RUNTIME_DIR`, `swaymsg -s` to its
own socket, a scratch script not kept): that command twice, a malformed
criteria list, and a config-only command for contrast.

**The pane on every window manager.** `cargo test --locked --test
pane_focus_wms -- --nocapture` (and `--test-threads=1` for the table below),
and `cargo test --locked --test pane_window -- --nocapture` for i3. The story
is the previous experiment's, with these changes:

- The pane opens through `NvimSession::ensure` with `nvim.sway_socket` set to
  the test's sway, exactly as the daemon reads `$SWAYSOCK`.
- "Shown above the focused window" is asserted after the open, after the user
  selects the holder again, and for the next passage's pane. On an X11
  window manager it is the X stacking order of the two top-level frames
  (`QueryTree` on the root); on sway, a floating pane over a tiled holder,
  from sway's tree; on KWin Wayland, `_NET_CLIENT_LIST_STACKING`, which KWin
  publishes from its own stacking order. `_NET_CLIENT_LIST_STACKING` must not
  disagree on any of them.
- The user's select must focus the pane: both the window manager and the X
  input focus name it.
- On sway, the pane on an empty workspace must be refused, with no window
  left behind.
- The positive control takes the shipped window apart one step at a time
  until the window manager focuses it: first `WM_CLASS` renamed to
  `spokenpad-control` (so the sway rule does not match), then also no
  `_NET_WM_USER_TIME`, then also `_NET_WM_WINDOW_TYPE_NORMAL`. The control is
  now sampled like the pane: one sample on it counts as focused.
- Ablations, recorded: user time 0 with a normal window type, and the shipped
  window without `_NET_WM_STATE_ABOVE`.
- New desktop: `kwin_x11 --no-kactivities` on an Xvfb above `:50`, with a
  private session bus and a generated `kwinrc`, at focus stealing prevention
  Low (the default) and None. Being an X11 window manager, it takes XTEST
  clicks and keys, so its select is a real click.

## Data

No recordings. The pane shows generated sentences.

## Results

### sway takes the rule at runtime

| command sent over IPC | sway's reply |
|---|---|
| `no_focus [instance="^spokenpad-pane$" class="^spokenpad-pane$"]` | `success: true` |
| the same again | `success: true`; sway's debug log: `no_focus already exists` |
| `no_focus [bogus="x"]` | `success: false`, `Token 'bogus' is not recognized` |
| `xwayland disable` (config-only) | `success: false`, `Unknown/invalid command 'xwayland'` |

### The pane, per window manager

One serial run of every test, confirmed by at least three parallel runs of
the whole file (every one passed after the harness fixes below).

| | i3 | sway | Openbox | KWin Wayland, Low | KWin Wayland, None | KWin X11, Low | KWin X11, None |
|---|---|---|---|---|---|---|---|
| focused on open and six redraws | not after the map (read once, not sampled) | never (219 samples) | never (280) | never (286) | never (302) | never (299) | never (319) |
| keys typed at the holder that reached it | — | 5 of 6 (see below) | 6 of 6 | not typed | not typed | 6 of 6 | 6 of 6 |
| shown above the focused window after the open | yes | yes | yes | yes | yes | yes | yes |
| the user selects the pane: focused | yes | yes | yes | yes | yes | yes | yes |
| still above after the user selects the holder again | yes | yes | yes | yes | yes | yes | yes |
| the next passage's pane | (the same window re-mapped: not focused) | never, above | never, above | never, above | never, above | never, above | never, above |
| a pane on an empty workspace | not focused | **refused**, no window | never | never | never | never | never |

### Controls and ablations

| window | sway | Openbox | KWin Wayland | KWin X11 |
|---|---|---|---|---|
| control: `WM_CLASS` spokenpad-control | **focused** | not focused | not focused | not focused |
| control: also no `_NET_WM_USER_TIME` | — | focused | not focused | not focused |
| control: also `_NET_WM_WINDOW_TYPE_NORMAL` | — | — | focused | focused |
| user time 0, `_NET_WM_WINDOW_TYPE_NORMAL` | not focused (the rule) | not focused | not focused | not focused |
| shipped window **without** `_NET_WM_STATE_ABOVE` | above (floating) | above | **below** the holder | **below** the holder |

On i3, `pane_window.rs` asserts the pane's frame above the base window in the
X stacking order after the map and after the user clicks back into the base
window; `_NET_CLIENT_LIST_STACKING` agrees.

### Harness findings

- **sway's Xwayland window manager can miss a property rewritten right after
  a window is created.** Before the fix, the renamed control was not focused
  in 3 of 8 runs; sway's tree then still listed it as `spokenpad-pane`, so the
  pane's rule matched it. With a 500 ms pause between creating the control
  and renaming it, sway's tree showed `spokenpad-control` in 10 of 10 runs.
  The pane writes all its properties once, before its first map, and the
  rule matched it in every run: it was never focused.
- **The first XTEST key on a fresh sway Xwayland is lost** whatever else
  happens: three keys typed at the holder before any pane existed arrived as
  2 of 3. That is the "5 of 6" above; no sample put the X focus anywhere but
  the holder.
- **Openbox ignored a window mapped right after it published its
  `_NET_SUPPORTING_WM_CHECK`**, in about one parallel run in three, also at
  c7a7d2b before this change (2 of 5 runs). The window stayed unmapped with
  an empty `_NET_CLIENT_LIST`. The harness now maps a throwaway window until
  Openbox manages it before the test starts; 6 of 6 parallel runs passed
  after that.
- `kwin_x11` puts a client two levels down (frame, wrapper), so the harness
  looks for the pane three levels deep.
- Leak check: `pgrep` for Xvfb, sway, openbox, kwin, Xwayland, a session
  `dbus-daemon` and `nvim --embed` found nothing of these tests' after the
  runs. (Another agent's Xvfb and `nvim --embed` were running before the
  runs and were gone after them.)

## Conclusion

- **sway 1.12 accepts `no_focus` at runtime over IPC**, and with the rule
  the pane was never focused in any stage of any run, while the same window
  under another `WM_CLASS` was. So the pane is supported on sway: the daemon
  adds the rule before every pane and opens it only on sway's success reply.
  On an empty focused workspace sway focuses any first window whatever the
  rules say, so the pane refuses to open there and the text goes to the
  pending passage. Nothing touches the user's sway configuration.
- **`_NET_WM_STATE_ABOVE` puts the pane on top without giving it the focus**
  on every window manager run. It is what fixes KWin, on Wayland and on X11:
  without it the pane lands below the active window.
- **KWin 6.7.5 on X11 never focuses the pane**, at both focus stealing
  prevention levels, and a real click focuses it. Its focus refusal needs the
  utility window type or the user time, like KWin on Wayland.
- **The user's click focuses the pane on every window manager**, so the user
  can edit in it.
- Not measured: a real Wayland key through sway's seat into the focused pane
  (no `wtype` installed), and whether a `swaymsg reload` between two
  dictations is handled — by construction it is, since the rule is sent
  before every pane.

Decision: [decisions.md](../decisions.md#pane-stays-on-top-and-sway-gets-a-runtime-no_focus-rule-2026-09-22).
