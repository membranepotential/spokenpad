//! Private, size-bounded diagnostic log; stdout belongs to the recovery CLI.
use anyhow::Result;
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// The log file is rotated before it would grow past this many bytes...
const MAX_LOG_BYTES: u64 = 1_000_000;
/// ...into `<log>.1`, the older ones moving up to `<log>.<ROTATED_LOGS>`,
/// and the oldest dropped.
const ROTATED_LOGS: usize = 3;

struct LogFile {
    path: PathBuf,
    file: File,
    bytes: u64,
}
fn open(path: &Path) -> std::io::Result<File> {
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(f)
}
impl LogFile {
    /// Opens the log at `path` for appending, creating it 0600 and any
    /// missing directory above it 0700.
    fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            crate::shell::dirs::create_private(parent)?;
        }
        let file = open(path)?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            path: path.to_owned(),
            file,
            bytes,
        })
    }
    fn write(&mut self, line: &str) -> std::io::Result<()> {
        if self.bytes + line.len() as u64 > MAX_LOG_BYTES {
            for i in (1..=ROTATED_LOGS).rev() {
                let from = if i == 1 {
                    self.path.clone()
                } else {
                    PathBuf::from(format!("{}.{}", self.path.display(), i - 1))
                };
                let to = PathBuf::from(format!("{}.{i}", self.path.display()));
                if from.exists() {
                    fs::rename(from, to)?;
                }
            }
            self.file = open(&self.path)?;
            self.bytes = 0;
        }
        self.file.write_all(line.as_bytes())?;
        self.bytes += line.len() as u64;
        Ok(())
    }
}
struct Logger {
    verbose: bool,
    file: Mutex<Option<LogFile>>,
}
impl Log for Logger {
    /// Debug always reaches the private log file; `verbose` only decides
    /// whether it is also copied to stderr, so the answer here does not
    /// depend on it.
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        m.level() <= Level::Debug
    }
    fn log(&self, r: &Record<'_>) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let now = chrono::Local::now();
        let line = format!(
            "{} {:5} {}: {}\n",
            now.format("%Y-%m-%d %H:%M:%S"),
            r.level(),
            r.target(),
            r.args()
        );
        if self.verbose || r.level() <= Level::Info {
            let _ = std::io::stderr().lock().write_all(line.as_bytes());
        }
        if let Ok(mut slot) = self.file.lock()
            && let Some(file) = slot.as_mut()
            && let Err(e) = file.write(&line)
        {
            eprintln!("spokenpad: diagnostic log failed: {e}");
            *slot = None;
        }
    }
    fn flush(&self) {
        if let Ok(mut slot) = self.file.lock()
            && let Some(f) = slot.as_mut()
        {
            let _ = f.file.flush();
        }
    }
}
pub fn init(verbose: bool, path: Option<&Path>) -> Result<()> {
    let default = crate::config::state_dir().join("spokenpad.log");
    let p = path.unwrap_or(&default);
    let file = if p.as_os_str().eq_ignore_ascii_case("none") {
        None
    } else {
        match LogFile::create(p) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("spokenpad: could not open log {}: {e}", p.display());
                None
            }
        }
    };
    log::set_boxed_logger(Box::new(Logger {
        verbose,
        file: Mutex::new(file),
    }))?;
    log::set_max_level(LevelFilter::Debug);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The log's directory is created private, as the rest of the state
    /// directory is, and the log rotates into numbered files at its limit.
    #[test]
    fn the_log_lives_in_a_private_directory_and_rotates() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("state/spokenpad/spokenpad.log");
        let mut log = LogFile::create(&path).unwrap();
        assert_eq!(mode(&temporary.path().join("state/spokenpad")), 0o700);
        assert_eq!(mode(&path), 0o600);
        let line = "x".repeat(MAX_LOG_BYTES as usize / 2 + 1);
        for _ in 0..=2 * ROTATED_LOGS {
            log.write(&line).unwrap();
        }
        let rotated = |i: usize| PathBuf::from(format!("{}.{i}", path.display()));
        assert!((1..=ROTATED_LOGS).all(|i| rotated(i).exists()));
        assert!(!rotated(ROTATED_LOGS + 1).exists(), "the oldest is dropped");
    }
}
