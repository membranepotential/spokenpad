# Inline preview layout and the lost scroll

_2026-09-23, at d94581f, Neovim 0.12.5 headless (`--clean`, and the bundled
init where noted)._

## Question

The user asked for the live preview to be drawn exactly where its text lands,
and reported a preview the window did not scroll to. Before building the
preview as inline virtual text, which nvim behaviours does that rely on, and
why did the window stop following?

## Method

Scratch Lua scripts, not kept, run as `nvim --headless --clean -c 'luafile
…'` against windows 20 to 72 columns wide: they set lines and extmarks, call
`redraw!`, and read the screen back with `screenstring()`, the view with
`winsaveview()`, and heights with `nvim_win_text_height()`,
`nvim_win_get_height()` and `winheight()`.

For the scroll failure, a replay script loaded the bundled init and
`src/lua/spokenpad.lua` in a headless nvim, called `Spokenpad.setup`, and
dictated six paragraphs of three chunks each: every chunk a growing preview
over four or five ticks, then its append. After every step it checked that
the newest word was on the screen, and whether the redraw had changed the view
the preview set. Variants: 72×21 and 40×11 windows, longer previews and
commits, and a resize every fourth step (`lines` ±3). The same replay, plus a
check that the screen is the same before and after each append of the text
the preview showed, ran against the new code.

## Data

Synthetic words only (`u1c2w3`, `mark57`, and the like); no recordings.

## Results

nvim behaviour:

| Probe | Result |
|---|---|
| Inline virtual text after the last character, `wrap` on | wraps with the line; `nvim_win_text_height` counts its rows |
| Same, `linebreak` on | wraps per cell: `linebreak` does not apply to virtual text |
| `linebreak` in buffer text | a row ends at a `breakat` character when the next word *and the blanks after it* do not fit; at the end of the line the word alone must fit |
| `virtcol()` | includes the cells `linebreak` pads a row with |
| Double-width character starting in a row's last cell | moved to the next row after a `>` filler, in buffer text and inline virtual text alike |
| `strdisplaywidth(char, col)` for such a character | 3: it adds the filler cell, measured against the current window |
| `nvim_win_get_height()` with a winbar | counts the winbar: 20, where `winheight()` is 19 |
| Buffer line whose last word ends in the last column, text appended after it | `linebreak` moves that word to the next row |

Replay, steps whose newest word was off the screen (of 108 steps; 162 in the
longer run at 72×21):

| Variant | Before | After |
|---|---|---|
| 72×21, default lengths | 0 | 0 |
| 72×21, resize every fourth step | 48 | 0 |
| 72×21, longer previews and commits | 0 | 0 |
| 40×11, longer previews and commits | 0 | 0 |
| 72×21, resize, longer previews and commits | 58 | 0 |

In the resize runs the loss began where a grow made nvim reset `skipcol`
(144 → 0) under an unchanged cursor; from then on the preview took the view
for a reader who had scrolled away. With the preview inline and the view
measured with `nvim_win_get_height()`, nvim scrolled one row at every redraw
where the text exactly filled the window, with the same effect; the old code
had hidden this by leaving a row free below its virtual lines.

The before/after screen check of the new code differed in 5 of 18 appends in
two variants: at 40×11 the preview was taller than the window and gave up its
oldest words, and at 72 columns the last committed word ended in the last
column and moved to the next row when the text landed.

## Conclusion

Inline virtual text can stand exactly where its text will land if the preview
writes out the `linebreak` wraps itself and counts the filler cell of a
double-width character; nvim does the rest of the drawing. The lost scroll was
the preview's own follow test: a resize, like a view nvim corrects at the next
redraw, read as the reader moving away. It is fixed by counting only the
cursor across a resize, and by measuring the window with `winheight()`. Taken
into [decisions.md](../decisions.md#the-preview-is-drawn-where-its-text-lands-2026-09-23).
