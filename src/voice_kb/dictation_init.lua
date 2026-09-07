-- voice-kb's own neovim configuration for the dictation window.
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

-- No chrome. The winbar is the whole UI: voice-kb draws the recording
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
vim.opt.textwidth = 0
vim.opt.spell = false

-- The file is saved after every utterance by voice-kb itself. Swap files,
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
-- Not in tension with voice-kb never touching the clipboard itself: that rule
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
  }) do
    local ok, hl = pcall(vim.api.nvim_get_hl, 0, { name = group })
    if ok then
      hl.bg, hl.ctermbg = nil, nil
      pcall(vim.api.nvim_set_hl, 0, group, hl)
    end
  end
end

local group = vim.api.nvim_create_augroup("VoiceKbInit", { clear = true })

inherit_terminal_background()
vim.api.nvim_create_autocmd("ColorScheme", {
  group = group,
  callback = inherit_terminal_background,
})

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
