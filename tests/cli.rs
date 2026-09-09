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
    let version = command(&dir).arg("--version").output().unwrap();
    assert_eq!(code(&version), 0);
    assert!(String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
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
