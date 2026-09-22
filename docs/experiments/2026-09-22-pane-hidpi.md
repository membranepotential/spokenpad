# The pane's text size against Alacritty's, on a HiDPI display

_2026-09-22, main at bcb219f plus this change. Alacritty 0.17.0 (94e7c887),
crossfont 0.8.1, winit 0.30.13, FreeType 2.14.3, fontconfig 2.18.3, Mesa
llvmpipe under Xvfb. Timings in the last section were taken while other
agents' builds ran on the machine._

## Question

On the user's display (3840x2160, `Xft.dpi: 192`) the pane's text was far
smaller than Alacritty's at the settings they use in both (`font.size = 12`
in Alacritty, "SauceCodePro Nerd Font Mono"). The pane took `nvim.font_size`
as pixels (default 16) and ignored the resolution, so its cells were 10x19
(`monospace`, Noto Mono) wherever it ran. How does Alacritty turn a point size
into cells, can the pane do the same thing exactly, and why did no test
notice?

## Method

**Read the source**, at the versions installed here:

- Alacritty v0.17.0 (release tarball from GitHub):
  `alacritty/src/display/mod.rs` (`Display::new` scales `font.size` by the
  window's scale factor; `compute_cell_size` floors `average_advance +
  offset.x` and `line_height + offset.y`),
  `alacritty/src/renderer/text/glyph_cache.rs` (`load_font_metrics` loads
  `m` and reads the rasteriser's metrics; a glyph's top is moved by the
  descent), `alacritty/src/renderer/rects.rs` (`create_rect`: underline and
  strikeout at `round(baseline - position - thickness / 2)`, clamped into the
  cell), `alacritty/src/config/font.rs` (default size 11.25),
  `alacritty/src/config/cursor.rs` and `display/cursor.rs` (cursor thickness
  `round(0.15 × cell width)`).
- crossfont 0.8.1 (crates.io): `src/lib.rs` (`Size` stores millionths of a
  point; `as_px = pt × 96 / 72`), `src/ft/mod.rs` (`metrics`: line height is
  the larger of FreeType's `height` and `ascender - descender`, the average
  advance is the hinted advance of `0`, underline and strikeout from `post`
  and OS/2 scaled by `x_scale`; `get_face` adds the pixel size to the
  fontconfig pattern; `ft_load_flags` maps fontconfig's hinting to FreeType
  load flags).
- winit 0.30.13 (`~/.cargo/registry`):
  `src/platform_impl/linux/x11/util/randr.rs` (scale factor is XSETTINGS'
  `Xft/DPI`, else the `Xft.dpi` resource, divided by 96; RandR's physical
  size only when neither is set; `WINIT_X11_SCALE_FACTOR` overrides all).
- FreeType 2.14.3 (savannah release tarball): `src/base/ftobjs.c`
  (`FT_Request_Metrics` and `ft_recompute_scaled_metrics` under
  `GRID_FIT_METRICS`: ascender ceiled, descender floored, height rounded;
  `FT_Load_Glyph` sends light hinting of a TrueType font to the autohinter
  and grid-fits the advance), `src/truetype/ttobjs.c` (`tt_size_reset`:
  integer ppem, but only in `hinted_metrics`, which the public size metrics
  never see), `src/autofit/afloader.c` (light mode rounds the advance at the
  unrounded scale), `src/sfnt/sfobjs.c` (which table the face's ascender,
  descender and height come from), `src/base/ftcalc.c` (`FT_MulFix`,
  `FT_DivFix`).

**Reproduce it** in `src/core/font.rs`, in integer 26.6 and 16.16 fixed point
where FreeType uses them, fed from the font's `head`, `hhea`, `OS/2` and `post`
tables and fontconfig's `hinting`, `hintstyle` and `antialias`.

**Measure Alacritty.** Each measurement starts an Xvfb (`-noreset`, so the
resources survive the last client), loads `Xft.dpi: N` with `xrdb -nocpp
-load`, and runs Alacritty with `LIBGL_ALWAYS_SOFTWARE=1`, a private `HOME`, a
config of `dimensions = 100x40`, `padding = 0`, the family and the size, and
reads its window size with `xwininfo` and the `Cell size` its `-v` log prints.
Cell = window / grid. The pane's cells for the same inputs came from
`Font::load`. A private `FONTCONFIG_FILE` that includes the system
configuration and assigns `hintstyle` gave the other two hinting modes.

Two pitfalls cost a run each and are worth knowing:

- an Xvfb **resets when its last client disconnects**, and forgets
  `RESOURCE_MANAGER`: `xrdb -merge` on a server nothing else holds open sets a
  property nobody will read. `-noreset`, or a client that stays connected (the
  test harness), fixes it;
- with no `RESOURCE_MANAGER`, x11rb's resource database (which winit uses)
  **falls back to `~/.Xresources`**, so Alacritty on a bare Xvfb picked up the
  user's 192 dpi. Every measurement runs with a private `HOME`.

## Data

Four families as fontconfig resolves them here: "SauceCodePro Nerd Font
Mono", `monospace` (Noto Mono), "DejaVu Sans Mono", "Liberation Mono".

- `hintslight` (this machine's default): 96, 120, 144, 168 and 192 dpi ×
  8, 9, 10, 10.5, 11, 11.25, 12, 13, 14 and 16.5 pt = 200 cells.
- `hintfull` and `hintnone`: 8 sizes with fractional pixel sizes each = 64.
- half-pixel sizes (11.625 pt at 96 dpi is 15.5 px, which FreeType's integer
  ppem rounds up), `hintfull` and `hintslight`: 40.

## Results

**All 304 Alacritty cells equal the pane's**, width and height, with no
tolerance.

The user's settings, before and after:

| font, size | dpi | Alacritty | pane before (16 px) | pane after |
|---|---|---|---|---|
| SauceCodePro Nerd Font Mono, 12 pt | 96 | 10x21 | 10x21 | 10x21 |
| SauceCodePro Nerd Font Mono, 12 pt | 192 | 19x41 | 10x21 | 19x41 |
| monospace, 12 pt | 96 | 10x19 | 10x19 | 10x19 |
| monospace, 12 pt | 192 | 19x38 | 10x19 | 19x38 |
| monospace, 11.25 pt (default) | 192 | 18x36 | — | 18x36 |
| monospace, 11.25 pt (default) | 144 | 14x27 | — | 14x27 |

The last four rows are also what `tests/pane_hidpi.rs` measures on its own
Xvfb from a live Alacritty's window.

The hinting modes matter, and differ where the pixel size is fractional:

| font | size, dpi | px | `hintnone` | `hintslight` | `hintfull` |
|---|---|---|---|---|---|
| SauceCodePro | 12 pt, 96 | 16 | 9x21 | 10x21 | 10x21 |
| Noto Mono | 12 pt, 96 | 16 | 9x19 | 10x19 | 10x19 |
| Noto Mono | 11.625 pt, 96 | 15.5 | — | 9x19 | 10x19 |
| SauceCodePro | 11.625 pt, 96 | 15.5 | — | 9x21 | 9x21 |

Without hinting the advance keeps its fraction (9.6 px) and the cell is its
floor. Full hinting of Noto Mono (`head.flags` bit 3 set, hinting bytecode
present) scales the advance at a 16-pixel em; SauceCodePro does not set that
bit, so full and slight agree for it.

Screenshots at 192 dpi, SauceCodePro 12 pt, 60x8 cells: the pane before
(600x168 pixels), the pane after (1140x328) and Alacritty (1140x328) show the
same glyph sizes and positions. They are not committed. `tests/pane_hidpi.rs`
writes its own pair per resolution into `SPOKENPAD_PANE_SCREENSHOTS`.

**A second finding, on the way.** `shell::pane::font`'s unit tests failed once
in a full test run under load. `fc-match` was killed after the drawing path's
250 ms bound, including for the four faces loaded when a pane opens, which is
not the drawing path:

| machine | `fc-match` mean | max | font tests failing |
|---|---|---|---|
| idle-ish (load 11, other builds running) | 17 ms | 21 ms | 0 of 15 runs |
| 96 busy-loop processes on 12 cores (load 26–35) | 280 ms | 354 ms | 6 of 6 runs, before |
| same | — | — | 0 of 6 runs, after |

## Conclusion

The pane now takes `nvim.font_size` in points, reads `Xft.dpi` from the root
window when it opens, and measures its cells exactly as Alacritty 0.17 does;
the default is Alacritty's 11.25 pt. The decorations follow Alacritty too:
baseline, underline and strikeout positions, the unfocused cursor's outline
thickness, and underline patterns that scale with the underline. No test
noticed before because every pane test ran on a server with no `Xft.dpi`,
where 16 px happens to be what 12 pt is; `tests/pane_hidpi.rs` now sets the
resource and compares with Alacritty itself.

The faces a pane loads when it opens may now take 5 s per `fc-match`; the
250 ms bound stays on the drawing path's fallback lookups.

Not matched: winit also reads XSETTINGS' `Xft/DPI` before the resource, and
falls back to RandR's physical size when neither is set; the pane reads only
the `Xft.dpi` resource and falls back to 96. On a desktop that sets the
resolution through an XSETTINGS daemon alone the two can still differ.

Decision: [decisions.md](../decisions.md#the-panes-font-size-is-in-points-measured-as-alacritty-measures-2026-09-22).
