-- Transactional Rust RPC extensions to the indicator module. This is spliced
-- into nvim_indicator.lua's footer, where `M`, `previous`, and its local
-- rendering helpers are in scope.

if previous then
  if previous.buf and vim.api.nvim_buf_is_valid(previous.buf) and vim.api.nvim_buf_is_loaded(previous.buf) then
    M.buf = previous.buf
  end
  M.state = previous.state or M.state
  M.levels = previous.levels or M.levels
  M.preview_views = previous.preview_views or M.preview_views
  M.last_append_id = previous.last_append_id
  M.last_append_result = previous.last_append_result
end

-- A live preview is provisional, so use the active colourscheme's Comment
-- foreground with italic text. This restores the clear grey preview used
-- before the Rust port without hard-coding a colour that fights the theme.
local shared_define_highlights = define_highlights
define_highlights = function()
  shared_define_highlights()
  local preview = vim.api.nvim_get_hl(0, { name = "Comment", link = false })
  preview.italic = true
  vim.api.nvim_set_hl(0, "SpokenpadPreview", preview)
end

-- Disable diagnostics before markdown FileType autocmds can lint the empty
-- backing buffer. The filter keeps every other buffer in the user's editor
-- configuration untouched, including buffers opened in this same instance.
local shared_setup = M.setup
function M.setup(buf)
  vim.diagnostic.enable(false, { bufnr = buf })
  shared_setup(buf)
  vim.diagnostic.enable(false, { bufnr = buf })
end

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

local function transactional_append(text, continued)
  if not M.buf or not vim.api.nvim_buf_is_valid(M.buf) then
    error("spokenpad: dictation buffer is gone")
  end

  local old_lines = vim.api.nvim_buf_get_lines(M.buf, 0, -1, false)
  local old_modified = vim.bo[M.buf].modified
  local last_text = last_text_line(old_lines)
  local followers = {}
  for _, win in ipairs(windows()) do
    followers[win] = follows_preview(win, old_lines, last_text)
  end
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
  local ok, write_error = pcall(vim.api.nvim_buf_call, M.buf, function()
    vim.cmd("silent noautocmd write")
  end)
  if not ok then
    vim.api.nvim_buf_set_lines(M.buf, 0, -1, false, old_lines)
    vim.bo[M.buf].modified = old_modified
    render_preview()
    error(write_error)
  end

  local last = vim.api.nvim_buf_line_count(M.buf)
  for _, win in ipairs(windows()) do
    if followers[win] then
      position_at_end(win, last, false, 0)
    end
  end
  M.state.preview = ""
  render_preview()
  return last
end

function M.append_once(id, text, continued)
  if M.last_append_id == id then
    return M.last_append_result
  end
  local result = transactional_append(text, continued)
  M.last_append_id = id
  M.last_append_result = result
  return result
end

_G.Spokenpad = M
return true
