//! Process-boundary checks: errors must not turn into plausible transcription.
use std::{
    process::{Command, Output},
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn command(directory: &TempDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spokenpad"));
    cmd.env("XDG_STATE_HOME", directory.path())
        .env("XDG_CONFIG_HOME", directory.path())
        .env("XDG_RUNTIME_DIR", directory.path())
        // The default mode is the pane, which reads the display for
        // `spokenpad check` and opens its window there: never the user's.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("SWAYSOCK");
    cmd.args(["--log-file", "none"]);
    cmd
}
fn code(output: &Output) -> i32 {
    output.status.code().expect("exited normally")
}
#[test]
fn help_and_version_do_not_initialize_devices_or_models() {
    let dir = tempfile::tempdir().unwrap();
    let help = command(&dir).arg("--help").output().unwrap();
    assert_eq!(code(&help), 0);
    assert!(String::from_utf8_lossy(&help.stdout).contains("transcribe"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("editor"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("toggle"));
    let version = command(&dir).arg("--version").output().unwrap();
    assert_eq!(code(&version), 0);
    assert!(String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
/// `spokenpad editor` becomes the editor. With a stand-in that prints its
/// arguments, the command line it hands nvim can be read back: the socket,
/// the ownership marker, and the dictation file last.
#[test]
fn the_editor_command_runs_nvim_on_the_socket_and_a_dictation_file() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("run/nvim.sock");
    let dictation = dir.path().join("dictation");
    let config = dir.path().join("editor.toml");
    std::fs::write(
        &config,
        format!(
            "[nvim]\neditor = ['sh', '-c', 'printf \"%s\\n\" \"$@\"', 'nvim']\nsocket_path = '{}'\ndictation_dir = '{}'\n",
            socket.display(),
            dictation.display()
        ),
    )
    .unwrap();
    let output = command(&dir)
        .arg("-c")
        .arg(&config)
        .arg("editor")
        .output()
        .unwrap();
    assert_eq!(
        code(&output),
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let argv: Vec<&str> = stdout.lines().collect();
    let listen = argv
        .iter()
        .position(|a| *a == "--listen")
        .expect("--listen");
    assert_eq!(argv[listen + 1], socket.to_str().unwrap());
    assert!(
        argv.iter()
            .any(|a| a.starts_with("let g:spokenpad_owner = '"))
    );
    let file = std::path::Path::new(argv.last().unwrap());
    assert_eq!(file.parent(), Some(dictation.as_path()));
    assert!(
        file.exists(),
        "the dictation file is created before nvim starts"
    );
    let marker = std::fs::read_to_string(dir.path().join("run/nvim.sock.owner")).unwrap();
    assert!(
        stdout.contains(&marker),
        "the marker file names this editor"
    );

    // Something that is not a socket at the socket path is refused, untouched.
    std::fs::remove_file(dir.path().join("run/nvim.sock.owner")).unwrap();
    std::fs::write(&socket, "not a socket").unwrap();
    let refused = command(&dir)
        .arg("-c")
        .arg(&config)
        .arg("editor")
        .output()
        .unwrap();
    assert_eq!(code(&refused), 1);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("refusing non-socket path"));
    assert_eq!(std::fs::read_to_string(&socket).unwrap(), "not a socket");
}

#[test]
fn missing_recording_is_exit_four_and_stdout_stays_empty() {
    let dir = tempfile::tempdir().unwrap();
    let output = command(&dir)
        .arg("transcribe")
        .arg(dir.path().join("missing.wav"))
        .output()
        .unwrap();
    assert_eq!(code(&output), 4);
    assert!(output.stdout.is_empty());
}
#[test]
fn wrong_sample_rate_rejected_before_loading_model() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wrong-rate.wav");
    let writer = hound::WavWriter::create(
        &path,
        hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    writer.finalize().unwrap();
    let output = command(&dir)
        .arg("--model-dir")
        .arg(dir.path().join("no-model"))
        .arg("transcribe")
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(code(&output), 4);
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("48000Hz"));
}
#[test]
fn invalid_config_fails_before_devices_and_missing_model_is_exit_two() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, "[preview]\ninterval_ms=1").unwrap();
    let output = command(&dir)
        .arg("-c")
        .arg(path)
        .arg("check")
        .output()
        .unwrap();
    assert_eq!(code(&output), 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("preview.interval_ms"));
    let output = command(&dir)
        .arg("--model-dir")
        .arg(dir.path().join("absent"))
        .arg("check")
        .output()
        .unwrap();
    assert_eq!(code(&output), 2);
    assert!(output.stdout.is_empty());
}
/// A key binding with no daemon behind it: a clear message, a nonzero exit,
/// and nothing created, not even the config it never reads.
#[test]
fn a_control_command_without_a_daemon_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("spokenpad.sock");
    for request in ["start", "stop", "toggle", "cancel"] {
        let output = command(&dir).arg(request).output().unwrap();
        assert_eq!(code(&output), 1, "{request}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no spokenpad daemon is listening")
                && stderr.contains(&socket.display().to_string()),
            "{request}: {stderr}"
        );
        assert!(output.stdout.is_empty());
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
/// Socket activation as `spokenpad.socket` does it, with systemd's own
/// `systemd-socket-activate`: it binds the socket and starts the daemon on
/// the first connection, which that very press is waiting on. The daemon
/// answers it before it loads a model -- here it has none, and stays up
/// saying so -- and leaves systemd's socket in place when it stops.
///
/// No microphone is opened: nothing starts a capture and the pre-roll is
/// off. No model is downloaded: both model paths are configured, so neither
/// is the default spokenpad fetches.
#[test]
fn a_socket_activated_daemon_answers_the_press_that_started_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("spokenpad")).unwrap();
    std::fs::write(
        root.join("spokenpad/config.toml"),
        format!(
            "[asr]\nmodel_dir = '{0}/no-model'\n[vad]\nmodel = '{0}/no-vad.onnx'\n[audio]\npreroll_ms = 0\n",
            root.display()
        ),
    )
    .unwrap();
    let socket = root.join("spokenpad.sock");
    let log = root.join("daemon.log");
    let mut activator = activate(root, &log);

    let pressed = Instant::now();
    let first = command(&dir).arg("cancel").output().unwrap();
    let answered = pressed.elapsed();
    assert_eq!(
        code(&first),
        0,
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    eprintln!("the first press was answered {answered:?} after it connected");
    assert!(answered < Duration::from_secs(2), "{answered:?}");

    // The load fails, and the daemon stays: presses keep being answered.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("no speech model")
    {
        assert!(
            Instant::now() < deadline,
            "the load never reported its failure: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let again = command(&dir).arg("cancel").output().unwrap();
    assert_eq!(
        code(&again),
        0,
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(
        activator.try_wait().unwrap().is_none(),
        "the daemon is still running"
    );

    // SAFETY: kill sends SIGTERM to a child this test spawned and has not
    // reaped, so its pid still names it.
    unsafe { libc::kill(activator.id() as libc::pid_t, libc::SIGTERM) };
    let status = activator.wait().unwrap();
    assert!(status.success(), "the daemon stops cleanly: {status}");
    assert!(
        socket.exists(),
        "systemd's socket is not the daemon's to remove"
    );
    let log = std::fs::read_to_string(&log).unwrap();
    assert!(log.contains("listening on"), "{log}");
}

/// Starts `systemd-socket-activate` on `root/spokenpad.sock`, which starts
/// the daemon, logging to `log`, at the first connection. Returns once it
/// listens.
fn activate(root: &std::path::Path, log: &std::path::Path) -> std::process::Child {
    let socket = root.join("spokenpad.sock");
    let activator = Command::new("systemd-socket-activate")
        .env("XDG_STATE_HOME", root)
        .env("XDG_CONFIG_HOME", root)
        .env("XDG_DATA_HOME", root)
        .env("XDG_RUNTIME_DIR", root)
        // Its child gets only what it is told to pass on, as a unit's
        // service gets only the user manager's environment.
        .args(["-E", "XDG_STATE_HOME", "-E", "XDG_CONFIG_HOME"])
        .args(["-E", "XDG_DATA_HOME", "-E", "XDG_RUNTIME_DIR"])
        .arg("--listen")
        .arg(&socket)
        .arg(env!("CARGO_BIN_EXE_spokenpad"))
        .arg("--log-file")
        .arg(log)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("systemd-socket-activate is part of systemd");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "systemd-socket-activate never listened"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    activator
}

/// A daemon started by hand holds the per-user lock. The one systemd starts
/// for a press answers that press -- another daemon is running -- and exits
/// with 3, which the unit does not restart on; the press does not wait, and
/// does not start it again.
#[test]
fn a_socket_activated_daemon_that_finds_another_answers_and_exits_three() {
    use std::os::fd::AsRawFd;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("spokenpad")).unwrap();
    let lock = std::fs::File::create(root.join("spokenpad/daemon.lock")).unwrap();
    // SAFETY: flock borrows the descriptor `lock` owns for the whole call and
    // retains no pointer.
    let held = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(held, 0, "the test holds the lock the other daemon would");
    let log = root.join("daemon.log");
    let mut activator = activate(root, &log);

    let press = command(&dir).arg("start").output().unwrap();
    assert_eq!(code(&press), 1);
    let stderr = String::from_utf8_lossy(&press.stderr);
    assert!(stderr.contains("another daemon is running"), "{stderr}");
    let status = activator.wait().unwrap();
    assert_eq!(status.code(), Some(3), "{status}");
    drop(lock);
}

/// A config from before the control socket names what replaced `[hotkey]`.
#[test]
fn a_hotkey_table_in_the_config_points_to_the_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.toml");
    std::fs::write(&path, "[hotkey]\nkey_code = 186\n").unwrap();
    let output = command(&dir)
        .arg("-c")
        .arg(path)
        .arg("check")
        .output()
        .unwrap();
    assert_eq!(code(&output), 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("spokenpad start"), "{stderr}");
}
