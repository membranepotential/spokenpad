use super::*;
use crate::{
    config::Mode,
    core::session::{Notice, RecordingStatus},
};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::{Child, Stdio},
};

/// The pane is the default, and a session with no X display — Wayland
/// without Xwayland, or `DISPLAY` never imported — cannot open one. It says
/// so, names attach mode, and remembers why for the notification that tells
/// the user where the text went instead.
#[test]
fn a_pane_without_a_display_says_to_use_attach_mode() {
    let directory = tempfile::tempdir().unwrap();
    let config = Nvim {
        display: None,
        socket_path: directory.path().join("nvim.sock"),
        dictation_dir: directory.path().join("dictation"),
        ..Nvim::default()
    };
    assert_eq!(config.mode, Mode::Pane, "the pane is the default mode");
    let mut session = NvimSession::new(config);
    let error = format!("{:#}", session.ensure(Want::Press).unwrap_err());
    assert!(error.contains("needs an X display"), "{error}");
    assert!(error.contains("nvim.mode = \"attach\""), "{error}");
    assert_eq!(session.refused.as_deref(), Some(error.as_str()));
}

/// In pane mode an editor of spokenpad's that no UI shows — what `:restart`
/// leaves on the socket, waiting for a UI the pane never gives it — is not
/// adopted, which would send the dictation where nobody sees it: it is
/// stopped, and a pane is opened instead (here none can, for want of a
/// display).
#[test]
fn a_pane_mode_editor_with_no_window_is_stopped_not_adopted() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = Nvim {
        mode: Mode::Pane,
        display: None,
        ..headless(directory.path())
    };
    let (_, mut invisible) = open_user_editor(&config);
    let mut session = NvimSession::new(config.clone());
    let error = format!("{:#}", session.ensure(Want::Press).unwrap_err());
    assert!(error.contains("needs an X display"), "{error}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while invisible.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the editor no window shows is still running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!config.socket_path.exists(), "its socket is cleared");
}

/// What `:restart` leaves on the socket: a server started with spokenpad's
/// `--cmd` that waits for a UI and so has not run it. It carries no marker
/// yet, and is stopped all the same once it has had `UI_GRACE` to show a
/// UI and has not. `tests/pane_render.rs` runs a real `:restart`.
#[test]
fn a_server_waiting_for_its_first_ui_is_stopped_in_pane_mode() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = Nvim {
        mode: Mode::Pane,
        display: None,
        ..headless(directory.path())
    };
    let (mut waiting, _ui) = waiting_for_a_ui(&config);
    let mut session = NvimSession::new(config.clone());
    let error = format!("{:#}", session.ensure(Want::Press).unwrap_err());
    assert!(error.contains("needs an X display"), "{error}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while waiting.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the server waiting for a UI is still running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!config.socket_path.exists(), "its socket is cleared");
}

/// The same server whose UI attaches within `UI_GRACE`, as the terminal of
/// `spokenpad editor` does right after it starts, is not stopped: the press
/// is refused as it was before the grace existed, and the editor runs on.
#[test]
fn a_starting_editor_that_gains_a_ui_is_not_stopped() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = Nvim {
        mode: Mode::Pane,
        display: None,
        ..headless(directory.path())
    };
    let (mut starting, mut ui) = waiting_for_a_ui(&config);
    let attaching = std::thread::spawn(move || {
        std::thread::sleep(UI_GRACE / 5);
        let attach = Value::Array(vec![
            Value::from(0),
            Value::from(1),
            Value::from("nvim_ui_attach"),
            Value::Array(vec![
                Value::from(40),
                Value::from(10),
                Value::Map(Vec::new()),
            ]),
        ]);
        rmpv::encode::write_value(&mut ui, &attach).unwrap();
        ui
    });
    let mut session = NvimSession::new(config.clone());
    let error = format!("{:#}", session.ensure(Want::Press).unwrap_err());
    assert!(error.contains("refusing unrelated nvim socket"), "{error}");
    let _ui = attaching.join().unwrap();
    assert!(
        starting.0.try_wait().unwrap().is_none(),
        "the editor that gained a UI was stopped"
    );
}

/// A server started as a pane's editor is, with the socket's marker, that
/// waits for its first UI: what `:restart` leaves behind, and what
/// `spokenpad editor` runs until its terminal attaches. Returns the editor
/// and its UI channel, whose output a thread drains.
fn waiting_for_a_ui(config: &Nvim) -> (UserEditor, std::process::ChildStdin) {
    let marker = new_marker().unwrap();
    write_marker(&marker_path(&config.socket_path), &marker).unwrap();
    let mut child = in_a_session_of_its_own(
        Command::new("nvim")
            .args(["--clean", "--embed", "--cmd"])
            .arg(owner_command(&marker))
            .arg("--listen")
            .arg(&config.socket_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )
    .spawn()
    .unwrap();
    let ui = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    std::thread::spawn(move || std::io::copy(&mut output, &mut std::io::sink()));
    let editor = UserEditor(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while RpcClient::connect(&config.socket_path, deadline).is_err() {
        assert!(Instant::now() < deadline, "the server never listened");
        std::thread::sleep(CONNECT_POLL);
    }
    (editor, ui)
}

/// The user manager's display and sockets replace the ones the daemon
/// started with; what the manager does not have, or had to quote, does not.
#[test]
fn the_managers_session_replaces_the_one_the_daemon_started_with() {
    let started = Nvim {
        display: None,
        sway_socket: Some(PathBuf::from("/run/user/1000/sway-ipc.old.sock")),
        runtime_dir: Some(PathBuf::from("/run/user/1000")),
        ..Nvim::default()
    };
    let mut nvim = started.clone();
    apply_manager_session(
        &mut nvim,
        "HOME=/home/someone\nDISPLAY=:1\nXDG_RUNTIME_DIR=\nLANG=$'de_DE.UTF-8'\n",
    );
    assert_eq!(nvim.display.as_deref(), Some(":1"));
    assert_eq!(nvim.sway_socket, started.sway_socket, "not in the listing");
    assert_eq!(
        nvim.runtime_dir, started.runtime_dir,
        "empty in the listing"
    );

    let mut nvim = started.clone();
    apply_manager_session(
        &mut nvim,
        "SWAYSOCK=/run/user/1000/sway-ipc.new.sock\nDISPLAY=$'weird\\n'\n",
    );
    assert_eq!(nvim.display, None, "a quoted value is not taken");
    assert_eq!(
        nvim.sway_socket.as_deref(),
        Some(Path::new("/run/user/1000/sway-ipc.new.sock"))
    );
}

/// Only a path that resolves into the dictation directory is trusted with a
/// transcript: not one that climbs out with `..`, not a symlink inside that
/// points out, and nothing while the directory does not exist.
#[test]
fn only_paths_that_resolve_into_the_dictation_directory_are_inside() {
    let directory = tempfile::tempdir().unwrap();
    let config = Nvim {
        dictation_dir: directory.path().join("dictation"),
        ..Nvim::default()
    };
    let outside = directory.path().join("elsewhere.md");
    fs::write(&outside, "").unwrap();
    assert_eq!(inside_dictation_dir(&config, &outside).unwrap(), None);

    fs::create_dir(&config.dictation_dir).unwrap();
    let file = config.dictation_dir.join("a.md");
    fs::write(&file, "").unwrap();
    assert_eq!(
        inside_dictation_dir(&config, &file).unwrap(),
        Some(file.canonicalize().unwrap())
    );
    let climbing = config.dictation_dir.join("..").join("elsewhere.md");
    assert_eq!(inside_dictation_dir(&config, &climbing).unwrap(), None);
    let link = config.dictation_dir.join("link.md");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert_eq!(inside_dictation_dir(&config, &link).unwrap(), None);
    let missing = config.dictation_dir.join("missing.md");
    assert_eq!(inside_dictation_dir(&config, &missing).unwrap(), None);
}

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

/// A fake `g:clipboard` that never touches the real one: `copy` stores what
/// it was given in a plain Lua global instead of shelling out to xclip/xsel,
/// and `paste` reads it back. Set with `--cmd`, which runs before nvim would
/// otherwise probe for a real provider (`:h g:clipboard`).
const TEST_CLIPBOARD_CMD: &str = r#"lua vim.g.clipboard = { name = "spokenpad-test", copy = { ["+"] = function(lines) _G.spokenpad_test_clipboard = lines end, ["*"] = function(lines) end }, paste = { ["+"] = function() return { _G.spokenpad_test_clipboard or {}, "v" } end, ["*"] = function() return { {}, "v" } end } }"#;

/// The configuration of an editor the test opens itself, the way the user
/// opens one with `spokenpad editor` in attach mode: `nvim --headless`, with
/// no configuration of the user's and a clipboard of its own.
fn headless(directory: &Path) -> Nvim {
    Nvim {
        mode: Mode::Attach,
        editor: vec![
            "nvim".to_owned(),
            "--headless".to_owned(),
            "-u".to_owned(),
            "NONE".to_owned(),
            "-i".to_owned(),
            "NONE".to_owned(),
            "--cmd".to_owned(),
            TEST_CLIPBOARD_CMD.to_owned(),
        ],
        socket_path: directory.join("nvim.sock"),
        dictation_dir: directory.join("dictation"),
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
            Patience::Deadline,
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
            Patience::Deadline,
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

/// `command`, set to start in a session of its own, as the editor inside a
/// pane is: detached from the test's terminal, and the leader of a process
/// group whose id is its pid.
fn in_a_session_of_its_own(command: &mut Command) -> &mut Command {
    // SAFETY: this closure calls only the async-signal-safe `setsid` between
    // fork and exec, and does not capture or allocate.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    }
}

/// An editor the test started, killed with its whole process group and
/// reaped on drop.
struct UserEditor(Child);

impl UserEditor {
    /// Its process group: every editor here is started with
    /// `process_group(0)`, which makes the group's id the editor's pid.
    fn group(&self) -> libc::pid_t {
        self.0.id() as libc::pid_t
    }
}

impl Drop for UserEditor {
    fn drop(&mut self) {
        // SAFETY: the group is the one this test's editor leads (see
        // `group`), and the editor is not reaped until the `wait` below, so
        // its id cannot have been reused; `kill` touches no memory.
        let _ = unsafe { libc::kill(-self.group(), libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

/// What `spokenpad editor` runs, started as a child so the test can reap it.
fn open_user_editor(config: &Nvim) -> (PathBuf, UserEditor) {
    let (mut passage, mut command) = editor_command(config).unwrap();
    passage.keep();
    let child = in_a_session_of_its_own(
        command
            .env("NVIM_LOG_FILE", config.socket_path.with_extension("log"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .spawn()
    .unwrap();
    let guard = UserEditor(child);
    // Until its startup is over, as it is by the time a user who ran
    // `spokenpad editor` presses the key, and as the daemon waits for a pane's
    // editor: a request that reaches Neovim mid-startup is not one the tests
    // below mean to exercise.
    let deadline = Instant::now() + Duration::from_secs(10);
    let started = || {
        let mut client = RpcClient::connect(&config.socket_path, deadline).ok()?;
        query_ownership(&mut client, deadline, Patience::Deadline)
            .ok()?
            .ready
    };
    while started().is_none() {
        assert!(
            Instant::now() < deadline,
            "the user's editor never finished starting"
        );
        std::thread::sleep(CONNECT_POLL);
    }
    (passage.path().to_owned(), guard)
}

/// An editor opened the way `spokenpad editor` opens one, and a session
/// attached to it as the daemon attaches on a key-down: the session, the
/// file it pinned, and the editor.
fn dictating(config: &Nvim) -> (NvimSession, PathBuf, UserEditor) {
    let (opened, editor) = open_user_editor(config);
    let mut session = NvimSession::new(config.clone());
    let path = session
        .ensure(Want::Press)
        .unwrap()
        .expect("the user's editor");
    assert_eq!(
        path, opened,
        "the session pinned the file the editor opened"
    );
    (session, path, editor)
}

/// Start an editor the way a pane starts the one inside it, without the
/// window: the session claims the file and writes the ownership marker, and
/// waits for the editor on its socket, giving up early if it exits.
fn spawn_like_a_pane(session: &mut NvimSession) -> (Result<bool>, UserEditor) {
    let config = session.config.clone();
    let mut fresh = NewFileGuard::claim(&config).unwrap();
    let marker = OwnershipMarker::write(&config.socket_path).unwrap();
    let init = resolve_init(&config).unwrap();
    let argv = editor_argv(&config, fresh.path(), marker.value(), init.as_deref()).unwrap();
    let mut editor = UserEditor(
        Command::new(&argv[0])
            .args(&argv[1..])
            .env("NVIM_LOG_FILE", config.socket_path.with_extension("log"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let attached = session.await_editor(&mut fresh, marker.value(), deadline, || {
        editor.0.try_wait().ok().flatten().is_some()
    });
    if matches!(attached, Ok(true)) {
        marker.keep();
    }
    (attached, editor)
}

#[test]
fn attach_mode_never_spawns_an_editor() {
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let mut session = NvimSession::new(config.clone());
    assert_eq!(session.ensure(Want::Press).unwrap(), None);
    assert!(!config.socket_path.exists());
    assert!(!config.dictation_dir.exists(), "no file before any text");
}

#[test]
fn attach_mode_adopts_the_users_editor_on_the_pending_passage() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let mut session = NvimSession::new(config.clone());
    assert_eq!(session.ensure(Want::Press).unwrap(), None);
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
    let pinned = session
        .ensure(Want::Press)
        .unwrap()
        .expect("the user's editor");
    assert_eq!(pinned, detached.path);
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

/// The daemon may fail to reach an editor the user just opened on the
/// pending passage (a probe that timed out while it started). The commit it
/// then writes directly must not land in the file that editor shows: its
/// buffer would go stale, and the next append's save would stop at nvim's
/// "file changed since reading" prompt or overwrite the direct write.
#[test]
fn a_passage_an_editor_opened_is_never_written_behind_its_back() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let mut session = NvimSession::new(config.clone());
    let first = session
        .append_detached("said with no editor", false)
        .unwrap();
    let (opened, _editor) = open_user_editor(&config);
    assert_eq!(opened, first.path);

    let second = session
        .append_detached("said while the editor started", false)
        .unwrap();
    assert_ne!(
        second.path, first.path,
        "the editor's file was written behind its back"
    );
    assert!(second.started);
    assert_eq!(
        fs::read_to_string(&first.path).unwrap(),
        "said with no editor\n"
    );

    assert_eq!(
        session.ensure(Want::Press).unwrap(),
        Some(first.path.clone())
    );
    session.append("said into the editor", false).unwrap();
    assert_eq!(
        fs::read_to_string(&first.path).unwrap(),
        "said with no editor\n\nsaid into the editor\n"
    );
    assert_eq!(
        fs::read_to_string(&second.path).unwrap(),
        "said while the editor started\n"
    );
    assert_eq!(
        passage::pending(&config),
        Some(second.path),
        "the passage no editor has shown stays pending"
    );
}

/// An editor that took the pending passage and then never started gives it
/// back, so the next editor still opens on it.
#[test]
fn an_editor_that_never_starts_leaves_the_passage_pending() {
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let session = NvimSession::new(config.clone());
    let first = session.append_detached("kept", false).unwrap();
    let (taken, _command) = editor_command(&config).unwrap();
    assert_eq!(taken.path(), first.path);
    assert_eq!(passage::pending(&config), None, "the passage is taken");
    drop(taken);
    assert_eq!(passage::pending(&config), Some(first.path.clone()));
    assert_eq!(fs::read_to_string(&first.path).unwrap(), "kept\n");
}

#[test]
fn a_second_editor_is_refused_while_one_is_listening() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
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
    let (mut session, path, _editor) = dictating(&headless(directory.path()));
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
        ("first \n", "more ", true),
        ("first\t\n", "more", true),
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
    let (mut first, path, _editor) = dictating(&config);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(first.append("$(touch nope)\nGrüße 東京", false).unwrap(), 2);
    first.close();

    let mut restarted = NvimSession::new(config);
    assert_eq!(
        restarted.ensure(Want::Press).unwrap().expect("an editor"),
        path
    );
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
    let (mut session, path, _editor) = dictating(&headless(directory.path()));

    assert_eq!(session.append("first\nsecond\n", false).unwrap(), 3);
    assert_eq!(session.append("", true).unwrap(), 2);
    assert_eq!(fs::read_to_string(path).unwrap(), "first\nsecond\n");
}

/// The editor inside a pane is adopted only once its ownership marker is set
/// and the user's own startup, `VimEnter` handlers included, has finished.
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
        "--headless".to_owned(),
        "-i".to_owned(),
        "NONE".to_owned(),
        "--cmd".to_owned(),
        "lua vim.wait(250)".to_owned(),
    ];
    config.init = Some(init);
    let mut session = NvimSession::new(config);
    let (attached, _editor) = spawn_like_a_pane(&mut session);
    assert!(attached.unwrap(), "the editor never answered");

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
    let (mut session, path, _editor) = dictating(&config);
    attach_ui(&mut session);

    session
        .set_indicator(&recording("draft words"), PreviewPlacement::NewParagraph)
        .unwrap();
    lua(
        &mut session,
        r#"
local buf = Spokenpad.buf
local marks = vim.api.nvim_buf_get_extmarks(buf, Spokenpad.ns, 0, -1, { details = true })
assert(#marks == 1, "preview must use exactly one extmark")
local row, col, details = marks[1][2], marks[1][3], marks[1][4]
assert(row == 0 and col == 0, "an empty buffer's preview is not at the start of line 1")
assert(details.virt_lines == nil, "the preview hangs virtual lines")
assert(details.virt_text[1][1] == "draft words", "an empty buffer's preview has no blank row")
assert(details.virt_text_pos == "inline", "the preview is not inline")
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
    assert_eq!(
        reloaded.ensure(Want::Press).unwrap().expect("an editor"),
        path
    );
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
    reloaded
        .set_indicator(&recording("next draft"), PreviewPlacement::NewParagraph)
        .unwrap();
    lua(
        &mut reloaded,
        r#"
local marks = vim.api.nvim_buf_get_extmarks(
  Spokenpad.buf, Spokenpad.ns, 0, -1, { details = true }
)
assert(#marks == 1, "preview after committed text must use exactly one extmark")
local row, col, details = marks[1][2], marks[1][3], marks[1][4]
assert(row == 0 and col == #"committed", "a new paragraph's preview is not after the text")
assert(details.virt_text_pos == "inline", "the preview is not inline")
-- The rest of the row "committed" is on, a blank row, then the preview.
local drawn = details.virt_text[1][1]
assert(drawn == string.rep(" ", 40 - #"committed" + 40) .. "next draft", vim.inspect(drawn))
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
        .set_indicator(
            &recording("growing preview keeps its newest tailmarker visible"),
            PreviewPlacement::Continuation,
        )
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
-- The daemon's next snapshot: the same indicator with a newer preview.
local function push_preview(text)
  Spokenpad.push(vim.tbl_extend("force", Spokenpad.state, { preview = text }))
  assert(Spokenpad.last_error == nil, Spokenpad.last_error)
end
push_preview("a longer replacement preview still follows through to growthmarker")
vim.cmd("redraw!")
assert(screen_contains("growthmarker"), "growing preview stopped following before commit")

vim.api.nvim_win_call(0, function()
  vim.cmd("normal! gg0zt")
end)
local before = vim.fn.winsaveview()
push_preview("do not snap back to moved reader")
vim.cmd("redraw!")
local after = vim.fn.winsaveview()
assert(vim.deep_equal(before, after), "preview update snapped a moved reader to the end")
assert(not screen_contains("do not snap"), "moved reader unexpectedly followed preview")
push_preview("still do not snap on a later update")
vim.cmd("redraw!")
assert(vim.deep_equal(before, vim.fn.winsaveview()), "later preview update resumed following")
assert(not screen_contains("still do not snap"), "later preview update snapped back")
return true
"#,
    );
}

/// Every cell of the 40x10 screen, one line per row.
fn screen(session: &mut NvimSession) -> String {
    let rows = lua(
        session,
        r#"
vim.cmd("redraw!")
local rows = {}
for row = 1, vim.o.lines do
  local line = ""
  for col = 1, vim.o.columns do
    line = line .. vim.fn.screenstring(row, col)
  end
  rows[#rows + 1] = line
end
return table.concat(rows, "\n")
"#,
    );
    rows.as_str().expect("the screen as text").to_owned()
}

/// The preview is drawn exactly where its text lands: a preview, then the
/// append of the same text, leaves every cell of the screen as it was, and
/// only the highlight changes. Cases: an empty buffer (no blank row), a new
/// paragraph (one blank row), a continued line with the space the append
/// adds and without one where the line already ends in a space, and the
/// wrapping 'linebreak' does that virtual text does not do by itself: a word
/// that would end in the last column followed by a blank, breaks after
/// punctuation, a word longer than a row, and double-width characters.
#[test]
fn the_preview_is_drawn_exactly_where_its_text_lands() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let (mut session, path, _editor) = dictating(&headless(directory.path()));
    attach_ui(&mut session);
    use PreviewPlacement::{Continuation, NewParagraph};
    let cases: &[(&[&str], &str, PreviewPlacement)] = &[
        (&[], "first words of a fresh file", NewParagraph),
        (&["committed text"], "a new paragraph", NewParagraph),
        (&["committed text"], "continues it", Continuation),
        (&["ends in a space "], "and goes on", Continuation),
        (
            &["0123456789"],
            "the-word-that-ends-in-column-40 and more after it",
            Continuation,
        ),
        (
            &["a line of thirty-three cells, ok."],
            "then well-known words, e.g. co-op/and/or, wrap after punctuation too",
            Continuation,
        ),
        (
            &[],
            "a supercalifragilisticexpialidocious-and-still-more-letters-than-a-row word",
            NewParagraph,
        ),
        (
            &["東京"],
            "大阪 名古屋 京都 札幌 福岡 神戸 横浜 仙台 広島 金沢 長崎 那覇",
            Continuation,
        ),
        // "広" would start in the last cell, so it starts the next row after
        // a filler cell; the 35 x's then end one cell short of the edge only
        // when that cell is counted, and move to the row after.
        (
            &["committed"],
            "a東京大阪名古屋京都札幌福岡神戸横浜仙台広島 xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx end",
            NewParagraph,
        ),
    ];
    for (committed, text, placement) in cases {
        let label = format!("{committed:?} then {text:?} ({placement:?})");
        lua(
            &mut session,
            "vim.api.nvim_buf_set_lines(Spokenpad.buf, 0, -1, false, {}) return true",
        );
        for line in *committed {
            session.append(line, false).unwrap();
        }
        let bare = screen(&mut session);
        session.set_indicator(&recording(text), *placement).unwrap();
        let previewed = screen(&mut session);
        let file = fs::read_to_string(&path).unwrap();
        assert!(
            !file.contains(text),
            "{label}: the preview reached the file"
        );
        session
            .append(text, *placement == PreviewPlacement::Continuation)
            .unwrap();
        let landed = screen(&mut session);
        assert_ne!(bare, previewed, "{label}: no preview drawn\n{bare}");
        assert_eq!(
            previewed, landed,
            "{label}: the text did not land where its preview was"
        );
        let marks = lua(
            &mut session,
            "return #vim.api.nvim_buf_get_extmarks(Spokenpad.buf, Spokenpad.ns, 0, -1, {})",
        );
        assert_eq!(
            marks.as_u64(),
            Some(0),
            "{label}: the landed preview stayed"
        );
    }
}

/// A window resized under a preview -- a tiled pane when a window opens
/// beside it -- has its view moved by nvim, not by the reader, and the
/// preview keeps following. Taken for a reader who scrolled away, the resize
/// stopped the preview following, with the end of the text off the screen.
#[test]
fn a_resized_window_keeps_following_the_preview() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let (mut session, _, _editor) = dictating(&headless(directory.path()));
    attach_ui(&mut session);
    let words = |prefix: &str, count: usize| {
        (0..count)
            .map(|index| format!("{prefix}{index}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    session.append(&words("said", 40), false).unwrap();
    lua(&mut session, "vim.o.cmdheight = 3 return true");
    session
        .set_indicator(
            &recording(&(words("tail", 12) + " first")),
            PreviewPlacement::Continuation,
        )
        .unwrap();
    let moved = lua(
        &mut session,
        r#"
vim.cmd("redraw!")
local before = vim.fn.winsaveview()
vim.o.cmdheight = 1
vim.cmd("redraw!")
local after = vim.fn.winsaveview()
return before.topline ~= after.topline or before.skipcol ~= after.skipcol
"#,
    );
    assert_eq!(
        moved.as_bool(),
        Some(true),
        "the resize left the view alone"
    );
    session
        .set_indicator(
            &recording(&(words("tail", 14) + " second")),
            PreviewPlacement::Continuation,
        )
        .unwrap();
    let shown = screen(&mut session);
    assert!(
        shown.contains("second"),
        "the preview stopped following:\n{shown}"
    );
}

/// The daemon sends a preview again only when it changes, and the
/// transcribing phase holds one for a second or more: a window resized under
/// it lays it out again itself, for its new width.
#[test]
fn a_resized_window_lays_the_preview_out_again() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let (mut session, _, _editor) = dictating(&headless(directory.path()));
    set_columns(&mut session, 40);
    session.append("committed text", false).unwrap();
    let text = "a preview that wraps at other words in thirty columns than in forty";
    session
        .set_indicator(&recording(text), PreviewPlacement::Continuation)
        .unwrap();
    set_columns(&mut session, 30);
    let previewed = screen(&mut session);
    session.append(text, true).unwrap();
    assert_eq!(previewed, screen(&mut session));
}

#[test]
fn an_idle_phase_deletes_the_preview_and_a_level_push_leaves_the_buffer_alone() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let (mut session, _, _editor) = dictating(&headless(directory.path()));
    session.append("committed", false).unwrap();
    session
        .set_indicator(
            &recording("provisional tail"),
            PreviewPlacement::NewParagraph,
        )
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
        session
            .set_indicator(&state, PreviewPlacement::NewParagraph)
            .unwrap();
    }
    let after = lua(
        &mut session,
        "return { vim.b[Spokenpad.buf].changedtick, vim.fn.winsaveview().topline }",
    );
    assert_eq!(
        before, after,
        "a level-only push changed the buffer or the view"
    );
    let meter_lit = "for _, level in ipairs(Spokenpad.levels) do \
                     if level ~= 0 then return true end end return false";
    assert_eq!(lua(&mut session, meter_lit).as_bool(), Some(true));

    session
        .set_indicator(&IndicatorState::default(), PreviewPlacement::NewParagraph)
        .unwrap();
    let marks = lua(
        &mut session,
        "return #vim.api.nvim_buf_get_extmarks(Spokenpad.buf, Spokenpad.ns, 0, -1, {})",
    );
    assert_eq!(marks.as_u64(), Some(0), "idle left the preview extmark");
    assert_eq!(
        lua(&mut session, meter_lit).as_bool(),
        Some(false),
        "the meter stayed lit after recording ended"
    );
    assert_eq!(
        fs::read_to_string(session.path().unwrap()).unwrap(),
        "committed\n"
    );
}

/// `setup` is called on every (re)connection, including one that happens
/// mid-recording after the pinned buffer was lost. The daemon then pushes the
/// indicator it already had, and a push redraws only what changed -- so
/// `setup` itself has to put the live preview into the buffer it just pinned.
#[test]
fn re_pinning_mid_recording_moves_the_preview_to_the_new_buffer() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let (mut session, _, _editor) = dictating(&headless(directory.path()));
    session
        .set_indicator(
            &recording("half a sentence"),
            PreviewPlacement::NewParagraph,
        )
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
        .set_indicator(
            &recording("half a sentence"),
            PreviewPlacement::NewParagraph,
        )
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
    let (mut session, _, _editor) = dictating(&headless(directory.path()));
    session.append("committed", false).unwrap();

    let mut idle = IndicatorState {
        notice: Some(held.clone()),
        ..IndicatorState::default()
    };
    // Wide enough for the whole notice beside the dated file name; how a
    // narrow window fits one is
    // `a_long_notice_keeps_the_phase_label_and_its_headline_in_a_narrow_window`.
    set_columns(&mut session, 120);
    session
        .set_indicator(&idle, PreviewPlacement::NewParagraph)
        .unwrap();
    let bar = rendered_winbar(&mut session);
    assert!(
        bar.contains(held.headline.as_ref()) && bar.contains(held.detail.as_ref()),
        "an idle notice is invisible, which is the whole defect: {bar}"
    );

    idle.notice = None;
    session
        .set_indicator(&idle, PreviewPlacement::NewParagraph)
        .unwrap();
    let bar = rendered_winbar(&mut session);
    assert!(bar.contains("spokenpad"), "the idle winbar is gone: {bar}");
    assert!(
        !bar.contains(held.headline.as_ref()),
        "a cleared notice stayed up: {bar}"
    );

    let mut live = recording("provisional tail");
    live.notice = Some(gap.clone());
    session
        .set_indicator(&live, PreviewPlacement::NewParagraph)
        .unwrap();
    let bar = winbar(&mut session);
    assert!(
        bar.contains("REC") && bar.contains(gap.headline.as_ref()),
        "{bar}"
    );
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
        !marks.contains(gap.headline.as_ref()),
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
    let (mut session, _, _editor) = dictating(&headless(directory.path()));
    let notice = Notice::MemoryCap {
        recovery: RecordingStatus::Recorded("/tmp/spokenpad/capture-example.wav".into()),
        minutes: 60,
    };
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
        session
            .set_indicator(state, PreviewPlacement::NewParagraph)
            .unwrap();
        let bar = rendered_winbar(&mut session);
        let phase = if state.phase == IndicatorPhase::Recording {
            "REC"
        } else {
            "spokenpad"
        };
        assert!(
            bar.contains(phase) && bar.contains(notice.headline.as_ref()),
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
    session
        .set_indicator(&capped, PreviewPlacement::NewParagraph)
        .unwrap();
    let bar = rendered_winbar(&mut session);
    assert!(
        bar.contains(notice.headline.as_ref()) && bar.contains(notice.detail.as_ref()),
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
    let (mut session, _, _editor) = dictating(&headless(directory.path()));
    session
        .set_indicator(&recording(""), PreviewPlacement::NewParagraph)
        .unwrap();
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
Spokenpad.push(vim.tbl_extend("force", Spokenpad.state, { level = 0.1 }))
assert(Spokenpad.last_error == nil, Spokenpad.last_error)
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
    let (mut session, _, _editor) = dictating(&headless(directory.path()));
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
return { adopted, vim.o.laststatus, vim.o.showtabline, vim.o.autowriteall }
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
    assert_eq!(
        adopted[3].as_bool(),
        Some(false),
        "'autowriteall' would write the transcript with the user's autocommands"
    );
}

/// Waits until `path` holds `expected`, and says what it held if it never
/// does.
#[track_caller]
fn wait_for_file(path: &Path, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let held = fs::read_to_string(path).unwrap();
        if held == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} holds {held:?}, not {expected:?}",
            path.display()
        );
        std::thread::sleep(CONNECT_POLL);
    }
}

/// The dictation file is a scratch pad: whatever the user types into it is
/// on disk at once, in Insert mode keystroke by keystroke, with no `:w`. The
/// write is spokenpad's own, so a format-on-save in the user's configuration
/// never runs on a transcript, and no other buffer is ever written by it.
#[test]
fn typing_into_the_dictation_buffer_saves_it_at_once() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut config = headless(directory.path());
    // A format-on-save, as a user's configuration may have one.
    config.editor.extend([
        "--cmd".to_owned(),
        "autocmd BufWritePre * let g:formatted = get(g:, 'formatted', 0) + 1".to_owned(),
    ]);
    let (mut session, path, _editor) = dictating(&config);
    session.append("dictated", false).unwrap();

    // Still in Insert mode: every keystroke is written.
    type_into(&session, "Go typed");
    wait_for_file(&path, "dictated\n typed\n");
    // A change in Normal mode.
    type_into(&session, "<Esc>ggdd");
    wait_for_file(&path, " typed\n");
    let state = lua(
        &mut session,
        "return { vim.bo[Spokenpad.buf].modified, vim.g.formatted or 0 }",
    );
    let state = state.as_array().unwrap();
    assert_eq!(
        state[0].as_bool(),
        Some(false),
        "the buffer is still unsaved"
    );
    assert_eq!(
        state[1].as_i64(),
        Some(0),
        "the user's format-on-save ran on the transcript"
    );

    // Another file open in the same editor is the user's to save.
    let other = directory.path().join("other.txt");
    fs::write(&other, "untouched\n").unwrap();
    type_into(&session, &format!(":split {}<CR>ggdd", other.display()));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fs::read_to_string(&other).unwrap(), "untouched\n");
    assert_eq!(fs::read_to_string(&path).unwrap(), " typed\n");
}

/// Leaving the dictation buffer — `:edit`, typed in one go with a change, as
/// a mapping or a macro runs it — writes it first, with spokenpad's own
/// write: the user's format-on-save does not run on it. Also with the user's
/// `set nohidden`.
#[test]
fn leaving_an_edited_dictation_buffer_writes_it_without_the_users_autocommands() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut config = headless(directory.path());
    let formatted = directory.path().join("formatted");
    // With 'nohidden' Neovim refuses to abandon a modified buffer before
    // BufLeave could write it, unless the buffer hides itself.
    config.editor.extend([
        "--cmd".to_owned(),
        "set nohidden".to_owned(),
        "--cmd".to_owned(),
        format!(
            "autocmd BufWritePre * call writefile(['ran'], '{}')",
            formatted.display()
        ),
    ]);
    let other = directory.path().join("other.txt");
    fs::write(&other, "other\n").unwrap();
    let (mut session, path, _editor) = dictating(&config);
    session.append("dictated", false).unwrap();
    session.append("second", false).unwrap();
    // A change whose own write has not run yet, as with typeahead.
    lua(
        &mut session,
        "vim.o.eventignore = 'TextChanged,TextChangedI,TextChangedP,InsertLeave'",
    );

    type_into(&session, &format!("ggdd:edit {}<CR>", other.display()));
    wait_for_file(&path, "\nsecond\n");
    assert!(
        !formatted.exists(),
        "leaving the buffer ran the user's format-on-save on the transcript"
    );
    assert_eq!(
        lua(&mut session, "return vim.fn.expand('%:t')").as_str(),
        Some("other.txt"),
        "the editor did not move to the other file"
    );
}

/// `:q` on a dictation buffer the user has just edited writes it and quits,
/// also when the change has not been saved on its own yet -- a typist faster
/// than the change events, simulated by ignoring them -- and with
/// spokenpad's own write rather than one that runs the user's autocommands.
#[test]
fn quitting_an_edited_dictation_buffer_writes_it_and_quits() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut config = headless(directory.path());
    let formatted = directory.path().join("formatted");
    config.editor.extend([
        "--cmd".to_owned(),
        format!(
            "autocmd BufWritePre * call writefile(['ran'], '{}')",
            formatted.display()
        ),
    ]);
    let (mut session, path, mut editor) = dictating(&config);
    session.append("dictated", false).unwrap();
    lua(
        &mut session,
        "vim.o.eventignore = 'TextChanged,TextChangedI,TextChangedP,InsertLeave'",
    );

    type_into(&session, "A and edited<Esc>:q<CR>");
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = editor.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "`:q` did not quit the editor");
        std::thread::sleep(CONNECT_POLL);
    };
    assert!(status.success(), "the editor quit with {status}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "dictated and edited\n");
    assert!(
        !formatted.exists(),
        "the write on `:q` ran the user's format-on-save"
    );
}

#[test]
fn an_append_while_the_user_types_keeps_both_texts_and_saves() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let (mut session, path, _editor) = dictating(&headless(directory.path()));
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
    let (mut session, path, _editor) = dictating(&headless(directory.path()));
    session.append("first", false).unwrap();
    lua(&mut session, "vim.bo[Spokenpad.buf].readonly = true");

    assert!(session.append("must roll back", false).is_err());
    assert!(
        !session.connected(),
        "a failed append left its client installed"
    );
    session.ensure(Want::Press).unwrap().expect("an editor");
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
    let (mut session, path, _editor) = dictating(&headless(directory.path()));
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
    let (mut session, path, _editor) = dictating(&headless(directory.path()));
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
    let (mut session, path, _editor) = dictating(&headless(directory.path()));
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
    let (mut session, path, editor) = dictating(&headless(directory.path()));
    drop(editor);

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
    let (mut session, first, _editor) = dictating(&headless(directory.path()));
    session.append("before", false).unwrap();

    lua(
        &mut session,
        "vim.api.nvim_buf_delete(Spokenpad.buf, { force = true })",
    );
    let second = session.ensure(Want::Press).unwrap().expect("an editor");
    assert_ne!(second, first, "ensure re-used a buffer that is gone");
    session.append("after", false).unwrap();
    assert_eq!(fs::read_to_string(&first).unwrap(), "before\n");
    assert_eq!(fs::read_to_string(&second).unwrap(), "after\n");
}

#[test]
fn a_socket_file_with_no_listener_is_cleared_for_the_next_editor() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    let abandoned = UnixListener::bind(&config.socket_path).unwrap();
    drop(abandoned);
    assert!(config.socket_path.exists());

    let mut session = NvimSession::new(config.clone());
    assert_eq!(session.ensure(Want::Press).unwrap(), None);
    assert!(
        !config.socket_path.exists(),
        "the stale socket is still there"
    );
    let (path, _editor) = open_user_editor(&config);
    assert_eq!(session.ensure(Want::Press).unwrap(), Some(path.clone()));
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
    let child = Command::new("nvim")
        .args(["--headless", "-u", "NONE", "--listen"])
        .arg(&config.socket_path)
        .args(["-i", "NONE"])
        .env("NVIM_LOG_FILE", directory.path().join("nvim.log"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let _editor = UserEditor(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !config.socket_path.exists() && Instant::now() < deadline {
        std::thread::sleep(CONNECT_POLL);
    }
    let mut session = NvimSession::new(config);
    let error = session.ensure(Want::Press).unwrap_err().to_string();
    assert!(error.contains("unrelated nvim socket"), "{error}");
}

#[test]
fn ordinary_file_at_socket_path_is_refused_and_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let config = headless(directory.path());
    fs::write(&config.socket_path, b"not a socket").unwrap();
    let mut session = NvimSession::new(config.clone());

    let error = session.ensure(Want::Press).unwrap_err().to_string();
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

    let error = session.ensure(Want::Press).unwrap_err().to_string();
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
    let mut session = NvimSession::new(config.clone());

    let (attached, _editor) = spawn_like_a_pane(&mut session);
    let error = attached.unwrap_err().to_string();
    assert!(error.contains("exited during startup"), "{error}");
    assert!(
        !marker_path(&config.socket_path).exists(),
        "a marker names an editor that never started"
    );
    let leftovers: Vec<_> = fs::read_dir(&config.dictation_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(leftovers.is_empty(), "spawn left {leftovers:?} behind");
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
        Value::Array(vec![Value::from("nvim"), Value::from("--embed")]),
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
            argv: vec!["nvim".to_owned(), "--embed".to_owned()],
        }
    );

    let argv = || Value::Array(Vec::new());
    let bare = Value::Array(vec![nil(), nil(), nil(), nil(), nil(), nil(), argv()]);
    assert_eq!(
        parse_ownership(&bare).unwrap(),
        Ownership {
            marker: None,
            ready: None,
            pinned: None,
            startup: None,
            argv: Vec::new(),
        }
    );

    for malformed in [
        Value::from(1),
        Value::Array(vec![nil(), nil()]),
        Value::Array(vec![nil(), nil(), nil(), nil(), nil(), nil()]),
        // A buffer with no name is not an adoptable buffer.
        Value::Array(vec![
            nil(),
            nil(),
            Value::from(3),
            nil(),
            nil(),
            nil(),
            argv(),
        ]),
        Value::Array(vec![
            nil(),
            nil(),
            nil(),
            nil(),
            Value::from(3),
            nil(),
            argv(),
        ]),
        Value::Array(vec![
            Value::from(1),
            nil(),
            nil(),
            nil(),
            nil(),
            nil(),
            argv(),
        ]),
        Value::Array(vec![nil(), nil(), nil(), nil(), nil(), nil(), nil()]),
        Value::Array(vec![
            nil(),
            nil(),
            nil(),
            nil(),
            nil(),
            nil(),
            Value::Array(vec![Value::from(1)]),
        ]),
    ] {
        assert!(
            parse_ownership(&malformed).is_err(),
            "accepted {malformed:?}"
        );
    }
}

/// An editor is spokenpad's by its command line as soon as it runs: the
/// `--cmd` spokenpad started it with, carrying this socket's marker.
#[test]
fn an_editor_is_started_with_the_marker_its_command_line_carries() {
    let config = Nvim {
        socket_path: PathBuf::from("/run/spokenpad.sock"),
        ..Nvim::default()
    };
    let marker = "f".repeat(64);
    let started = |argv: Vec<String>| Ownership {
        marker: None,
        ready: None,
        pinned: None,
        startup: None,
        argv,
    };
    let argv = editor_argv(&config, Path::new("/t.md"), &marker, None).unwrap();
    assert!(started(argv.clone()).started_with(&marker));
    assert!(!started(argv.clone()).started_with(&"e".repeat(64)));
    let as_file: Vec<String> = argv
        .into_iter()
        .map(|argument| match argument.as_str() {
            "--cmd" => "-c".to_owned(),
            _ => argument,
        })
        .collect();
    assert!(
        !started(as_file).started_with(&marker),
        "only a `--cmd` sets the marker before startup"
    );
}

#[test]
fn editor_argv_names_the_socket_and_refuses_an_unquotable_colorscheme() {
    let mut config = Nvim {
        socket_path: PathBuf::from("/run/spokenpad.sock"),
        ..Nvim::default()
    };
    let argv = editor_argv(
        &config,
        Path::new("/state/dictation/today.md"),
        &"f".repeat(64),
        Some(Path::new("/state/private/dictation_init.lua")),
    )
    .unwrap();
    assert_eq!(argv[0], "nvim");
    assert!(argv.contains(&format!("let g:spokenpad_owner = '{}'", "f".repeat(64))));
    assert_eq!(argv[argv.len() - 5], "--listen");
    assert_eq!(argv[argv.len() - 4], "/run/spokenpad.sock");
    assert_eq!(argv[argv.len() - 1], "/state/dictation/today.md");
    let init = argv.iter().position(|argument| argument == "-u").unwrap();
    assert_eq!(argv[init + 1], "/state/private/dictation_init.lua");

    config.colorscheme = Some("tokyonight-moon".to_owned());
    let themed = editor_argv(&config, Path::new("/t.md"), "f", None).unwrap();
    assert!(
        themed
            .iter()
            .any(|argument| argument.contains("SpokenpadColorscheme('tokyonight-moon', false)")),
        "{themed:?}"
    );

    for hostile in ["", "x'); os.execute('rm -rf ~", "scheme\nname", "sch eme"] {
        config.colorscheme = Some(hostile.to_owned());
        let error = editor_argv(&config, Path::new("/t.md"), "f", None)
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

/// The only clipboard write is the opt-in whole-buffer copy: the bundled
/// init does not route every yank and delete to the system clipboard.
#[test]
fn the_bundled_init_leaves_yanks_off_the_clipboard() {
    if !nvim_or_skip() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let init = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lua/dictation_init.lua");
    let output = Command::new("nvim")
        .args(["--headless", "-i", "NONE", "-u"])
        .arg(&init)
        .args(["-c", "lua io.stdout:write(vim.o.clipboard)", "-c", "qall!"])
        .env("NVIM_LOG_FILE", directory.path().join("nvim.log"))
        .env("XDG_CONFIG_HOME", directory.path())
        .env("XDG_DATA_HOME", directory.path())
        .env("XDG_STATE_HOME", directory.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", output.status);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
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
    let (mut session, _, _editor) = dictating(&headless(directory.path()));
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
    let (mut session, _, _editor) = dictating(&headless(directory.path()));

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
    let (mut session, _, _editor) = dictating(&config);
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
        .set_indicator(
            &recording("nobody is listening"),
            PreviewPlacement::NewParagraph,
        )
        .unwrap();
    session.close();
    assert!(!session.connected());
}

/// Replays a dictation of several paragraphs, each committed in progressive
/// chunks under a growing preview, and checks after every step that the newest
/// word is on screen, and that nvim's next redraw keeps the view the preview
/// set: a view nvim corrects reads as a reader who moved away, and the preview
/// stopped following for the rest of it. That happened for a view computed
/// with the winbar counted as a text row, and for every resize, which the
/// replay does every fifth step, as a tiled pane is resized when a window
/// opens beside it. Runs under the bundled init and a global `scrolloff`,
/// which would otherwise pull the view back.
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
    let (mut session, _, _editor) = dictating(&config);
    attach_ui(&mut session);
    lua(
        &mut session,
        r#"
vim.go.scrolloff = 3
_G.check = function(needle, step) -- "" when visible, else a report
  vim.cmd("redraw!")
  local win = vim.api.nvim_get_current_win()
  local left, view = Spokenpad.preview_views[win], vim.fn.winsaveview()
  if type(left) == "table" and (left.topline ~= view.topline or left.skipcol ~= view.skipcol) then
    return step .. ": the redraw moved the view the preview set, from " .. vim.inspect(left)
      .. " to " .. vim.inspect(view) .. "\n"
  end
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
            let placement = if continued {
                PreviewPlacement::Continuation
            } else {
                PreviewPlacement::NewParagraph
            };
            for grow in 1..5 {
                step += 1;
                if step % 5 == 0 {
                    lua(
                        &mut session,
                        "vim.o.cmdheight = 4 - vim.o.cmdheight return true",
                    );
                }
                let preview = (0..grow * 3)
                    .map(|i| format!("p{utterance}c{chunk}w{i}"))
                    .collect::<Vec<_>>()
                    .join(" ")
                    + &format!(" mark{step}");
                session
                    .set_indicator(&recording(&preview), placement)
                    .unwrap();
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
        session
            .set_indicator(&idle, PreviewPlacement::NewParagraph)
            .unwrap();
    }
    let failures = lua(&mut session, "return _G.failures");
    assert_eq!(
        failures.as_str(),
        Some(""),
        "{}",
        failures.as_str().unwrap()
    );
}

#[test]
fn a_held_call_is_waited_for_up_to_the_cap_and_never_while_stopping() {
    assert_eq!(held_verdict(Duration::ZERO, false), HeldVerdict::Wait);
    assert_eq!(
        held_verdict(HELD_AT_MOST - Duration::from_millis(1), false),
        HeldVerdict::Wait
    );
    assert_eq!(held_verdict(HELD_AT_MOST, false), HeldVerdict::GiveUp);
    assert_eq!(held_verdict(Duration::ZERO, true), HeldVerdict::Abandon);
    assert_eq!(held_verdict(HELD_AT_MOST, true), HeldVerdict::Abandon);
}

/// Keys typed into the editor over its socket, as a user at its keyboard
/// would: `nvim_input` is answered even while a command is half typed.
fn type_into(session: &NvimSession, keys: &str) {
    let status = Command::new("nvim")
        .arg("--server")
        .arg(&session.config.socket_path)
        .args(["--remote-send", keys])
        .status()
        .expect("run nvim as a client");
    assert!(status.success(), "typing {keys:?} failed");
}

/// Stop (`SIGSTOP`) or continue the editor's whole process group.
fn freeze(editor: &UserEditor, frozen: bool) {
    let signal = if frozen { libc::SIGSTOP } else { libc::SIGCONT };
    // SAFETY: the group is the one the editor this test spawned leads, which
    // `kill` only signals; no memory is involved.
    assert_eq!(unsafe { libc::kill(-editor.group(), signal) }, 0);
}

/// Once the daemon is stopping, an append is given up on well within the
/// shutdown grace however the editor fails to answer, and never reconnected
/// and repeated: held behind a half-typed command or frozen, with the flag
/// set before the append is sent or while it waits.
#[test]
fn a_stopping_daemon_gives_up_on_an_append_within_the_grace() {
    if !nvim_or_skip() {
        return;
    }
    // Leaves the pane's teardown its room inside `SHUTDOWN_GRACE`.
    const BUDGET: Duration = Duration::from_millis(800);
    for (frozen, flag_first) in [(false, true), (false, false), (true, true), (true, false)] {
        let directory = tempfile::tempdir().unwrap();
        let (mut session, path, editor) = dictating(&headless(directory.path()));
        if frozen {
            freeze(&editor, true);
        } else {
            type_into(&session, "2");
        }
        let quitting = session.quitting();
        let label = format!(
            "a {} editor, the flag set {} the append",
            if frozen { "frozen" } else { "held" },
            if flag_first { "before" } else { "during" }
        );
        let (result, flagged) = if flag_first {
            quitting.store(true, Ordering::Release);
            let flagged = Instant::now();
            (session.append("given up", false), flagged)
        } else {
            let setter = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                quitting.store(true, Ordering::Release);
                Instant::now()
            });
            let result = session.append("given up", false);
            (result, setter.join().unwrap())
        };
        let after_flag = flagged.elapsed();
        println!(
            "{label}: given up {} ms after the flag",
            after_flag.as_millis()
        );
        assert!(
            matches!(result, Err(AppendFailure::Unconfirmed(_))),
            "{label}: {result:?}"
        );
        assert!(after_flag < BUDGET, "{label}: took {after_flag:?}");
        assert!(!session.connected(), "{label}: still connected");
        if frozen {
            freeze(&editor, false);
        }
        type_into(&session, "<Esc>");
        // A held request is dropped with its connection. A frozen editor
        // reads the request and the close together when it continues, and
        // usually runs the request first: the documented case where the text
        // is in the window as well as the pending passage. On a loaded
        // machine it was also seen to drop it, as it drops a held one. Never
        // twice in either.
        std::thread::sleep(Duration::from_millis(300));
        let written = fs::read_to_string(&path).unwrap();
        let possible: &[&str] = if frozen { &["given up\n", ""] } else { &[""] };
        assert!(possible.contains(&written.as_str()), "{label}: {written:?}");
    }
}

/// The line a pane logs when it opens says the layout asked for and the one
/// applied, and why they differ, so a pane that floats where "tiled" was set
/// is explained by the log.
#[test]
fn an_opened_pane_logs_its_layout_as_asked_and_as_applied() {
    assert_eq!(
        pane_opened(PaneLayout::Tiled, PaneLayout::Tiled, Some("i3"), (72, 20)),
        "opened the pane tiled under i3, 72x20 cells"
    );
    assert_eq!(
        pane_opened(
            PaneLayout::Tiled,
            PaneLayout::Floating,
            Some("awesome"),
            (72, 20)
        ),
        "opened the pane floating (asked tiled: awesome is not proven to keep a tiled pane \
         unfocused) under awesome, 72x20 cells"
    );
    assert_eq!(
        pane_opened(PaneLayout::Floating, PaneLayout::Floating, None, (40, 8)),
        "opened the pane floating under an unnamed window manager, 40x8 cells"
    );
}
