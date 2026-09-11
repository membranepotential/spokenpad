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
    fn write(&mut self, line: &str) -> std::io::Result<()> {
        if self.bytes + line.len() as u64 > 1_000_000 {
            for i in (1..=3).rev() {
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
        let result = (|| -> std::io::Result<LogFile> {
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = open(p)?;
            let bytes = file.metadata()?.len();
            Ok(LogFile {
                path: p.to_owned(),
                file,
                bytes,
            })
        })();
        match result {
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
