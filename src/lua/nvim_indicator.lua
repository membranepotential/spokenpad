-- The nvim half of spokenpad: the dictation buffer and its indicator.
--
-- Loaded once per connection by Rust's `NvimSession` via
-- `nvim_exec_lua`, so it is re-applied automatically whenever the daemon
-- reattaches to a new nvim. Everything it defines hangs off `_G.Spokenpad`;
-- the Rust side calls `Spokenpad.setup`, transactional `Spokenpad.append_once`
-- (provided by nvim_rust.lua), and `Spokenpad.set_state`.
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

local M = {}

local BARS = { "\u{2581}", "\u{2582}", "\u{2583}", "\u{2584}", "\u{2585}", "\u{2586}", "\u{2587}", "\u{2588}" }
local METER_CELLS = 24
local PREVIEW_EXTMARK = 1
local PREVIEW_MAX_LINES = 8
local PREVIEW_GUTTER = 2  -- breathing room so wrapped text never touches the edge

-- Speech barely moves a linear meter, so the same perceptual curve the Qt
-- overlay used (level ^ 0.6) is applied before picking a bar glyph.
local METER_GAMMA = 0.6
local AUDIBLE = 0.02

M.state = { phase = "idle", level = 0.0, preview = "", latched = false, previewing = true }
M.levels = {}
M.preview_views = {}
M.buf = nil
M.ns = vim.api.nvim_create_namespace("spokenpad")

for _ = 1, METER_CELLS do
  M.levels[#M.levels + 1] = 0.0
end

--- Highlight groups, linked to the colourscheme rather than hard-coded, so the
--- indicator follows whatever theme the user's own config loaded.
local function define_highlights()
  local links = {
    SpokenpadRec = "DiagnosticError",
    SpokenpadWork = "DiagnosticInfo",
    SpokenpadIdle = "Comment",
    SpokenpadMuted = "NonText",
    SpokenpadPreview = "Comment",
    SpokenpadFile = "Directory",
  }
  for name, target in pairs(links) do
    vim.api.nvim_set_hl(0, name, { link = target, default = true })
  end
end

local function meter()
  local out = {}
  for _, level in ipairs(M.levels) do
    local amp = level ^ METER_GAMMA
    local index = math.max(1, math.min(#BARS, math.floor(amp * #BARS + 0.5)))
    local group = amp > AUDIBLE and "SpokenpadRec" or "SpokenpadMuted"
    out[#out + 1] = "%#" .. group .. "#" .. BARS[index]
  end
  return table.concat(out)
end

--- The winbar string. Rebuilt on every state push; `winbar` is set
--- window-locally on every window showing the dictation buffer, so a global
--- winbar from the user's own config (lualine, breadcrumbs) is overridden for
--- this buffer only and left alone everywhere else.
local function winbar()
  local phase = M.state.phase
  if phase == "recording" then
    -- A latched recording says so, because the way to stop it is different:
    -- the key has already been released, and pressing it again is what ends
    -- it. Someone who cannot tell which mode they are in is stuck.
    local label = M.state.latched and "\u{25cf} REC \u{1f512} press to stop " or "\u{25cf} REC "
    local bar = "%#SpokenpadRec#" .. label .. "%*" .. meter()
    if not M.state.previewing then
      -- Previews stop past the in-memory ceiling, or -- without a VAD model,
      -- where nothing settles and every preview decodes the whole capture --
      -- past preview.max_seconds. Saying so is the difference between "it is
      -- still recording, it just stopped showing me" and "it has silently
      -- dropped what I am saying".
      bar = bar .. "%#SpokenpadIdle# preview paused, still recording%*"
    end
    return bar
  elseif phase == "transcribing" then
    -- Say the text below is the preview, not the result. Otherwise a preview
    -- left standing through the decode reads as finished text that then
    -- changes under the reader.
    local bar = "%#SpokenpadWork#\u{25cf} transcribing\u{2026}%*"
    if M.state.preview ~= "" then
      bar = bar .. " %#SpokenpadIdle#showing the live preview until it lands%*"
    end
    return bar
  end
  local name = M.buf and vim.api.nvim_buf_get_name(M.buf) or ""
  return "%#SpokenpadIdle#\u{25cb} spokenpad%* %#SpokenpadFile#" .. vim.fn.fnamemodify(name, ":t") .. "%*"
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

local function render()
  local bar = winbar()
  for _, win in ipairs(windows()) do
    vim.api.nvim_set_option_value("winbar", bar, { win = win })
  end
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

local function position_at_end(win, last_text, center, rows_below)
  local row = math.max(last_text, 1)
  vim.api.nvim_win_call(win, function()
    -- Let normal-mode `$` perform the wrapped-line scroll. Setting the byte
    -- column directly is clamped to the final currently visible screen row
    -- when one buffer line is taller than the window, even with smoothscroll.
    vim.api.nvim_win_set_cursor(win, { row, 0 })
    vim.cmd("normal! $")
    vim.cmd(center and "normal! zz" or "normal! zb")
    if rows_below > 0 then
      -- Neovim will not scroll past EOF merely because virtual lines hang
      -- below it: both `zz` and CTRL-E stop with the real final row at the
      -- bottom. With smoothscroll, skip exactly enough wrapped display rows
      -- to make room for the bounded virtual tail.
      local view = vim.fn.winsaveview()
      local text_rows = vim.api.nvim_win_text_height(win, {
        start_row = row - 1,
        end_row = row - 1,
      }).all
      -- Leave one real screen row below the decoration. Neovim reserves the
      -- final window row at EOF instead of drawing the last virtual line in
      -- it, even though `nvim_win_text_height()` reports an exact fit.
      local skipped_rows = math.max(
        0,
        text_rows + rows_below - vim.api.nvim_win_get_height(win) + 1
      )
      view.skipcol = skipped_rows * vim.api.nvim_win_get_width(win)
      vim.fn.winrestview(view)
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
  -- there was nothing to read. It is replaced when the real text lands
  -- (`M.append` clears it), which is the only correct moment to drop it.
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
      position_at_end(win, last_text, true, last_text == 0 and 0 or #wrapped)
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
--- Globals are set as well as window-locals because this is a dedicated
--- instance, not the user's general editor -- their own nvim is untouched.
local function apply_chrome()
  vim.opt.laststatus = 0
  vim.opt.showtabline = 0
  vim.opt.ruler = false
  for _, win in ipairs(windows()) do
    local opts = { win = win }
    -- Each utterance is one long line; without wrap it runs off the right
    -- edge and the user cannot read what they just dictated.
    vim.api.nvim_set_option_value("wrap", true, opts)
    vim.api.nvim_set_option_value("linebreak", true, opts)
    vim.api.nvim_set_option_value("smoothscroll", true, opts)
    vim.api.nvim_set_option_value("number", false, opts)
    vim.api.nvim_set_option_value("relativenumber", false, opts)
    vim.api.nvim_set_option_value("cursorline", false, opts)
    vim.api.nvim_set_option_value("signcolumn", "no", opts)
    vim.api.nvim_set_option_value("foldcolumn", "0", opts)
    vim.api.nvim_set_option_value("colorcolumn", "", opts)
    vim.api.nvim_set_option_value("list", false, opts)
  end
end

--- Bind to the dictation buffer. Idempotent: called on every (re)connection.
function M.setup(buf)
  M.buf = buf
  define_highlights()
  vim.api.nvim_set_option_value("filetype", "markdown", { buf = buf })
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
  render()
end

--- Append committed text, then save.
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
function M.append(text, continued)
  if not M.buf or not vim.api.nvim_buf_is_valid(M.buf) then
    error("spokenpad: dictation buffer is gone")
  end
  local lines = vim.api.nvim_buf_get_lines(M.buf, 0, -1, false)

  -- Trailing blank lines are trimmed before appending, so the gap between
  -- utterances is always exactly one blank line however the buffer got into
  -- its current shape -- an editing session in the window, a plugin adding a
  -- final newline. Without this the separation drifts and never recovers.
  local last_text = last_text_line(lines)
  local followers = {}
  for _, win in ipairs(windows()) do
    followers[win] = follows_preview(win, lines, last_text)
  end

  -- Paragraph separation, not a running wall of text: one blank line between
  -- utterances, and none at the very top of a fresh file. `continued` instead
  -- extends the last line, so the segments of one utterance read as the one
  -- sentence-stream they are.
  local addition
  local from = last_text
  if continued and last_text > 0 then
    from = last_text - 1
    addition = { lines[last_text] .. " " .. text }
  elseif last_text == 0 then
    addition = { text }
  else
    addition = { "", text }
  end
  vim.api.nvim_buf_set_lines(M.buf, from, -1, false, addition)
  local last = vim.api.nvim_buf_line_count(M.buf)

  -- Follow the text only for a reader who was already at the end. Someone who
  -- scrolled up to re-read or edit an earlier passage keeps their place --
  -- yanking the cursor away mid-edit is exactly the kind of interruption this
  -- window exists to avoid.
  for _, win in ipairs(windows()) do
    if followers[win] then
      position_at_end(win, last, false, 0)
    end
  end

  M.state.preview = ""
  render_preview()

  -- `noautocmd`, deliberately. The dictation window runs a bundled config
  -- with nothing on BufWritePre, but `nvim.init` can point at any config, and
  -- a format-on-save that reflowed dictated prose would rewrite the
  -- transcript behind the user's back.
  vim.api.nvim_buf_call(M.buf, function()
    vim.cmd("silent! noautocmd write")
  end)
  return last
end

--- Push indicator state. Called as a notification, many times a second.
---
--- Any field may be omitted; only what is present changes. `level` is folded
--- into the rolling meter history rather than replacing it.
function M.set_state(update)
  local phase_changed = update.phase ~= nil and update.phase ~= M.state.phase
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
  if update.preview ~= nil or phase_changed then
    render_preview()
  end
end

_G.Spokenpad = M
return true
