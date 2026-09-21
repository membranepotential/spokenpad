//! Process-boundary checks: errors must not turn into plausible transcription.
use std::process::{Command, Output};
use tempfile::TempDir;

fn command(directory: &TempDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spokenpad"));
    cmd.env("XDG_STATE_HOME", directory.path())
        .env("XDG_CONFIG_HOME", directory.path())
        .env("XDG_RUNTIME_DIR", directory.path());
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
