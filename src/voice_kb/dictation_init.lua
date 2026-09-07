-- voice-kb's own neovim configuration for the dictation window.
--
-- Loaded with `nvim -u <this file>`, which means no user config, no plugin
-- manager, no colorscheme, no LSP -- and, importantly, no `BufWritePre`
-- autocommand that could reflow dictated prose on the save after every
-- utterance.
--
-- Two reasons this is bundled rather than borrowing the user's config:
--
--  1. **The repo owns its own behaviour.** Everything the dictation window
--     looks and acts like is in this repository, so it is the same on a fresh
--     machine as it is here, and a change to somebody's dotfiles cannot
--     quietly change what voice-kb does.
--
--  2. **The chrome is off before the first draw.** Stripping it over RPC
--     after attaching worked, but the window had already painted a status
--     line, a tab line and a sign column, so opening it flashed a normal
--     editor for a moment before settling. Setting it here means the first
--     frame is the final one.
--
-- Startup cost is *not* one of the reasons: measured time-to-RPC-ready is
-- 0.26s under a full LazyVim config and 0.28s under this one. The ~1s cold
-- open that used to be visible came from voice-kb's own readiness probe, not
-- from nvim.
--
-- `nvim.init` in the config points somewhere else if you want your own setup
-- in this window instead. Nothing here is required for correctness -- the
-- indicator applies what it needs itself (see `nvim_indicator.lua`) -- so a
-- different config loses the guarantees above but still works.

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
vim.opt.clipboard = ""
vim.opt.termguicolors = true

-- A quiet, readable default that does not depend on a colorscheme being
-- installed. `background` follows the terminal, so this inherits whatever
-- the user's alacritty theme already is rather than fighting it.
vim.cmd.colorscheme("habamax")
vim.opt.winbar = ""

-- Enough of an editor to fix a misheard word without reaching for a manual:
-- `q` closes the window (which ends the passage -- the next dictation starts
-- a new file), and the usual write/quit still work.
vim.keymap.set("n", "q", "<Cmd>quit<CR>", { desc = "close the dictation window" })
