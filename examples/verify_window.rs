//! Manually-run i3/sway smoke check for the graphical Neovim sink.

use anyhow::{Context, Result, anyhow, ensure};
use rmpv::Value;
use spokenpad::{
    config::{Config, Mode},
    core::{session::Notice, state::IndicatorPhase},
    shell::{
        nvim::{IndicatorState, NvimSession},
        wm::Wm,
    },
};
use std::{
    fs,
    io::Write,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const TEST_TEXT: &str = "Spokenpad window smoke test.\nGrüße — 東京 🌿";
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn main() -> Result<()> {
    let workspace = TempDir::new().context("create temporary verification workspace")?;
    let socket_path = workspace.path().join("nvim.sock");
    let dictation_dir = workspace.path().join("dictation");
    let cleanup = CleanupGuard::new(workspace, socket_path.clone());

    let verification = verify(&socket_path, &dictation_dir);
    let shutdown = cleanup.finish();
    match (verification, shutdown) {
        (Ok(()), Ok(())) => {
            println!(
                "PASS: saved-file content verified exactly; focused window unchanged after ensure and append"
            );
            Ok(())
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(verification), Err(shutdown)) => Err(anyhow!(
            "{verification:#}; graceful cleanup also failed: {shutdown:#}"
        )),
    }
}

fn verify(socket_path: &Path, dictation_dir: &Path) -> Result<()> {
    let wm = Wm::connect().context("this check needs a running i3 or sway")?;
    let active_window = || wm.focused_node();
    let before = active_window()?;
    // Exercise the deployed terminal and editor, including bundled init and
    // theme choices, in managed mode whatever the config says: this checks
    // the window spokenpad opens itself. Only the test's socket and
    // transcript location are isolated below.
    let mut config = Config::load(None)?.nvim;
    config.mode = Mode::Managed;
    ensure!(
        config.window_instance == "spokenpad",
        "configured window instance no longer matches the packaged no_focus rules"
    );
    // This check opens a window of its own and then kills it. Running it
    // beside a live dictation window would adopt that window's socket and
    // close someone's passage, so refuse before anything is opened.
    let criteria = config
        .terminal
        .focus_criteria(&config.window_instance, wm.kind())?;
    ensure!(
        wm.find(&criteria)?.is_none(),
        "a {} window is already open; close the dictation window before running this check",
        config.window_instance
    );
    config.socket_path = socket_path.to_owned();
    config.dictation_dir = dictation_dir.to_owned();

    let mut session = NvimSession::new(config);
    let path = session
        .ensure()
        .context("open graphical dictation Neovim")?
        .context("managed mode opened no editor")?;
    ensure!(
        active_window()? == before,
        "focused window changed while ensuring the Neovim session"
    );

    let mut indicator = IndicatorState {
        phase: IndicatorPhase::Recording,
        preview: "Provisional first preview — never saved.".to_owned(),
        ..IndicatorState::default()
    };
    session.set_indicator(&indicator)?;
    ensure!(
        active_window()? == before,
        "focused window changed while showing the first preview"
    );

    session
        .append(TEST_TEXT, false)
        .context("append fixed smoke text")?;
    indicator.preview = "Provisional trailing preview — never saved.".to_owned();
    // The notice rides in the winbar, beside the phase label, while the
    // preview stays virtual text below the transcript: this push is what a
    // human checks that against.
    indicator.notice = Some(Notice::HeldTooBriefly.text());
    session.set_indicator(&indicator)?;
    ensure!(
        active_window()? == before,
        "focused window changed while appending to Neovim"
    );
    let expected = format!("{TEST_TEXT}\n");
    ensure!(
        fs::read_to_string(&path).context("read saved smoke-test file")? == expected,
        "saved file content did not exactly match the fixed smoke text"
    );
    drop(session);
    Ok(())
}

struct CleanupGuard {
    workspace: Option<TempDir>,
    socket_path: PathBuf,
    finished: bool,
}

impl CleanupGuard {
    fn new(workspace: TempDir, socket_path: PathBuf) -> Self {
        Self {
            workspace: Some(workspace),
            socket_path,
            finished: false,
        }
    }

    fn finish(mut self) -> Result<()> {
        let result = self.shutdown();
        self.finished = true;
        if let Err(error) = result {
            let retained = self.retain_workspace();
            return Err(match retained {
                Some(path) => error.context(format!(
                    "temporary workspace retained at {} for manual cleanup",
                    path.display()
                )),
                None => error,
            });
        }
        Ok(())
    }

    fn shutdown(&self) -> Result<()> {
        if !self.socket_path.try_exists().with_context(|| {
            format!(
                "inspect temporary Neovim socket {}",
                self.socket_path.display()
            )
        })? {
            return Ok(());
        }

        let mut stream = UnixStream::connect(&self.socket_path).with_context(|| {
            format!(
                "connect only to temporary Neovim socket {}",
                self.socket_path.display()
            )
        })?;
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .context("set Neovim shutdown write deadline")?;
        let message = Value::Array(vec![
            Value::from(2),
            Value::from("nvim_command"),
            Value::Array(vec![Value::from("qa!")]),
        ]);
        let mut payload = Vec::new();
        rmpv::encode::write_value(&mut payload, &message)
            .context("encode Neovim shutdown notification")?;
        stream
            .write_all(&payload)
            .context("send Neovim shutdown notification before deadline")?;
        stream
            .flush()
            .context("flush Neovim shutdown notification before deadline")?;
        // Keep this channel alive until Neovim processes the notification.
        // Closing immediately can discard a queued asynchronous RPC on EOF.
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while self.socket_path.try_exists().with_context(|| {
            format!(
                "inspect temporary Neovim socket {}",
                self.socket_path.display()
            )
        })? {
            ensure!(
                Instant::now() < deadline,
                "temporary Neovim socket did not disappear after qa! notification"
            );
            thread::sleep(POLL_INTERVAL);
        }
        Ok(())
    }

    fn retain_workspace(&mut self) -> Option<PathBuf> {
        self.workspace.take().map(TempDir::keep)
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Err(error) = self.shutdown() {
            if let Some(path) = self.retain_workspace() {
                eprintln!(
                    "verify_window cleanup failed; temporary workspace retained at {}: {error:#}",
                    path.display()
                );
            } else {
                eprintln!("verify_window cleanup failed: {error:#}");
            }
        }
    }
}
