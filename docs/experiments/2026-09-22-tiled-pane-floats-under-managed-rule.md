# A tiled pane floats under the managed-mode window rule

_2026-09-22, at 8eba906 plus the `--tiled` flag of `examples/pane.rs`; i3
4.25.1 on a private Xvfb._

## Question

The user set `nvim.pane_layout = "tiled"` on i3 4.25.1 and saw a floating
pane, while `tests/pane_focus_wms.rs` tiles it on a headless i3 of the same
version, and the log said nothing about the layout. What in a real i3 session
floats a pane that asks to be tiled?

The window itself was ruled out by reading `shell/pane/x11.rs`: a tiled pane
is `_NET_WM_WINDOW_TYPE_NORMAL`, has no `WM_TRANSIENT_FOR`, sets no
`_NET_WM_STATE_MODAL`, and its `WM_NORMAL_HINTS` give a minimum size of 16x16
and no maximum, so none of the conditions under which i3 floats a window by
itself applies. `_NET_WM_STATE_ABOVE` is set on both layouts and the headless
test tiles the pane with it. What the headless i3 lacks is the user's
configuration: until 2026-09-22 the package shipped
`packaging/i3/spokenpad.conf` for managed mode, and the README said to
include it. Its rule is `for_window [instance="spokenpad"] floating enable`,
and i3 reads a criterion's value as a PCRE pattern that may match anywhere in
the name. The pane's instance is `spokenpad-pane`.

## Method

A scratch script, not kept: for each variant of an i3 configuration, a
private `Xvfb :91`–`:100`, a private i3 with a generated configuration, then
the pane example with and without `--tiled`, under a temporary `HOME` and
`XDG_*` directories, and i3's tree read over its socket for the node whose
instance is `spokenpad-pane`. For one variant and layout, in shell terms:

```sh
Xvfb :91 -screen 0 1280x800x24 -nolisten tcp &
printf 'focus_follows_mouse no\nipc-socket %s\n%s' "$root/i3.sock" "$variant" > "$root/i3.config"
DISPLAY=:91 i3 -c "$root/i3.config" &
target/debug/examples/pane --display :91 --bundled --quit-after 4 \
  --socket "$root/nvim.sock" --file "$root/d.md" --tiled &
sleep 2
i3-msg -s "$root/i3.sock" -t get_tree   # the spokenpad-pane node's "floating"
```

Nothing ran on the user's display or read the user's configuration.

## Data

No recordings. Five configurations:

- no rule;
- `for_window [instance="spokenpad"] floating enable` (the shipped rule);
- that rule and `no_focus [instance="spokenpad"]` (the whole shipped file);
- `for_window [instance="^spokenpad$"] floating enable` (anchored);
- `no_focus [instance="spokenpad"]` only.

## Results

| i3 configuration | pane opened tiled | pane opened floating |
|---|---|---|
| no rule | `auto_off` (tiled) | `auto_on` |
| shipped float rule | **`user_on` (floating)** | `auto_on` |
| whole shipped file | **`user_on` (floating)** | `auto_on` |
| anchored float rule | `auto_off` (tiled) | `auto_on` |
| `no_focus` only | `auto_off` (tiled) | `auto_on` |

`user_on` is i3's state for a window a command or a `for_window` rule
floated; `auto_on` is its own decision, here from the utility window type.

## Conclusion

The managed-mode rule floats the tiled pane: `instance="spokenpad"` matches
`spokenpad-pane` because i3 does not anchor criteria. A configuration that
includes the old `packaging/i3/spokenpad.conf`, or a copy of its rules, turns
`pane_layout = "tiled"` into a floating pane on i3 4.25.1, which is the
reported behaviour. That the user's configuration has the rule is not
verified here; the user can check with
`i3-msg -t get_config | grep 'instance="spokenpad"'`.

The shipped file no longer carries the rule (managed mode is removed,
[decisions.md](../decisions.md#managed-mode-is-removed-the-pane-replaces-it-2026-09-22));
a linked copy loses it with the package update, a copied one has to be
edited by hand. The pane now logs the layout it opened with, as asked and as
applied, which shows that spokenpad asked i3 to tile it; a pane that still
floats after that line says "tiled" is floated by the window manager's
configuration.
