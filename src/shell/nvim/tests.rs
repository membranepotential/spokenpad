use super::*;
use crate::{
    config::Mode,
    core::session::{Notice, RecordingStatus},
};
use std::os::unix::{fs::PermissionsExt, net::UnixListener};

/// The editor tests need a real Neovim. A missing one is a broken environment,
/// not a reason to report a green suite, so it fails unless the operator says
/// otherwise.
fn nvim_or_skip() -> bool {
    if Command::new("nvim").arg("--version").output().is_ok() {
        return true;
    }
    assert!(
        std::env::var_os("SPOKENPAD_ALLOW_MISSING_NVIM").is_some(),
        "nvim is not installed; these tests exercise the real editor. \
         Set SPOKENPAD_ALLOW_MISSING_NVIM=1 to skip them deliberately."
    );
    false
}

struct ProcessGroupGuard(libc::pid_t);

impl ProcessGroupGuard {
    fn for_session(session: &NvimSession) -> Self {
        Self(session.process.as_ref().expect("session spawned nvim").id() as libc::pid_t)
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        // SAFETY: spawned editors create a process group whose id is their pid.
        let _ = unsafe { libc::kill(-self.0, libc::SIGKILL) };
    }
}

/// A fake `g:clipboard` that never touches the real one: `copy` stores what
/// it was given in a plain Lua global instead of shelling out to xclip/xsel,
/// and `paste` reads it back. Set with `--cmd`, which runs before nvim would
/// otherwise probe for a real provider (`:h g:clipboard`).
const TEST_CLIPBOARD_CMD: &str = r#"lua vim.g.clipboard = { name = "spokenpad-test", copy = { ["+"] = function(lines) _G.spokenpad_test_clipboard = lines end, ["*"] = function(lines) end }, paste = { ["+"] = function() return { _G.spokenpad_test_clipboard or {}, "v" } end, ["*"] = function() return { {}, "v" } end } }"#;

fn headless(directory: &Path) -> Nvim {
    Nvim {
        mode: Mode::Managed,
        terminal: Terminal::Headless,
        editor: vec![
            "nvim".to_owned(),
            "-u".to_owned(),
            "NONE".to_owned(),
            "-i".to_owned(),
            "NONE".to_owned(),
            "--cmd".to_owned(),
            TEST_CLIPBOARD_CMD.to_owned(),
        ],
        socket_path: directory.join("nvim.sock"),
        dictation_dir: directory.join("dictation"),
        startup_timeout_s: 10.0,
        ..Nvim::default()
    }
}

/// The test clipboard's current `+` contents, joined on `\n` the way the real
/// system clipboard would hold multiple lines; `None` while it was never
/// written to, which is how an untouched clipboard is told apart from one
/// that was set to the empty string.
fn test_clipboard(session: &mut NvimSession) -> Option<String> {
    lua(
        session,
        r#"
if _G.spokenpad_test_clipboard == nil then
  return vim.NIL
end
return table.concat(_G.spokenpad_test_clipboard, "\n")
"#,
    )
    .as_str()
    .map(str::to_owned)
}

/// Runs a Lua chunk in the connected editor and returns its value.
fn lua(session: &mut NvimSession, chunk: &str) -> Value {
    session
        .connection
        .as_mut()
        .expect("session is connected")
        .client
        .request(
            "nvim_exec_lua",
            vec![Value::from(chunk), Value::Array(Vec::new())],
            deadline(SETUP_TIMEOUT),
        )
        .unwrap_or_else(|error| panic!("lua chunk failed: {error}\n{chunk}"))
}

/// Gives the editor a 40x10 grid, so screen-position assertions mean something.
fn attach_ui(session: &mut NvimSession) {
    session
        .connection
        .as_mut()
        .expect("session is connected")
        .client
        .request(
            "nvim_ui_attach",
            vec![
                Value::from(40),
                Value::from(10),
                Value::Map(vec![
                    (Value::from("rgb"), Value::from(true)),
                    (Value::from("ext_linegrid"), Value::from(true)),
                ]),
            ],
            deadline(SETUP_TIMEOUT),
        )
        .unwrap();
}

/// The indicator a detaching session left behind. `close` confirms its push,
/// so this holds the moment it returns — a plain notification would be
/// discarded with the unparsed input when the channel reaches EOF.
fn assert_idle_indicator(session: &mut NvimSession) {
    let state = lua(
        session,
        r#"
return {
  Spokenpad.state.phase,
  Spokenpad.state.preview,
  Spokenpad.state.notice,
  #vim.api.nvim_buf_get_extmarks(Spokenpad.buf, Spokenpad.ns, 0, -1, {}),
}
"#,
    );
    let state = state.as_array().expect("four indicator fields");
    assert_eq!(state[0].as_str(), Some("idle"), "detaching left {state:?}");
    assert_eq!(state[1].as_str(), Some(""), "detaching left {state:?}");
    assert_eq!(state[2].as_str(), Some(""), "detaching left {state:?}");
    assert_eq!(state[3].as_u64(), Some(0), "detaching left {state:?}");
}

/// The window-local winbar of the window the dictation buffer is shown in.
fn winbar(session: &mut NvimSession) -> String {
    lua(
        session,
        "return vim.api.nvim_get_option_value('winbar', { win = 0 })",
    )
    .as_str()
    .expect("winbar is a string")
    .to_owned()
}

/// What the window actually draws, truncation and all. The `winbar` option is
/// a statusline expression, so only nvim can say what survives the width of
/// the window it is set on -- which is the whole question a notice raises.
fn rendered_winbar(session: &mut NvimSession) -> String {
    lua(
        session,
        r#"
local win = vim.api.nvim_get_current_win()
return vim.api.nvim_eval_statusline(
  vim.api.nvim_get_option_value("winbar", { win = win }),
  { winid = win, use_winbar = true, maxwidth = vim.api.nvim_win_get_width(win) }
).str
"#,
    )
    .as_str()
    .expect("rendered winbar is a string")
    .to_owned()
}

/// Resizes the editor, and confirms the window took the width: every fitting
/// decision the winbar makes is measured against it.
fn set_columns(session: &mut NvimSession, columns: u32) {
    let width = lua(
        session,
        &format!("vim.o.columns = {columns}\nreturn vim.api.nvim_win_get_width(0)"),
    );
    assert_eq!(width.as_u64(), Some(u64::from(columns)), "resize refused");
}

fn recording(preview: &str) -> IndicatorState {
    IndicatorState {
        phase: IndicatorPhase::Recording,
        level: 0.5,
        preview: preview.to_owned(),
        notice: None,
        latched: false,
        previewing: true,
    }
}

/// Attach mode's configuration: the same headless editor, which the test
/// starts itself the way `spokenpad editor` would.
fn attach(directory: &Path) -> Nvim {
    let mut config = headless(directory);
    config.mode = Mode::Attach;
    config.editor.insert(1, "--headless".to_owned());
    config
}

/// The editor a test started as the user would, killed and reaped on drop.
struct UserEditor(Child);

impl Drop for UserEditor {
    fn drop(&mut self) {
        drop(ProcessGroupGuard(self.0.id() as libc::pid_t));
        let _ = self.0.wait();
    }
}

/// What `spokenpad editor` runs, started as a child so the test can reap it.
fn open_user_editor(config: &Nvim) -> (PathBuf, UserEditor) {
    let (mut passage, mut command) = editor_command(config).unwrap();
    passage.keep();
    let child = command
        .env("NVIM_LOG_FILE", config.socket_path.with_extension("log"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let guard = UserEditor(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while RpcClient::connect(&config.socket_path, deadline).is_err() {
        assert!(
            Instant::now() < deadline,
            "the user's editor never listened"
        );
        std::thread::sleep(CONNECT_POLL);
    }
    (passage.path().to_owned(), guard)
}

#[test]
fn attach_mode_never_spawns_an_editor() {
    let directory = tempfile::tempdir().unwrap();
    let config = attach(directory.path());
    let mut session = NvimSession::new(config.clone());
    assert_eq!(session.ensure().unwrap(), None);
    assert!(session.process.is_none());
    assert!(!config.socket_path.exists());
    assert!(!config.dictation_dir.exists(), "no file before any text");
}

#[test]
fn attach_mode_adopts_the_users_editor_on_the_pending_passage() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = attach(directory.path());
    let mut session = NvimSession::new(config.clone());
    assert_eq!(session.ensure().unwrap(), None);
    let detached = session
        .append_detached("said with no editor", false)
        .unwrap();
    assert!(detached.started);
    assert_eq!(passage::pending(&config), Some(detached.path.clone()));

    let (opened, _editor) = open_user_editor(&config);
    assert_eq!(
        opened, detached.path,
        "the editor opens the pending passage"
    );
    let pinned = session.ensure().unwrap().expect("the user's editor");
    assert_eq!(pinned, detached.path);
    assert!(session.process.is_none(), "attach mode spawned an editor");
    assert_eq!(passage::pending(&config), None, "the pointer is settled");
    session.append("said into the editor", false).unwrap();
    assert_eq!(
        fs::read_to_string(&pinned).unwrap(),
        "said with no editor\n\nsaid into the editor\n"
    );
    // The user's editor is dedicated to dictation, so its chrome comes off.
    assert_eq!(
        lua(&mut session, "return vim.o.laststatus").as_i64(),
        Some(0)
    );
}

#[test]
fn a_second_editor_is_refused_while_one_is_listening() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = attach(directory.path());
    let (_, _editor) = open_user_editor(&config);
    let error = editor_command(&config).err().expect("refused").to_string();
    assert!(error.contains("already open"), "{error}");
}

/// The pending passage is written by Rust and the editor's paragraphs by
/// Lua. A file that changes shape depending on which one wrote it would
/// drift, so both are run on the same inputs.
#[test]
fn the_detached_paragraph_rule_matches_the_editors() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    let path = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    for (existing, text, continued) in [
        ("", "first", false),
        ("", "first", true),
        ("first\n", "second", false),
        ("first\n", "more", true),
        ("first\n", "", true),
        ("first\n\n\n \t\n", "second", false),
        ("a\nb\n", "c\nd\n", false),
        ("\n\n", "x", false),
        ("Grüße\n", "東京", true),
    ] {
        fs::write(&path, existing).unwrap();
        lua(
            &mut session,
            "vim.api.nvim_buf_call(Spokenpad.buf, function() vim.cmd('silent edit!') end)",
        );
        session.append(text, continued).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            passage::append_paragraph(existing, text, continued),
            "{existing:?} + {text:?} (continued: {continued})"
        );
    }
}

#[test]
fn headless_append_literal_text_and_adopt() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let mut first = NvimSession::new(config.clone());
    let path = first.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&first);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(first.append("$(touch nope)\nGrüße 東京", false).unwrap(), 2);
    first.close();

    let mut restarted = NvimSession::new(config);
    assert_eq!(restarted.ensure().unwrap().expect("an editor"), path);
    restarted.append("continued", true).unwrap();
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "$(touch nope)\nGrüße 東京 continued\n"
    );
}

#[test]
fn headless_append_preserves_trailing_lf_and_empty_continuation() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    let path = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);

    assert_eq!(session.append("first\nsecond\n", false).unwrap(), 3);
    assert_eq!(session.append("", true).unwrap(), 2);
    assert_eq!(fs::read_to_string(path).unwrap(), "first\nsecond\n");
}

#[test]
fn spawned_editor_waits_for_owner_and_user_init() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let init = directory.path().join("delayed-init.lua");
    fs::write(
        &init,
        r#"
vim.wait(250)
vim.g.spokenpad_init_finished = true
vim.api.nvim_create_autocmd("VimEnter", {
  once = true,
  callback = function()
    vim.wait(250)
    vim.g.spokenpad_vimenter_finished = true
  end,
})
"#,
    )
    .unwrap();
    let mut config = headless(directory.path());
    config.editor = vec![
        "nvim".to_owned(),
        "-i".to_owned(),
        "NONE".to_owned(),
        "--cmd".to_owned(),
        "lua vim.wait(250)".to_owned(),
    ];
    config.init = Some(init);
    let mut session = NvimSession::new(config);
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);

    let ready = lua(
        &mut session,
        "return vim.g.spokenpad_init_finished == true and vim.g.spokenpad_vimenter_finished == true",
    );
    assert_eq!(ready.as_bool(), Some(true));
}

#[test]
fn headless_preview_is_inline_virtual_and_buffer_local() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let mut session = NvimSession::new(config.clone());
    let path = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    attach_ui(&mut session);

    session.set_indicator(&recording("draft words")).unwrap();
    lua(
        &mut session,
        r#"
local buf = Spokenpad.buf
local marks = vim.api.nvim_buf_get_extmarks(buf, Spokenpad.ns, 0, -1, { details = true })
assert(#marks == 1, "preview must use exactly one extmark")
local details = marks[1][4]
assert(details.virt_lines == nil, "short empty-buffer preview added a virtual spacer")
assert(details.virt_text[1][1] == "draft words", "preview must overlay the empty buffer row")
assert(details.virt_text_pos == "overlay", "empty-buffer preview is not inline")
assert(vim.deep_equal(vim.api.nvim_buf_get_lines(buf, 0, -1, false), { "" }))

vim.cmd("redraw!")
local function screen_contains(needle)
  for row = 1, 10 do
    local line = ""
    for col = 1, 40 do
      line = line .. vim.fn.screenstring(row, col)
    end
    if line:find(needle, 1, true) then
      return true
    end
  end
  return false
end
assert(screen_contains("draft words"), "first preview exists but is clipped off-screen")

vim.fn.setreg("z", "sentinel")
vim.api.nvim_buf_call(buf, function()
  vim.cmd([[normal! gg0"zy$]])
end)
assert(not vim.fn.getreg("z"):find("draft words", 1, true), "preview leaked into yank")

assert(not vim.diagnostic.is_enabled({ bufnr = buf }), "owned buffer diagnostics enabled")
local ordinary = vim.api.nvim_create_buf(true, false)
assert(vim.diagnostic.is_enabled({ bufnr = ordinary }), "ordinary buffer diagnostics disabled")
vim.api.nvim_buf_delete(ordinary, { force = true })

for _, foreground in ipairs({ 0x123456, 0xabcdef }) do
  vim.api.nvim_set_hl(0, "Normal", { fg = foreground })
  vim.api.nvim_set_hl(0, "Comment", { fg = foreground + 1 })
  vim.api.nvim_exec_autocmds("ColorScheme", { modeline = false })
  local comment = vim.api.nvim_get_hl(0, { name = "Comment", link = false })
  local preview = vim.api.nvim_get_hl(0, { name = "SpokenpadPreview", link = false })
  assert(preview.fg == comment.fg, "preview foreground drifted from Comment")
  assert(preview.italic == true, "preview lost its provisional italic")
end
return true
"#,
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "");

    session.close();
    let mut reloaded = NvimSession::new(config);
    assert_eq!(reloaded.ensure().unwrap().expect("an editor"), path);
    attach_ui(&mut reloaded);
    // `close` pushes the idle indicator as a bounded request, so the editor
    // has applied it by the time `close` returns.
    assert_idle_indicator(&mut reloaded);
    lua(&mut reloaded, "vim.cmd('messages clear')");

    reloaded.append("committed", false).unwrap();
    let messages = lua(
        &mut reloaded,
        "return vim.api.nvim_exec2('messages', { output = true }).output",
    );
    assert_eq!(messages.as_str(), Some(""), "successful write was noisy");
    reloaded.set_indicator(&recording("next draft")).unwrap();
    lua(
        &mut reloaded,
        r#"
local marks = vim.api.nvim_buf_get_extmarks(
  Spokenpad.buf, Spokenpad.ns, 0, -1, { details = true }
)
assert(#marks == 1, "preview after committed text must use exactly one extmark")
local details = marks[1][4]
assert(#details.virt_lines == 1, "preview after committed text has a virtual spacer")
assert(details.virt_lines[1][1][1] == "next draft", "preview text must be first")
assert(details.virt_lines_above == false, "non-empty preview must follow committed text")
assert(vim.deep_equal(
  vim.api.nvim_buf_get_lines(Spokenpad.buf, 0, -1, false), { "committed" }
))
return true
"#,
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "committed\n");

    let long_paragraph = (0..48)
        .map(|index| format!("word{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    reloaded.append(&long_paragraph, true).unwrap();
    lua(
        &mut reloaded,
        r#"
vim.cmd("redraw!")
local visible = false
for row = 1, 10 do
  local line = ""
  for col = 1, 40 do
    line = line .. vim.fn.screenstring(row, col)
  end
  visible = visible or line:find("word47", 1, true) ~= nil
end
assert(visible, "committed tail fell below the viewport")
return true
"#,
    );
    reloaded
        .set_indicator(&recording(
            "growing preview keeps its newest tailmarker visible",
        ))
        .unwrap();
    lua(
        &mut reloaded,
        r#"
vim.cmd("redraw!")
local function screen_contains(needle)
  local rows = {}
  for row = 1, 10 do
    local line = ""
    for col = 1, 40 do
      line = line .. vim.fn.screenstring(row, col)
    end
    rows[#rows + 1] = line
    if line:find(needle, 1, true) then
      return true, table.concat(rows, "|")
    end
  end
  return false, table.concat(rows, "|")
end
local tail_visible, rendered = screen_contains("tailmarker")
assert(tail_visible, "preview tail fell below a long wrapped paragraph: " .. rendered
  .. " view=" .. vim.inspect(vim.fn.winsaveview())
  .. " height=" .. vim.inspect(vim.api.nvim_win_text_height(0, { start_row = 0, end_row = 0 }))
  .. " winheight=" .. vim.api.nvim_win_get_height(0))
Spokenpad.set_state({
  preview = "a longer replacement preview still follows through to growthmarker",
})
vim.cmd("redraw!")
assert(screen_contains("growthmarker"), "growing preview stopped following before commit")

vim.api.nvim_win_call(0, function()
  vim.cmd("normal! gg0zt")
end)
local before = vim.fn.winsaveview()
Spokenpad.set_state({ preview = "do not snap back to moved reader" })
vim.cmd("redraw!")
local after = vim.fn.winsaveview()
assert(vim.deep_equal(before, after), "preview update snapped a moved reader to the end")
assert(not screen_contains("do not snap"), "moved reader unexpectedly followed preview")
Spokenpad.set_state({ preview = "still do not snap on a later update" })
vim.cmd("redraw!")
assert(vim.deep_equal(before, vim.fn.winsaveview()), "later preview update resumed following")
assert(not screen_contains("still do not snap"), "later preview update snapped back")
return true
"#,
    );
}

#[test]
fn an_idle_phase_deletes_the_preview_and_a_level_push_leaves_the_buffer_alone() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    session.append("committed", false).unwrap();
    session
        .set_indicator(&recording("provisional tail"))
        .unwrap();
    let marks = lua(
        &mut session,
        "return #vim.api.nvim_buf_get_extmarks(Spokenpad.buf, Spokenpad.ns, 0, -1, {})",
    );
    assert_eq!(marks.as_u64(), Some(1));

    // Ten level samples a second are the common case, and each one carries the
    // whole indicator. None of them may touch the buffer or move the reader.
    let before = lua(
        &mut session,
        "return { vim.b[Spokenpad.buf].changedtick, vim.fn.winsaveview().topline }",
    );
    for step in 0..5 {
        let mut state = recording("provisional tail");
        state.level = f64::from(step) / 10.0;
        session.set_indicator(&state).unwrap();
    }
    let after = lua(
        &mut session,
        "return { vim.b[Spokenpad.buf].changedtick, vim.fn.winsaveview().topline }",
    );
    assert_eq!(
        before, after,
        "a level-only push changed the buffer or the view"
    );

    session.set_indicator(&IndicatorState::default()).unwrap();
    let marks = lua(
        &mut session,
        "return #vim.api.nvim_buf_get_extmarks(Spokenpad.buf, Spokenpad.ns, 0, -1, {})",
    );
    assert_eq!(marks.as_u64(), Some(0), "idle left the preview extmark");
    assert_eq!(
        fs::read_to_string(session.path().unwrap()).unwrap(),
        "committed\n"
    );
}

/// `setup` is called on every (re)connection, including one that happens
/// mid-recording after the pinned buffer was lost. The daemon then pushes the
/// indicator it already had, and `set_state` redraws only what changed -- so
/// `setup` itself has to put the live preview into the buffer it just pinned.
#[test]
fn re_pinning_mid_recording_moves_the_preview_to_the_new_buffer() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    session
        .set_indicator(&recording("half a sentence"))
        .unwrap();

    let marks = lua(
        &mut session,
        r#"
local second = vim.api.nvim_create_buf(true, false)
Spokenpad.setup(second, true)
return {
  #vim.api.nvim_buf_get_extmarks(second, Spokenpad.ns, 0, -1, {}),
  Spokenpad.last_error or vim.NIL,
}
"#,
    );
    let marks = marks.as_array().expect("two fields");
    assert_eq!(
        marks[0].as_u64(),
        Some(1),
        "the re-pinned buffer has no preview"
    );
    assert_eq!(marks[1], Value::Nil, "setup raised: {marks:?}");

    // And an unchanged push after the re-pin leaves it exactly there.
    session
        .set_indicator(&recording("half a sentence"))
        .unwrap();
    let after = lua(
        &mut session,
        r#"
local marks = vim.api.nvim_buf_get_extmarks(
  Spokenpad.buf, Spokenpad.ns, 0, -1, { details = true }
)
return { #marks, marks[1] and marks[1][4].virt_text[1][1] or vim.NIL }
"#,
    );
    let after = after.as_array().expect("two fields");
    assert_eq!(after[0].as_u64(), Some(1));
    assert_eq!(after[1].as_str(), Some("half a sentence"));
}

/// A notice says what happened to the capture the user just made, so it has to
/// be readable in the phase they are in when it is raised -- idle, for a tap
/// too short to have recorded anything, or for one that outlives the decode --
/// and it must never be mistaken for the preview, which is dictated text.
#[test]
fn a_notice_is_winbar_text_in_every_phase_and_never_the_preview() {
    if !nvim_or_skip() {
        return;
    }
    let held = Notice::HeldTooBriefly.text();
    let gap = Notice::MicrophoneGap.text();
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    session.append("committed", false).unwrap();

    let mut idle = IndicatorState {
        notice: Some(held.clone()),
        ..IndicatorState::default()
    };
    // Wide enough for the whole notice beside the dated file name; how a
    // narrow window fits one is
    // `a_long_notice_keeps_the_phase_label_and_its_headline_in_a_narrow_window`.
    set_columns(&mut session, 120);
    session.set_indicator(&idle).unwrap();
    let bar = rendered_winbar(&mut session);
    assert!(
        bar.contains(held.headline) && bar.contains(held.detail.as_ref()),
        "an idle notice is invisible, which is the whole defect: {bar}"
    );

    idle.notice = None;
    session.set_indicator(&idle).unwrap();
    let bar = rendered_winbar(&mut session);
    assert!(bar.contains("spokenpad"), "the idle winbar is gone: {bar}");
    assert!(
        !bar.contains(held.headline),
        "a cleared notice stayed up: {bar}"
    );

    let mut live = recording("provisional tail");
    live.notice = Some(gap.clone());
    session.set_indicator(&live).unwrap();
    let bar = winbar(&mut session);
    assert!(bar.contains("REC") && bar.contains(gap.headline), "{bar}");
    assert!(
        !bar.contains("provisional tail"),
        "the live tail belongs below the transcript, not in the winbar: {bar}"
    );
    let marks = lua(
        &mut session,
        "return vim.json.encode(vim.api.nvim_buf_get_extmarks(Spokenpad.buf, Spokenpad.ns, 0, -1, { details = true }))",
    );
    let marks = marks.as_str().expect("encoded extmarks").to_owned();
    assert!(
        marks.contains("provisional tail"),
        "the preview must stay extmark virtual text: {marks}"
    );
    assert!(
        !marks.contains(gap.headline),
        "the notice reached the preview: {marks}"
    );
    let lines = lua(
        &mut session,
        "return vim.json.encode(vim.api.nvim_buf_get_lines(Spokenpad.buf, 0, -1, false))",
    );
    let lines = lines.as_str().expect("encoded lines").to_owned();
    assert_eq!(
        lines, r#"["committed"]"#,
        "provisional text reached the buffer"
    );
    let file = fs::read_to_string(session.path().unwrap()).unwrap();
    assert_eq!(file, "committed\n", "provisional text reached the file");
}

/// A notice is a sentence and a dictation window is small, so at 40 columns
/// the bar cannot hold the phase, the meter and the whole notice at once. What
/// it must never do is drop the phase and the beginning of the reason, which
/// is exactly what letting nvim truncate from the left did: the memory-cap
/// notice rendered as `<aining audio is in /tmp/capture-example.wav — recover`.
#[test]
fn a_long_notice_keeps_the_phase_label_and_its_headline_in_a_narrow_window() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    let notice = Notice::MemoryCap(RecordingStatus::Recorded(
        "/tmp/spokenpad/capture-example.wav".into(),
    ));
    let notice = notice.text();
    // The state the memory cap actually leaves behind: still recording, with
    // previews stopped for good.
    let mut capped = recording("");
    capped.previewing = false;
    capped.notice = Some(notice.clone());

    let idle = IndicatorState {
        notice: Some(notice.clone()),
        ..IndicatorState::default()
    };

    set_columns(&mut session, 40);
    for state in [&capped, &idle] {
        session.set_indicator(state).unwrap();
        let bar = rendered_winbar(&mut session);
        let phase = if state.phase == IndicatorPhase::Recording {
            "REC"
        } else {
            "spokenpad"
        };
        assert!(
            bar.contains(phase) && bar.contains(notice.headline),
            "the phase and the headline are the two things 40 columns must keep: {bar:?}"
        );
        assert!(
            !bar.contains('<'),
            "nvim truncated the winbar instead of the daemon fitting it: {bar:?}"
        );
        assert!(
            !bar.contains("capture-example"),
            "a detail that cannot fit whole is not shown at all: {bar:?}"
        );
    }

    // Given the room, the detail joins the headline -- naming the recovery WAV
    // by file name, because the winbar has a window's width, not a path's.
    set_columns(&mut session, 200);
    session.set_indicator(&capped).unwrap();
    let bar = rendered_winbar(&mut session);
    assert!(
        bar.contains(notice.headline) && bar.contains(notice.detail.as_ref()),
        "a wide window shows the whole notice: {bar:?}"
    );
    assert!(
        bar.contains("capture-example.wav") && !bar.contains("/tmp"),
        "the winbar names the file; the log names the directory: {bar:?}"
    );
}

#[test]
fn a_window_that_stops_showing_the_buffer_gets_its_winbar_back() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    session.set_indicator(&recording("")).unwrap();
    let bar = lua(
        &mut session,
        "return vim.api.nvim_get_option_value('winbar', { win = 0 })",
    );
    assert!(
        bar.as_str().is_some_and(|bar| bar.contains("REC")),
        "the dictation window shows no indicator: {bar}"
    );

    // The user opens another file in that window. A frozen "REC" winbar over
    // someone else's file is worse than no winbar at all.
    let restored = lua(
        &mut session,
        r#"
vim.o.winbar = "USER GLOBAL"
vim.cmd.edit(vim.fn.tempname())
Spokenpad.set_state({ level = 0.1 })
return vim.api.nvim_get_option_value("winbar", { win = 0 })
"#,
    );
    assert_eq!(restored.as_str(), Some("USER GLOBAL"));
}

#[test]
fn chrome_globals_are_set_only_in_an_editor_spokenpad_opened() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    assert_eq!(
        lua(&mut session, "return vim.o.laststatus").as_i64(),
        Some(0),
        "a dedicated editor keeps its statusline"
    );

    let adopted = lua(
        &mut session,
        r#"
-- An editor spokenpad merely adopted: its globals are the user's.
Spokenpad.dedicated = false
vim.o.laststatus = 2
vim.o.showtabline = 2
Spokenpad.setup(Spokenpad.buf, false)
local adopted = { vim.o.laststatus, vim.o.showtabline,
  vim.api.nvim_get_option_value("wrap", { win = 0 }) }
Spokenpad.setup(Spokenpad.buf, true)
return { adopted, vim.o.laststatus, vim.o.showtabline }
"#,
    );
    let adopted = adopted.as_array().unwrap();
    let user = adopted[0].as_array().unwrap();
    assert_eq!(user[0].as_i64(), Some(2), "adopting cleared laststatus");
    assert_eq!(user[1].as_i64(), Some(2), "adopting cleared showtabline");
    assert_eq!(
        user[2].as_bool(),
        Some(true),
        "adopting skipped the window-local prose settings"
    );
    assert_eq!(adopted[1].as_i64(), Some(0), "dedicated kept a statusline");
    assert_eq!(adopted[2].as_i64(), Some(0), "dedicated kept a tabline");
}

#[test]
fn an_append_while_the_user_types_keeps_both_texts_and_saves() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    let path = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    attach_ui(&mut session);

    lua(&mut session, "vim.api.nvim_input('ityped by hand')");
    let mode = lua(&mut session, "return vim.fn.mode()");
    assert_eq!(mode.as_str(), Some("i"), "the editor is not in insert mode");

    session.append("dictated", false).unwrap();

    let state = lua(
        &mut session,
        r#"
return {
  vim.fn.mode(),
  table.concat(vim.api.nvim_buf_get_lines(Spokenpad.buf, 0, -1, false), "|"),
  vim.bo[Spokenpad.buf].modified,
}
"#,
    );
    let state = state.as_array().unwrap();
    assert_eq!(state[0].as_str(), Some("i"), "the append left insert mode");
    assert_eq!(
        state[1].as_str(),
        Some("typed by hand||dictated"),
        "the append corrupted the user's own edit"
    );
    assert_eq!(
        state[2].as_bool(),
        Some(false),
        "the buffer was left modified but unsaved"
    );
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "typed by hand\n\ndictated\n"
    );
}

#[test]
fn failed_save_rolls_back_before_the_next_append() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    let path = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    session.append("first", false).unwrap();
    lua(&mut session, "vim.bo[Spokenpad.buf].readonly = true");

    assert!(session.append("must roll back", false).is_err());
    assert!(
        !session.connected(),
        "a failed append left its client installed"
    );
    session.ensure().unwrap().expect("an editor");
    lua(&mut session, "vim.bo[Spokenpad.buf].readonly = false");
    session.append("second", false).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "first\n\nsecond\n");
}

/// Performs the next append and then stalls past the RPC deadline, so the
/// daemon cannot tell "not appended" from "appended, reply lost". `before` is
/// a Lua statement run after the append and before the stall.
fn stall_the_next_append(session: &mut NvimSession, before: &str) {
    lua(
        session,
        &format!(
            r#"
local append_once = Spokenpad.append_once
local stalled = false
Spokenpad.append_once = function(...)
  local result = append_once(...)
  if not stalled then
    stalled = true
    {before}
    vim.wait(2200)
  end
  return result
end
"#
        ),
    );
}

#[test]
fn append_retries_an_ambiguous_timeout_exactly_once() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    let path = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    stall_the_next_append(&mut session, "");

    assert_eq!(session.append("only once", false).unwrap(), 1);
    assert_eq!(fs::read_to_string(path).unwrap(), "only once\n");
}

#[test]
fn a_retry_that_cannot_reconnect_leaves_the_session_disconnected() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    let path = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    // The editor stops listening while the reply is in flight, so the retry
    // has nowhere to go. Whatever happens, no half-used client may survive.
    stall_the_next_append(&mut session, "vim.fn.serverstop(vim.fn.serverlist()[1])");

    // The request was sent, so it may have landed: never "not sent".
    let AppendFailure::Unconfirmed(error) = session.append("landed once", false).unwrap_err()
    else {
        panic!("a sent append was reported as not sent");
    };
    let error = format!("{error:#}");
    assert!(
        error.contains("outcome is unknown after timeout"),
        "{error}"
    );
    assert!(
        !session.connected(),
        "a desynchronized client was left installed"
    );
    // The text did land: a lost reply is not a lost utterance.
    assert_eq!(fs::read_to_string(path).unwrap(), "landed once\n");
}

/// The retry is idempotent only while the editor still pins the buffer the
/// first attempt wrote into: the Lua cache is keyed on it. If the buffer is
/// gone, reattaching pins a *fresh* file and the repeat would write the same
/// utterance into a second one -- so the retry refuses instead.
#[test]
fn a_retry_that_would_repin_another_file_refuses_to_append_twice() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    let path = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    stall_the_next_append(
        &mut session,
        "vim.api.nvim_buf_delete(Spokenpad.buf, { force = true })",
    );

    let AppendFailure::Unconfirmed(error) = session.append("exactly once", false).unwrap_err()
    else {
        panic!("a sent append was reported as not sent");
    };
    let error = format!("{error:#}");
    assert!(
        error.contains("the dictation file changed during reconnect"),
        "{error}"
    );
    assert!(!session.connected(), "a repinned client was left installed");
    assert_eq!(fs::read_to_string(&path).unwrap(), "exactly once\n");
    for entry in fs::read_dir(&session.config.dictation_dir).unwrap() {
        let other = entry.unwrap().path();
        if other != path {
            assert_eq!(
                fs::read_to_string(&other).unwrap(),
                "",
                "the utterance landed in {} as well",
                other.display()
            );
        }
    }
}

/// A write to an editor that has exited fails before the editor could read
/// anything, so the text is certainly not in it: that is the one failure
/// the caller may deliver elsewhere without risking a second copy.
#[test]
fn an_append_to_an_editor_that_exited_is_reported_as_not_sent() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    let path = session.ensure().unwrap().expect("an editor");
    drop(ProcessGroupGuard::for_session(&session));
    session.process.as_mut().unwrap().wait().unwrap();

    let failure = session.append("never sent", false).unwrap_err();
    assert!(matches!(failure, AppendFailure::NotSent(_)), "{failure}");
    assert!(!session.connected());
    assert_eq!(fs::read_to_string(path).unwrap(), "");
}

#[test]
fn deleting_the_dictation_buffer_repins_on_the_next_ensure() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    let first = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    session.append("before", false).unwrap();

    lua(
        &mut session,
        "vim.api.nvim_buf_delete(Spokenpad.buf, { force = true })",
    );
    let second = session.ensure().unwrap().expect("an editor");
    assert_ne!(second, first, "ensure re-used a buffer that is gone");
    session.append("after", false).unwrap();
    assert_eq!(fs::read_to_string(&first).unwrap(), "before\n");
    assert_eq!(fs::read_to_string(&second).unwrap(), "after\n");
}

#[test]
fn a_socket_file_with_no_listener_is_replaced_by_a_fresh_editor() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let abandoned = UnixListener::bind(&config.socket_path).unwrap();
    drop(abandoned);
    assert!(config.socket_path.exists());

    let mut session = NvimSession::new(config.clone());
    let path = session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    session.append("after a stale socket", false).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "after a stale socket\n");
}

#[test]
fn unrelated_socket_is_rejected_without_mutation() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let mut child = Command::new("nvim")
        .args(["--headless", "-u", "NONE", "--listen"])
        .arg(&config.socket_path)
        .args(["-i", "NONE"])
        .env("NVIM_LOG_FILE", directory.path().join("nvim.log"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let _guard = ProcessGroupGuard(child.id() as libc::pid_t);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !config.socket_path.exists() && Instant::now() < deadline {
        std::thread::sleep(CONNECT_POLL);
    }
    let mut session = NvimSession::new(config);
    let error = session.ensure().unwrap_err().to_string();
    assert!(error.contains("unrelated nvim socket"), "{error}");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn ordinary_file_at_socket_path_is_refused_and_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    fs::write(&config.socket_path, b"not a socket").unwrap();
    let mut session = NvimSession::new(config.clone());

    let error = session.ensure().unwrap_err().to_string();
    assert!(error.contains("refusing non-socket path"), "{error}");
    assert_eq!(fs::read(&config.socket_path).unwrap(), b"not a socket");
    assert!(!config.dictation_dir.exists());
}

#[test]
fn symlink_at_socket_path_is_refused_and_its_target_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let target = directory.path().join("private.txt");
    fs::write(&target, b"someone else's file").unwrap();
    std::os::unix::fs::symlink(&target, &config.socket_path).unwrap();
    let mut session = NvimSession::new(config.clone());

    let error = session.ensure().unwrap_err().to_string();
    assert!(error.contains("refusing non-socket path"), "{error}");
    assert!(
        fs::symlink_metadata(&config.socket_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).unwrap(), b"someone else's file");
}

#[test]
fn a_failed_spawn_leaves_no_empty_dictation_file() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = headless(directory.path());
    // An "editor" that exits immediately: the spawn succeeds, startup does not.
    config.editor = vec!["true".to_owned()];
    config.startup_timeout_s = 2.0;
    let mut session = NvimSession::new(config.clone());

    let error = session.ensure().unwrap_err().to_string();
    assert!(error.contains("exited during startup"), "{error}");
    let leftovers: Vec<_> = fs::read_dir(&config.dictation_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(leftovers.is_empty(), "spawn left {leftovers:?} behind");
}

#[test]
fn a_headless_editor_is_told_so_right_after_its_own_command() {
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let argv = spawn_argv(&config, Path::new("/t.md"), "f", None, None).unwrap();
    assert_eq!(argv[0], "nvim");
    assert_eq!(argv[config.editor.len()], "--headless");
    assert_eq!(argv.last().map(String::as_str), Some("/t.md"));
}

#[test]
fn ownership_is_parsed_from_exactly_the_six_documented_fields() {
    let nil = || Value::Nil;
    let pinned = Value::Array(vec![
        Value::from("a".repeat(64)),
        Value::from("a".repeat(64)),
        Value::from(7),
        Value::from("/home/u/dictation/today.md"),
        Value::from(2),
        Value::from("/home/u/dictation/started.md"),
    ]);
    assert_eq!(
        parse_ownership(&pinned).unwrap(),
        Ownership {
            marker: Some("a".repeat(64)),
            ready: Some("a".repeat(64)),
            pinned: Some(Pinned {
                buffer: 7,
                name: "/home/u/dictation/today.md".to_owned(),
            }),
            startup: Some(Pinned {
                buffer: 2,
                name: "/home/u/dictation/started.md".to_owned(),
            }),
        }
    );

    let bare = Value::Array(vec![nil(), nil(), nil(), nil(), nil(), nil()]);
    assert_eq!(
        parse_ownership(&bare).unwrap(),
        Ownership {
            marker: None,
            ready: None,
            pinned: None,
            startup: None,
        }
    );

    for malformed in [
        Value::from(1),
        Value::Array(vec![nil(), nil()]),
        Value::Array(vec![nil(), nil(), nil(), nil()]),
        // A buffer with no name is not an adoptable buffer.
        Value::Array(vec![nil(), nil(), Value::from(3), nil(), nil(), nil()]),
        Value::Array(vec![nil(), nil(), nil(), nil(), Value::from(3), nil()]),
        Value::Array(vec![Value::from(1), nil(), nil(), nil(), nil(), nil()]),
    ] {
        assert!(
            parse_ownership(&malformed).is_err(),
            "accepted {malformed:?}"
        );
    }
}

#[test]
fn spawn_argv_interpolates_the_window_and_refuses_an_unquotable_colorscheme() {
    let mut config = Nvim {
        socket_path: PathBuf::from("/run/spokenpad.sock"),
        ..Nvim::default()
    };
    let rect = Rect {
        x: -1920,
        y: 40,
        width: 600,
        height: 400,
    };
    let argv = spawn_argv(
        &config,
        Path::new("/state/dictation/today.md"),
        &"f".repeat(64),
        Some(rect),
        Some(Path::new("/state/private/dictation_init.lua")),
    )
    .unwrap();
    assert_eq!(argv[..3], ["alacritty", "--class", "spokenpad"]);
    assert!(argv.contains(&"window.position.x=-1920".to_owned()));
    assert!(argv.contains(&"window.position.y=40".to_owned()));
    assert_eq!(argv[argv.len() - 5], "--listen");
    assert_eq!(argv[argv.len() - 4], "/run/spokenpad.sock");
    assert_eq!(argv[argv.len() - 1], "/state/dictation/today.md");
    let init = argv.iter().position(|argument| argument == "-u").unwrap();
    assert_eq!(argv[init + 1], "/state/private/dictation_init.lua");

    config.colorscheme = Some("tokyonight-moon".to_owned());
    let themed = spawn_argv(&config, Path::new("/t.md"), "f", None, None).unwrap();
    assert!(
        themed
            .iter()
            .any(|argument| argument.contains("SpokenpadColorscheme('tokyonight-moon', false)")),
        "{themed:?}"
    );

    for hostile in ["", "x'); os.execute('rm -rf ~", "scheme\nname", "sch eme"] {
        config.colorscheme = Some(hostile.to_owned());
        let error = spawn_argv(&config, Path::new("/t.md"), "f", None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("[A-Za-z0-9_.-]+"), "accepted {hostile:?}");
    }
}

#[test]
fn dictation_files_suffix_around_a_collision() {
    let directory = tempfile::tempdir().unwrap();
    let config = Nvim {
        dictation_dir: directory.path().join("dictation"),
        file_template: "fixed.md".to_owned(),
        ..Nvim::default()
    };
    assert_eq!(
        new_file(&config).unwrap(),
        config.dictation_dir.join("fixed.md")
    );
    assert_eq!(
        new_file(&config).unwrap(),
        config.dictation_dir.join("fixed-1.md")
    );
    let extensionless = Nvim {
        file_template: "fixed".to_owned(),
        ..config
    };
    assert_eq!(
        new_file(&extensionless).unwrap(),
        extensionless.dictation_dir.join("fixed")
    );
    assert_eq!(
        new_file(&extensionless).unwrap(),
        extensionless.dictation_dir.join("fixed-1")
    );
}

#[test]
fn an_ownership_marker_must_be_sixty_four_hex_digits() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nvim.sock.owner");
    assert_eq!(read_marker(&path).unwrap(), None);

    let valid = "0123456789abcdef".repeat(4);
    write_marker(&path, &valid).unwrap();
    assert_eq!(read_marker(&path).unwrap().as_deref(), Some(valid.as_str()));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    for invalid in ["", &valid[1..], &format!("{valid}0"), &"z".repeat(64)] {
        fs::write(&path, invalid).unwrap();
        let error = read_marker(&path).unwrap_err().to_string();
        assert!(error.contains("invalid nvim ownership marker"), "{error}");
    }
}

#[test]
fn the_marker_file_sits_beside_the_socket() {
    assert_eq!(
        marker_path(Path::new("/run/user/1000/spokenpad-nvim.sock")),
        PathBuf::from("/run/user/1000/spokenpad-nvim.sock.owner")
    );
}

#[test]
fn bundled_lua_loads_cleanly_under_the_installed_nvim() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    for file in ["spokenpad.lua", "dictation_init.lua"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/lua")
            .join(file);
        let output = Command::new("nvim")
            .args(["--headless", "-u", "NONE", "-i", "NONE", "-c"])
            .arg(format!("luafile {}", path.display()))
            .args(["-c", "qall!"])
            .env("NVIM_LOG_FILE", directory.path().join("nvim.log"))
            .output()
            .unwrap();
        let complaints = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && complaints.is_empty(),
            "{file} did not load: {} {complaints}",
            output.status
        );
    }
}

#[test]
fn an_indicator_level_is_clamped_into_the_meter_range() {
    // The meter raises its input to a fractional power, so a negative or
    // non-finite level would reach the editor as a NaN bar index.
    assert_eq!(clamp_level(0.5), 0.5);
    assert_eq!(clamp_level(-1.0), 0.0);
    assert_eq!(clamp_level(2.0), 1.0);
    assert_eq!(clamp_level(f64::NAN), 0.0);
    assert_eq!(clamp_level(f64::INFINITY), 0.0);
    assert_eq!(clamp_level(f64::NEG_INFINITY), 0.0);
}

#[test]
fn copy_buffer_sets_the_clipboard_to_exactly_the_buffer_text() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    assert_eq!(
        test_clipboard(&mut session),
        None,
        "clipboard set before any copy"
    );

    session.append("first utterance", false).unwrap();
    session.append("second utterance", false).unwrap();
    let outcome = session.copy_buffer().unwrap();
    assert_eq!(outcome, CopyOutcome::Copied, "{outcome:?}");
    assert_eq!(
        test_clipboard(&mut session).as_deref(),
        Some("first utterance\n\nsecond utterance"),
        "the clipboard must hold the whole buffer, not just the last append"
    );

    // A user edit belongs too: the rule is the whole buffer, not what the
    // daemon itself appended.
    lua(
        &mut session,
        r#"vim.api.nvim_buf_set_lines(Spokenpad.buf, -1, -1, false, { "", "typed by hand" })"#,
    );
    let outcome = session.copy_buffer().unwrap();
    assert_eq!(outcome, CopyOutcome::Copied, "{outcome:?}");
    assert_eq!(
        test_clipboard(&mut session).as_deref(),
        Some("first utterance\n\nsecond utterance\n\ntyped by hand")
    );
}

#[test]
fn copy_buffer_leaves_the_clipboard_untouched_on_an_empty_buffer() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);

    let outcome = session.copy_buffer().unwrap();
    assert_eq!(outcome, CopyOutcome::Empty, "{outcome:?}");
    assert_eq!(
        test_clipboard(&mut session),
        None,
        "an empty buffer must not touch the clipboard"
    );

    // Trailing blank lines only -- still nothing worth copying.
    lua(
        &mut session,
        r#"vim.api.nvim_buf_set_lines(Spokenpad.buf, 0, -1, false, { "", "" })"#,
    );
    let outcome = session.copy_buffer().unwrap();
    assert_eq!(outcome, CopyOutcome::Empty, "{outcome:?}");
    assert_eq!(test_clipboard(&mut session), None);
}

#[test]
fn copy_buffer_reports_a_missing_clipboard_provider() {
    if !nvim_or_skip() {
        return;
    }
    // No provider at all: `setreg('+')` then prints "No provider" and still
    // returns 0, which must not be reported as a copy.
    let directory = tempfile::tempdir().unwrap();
    let mut config = headless(directory.path());
    let last = config.editor.len() - 1;
    config.editor[last] = "let g:loaded_clipboard_provider = 1".to_owned();
    let mut session = NvimSession::new(config);
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    session.append("text to copy", false).unwrap();

    let outcome = session.copy_buffer().unwrap();
    assert!(
        matches!(&outcome, CopyOutcome::Failed(reason) if reason.contains("no clipboard provider")),
        "{outcome:?}"
    );
}

#[test]
fn a_disconnected_session_accepts_indicator_pushes_silently() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = NvimSession::new(headless(directory.path()));
    assert!(!session.connected());
    session
        .set_indicator(&recording("nobody is listening"))
        .unwrap();
    session.close();
    assert!(!session.connected());
}

/// Replays a dictation of several paragraphs, each committed in progressive
/// chunks under a growing preview, and checks after every step that the newest
/// word is on screen. The preview hangs below EOF, where neovim will not
/// scroll by itself; a view that missed it once also stopped following for
/// the rest of the preview. Runs under the bundled init and a global
/// `scrolloff`, which would otherwise pull the view back.
#[test]
fn the_newest_text_stays_visible_across_paragraphs() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut config = headless(directory.path());
    config.editor.push("--cmd".to_owned());
    config.editor.push(format!(
        "luafile {}/src/lua/dictation_init.lua",
        env!("CARGO_MANIFEST_DIR")
    ));
    let mut session = NvimSession::new(config);
    session.ensure().unwrap().expect("an editor");
    let _guard = ProcessGroupGuard::for_session(&session);
    attach_ui(&mut session);
    lua(
        &mut session,
        r#"
vim.go.scrolloff = 3
_G.check = function(needle, step) -- "" when visible, else a report
  vim.cmd("redraw!")
  local rows = {}
  for row = 1, 10 do
    local line = ""
    for col = 1, 40 do line = line .. vim.fn.screenstring(row, col) end
    rows[#rows + 1] = line
    if line:find(needle, 1, true) then return "" end
  end
  return step .. ": " .. needle .. " missing; view=" .. vim.inspect(vim.fn.winsaveview()) .. " pv=" .. vim.inspect(Spokenpad.preview_views) .. "\n" .. table.concat(rows, "\n") .. "\n"
end
_G.failures = ""
return true"#,
    );
    let mut step = 0;
    for utterance in 0..6 {
        let mut continued = false;
        for chunk in 0..3 {
            for grow in 1..5 {
                step += 1;
                let preview = (0..grow * 3)
                    .map(|i| format!("p{utterance}c{chunk}w{i}"))
                    .collect::<Vec<_>>()
                    .join(" ")
                    + &format!(" mark{step}");
                session.set_indicator(&recording(&preview)).unwrap();
                lua(
                    &mut session,
                    &format!(
                        "_G.failures = _G.failures .. check('mark{step}', {step}) return true"
                    ),
                );
            }
            step += 1;
            let text = (0..12)
                .map(|i| format!("u{utterance}c{chunk}w{i}"))
                .collect::<Vec<_>>()
                .join(" ")
                + &format!(" commit{step}");
            session.append(&text, continued).unwrap();
            continued = true;
            lua(
                &mut session,
                &format!("_G.failures = _G.failures .. check('commit{step}', {step}) return true"),
            );
        }
        let mut idle = recording("");
        idle.phase = IndicatorPhase::Idle;
        session.set_indicator(&idle).unwrap();
    }
    let failures = lua(&mut session, "return _G.failures");
    assert_eq!(
        failures.as_str(),
        Some(""),
        "{}",
        failures.as_str().unwrap()
    );
}
