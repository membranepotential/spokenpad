-- The nvim half of voice-kb: the dictation buffer and its indicator.
--
-- Loaded once per connection by `voice_kb.nvim.NvimSession` via
-- `nvim_exec_lua`, so it is re-applied automatically whenever the daemon
-- reattaches to a new nvim. Everything it defines hangs off `_G.VoiceKb`;
-- the Python side calls nothing but `VoiceKb.setup`, `VoiceKb.append` and
-- `VoiceKb.set_state`.
--
-- Two things live here rather than in Python, for the same reason: they are
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

-- Speech barely moves a linear meter, so the same perceptual curve the Qt
-- overlay used (level ^ 0.6) is applied before picking a bar glyph.
local METER_GAMMA = 0.6
local AUDIBLE = 0.02

M.state = { phase = "idle", level = 0.0, preview = "" }
M.levels = {}
M.buf = nil
M.ns = vim.api.nvim_create_namespace("voice_kb")

for _ = 1, METER_CELLS do
  M.levels[#M.levels + 1] = 0.0
end

--- Highlight groups, linked to the colourscheme rather than hard-coded, so the
--- indicator follows whatever theme the user's own config loaded.
local function define_highlights()
  local links = {
    VoiceKbRec = "DiagnosticError",
    VoiceKbWork = "DiagnosticInfo",
    VoiceKbIdle = "Comment",
    VoiceKbMuted = "NonText",
    VoiceKbPreview = "Comment",
    VoiceKbFile = "Directory",
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
    local group = amp > AUDIBLE and "VoiceKbRec" or "VoiceKbMuted"
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
    return "%#VoiceKbRec#\u{25cf} REC %*" .. meter()
  elseif phase == "transcribing" then
    return "%#VoiceKbWork#\u{25cf} transcribing\u{2026}%*"
  end
  local name = M.buf and vim.api.nvim_buf_get_name(M.buf) or ""
  return "%#VoiceKbIdle#\u{25cb} voice-kb%* %#VoiceKbFile#" .. vim.fn.fnamemodify(name, ":t") .. "%*"
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

--- The preview, as virtual lines hanging below the end of the buffer -- which
--- is exactly where the committed text will land, so the user reads it in
--- place rather than in a second widget somewhere else on screen.
local function render_preview()
  if not M.buf or not vim.api.nvim_buf_is_valid(M.buf) then
    return
  end
  pcall(vim.api.nvim_buf_del_extmark, M.buf, M.ns, PREVIEW_EXTMARK)
  local text = M.state.preview
  if text == "" or M.state.phase ~= "recording" then
    return
  end
  local last = vim.api.nvim_buf_line_count(M.buf) - 1
  pcall(vim.api.nvim_buf_set_extmark, M.buf, M.ns, last, 0, {
    id = PREVIEW_EXTMARK,
    virt_lines = { { { "\u{2026} " .. text, "VoiceKbPreview" } } },
    virt_lines_above = false,
  })
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
  local group = vim.api.nvim_create_augroup("VoiceKb", { clear = true })
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

--- Append one committed utterance as its own paragraph, then save.
---
--- Returns the buffer's new line count, which is what the daemon logs -- a
--- request rather than a notification, so a failure to land text is visible
--- instead of silent. That is the whole point of this project.
function M.append(text)
  if not M.buf or not vim.api.nvim_buf_is_valid(M.buf) then
    error("voice-kb: dictation buffer is gone")
  end
  local lines = vim.api.nvim_buf_get_lines(M.buf, 0, -1, false)
  local blank = #lines == 1 and lines[1] == ""

  -- Paragraph separation, not a running wall of text: one blank line between
  -- utterances, and none at the very top of a fresh file.
  local addition = blank and { text } or { "", text }
  local start = blank and 0 or -1
  local previous_last = #lines
  vim.api.nvim_buf_set_lines(M.buf, start, -1, false, addition)
  local last = vim.api.nvim_buf_line_count(M.buf)

  -- Follow the text only for a reader who was already at the end. Someone who
  -- scrolled up to re-read or edit an earlier passage keeps their place --
  -- yanking the cursor away mid-edit is exactly the kind of interruption this
  -- window exists to avoid.
  for _, win in ipairs(windows()) do
    local row = vim.api.nvim_win_get_cursor(win)[1]
    if row >= previous_last then
      pcall(vim.api.nvim_win_set_cursor, win, { last, 0 })
    end
  end

  M.state.preview = ""
  render_preview()

  -- `noautocmd`, deliberately: the user's own config may format on save, and
  -- reflowing dictated prose behind their back would rewrite the transcript.
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
  if update.preview ~= nil then
    M.state.preview = update.preview
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

_G.VoiceKb = M
return true
