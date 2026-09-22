-- The nvim half of spokenpad: the dictation buffer and its indicator.
--
-- Loaded once per connection by Rust's `NvimSession` via `nvim_exec_lua`, so
-- it is re-applied automatically whenever the daemon reattaches to a running
-- nvim. Everything it defines hangs off `_G.Spokenpad`; the Rust side calls
-- `Spokenpad.setup`, transactional `Spokenpad.append_once`, and
-- `Spokenpad.push`.
--
-- Reloading must not lose what the running editor already holds, so the first
-- line keeps the previous module and the block below carries its state over:
-- the pinned buffer, the indicator state, the meter history, and the append
-- de-duplication cache. That is why this file is a chunk evaluated over RPC
-- rather than a `require`d module.
--
-- Two things live here rather than in the daemon, for the same reason: they are
-- per-frame work that would otherwise be a round trip.
--
--  * the **level meter** keeps its own rolling history, so the daemon sends
--    one float per update instead of the whole window; and
--  * the **preview** is an extmark's virtual text, never buffer content, so
--    "a preview is never committed" holds *physically* -- virtual text
--    cannot be written to the file, yanked, or undone into the buffer. See
--    docs/constraints.md, "One-shot committed decode".

local previous = _G.Spokenpad

local M = {}

local BARS = { "\u{2581}", "\u{2582}", "\u{2583}", "\u{2584}", "\u{2585}", "\u{2586}", "\u{2587}", "\u{2588}" }
local METER_CELLS = 24
local PREVIEW_EXTMARK = 1
local PREVIEW_MAX_LINES = 8
local PREVIEW_GUTTER = 2  -- breathing room so wrapped text never touches the edge
-- Only for a winbar built before any window shows the buffer; every real one
-- is measured against the window it is about to be set on.
local DEFAULT_WINBAR_WIDTH = 80
-- How long after the last change in Insert mode the dictation buffer is
-- written. Every write fsyncs ('fsync' is on by default), and on slow
-- storage a write per keystroke would stall Neovim's main loop -- the
-- daemon's appends included. Leaving Insert mode writes at once.
local INSERT_SAVE_DELAY_MS = 300

-- Speech barely moves a linear meter, so the same perceptual curve the Qt
-- overlay used (level ^ 0.6) is applied before picking a bar glyph.
local METER_GAMMA = 0.6
local AUDIBLE = 0.02

-- `notice` is the headline the winbar always draws; `notice_detail` is the
-- sentence behind it, drawn only when the window has room for all of it. The
-- daemon sends them as two fields so that neither side has to split a sentence
-- the other composed.
M.state = {
  phase = "idle",
  level = 0.0,
  preview = "",
  notice = "",
  notice_detail = "",
  latched = false,
  previewing = true,
}
M.levels = {}
M.preview_views = {}
M.winbar_windows = {}
M.buf = nil
M.dedicated = false
M.ns = vim.api.nvim_create_namespace("spokenpad")

for _ = 1, METER_CELLS do
  M.levels[#M.levels + 1] = 0.0
end

-- Carry the running editor's state across a reload. A daemon restart must not
-- blank the indicator, forget which buffer is pinned, or replay an append
-- whose reply was lost.
if previous then
  if previous.buf and vim.api.nvim_buf_is_valid(previous.buf) and vim.api.nvim_buf_is_loaded(previous.buf) then
    M.buf = previous.buf
  end
  -- Field-wise, so state carried over from an older daemon's chunk keeps this
  -- one's default for any field that version did not have.
  M.state = vim.tbl_extend("force", M.state, previous.state or {})
  M.levels = previous.levels or M.levels
  M.preview_views = previous.preview_views or M.preview_views
  M.winbar_windows = previous.winbar_windows or M.winbar_windows
  M.dedicated = previous.dedicated or false
  M.last_append_id = previous.last_append_id
  M.last_append_buf = previous.last_append_buf
  M.last_append_result = previous.last_append_result
  -- The one timer for Insert-mode writes; a reload restarts it with this
  -- chunk's callback the next time a change arrives.
  M.save_timer = previous.save_timer
end

--- Highlight groups, linked to the colourscheme rather than hard-coded, so the
--- indicator follows whatever theme the user's own config loaded.
---
--- The preview and the notice are the exceptions, each wanting something a
--- link cannot express: the preview, the active colourscheme's Comment
--- foreground *with italics* -- the clear grey the Qt overlay used, without
--- hard-coding a colour that fights the theme -- and the notice, the theme's
--- warning foreground with a fallback, whichever of `WarningMsg` and
--- `DiagnosticWarn` the colourscheme actually sets.
local function define_highlights()
  local links = {
    SpokenpadRec = "DiagnosticError",
    SpokenpadWork = "DiagnosticInfo",
    SpokenpadIdle = "Comment",
    SpokenpadMuted = "NonText",
    SpokenpadPreview = "Comment",
    SpokenpadNotice = "WarningMsg",
    SpokenpadFile = "Directory",
  }
  for name, target in pairs(links) do
    vim.api.nvim_set_hl(0, name, { link = target, default = true })
  end
  local preview = vim.api.nvim_get_hl(0, { name = "Comment", link = false })
  preview.italic = true
  vim.api.nvim_set_hl(0, "SpokenpadPreview", preview)
  -- A notice is a warning about the capture just made, so it takes the theme's
  -- warning colour: `WarningMsg`, or `DiagnosticWarn` in a colourscheme that
  -- leaves it unset. If neither resolves to a colour the default link above
  -- stands, which is still better than a group set to nothing.
  local notice = vim.api.nvim_get_hl(0, { name = "WarningMsg", link = false })
  if not notice.fg then
    notice = vim.api.nvim_get_hl(0, { name = "DiagnosticWarn", link = false })
  end
  if notice.fg then
    vim.api.nvim_set_hl(0, "SpokenpadNotice", notice)
  end
end

--- The newest `cells` levels as bar glyphs, one column each. The meter is the
--- one part of the winbar that shrinks: a window too narrow to hold the phase,
--- a notice and twenty-four cells gives the cells up, because they are the
--- only part that says nothing the user cannot see again a tenth of a second
--- later.
local function meter(cells)
  local out = {}
  for index = #M.levels - cells + 1, #M.levels do
    local amp = M.levels[index] ^ METER_GAMMA
    local bar = math.max(1, math.min(#BARS, math.floor(amp * #BARS + 0.5)))
    local group = amp > AUDIBLE and "SpokenpadRec" or "SpokenpadMuted"
    out[#out + 1] = "%#" .. group .. "#" .. BARS[bar]
  end
  return table.concat(out)
end

--- A winbar is a statusline expression, so any `%` in text that came from
--- outside it -- a file name, a notice naming one -- has to be doubled, or
--- nvim reads it as an item and draws something else entirely.
local function escape(text)
  return (text:gsub("%%", "%%%%"))
end

local function highlighted(group, text)
  return "%#" .. group .. "#" .. escape(text) .. "%*"
end

--- The winbar string for `win`, rebuilt on every state push.
---
--- Two things are drawn at any width: the phase label, and -- when there is
--- one -- the notice's **headline**. Everything else is fitted around them.
--- A dictation window is small and a notice is a sentence, so at 40 columns
--- the phase, a 24-cell meter and the whole sentence cannot all be there; what
--- used to happen is that the winbar was truncated from the left and the user
--- was left with `<aining audio is in /tmp/capture.wav -- recover ...`, having
--- lost both the phase and the first half of the reason.
---
--- So the daemon sends the notice in two pieces. The headline is always drawn.
--- The **detail** is appended only when what is already in the bar leaves room
--- for all of it: half a sentence ending mid-word is worse than no sentence.
--- `%<` sits between the two as the backstop -- if the measurement is ever
--- wrong, a window resized between this push and the redraw, nvim truncates
--- the detail and never the phase or the headline.
---
--- What is drawn in every phase, and why it is the winbar rather than the
--- preview: a tap too short to record and a capture whose notice outlives its
--- decode are both idle by the time the user can read anything, and the
--- preview is the live tail -- a notice standing in its place cannot be told
--- apart from dictated text.
local function winbar(win)
  local width = win and vim.api.nvim_win_get_width(win) or DEFAULT_WINBAR_WIDTH
  local notice, detail = M.state.notice, M.state.notice_detail
  local headline = notice ~= "" and (" \u{26a0} " .. notice) or ""
  local bar = ""
  -- The headline is appended last but reserved first: what it costs is what
  -- the rest of the bar may not spend.
  local used = vim.fn.strdisplaywidth(headline)
  local function add(group, text)
    bar = bar .. highlighted(group, text)
    used = used + vim.fn.strdisplaywidth(text)
  end
  local function add_if_it_fits(group, text)
    if used + vim.fn.strdisplaywidth(text) <= width then
      add(group, text)
    end
  end

  local phase = M.state.phase
  if phase == "recording" then
    -- A latched recording says so, because the way to stop it is different:
    -- the key has already been released, and pressing it again is what ends
    -- it. Someone who cannot tell which mode they are in is stuck.
    add("SpokenpadRec", M.state.latched and "\u{25cf} REC \u{1f512} press to stop " or "\u{25cf} REC ")
    local note = ""
    if not M.state.previewing then
      -- No preview is coming: either none ever was (previews turned off, or a
      -- daemon with no VAD model, where nothing settles and a preview would
      -- have to re-decode the whole capture), or they stopped for a long
      -- uncommitted tail or the memory cap. One flag cannot tell those apart,
      -- and the one that matters is the same either way: the recording is
      -- running and only the live text is missing. Saying so is the difference
      -- between "it just stopped showing me" and "it has silently dropped what
      -- I am saying" -- which is why both phrasings keep "recording".
      note = " no live preview, still recording"
      -- Dropped only when it does not fit beside a notice -- and a notice is
      -- then the more specific reason the preview is missing (the memory cap,
      -- the paused preview), so nothing is lost by preferring it.
      if used + vim.fn.strdisplaywidth(note) > width then
        note = ""
      end
    end
    local room = width - used - vim.fn.strdisplaywidth(note)
    local cells = math.max(0, math.min(METER_CELLS, room))
    bar = bar .. meter(cells)
    used = used + cells
    if note ~= "" then
      add("SpokenpadIdle", note)
    end
  elseif phase == "transcribing" then
    -- Say the text below is the preview, not the result. Otherwise a preview
    -- left standing through the decode reads as finished text that then
    -- changes under the reader.
    add("SpokenpadWork", "\u{25cf} transcribing\u{2026}")
    if M.state.preview ~= "" then
      add_if_it_fits("SpokenpadIdle", " showing the live preview until it lands")
    end
  else
    local name = M.buf and vim.api.nvim_buf_get_name(M.buf) or ""
    add("SpokenpadIdle", "\u{25cb} spokenpad")
    add_if_it_fits("SpokenpadFile", " " .. vim.fn.fnamemodify(name, ":t"))
  end

  if notice ~= "" then
    bar = bar .. "%#SpokenpadNotice#" .. escape(headline) .. "%<"
    -- " -- " is three columns; the detail is all or nothing.
    if detail ~= "" and used + 3 + vim.fn.strdisplaywidth(detail) <= width then
      bar = bar .. " \u{2014} " .. escape(detail)
    end
    bar = bar .. "%*"
  end
  return bar
end

--- Windows currently displaying the dictation buffer. Usually exactly one;
--- a split or a second tab is the user's business, and both get the winbar.
local function windows()
  local out = {}
  if not M.buf or not vim.api.nvim_buf_is_valid(M.buf) then
    return out
  end
  for _, win in ipairs(vim.api.nvim_list_wins()) do
    if vim.api.nvim_win_is_valid(win) and vim.api.nvim_win_get_buf(win) == M.buf then
      out[#out + 1] = win
    end
  end
  return out
end

--- The winbar is set window-locally on every window showing the dictation
--- buffer, so a global winbar from the user's own config (lualine,
--- breadcrumbs) is overridden for this buffer only and left alone everywhere
--- else. It is built per window, because the two may differ in width and the
--- bar is fitted to the window it goes on.
local function render()
  local shown = {}
  for _, win in ipairs(windows()) do
    vim.api.nvim_set_option_value("winbar", winbar(win), { win = win })
    shown[win] = true
  end
  -- A window that stops showing the dictation buffer -- `:e other.md` in it --
  -- would otherwise keep a frozen "REC" winbar over someone else's file. An
  -- empty window-local value is how nvim says "use the global one", so this
  -- hands the window back to whatever the user's own config put there.
  for win in pairs(M.winbar_windows) do
    if not shown[win] and vim.api.nvim_win_is_valid(win) then
      pcall(vim.api.nvim_set_option_value, "winbar", "", { win = win })
    end
  end
  M.winbar_windows = shown
end

--- Word-wrap `text` to `width` display columns, keeping the last `max_lines`.
---
--- Virtual text does **not** wrap -- nvim truncates a `virt_lines` chunk at
--- the window edge -- so a preview of a long passage showed only its first
--- line and looked like it had stopped updating. Wrapping has to be done
--- here, by hand.
---
--- Measured in display columns via `strdisplaywidth`, not bytes: dictation is
--- routinely German here, and counting bytes would wrap "längeren" several
--- columns early and a CJK character several columns late.
---
--- The *tail* is kept when there is too much, for the same reason the meter
--- shows recent levels: the newest words are the ones being checked against
--- what was just said. Since progressive commit (docs/progressive-commit.md)
--- the preview is only the open tail -- at most one chunk -- so this rarely
--- has anything to cut; everything before it is already in the buffer above.
local function wrap(text, width, max_lines)
  if width < 8 then
    return { text }
  end
  local lines, current = {}, ""
  for word in text:gmatch("%S+") do
    local candidate = current == "" and word or (current .. " " .. word)
    if vim.fn.strdisplaywidth(candidate) <= width then
      current = candidate
    else
      if current ~= "" then
        lines[#lines + 1] = current
      end
      -- A single word wider than the window is left long rather than split
      -- mid-word; nvim truncates it, which is the lesser evil for a preview.
      current = word
    end
  end
  if current ~= "" then
    lines[#lines + 1] = current
  end
  if #lines <= max_lines then
    return lines
  end
  local tail = {}
  for i = #lines - max_lines + 1, #lines do
    tail[#tail + 1] = lines[i]
  end
  tail[1] = "\u{2026} " .. tail[1]
  return tail
end

--- Index of the last line with text on it, ignoring trailing blanks; 0 when
--- the buffer holds nothing. Both the preview and the append use this, and
--- they must agree about which paragraph the provisional text follows.
local function last_text_line(lines)
  local n = #lines
  while n > 0 and lines[n]:match("^%s*$") do
    n = n - 1
  end
  return n
end

--- The parts of a view that change when a person moves or scrolls it. Keeping
--- this snapshot lets preview updates follow a passive reader without pulling
--- the window back down after they deliberately moved away.
local function view_signature(win)
  local view
  vim.api.nvim_win_call(win, function()
    view = vim.fn.winsaveview()
  end)
  return {
    lnum = view.lnum,
    col = view.col,
    topline = view.topline,
    topfill = view.topfill,
    leftcol = view.leftcol,
    skipcol = view.skipcol,
  }
end

local function same_view(left, right)
  return left.lnum == right.lnum
    and left.col == right.col
    and left.topline == right.topline
    and left.topfill == right.topfill
    and left.leftcol == right.leftcol
    and left.skipcol == right.skipcol
end

local function text_end_is_visible(win, lines, last_text)
  local row = math.max(last_text, 1)
  local cursor = vim.api.nvim_win_get_cursor(win)
  if cursor[1] ~= row then
    return false
  end
  local text = lines[row] or ""
  return vim.fn.screenpos(win, row, #text + 1).row > 0
end

local function follows_preview(win, lines, last_text)
  local prior = M.preview_views[win]
  if prior == false then
    return false
  elseif prior then
    return same_view(prior, view_signature(win))
  end
  return text_end_is_visible(win, lines, last_text)
end

--- Scroll `win` so the end of the text, and the `rows_below` preview rows
--- hanging under it, end at the bottom of the window.
---
--- Neovim will not scroll past EOF merely because virtual lines hang below
--- it: `zb`, `zz` and CTRL-E all stop with the real final row at the bottom,
--- and `nvim_win_text_height()` does not count virtual lines below the last
--- buffer line. So the view is computed here: walk up from the last line until
--- the text and the preview fill the window, then let smoothscroll's `skipcol`
--- hide the surplus rows of that top line.
---
--- The cursor goes to the end of the text for a reader, and stays where it is
--- for someone typing in this window: in Insert or Replace mode it is where
--- the next key lands, and moving it there onto the last character put what
--- they typed into the middle of the last dictated word.
local function position_at_end(win, last_text, rows_below)
  local row = math.max(last_text, 1)
  local typing = win == vim.api.nvim_get_current_win()
    and vim.api.nvim_get_mode().mode:match("^[iR]") ~= nil
  local cursor = vim.api.nvim_win_get_cursor(win)
  vim.api.nvim_win_call(win, function()
    -- Let normal-mode `$` perform the wrapped-line scroll. Setting the byte
    -- column directly is clamped to the final currently visible screen row
    -- when one buffer line is taller than the window, even with smoothscroll.
    vim.api.nvim_win_set_cursor(win, { row, 0 })
    vim.cmd("normal! $")
    -- Leave one real screen row below a preview. Neovim reserves the final
    -- window row at EOF instead of drawing the last virtual line in it, even
    -- though the arithmetic says it fits.
    local below = rows_below > 0 and rows_below + 1 or 0
    local height = vim.api.nvim_win_get_height(win)
    local function rows_from(top)
      return vim.api.nvim_win_text_height(win, { start_row = top - 1, end_row = row - 1 }).all
        + below
    end
    local top = row
    while top > 1 and rows_from(top) < height do
      top = top - 1
    end
    local view = vim.fn.winsaveview()
    view.topline = top
    view.topfill = 0
    view.skipcol = math.max(0, rows_from(top) - height) * vim.api.nvim_win_get_width(win)
    vim.fn.winrestview(view)
    if typing then
      vim.api.nvim_win_set_cursor(win, cursor)
    end
  end)
end

--- The preview hangs directly below the transcript with no virtual spacer, so
--- the user reads it in place rather than in a second widget. The real blank
--- line between utterances is added only when a new paragraph commits.
local function render_preview()
  if not M.buf or not vim.api.nvim_buf_is_valid(M.buf) then
    return
  end
  local text = M.state.preview
  -- Held through `transcribing`, not just `recording`. The decode takes a
  -- second or more on a long passage, and clearing the preview at the key
  -- release blanked the screen for exactly as long as the user had to wait --
  -- so the one moment they most want to start reading was the one moment
  -- there was nothing to read. It is replaced when the real text lands (the
  -- append clears it), which is the only correct moment to drop it.
  if text == "" or (M.state.phase ~= "recording" and M.state.phase ~= "transcribing") then
    pcall(vim.api.nvim_buf_del_extmark, M.buf, M.ns, PREVIEW_EXTMARK)
    M.preview_views = {}
    return
  end

  local wins = windows()
  local width, height = 80, PREVIEW_MAX_LINES * 2 + 1
  if wins[1] then
    width = vim.api.nvim_win_get_width(wins[1])
    height = vim.api.nvim_win_get_height(wins[1])
  end
  local lines = vim.api.nvim_buf_get_lines(M.buf, 0, -1, false)
  local last_text = last_text_line(lines)
  local followers = {}
  for _, win in ipairs(wins) do
    followers[win] = follows_preview(win, lines, last_text)
  end
  pcall(vim.api.nvim_buf_del_extmark, M.buf, M.ns, PREVIEW_EXTMARK)

  -- Bound the preview to at most half the window so both its context and its
  -- newest word remain visible after one very long wrapped paragraph.
  local max_lines = math.max(1, math.min(PREVIEW_MAX_LINES, math.floor((height - 1) / 2)))
  local wrapped = wrap(text, width - PREVIEW_GUTTER, max_lines)
  local virt_lines = {}
  local first_virtual = last_text == 0 and 2 or 1
  for index = first_virtual, #wrapped do
    local line = wrapped[index]
    virt_lines[#virt_lines + 1] = { { line, "SpokenpadPreview" } }
  end

  -- On an empty buffer a virtual line above line 1 is clipped by a viewport
  -- whose topline is already line 1. Put the first preview row over the empty
  -- buffer line instead; it is still an extmark decoration and therefore can
  -- never enter the file, a yank, or undo history. Remaining rows hang below.
  local anchor = math.max(last_text - 1, 0)
  local options = {
    id = PREVIEW_EXTMARK,
    virt_lines = virt_lines,
    virt_lines_above = false,
  }
  if last_text == 0 then
    options.virt_text = { { wrapped[1], "SpokenpadPreview" } }
    options.virt_text_pos = "overlay"
  end
  pcall(vim.api.nvim_buf_set_extmark, M.buf, M.ns, anchor, 0, options)

  M.preview_views = {}
  for _, win in ipairs(wins) do
    if followers[win] then
      position_at_end(win, last_text, last_text == 0 and 0 or #wrapped)
      M.preview_views[win] = view_signature(win)
    else
      -- A deliberate move remains sticky for the rest of this preview. The
      -- cursor can still be on the final buffer line after a screen-line
      -- scroll, so re-detecting from that alone would snap back next tick.
      M.preview_views[win] = false
    end
  end
end

--- Strip the editor down to a reading surface.
---
--- This nvim exists to hold dictated prose while the user reads something
--- else, so the gutters, statusline and tabline a general-purpose config
--- brings are noise around a small floating window. The winbar indicator is
--- the one piece of chrome that earns its space, which is also why
--- `laststatus` goes to 0: the statusline would otherwise sit below the text
--- saying nothing the winbar does not already say.
---
--- The window-local half applies wherever the dictation buffer is shown. The
--- *global* half only applies in an editor spokenpad opened for the purpose,
--- because there it is the whole instance and nothing else is in it; in an
--- adopted editor those globals are the user's own and are left alone.
local function apply_chrome()
  if M.dedicated then
    vim.opt.laststatus = 0
    vim.opt.showtabline = 0
    vim.opt.ruler = false
  end
  for _, win in ipairs(windows()) do
    local opts = { win = win }
    -- Each utterance is one long line; without wrap it runs off the right
    -- edge and the user cannot read what they just dictated.
    vim.api.nvim_set_option_value("wrap", true, opts)
    vim.api.nvim_set_option_value("linebreak", true, opts)
    vim.api.nvim_set_option_value("smoothscroll", true, opts)
    -- `position_at_end` scrolls the text's last line up to make room for the
    -- preview hanging below it; a `scrolloff` would scroll it straight back.
    vim.api.nvim_set_option_value("scrolloff", 0, opts)
    vim.api.nvim_set_option_value("number", false, opts)
    vim.api.nvim_set_option_value("relativenumber", false, opts)
    vim.api.nvim_set_option_value("cursorline", false, opts)
    vim.api.nvim_set_option_value("signcolumn", "no", opts)
    vim.api.nvim_set_option_value("foldcolumn", "0", opts)
    vim.api.nvim_set_option_value("colorcolumn", "", opts)
    vim.api.nvim_set_option_value("list", false, opts)
  end
end

--- Write what the user changed in the dictation buffer, the way an append is
--- written: `noautocmd`, so a format-on-save in the user's config cannot
--- reflow a transcript, and `lockmarks`, so the write leaves the `'[` and `']`
--- marks where the user's last change put them. Only the pinned buffer, and
--- only when it has unsaved changes: an append has already written its own.
--- A write that fails leaves the buffer modified and says nothing -- a
--- message here could raise a prompt, which would hold every call the daemon
--- sends -- and the next change, the next append or closing the pane tries
--- again.
local function save(buf)
  if buf ~= M.buf
    or not vim.api.nvim_buf_is_loaded(buf)
    or not vim.bo[buf].modified
    or vim.bo[buf].buftype ~= ""
    or vim.api.nvim_buf_get_name(buf) == ""
  then
    return
  end
  pcall(vim.api.nvim_buf_call, buf, function()
    vim.cmd("silent lockmarks noautocmd write")
  end)
end

--- `save`, once Insert mode has been quiet for `INSERT_SAVE_DELAY_MS`: each
--- change restarts the wait.
local function save_soon(buf)
  if buf ~= M.buf then
    return
  end
  M.save_timer = M.save_timer or vim.uv.new_timer()
  M.save_timer:stop()
  M.save_timer:start(INSERT_SAVE_DELAY_MS, 0, vim.schedule_wrap(function()
    if M.buf then
      save(M.buf)
    end
  end))
end

--- Bind to the dictation buffer. Idempotent: called on every (re)connection.
---
--- `dedicated` says whether this editor was opened by spokenpad for dictation
--- and nothing else. It is remembered across reloads, so a daemon restart
--- that reattaches to the window it opened last time keeps its chrome.
function M.setup(buf, dedicated)
  M.buf = buf
  M.dedicated = M.dedicated or dedicated == true
  -- Before the markdown FileType autocmds can lint the empty backing buffer,
  -- and again after, since a plugin may enable diagnostics from FileType. The
  -- buffer filter keeps every other buffer in the user's editor untouched.
  vim.diagnostic.enable(false, { bufnr = buf })
  define_highlights()
  vim.api.nvim_set_option_value("filetype", "markdown", { buf = buf })
  vim.diagnostic.enable(false, { bufnr = buf })
  -- Leaving it hides it, whatever 'hidden' says: with `set nohidden`,
  -- `:edit other` on a change whose write has not run yet would stop at E37
  -- before BufLeave could write it.
  vim.api.nvim_set_option_value("bufhidden", "hide", { buf = buf })
  apply_chrome()
  -- A colourscheme change clears `default = true` links, and a window can
  -- start showing this buffer at any time, so both re-assert the indicator
  -- rather than leaving a blank winbar until the next dictation. The chrome
  -- is re-applied on the same events because a plugin that reacts to
  -- BufWinEnter (a statusline, a gutter) would otherwise put itself back.
  local group = vim.api.nvim_create_augroup("Spokenpad", { clear = true })
  vim.api.nvim_create_autocmd({ "BufWinEnter", "WinNew", "FileType" }, {
    group = group,
    buffer = buf,
    callback = function()
      apply_chrome()
      render()
    end,
  })
  vim.api.nvim_create_autocmd("ColorScheme", {
    group = group,
    callback = function()
      define_highlights()
      render()
    end,
  })
  -- The dictation file is a scratch pad the user never has to save: a
  -- change in Normal mode is written at once, one in Insert mode once the
  -- typing pauses, and leaving Insert mode writes at once too, so an editor
  -- that dies loses at most the last moment of typing.
  vim.api.nvim_create_autocmd({ "TextChanged", "InsertLeave", "BufLeave" }, {
    group = group,
    buffer = buf,
    callback = function(args)
      save(args.buf)
    end,
  })
  vim.api.nvim_create_autocmd({ "TextChangedI", "TextChangedP" }, {
    group = group,
    buffer = buf,
    callback = function(args)
      save_soon(args.buf)
    end,
  })
  -- TextChanged waits for typeahead, so `dd:q` typed in one go, or run by a
  -- mapping, reaches the `:quit` with the buffer still modified. QuitPre
  -- runs before `:quit`, `:wq` and `:qall` look at what is unsaved, from
  -- whichever buffer they are typed in, and VimLeavePre before any other
  -- way out; BufLeave above covers `:edit` and `:bnext`. That is also why
  -- 'autowriteall' is not set: it would write this buffer with the user's
  -- autocommands, format-on-save included.
  vim.api.nvim_create_autocmd({ "QuitPre", "VimLeavePre" }, {
    group = group,
    callback = function()
      if M.buf then
        save(M.buf)
      end
    end,
  })
  render()
  -- The preview belongs to the capture, not to the buffer it was last drawn
  -- in: a daemon that re-pins mid-recording pushes the same indicator state
  -- afterwards, and `set_state` redraws only what changed. Without this, the
  -- live preview would stay in the abandoned buffer until the next word.
  render_preview()
end

--- Split `text` into buffer lines on literal newlines, keeping empty ones.
--- `vim.split` would do this, but not the empty trailing line that a text
--- ending in "\n" must keep producing for `nvim_buf_set_lines`.
local function literal_lines(text)
  local lines = {}
  local start = 1
  while true do
    local newline = text:find("\n", start, true)
    if not newline then
      lines[#lines + 1] = text:sub(start)
      return lines
    end
    lines[#lines + 1] = text:sub(start, newline - 1)
    start = newline + 1
  end
end

--- Append committed text and save, or leave the buffer exactly as it was.
---
--- One utterance is delivered as several calls, one per speech segment the
--- VAD found, so that text starts landing while the rest is still decoding.
--- `continued` says which: false opens a new paragraph, true extends the one
--- the previous segment started. An utterance is therefore still exactly one
--- paragraph -- it just grows a piece at a time instead of arriving whole.
---
--- Returns the buffer's new line count, which is what the daemon logs -- a
--- request rather than a notification, so a failure to land text is visible
--- instead of silent. That is the whole point of this project.
local function transactional_append(text, continued)
  if not M.buf or not vim.api.nvim_buf_is_valid(M.buf) then
    error("spokenpad: dictation buffer is gone")
  end

  local old_lines = vim.api.nvim_buf_get_lines(M.buf, 0, -1, false)
  local old_modified = vim.bo[M.buf].modified
  -- Trailing blank lines are ignored when appending, so the gap between
  -- utterances is always exactly one blank line however the buffer got into
  -- its current shape -- an editing session in the window, a plugin adding a
  -- final newline. Without this the separation drifts and never recovers.
  local last_text = last_text_line(old_lines)
  local followers = {}
  for _, win in ipairs(windows()) do
    followers[win] = follows_preview(win, old_lines, last_text)
  end
  -- Paragraph separation, not a running wall of text: one blank line between
  -- utterances, and none at the very top of a fresh file. `continued` instead
  -- extends the last line, so the segments of one utterance read as the one
  -- sentence-stream they are.
  local addition = literal_lines(text)
  local from = last_text
  if continued and last_text > 0 then
    from = last_text - 1
    local separator = addition[1] == "" and "" or " "
    addition[1] = old_lines[last_text] .. separator .. addition[1]
  elseif last_text > 0 then
    table.insert(addition, 1, "")
  end

  vim.api.nvim_buf_set_lines(M.buf, from, -1, false, addition)
  -- `noautocmd`, deliberately. The dictation window runs a bundled config
  -- with nothing on BufWritePre, but `nvim.init` can point at any config, and
  -- a format-on-save that reflowed dictated prose would rewrite the
  -- transcript behind the user's back.
  local ok, write_error = pcall(vim.api.nvim_buf_call, M.buf, function()
    vim.cmd("silent noautocmd write")
  end)
  if not ok then
    vim.api.nvim_buf_set_lines(M.buf, 0, -1, false, old_lines)
    vim.bo[M.buf].modified = old_modified
    render_preview()
    error(write_error)
  end

  -- Follow the text only for a reader who was already at the end. Someone who
  -- scrolled up to re-read or edit an earlier passage keeps their place --
  -- yanking the cursor away mid-edit is exactly the kind of interruption this
  -- window exists to avoid.
  local last = vim.api.nvim_buf_line_count(M.buf)
  for _, win in ipairs(windows()) do
    if followers[win] then
      position_at_end(win, last, 0)
    end
  end
  M.state.preview = ""
  render_preview()
  return last
end

--- Append exactly once per operation id.
---
--- A reply lost to a timeout leaves the daemon unable to tell "not appended"
--- from "appended, reply lost", so it reconnects and repeats the same id. The
--- cached result answers the repeat instead of writing the text twice. It is
--- keyed on the pinned buffer as well: after a reconnection pinned a
--- different buffer, the cached line count describes a buffer this text never
--- reached, and replaying the append really is the right answer.
function M.append_once(id, text, continued)
  if M.last_append_id == id and M.last_append_buf == M.buf then
    return M.last_append_result
  end
  local result = transactional_append(text, continued)
  M.last_append_id = id
  M.last_append_buf = M.buf
  M.last_append_result = result
  return result
end

--- Push indicator state.
---
--- Any field may be omitted; only what is present changes. `level` is folded
--- into the rolling meter history rather than replacing it.
function M.set_state(update)
  local phase_changed = update.phase ~= nil and update.phase ~= M.state.phase
  local preview_before = M.state.preview
  if update.phase ~= nil then
    M.state.phase = update.phase
  end
  if phase_changed and M.state.phase == "recording" then
    -- Drop the previous utterance's preview the moment a new one starts.
    -- Now that a preview survives `transcribing`, one that was never replaced
    -- by committed text -- a decode that produced nothing, a cancellation --
    -- would otherwise still be sitting there when the user speaks again, and
    -- would read as the beginning of what they are saying now.
    --
    -- Before `update.preview` is applied, not after: a caller may set the
    -- phase and a preview in one call, and that preview is news, not stale.
    M.state.preview = ""
  end
  if update.preview ~= nil then
    M.state.preview = update.preview
  end
  -- The empty string is how the daemon says "no notice"; it sends one on every
  -- push, and clears it on the next key press. Nothing here expires it.
  if update.notice ~= nil then
    M.state.notice = update.notice
  end
  if update.notice_detail ~= nil then
    M.state.notice_detail = update.notice_detail
  end
  if update.latched ~= nil then
    M.state.latched = update.latched
  end
  if update.previewing ~= nil then
    M.state.previewing = update.previewing
  end
  if update.level ~= nil then
    M.state.level = update.level
    table.remove(M.levels, 1)
    M.levels[#M.levels + 1] = update.level
  end
  if phase_changed and M.state.phase ~= "recording" then
    -- Leave no half-lit meter behind when recording ends.
    for i = 1, #M.levels do
      M.levels[i] = 0.0
    end
  end
  render()
  -- Compared, not merely present: the daemon pushes the whole indicator state
  -- ten times a second, and rebuilding the preview extmark on every level
  -- sample would scroll the window under a reader for nothing.
  if phase_changed or M.state.preview ~= preview_before then
    render_preview()
  end
end

--- Notification entry point: pushes state and never raises.
---
--- The daemon sends indicator updates as notifications, so an error here has
--- no reply to travel back on -- it surfaces as a message in the editor, and
--- a message long enough to need a hit-enter prompt would sit unanswerable in
--- a window that cannot take focus. The failure is kept for inspection
--- instead.
function M.push(update)
  local ok, failure = pcall(M.set_state, update)
  if not ok then
    M.last_error = tostring(failure)
  end
end

--- Copies the whole dictation buffer -- every press in this window, and
--- anything the user typed by hand -- to the `+` register, through whatever
--- clipboard provider nvim's own `setreg` resolves (xclip/xsel under X11).
--- The daemon spawns no clipboard process itself; this is the one place
--- spokenpad's "never touch the clipboard" rule is deliberately lifted, and
--- only this far: nothing is pasted, and no other window is ever written to.
---
--- Trailing blank lines are stripped first, the same way `transactional_append`
--- ignores them, so the copy never carries a dangling blank past the last
--- utterance. A buffer with nothing left after that is left alone -- the
--- clipboard keeps whatever it already held.
---
--- Called over a *request*, not a notification (see `shell/nvim/mod.rs`), so
--- it must never raise: a missing clipboard provider would otherwise surface
--- as a message in a window that cannot take focus to answer it. Instead it
--- reports what happened as `{ status, detail }`, `status` one of `"copied"`,
--- `"empty"`, or `"error"` (with `detail` the failure message), for the
--- daemon to log.
function M.copy_buffer()
  if not M.buf or not vim.api.nvim_buf_is_valid(M.buf) then
    return { "error", "spokenpad: dictation buffer is gone" }
  end
  local lines = vim.api.nvim_buf_get_lines(M.buf, 0, -1, false)
  local last = last_text_line(lines)
  if last == 0 then
    return { "empty", vim.NIL }
  end
  -- `setreg('+')` without a provider prints "No provider" and still returns
  -- 0, so success from it only means the provider was handed the text.
  if vim.fn.has("clipboard") ~= 1 then
    return { "error", "no clipboard provider (see :checkhealth provider)" }
  end
  local text = table.concat(lines, "\n", 1, last)
  local ok, result = pcall(vim.fn.setreg, "+", text)
  if not ok then
    return { "error", tostring(result) }
  end
  if result ~= 0 then
    return { "error", "nvim setreg('+', ...) reported failure" }
  end
  return { "copied", vim.NIL }
end

_G.Spokenpad = M
return true
