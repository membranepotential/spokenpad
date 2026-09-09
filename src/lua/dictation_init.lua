-- spokenpad's own neovim configuration for the dictation window.
--
-- Loaded with `nvim -u <this file>`, which means no user config, no plugin
-- manager, no colorscheme, no LSP -- and, importantly, no `BufWritePre`
-- autocommand that could reflow dictated prose on the save after every
-- utterance.
--
-- **This is the fallback, not the default.** `nvim.init` is unset by default,
-- which means the window runs the user's own nvim configuration -- their
-- colourscheme, their keybindings, their yank flash. That was tried the other
-- way round first, with this file as the default, and the verdict from use
-- was plain: it never looked like their neovim, and every missing habit had
-- to be reimplemented here one at a time to no real end. A dictation window
-- is still an editor, and people want their editor.
--
-- What this file is for is a machine with no nvim configuration of its own,
-- and as the written-down statement of what the window actually needs. Point
-- `nvim.init` at it to get exactly that, at the price of the above.
--
-- Nothing here is required for correctness either way. `nvim_indicator.lua`
-- applies the chrome and prose settings itself over RPC and re-applies them
-- on BufWinEnter/WinNew/FileType, so they hold under any configuration; and
-- committed text is written with `noautocmd`, so a format-on-save cannot
-- reflow dictated prose whatever the user's config does on write.

-- Neovim's Lua bytecode cache. Free, and it is the first line of any config
-- that cares about startup: modules are compiled once and loaded from
-- $XDG_CACHE_HOME/nvim/luacache_chunks afterwards.
vim.loader.enable()

-- No chrome. The winbar is the whole UI: spokenpad draws the recording
-- indicator and level meter there, and the preview hangs below the text as
-- virtual lines. Everything else is noise in a window you are not editing in.
vim.opt.laststatus = 0
vim.opt.showtabline = 0
vim.opt.ruler = false
vim.opt.showmode = false
vim.opt.showcmd = false
vim.opt.number = false
vim.opt.relativenumber = false
vim.opt.signcolumn = "no"
vim.opt.foldcolumn = "0"
vim.opt.colorcolumn = ""
vim.opt.cursorline = false
vim.opt.list = false
vim.opt.fillchars = { eob = " " }

-- Prose, not code. Soft wrap at word boundaries so a long utterance reads as
-- a paragraph, and `linebreak` so it never splits mid-word.
vim.opt.wrap = true
vim.opt.linebreak = true
vim.opt.breakindent = true
vim.opt.smoothscroll = true
vim.opt.textwidth = 0
vim.opt.spell = false

-- The file is saved after every utterance by spokenpad itself. Swap files,
-- backups and undo files would all be written next to a transcript directory
-- that is meant to hold transcripts and nothing else -- and a swap file left
-- behind by a killed daemon turns the next dictation into a recovery prompt,
-- in a window that cannot take focus to answer it.
vim.opt.swapfile = false
vim.opt.backup = false
vim.opt.writebackup = false
vim.opt.undofile = false

-- Follow the file if something else changes it, without asking. The daemon
-- writes through the RPC connection rather than behind nvim's back, so this
-- should never fire -- but a prompt here would be unanswerable.
vim.opt.autoread = true

vim.opt.mouse = "a"
vim.opt.termguicolors = true

-- Yank goes to the system clipboard, as it does in almost everyone's config.
-- This window is where you read a transcript and copy a piece of it out, so a
-- `y` that does not reach the clipboard makes it useless for its actual job.
--
-- Not in tension with spokenpad never touching the clipboard itself: that rule
-- is about the *daemon* not writing anywhere the user did not ask it to. A
-- person pressing `y` in their own editor has asked.
vim.opt.clipboard = "unnamedplus"

-- No colourscheme, deliberately: this inherits the terminal's own colours, so
-- the window looks like every other terminal on the desktop instead of like
-- whatever theme happened to be bundled. Setting one (`habamax`) painted its
-- own grey background over a black alacritty and looked broken, which is
-- exactly the failure this avoids.
--
-- The background is cleared explicitly rather than merely left alone. nvim
-- paints `Normal` opaquely by default, so without this the terminal's
-- background never shows through and a themed alacritty is overpainted.
local function inherit_terminal_background()
  for _, group in ipairs({
    "Normal",
    "NormalNC",
    "NormalFloat",
    "EndOfBuffer",
    "SignColumn",
    "WinBar",
    "WinBarNC",
    "MsgArea",
    "LineNr",
    "FoldColumn",
    "NonText",
    "Folded",
  }) do
    local ok, hl = pcall(vim.api.nvim_get_hl, 0, { name = group })
    if ok then
      hl.bg, hl.ctermbg = nil, nil
      pcall(vim.api.nvim_set_hl, 0, group, hl)
    end
  end
end

local group = vim.api.nvim_create_augroup("SpokenpadInit", { clear = true })

inherit_terminal_background()
vim.api.nvim_create_autocmd("ColorScheme", {
  group = group,
  callback = inherit_terminal_background,
})

--- Load a colourscheme by name from wherever a plugin manager put it.
---
--- Only the one directory that actually provides it is added to the
--- runtimepath -- not every installed plugin -- so this brings in the colours
--- without the plugin scripts, autocommands or format-on-save that come with
--- loading a whole configuration. Measured against this user's LazyVim setup:
--- 0.78s to open the window with everything, 0.30s with just the theme.
---
--- The background stays the terminal's unless `opaque` is passed. A theme
--- loaded straight from its plugin directory is the theme's *defaults*, not
--- the theme as the user configured it -- this one is set `transparent = true`
--- in their config, so taking tokyonight's own #222436 painted a grey-blue
--- block inside alacritty's black border and looked nothing like their
--- editor. Inheriting reproduces `transparent = true` exactly, and is the
--- right default for a window floating over a terminal regardless.
function _G.SpokenpadColorscheme(name, opaque)
  local data = vim.fn.stdpath("data")
  for _, pattern in ipairs({
    data .. "/lazy/*/colors/" .. name .. ".*",
    data .. "/site/pack/*/start/*/colors/" .. name .. ".*",
    data .. "/site/pack/*/opt/*/colors/" .. name .. ".*",
  }) do
    local found = vim.fn.glob(pattern, false, true)
    if #found > 0 then
      vim.opt.runtimepath:append(vim.fn.fnamemodify(found[1], ":h:h"))
      break
    end
  end
  if opaque then
    vim.api.nvim_clear_autocmds({ group = group, event = "ColorScheme" })
  end
  if not pcall(vim.cmd.colorscheme, name) then
    -- Never a visible error: this window cannot take focus, so a message
    -- waiting for a keypress in it would sit there unanswerable.
    vim.notify("spokenpad: colourscheme " .. name .. " not found", vim.log.levels.WARN)
    inherit_terminal_background()
  end
end

-- Flash what you just yanked, the way nvim's own default configuration does.
-- Built in (`vim.hl.on_yank`), no plugin: copying a line out of a transcript
-- is the main thing anyone does in this window by hand, and without the flash
-- there is no feedback at all that it worked.
vim.api.nvim_create_autocmd("TextYankPost", {
  group = group,
  callback = function()
    -- `vim.hl` on 0.11+, `vim.highlight` before it.
    local hl = vim.hl or vim.highlight
    if hl and hl.on_yank then
      hl.on_yank({ higroup = "Visual", timeout = 200 })
    end
  end,
})

vim.opt.winbar = ""

-- Enough of an editor to fix a misheard word without reaching for a manual:
-- `q` closes the window (which ends the passage -- the next dictation starts
-- a new file), and the usual write/quit still work.
vim.keymap.set("n", "q", "<Cmd>quit<CR>", { desc = "close the dictation window" })

-- Move by what you can see, not by what is in the file. An utterance is one
-- buffer line wrapped over many screen rows, so plain `j` leaps a whole
-- paragraph and reading a long transcript by keyboard is unusable. `gg`, `G`
-- and a counted `5j` keep meaning exactly what they always did -- the count
-- check is what preserves that, and it is why this is an expression mapping
-- rather than a plain one.
for _, key in ipairs({ "j", "k" }) do
  vim.keymap.set({ "n", "x" }, key, function()
    return vim.v.count == 0 and ("g" .. key) or key
  end, { expr = true, desc = "move by screen line in wrapped prose" })
end
