//! Downloads spokenpad's own default model set (Parakeet TDT + Silero VAD)
//! from the pinned URLs in [`core::models`](crate::core::models), verifying
//! the pinned size and sha256 of every file -- already on disk, or freshly
//! downloaded -- before it is used. This is the only network access
//! anywhere in spokenpad: the URLs are fixed literals in `core::models`,
//! never built from configuration or user input.
//!
//! Every request goes over HTTPS, redirects included, each phase of it under
//! a timeout, and no body is read past its pinned size. One process at a
//! time downloads into a models directory: the daemon's own download,
//! `spokenpad fetch-models`, `check` and `transcribe` take the same lock.
use crate::{
    config::{Asr, Vad, models_dir},
    core::models::{ModelFile, asr_uses_default_model, files_to_ensure},
};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    time::Duration,
};

/// One thing that happened to a file, for a caller to report as progress: on
/// stderr for `spokenpad fetch-models`, in the log for the daemon's own
/// automatic download before it loads a model.
pub enum FetchEvent<'a> {
    /// Already on disk with the pinned size and sha256; nothing downloaded.
    Present(&'a ModelFile),
    /// A download for this file is starting.
    Downloading(&'a ModelFile),
    /// `downloaded` of `file.size` bytes written to the `.part` file so far.
    Progress {
        file: &'a ModelFile,
        downloaded: u64,
    },
    /// Downloaded, verified, and renamed into place.
    Verified(&'a ModelFile),
}

/// The lock file in a models directory. It is never removed: `flock` locks
/// the open file, so a lock file deleted and created again would let two
/// processes each hold "the" lock.
const LOCK_FILE: &str = ".fetch.lock";
/// How long resolving, connecting, sending the request and receiving the
/// response headers may each take, on every redirect hop.
const PHASE_TIMEOUT: Duration = Duration::from_secs(30);
/// Hugging Face answers with one redirect to its CDN, GitHub with one to its
/// object store.
const MAX_REDIRECTS: u32 = 5;
/// The slowest link a body is given time for. ureq bounds the whole body,
/// not each read, so a connection that stalls ends when the file could have
/// arrived at this rate.
const MIN_BYTES_PER_SECOND: u64 = 64 * 1024;

/// Which URLs an agent may fetch.
#[derive(Clone, Copy)]
enum Schemes {
    /// Only `https://`, on every redirect hop: what spokenpad uses.
    HttpsOnly,
    /// Plain `http://` too: only for the tests' local server.
    #[cfg(test)]
    AlsoHttp,
}

/// The one HTTP client spokenpad has. Proxies come from the environment, as
/// ureq's defaults take them.
fn agent(schemes: Schemes) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(matches!(schemes, Schemes::HttpsOnly))
        .max_redirects(MAX_REDIRECTS)
        .timeout_resolve(Some(PHASE_TIMEOUT))
        .timeout_connect(Some(PHASE_TIMEOUT))
        .timeout_send_request(Some(PHASE_TIMEOUT))
        .timeout_recv_response(Some(PHASE_TIMEOUT))
        .build()
        .into()
}

/// How long the body of a file of `size` bytes may take.
fn body_budget(size: u64) -> Duration {
    PHASE_TIMEOUT + Duration::from_secs(size / MIN_BYTES_PER_SECOND)
}

/// Downloads every file in `files` that is missing or does not verify (wrong
/// size or sha256) into `dest_dir`; a file that already verifies is left
/// untouched and never re-downloaded. Each download is written to
/// `<name>.part` next to the final path and renamed into place only once its
/// size and sha256 match the pin, so a process killed mid-download, or a
/// corrupted mirror, never leaves behind a file spokenpad would go on to
/// load. Idempotent: re-running with the same arguments downloads nothing
/// once every file verifies.
pub fn fetch_models(
    dest_dir: &Path,
    files: &[ModelFile],
    on_event: impl FnMut(FetchEvent<'_>),
) -> Result<()> {
    fetch_with(&agent(Schemes::HttpsOnly), dest_dir, files, on_event)
}

fn fetch_with(
    agent: &ureq::Agent,
    dest_dir: &Path,
    files: &[ModelFile],
    mut on_event: impl FnMut(FetchEvent<'_>),
) -> Result<()> {
    let _lock = lock_dir(dest_dir)?;
    for file in files {
        let dest = dest_dir.join(&file.relative_path);
        if verified(&dest, file)? {
            on_event(FetchEvent::Present(file));
            continue;
        }
        on_event(FetchEvent::Downloading(file));
        download(agent, &dest, file, &mut |downloaded| {
            on_event(FetchEvent::Progress { file, downloaded });
        })?;
        on_event(FetchEvent::Verified(file));
    }
    Ok(())
}

/// Takes the models directory's lock, creating the directory, and waits for
/// it, saying so, while another process holds it: that process is
/// downloading the same files, which this one then finds in place. The lock
/// lasts as long as the returned file is open.
fn lock_dir(dest_dir: &Path) -> Result<fs::File> {
    fs::create_dir_all(dest_dir).with_context(|| format!("create {}", dest_dir.display()))?;
    let path = dest_dir.join(LOCK_FILE);
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    if flock(&file, libc::LOCK_EX | libc::LOCK_NB).is_ok() {
        return Ok(file);
    }
    log::info!(
        "another spokenpad is downloading into {}; waiting for it",
        dest_dir.display()
    );
    flock(&file, libc::LOCK_EX).with_context(|| format!("lock {}", path.display()))?;
    Ok(file)
}

fn flock(file: &fs::File, operation: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: flock takes an integer descriptor, which `file` keeps open
        // for the whole call, and touches no memory.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Downloads the default model files this configuration would load and
/// does not have yet (see [`files_to_ensure`]): nothing for a
/// user-configured `asr.model_dir` or `vad.model`. Reports
/// `(done, total)` bytes as it goes.
///
/// Whether anything is missing is decided by file size alone, which reads no
/// file, so that the daemon can ask at every start: hashing the 670 MB set
/// takes seconds. A file of the pinned size with the wrong bytes then fails
/// to load, and `spokenpad fetch-models`, which hashes every file, replaces
/// it.
pub fn ensure_defaults(asr: &Asr, vad: &Vad, on_bytes: impl FnMut(u64, u64)) -> Result<()> {
    let files = files_to_ensure(asr, vad);
    let dest = models_dir();
    if all_sized(&dest, &files)? {
        return Ok(());
    }
    log::info!("downloading missing default models into {}", dest.display());
    fetch_counting(&dest, &files, on_bytes).map(|_| ())
}

/// What to do about a speech model that is in place and did not load.
pub enum Repair {
    /// Files of the default set did not match their pinned sha256, and were
    /// downloaded again: loading is worth another try.
    Replaced,
    /// Every default file matches its pin: loading again cannot help.
    Verified,
    /// The model is a configured one, which spokenpad never downloads.
    NotOurs,
}

/// Hashes the default files this configuration loads against their pins,
/// and downloads again any that do not match: for a model that
/// [`ensure_defaults`] found at the right size and that did not load.
pub fn repair_defaults(asr: &Asr, vad: &Vad, on_bytes: impl FnMut(u64, u64)) -> Result<Repair> {
    if !asr_uses_default_model(asr) {
        return Ok(Repair::NotOurs);
    }
    let files = files_to_ensure(asr, vad);
    log::warn!("verifying the default models against their pinned sha256");
    Ok(if fetch_counting(&models_dir(), &files, on_bytes)? {
        Repair::Replaced
    } else {
        Repair::Verified
    })
}

/// [`fetch_models`] with its progress summed over the whole set. Returns
/// whether anything was downloaded.
fn fetch_counting(
    dest: &Path,
    files: &[ModelFile],
    mut on_bytes: impl FnMut(u64, u64),
) -> Result<bool> {
    let mut downloaded_any = false;
    let total = files.iter().map(|f| f.size).sum();
    let mut done = 0;
    fetch_models(dest, files, |event| match event {
        FetchEvent::Present(file) | FetchEvent::Verified(file) => {
            log::info!("model {} is in place", file.relative_path.display());
            done += file.size;
            on_bytes(done, total);
        }
        FetchEvent::Downloading(file) => {
            downloaded_any = true;
            log::info!(
                "downloading {} ({} bytes)",
                file.relative_path.display(),
                file.size
            );
        }
        FetchEvent::Progress { downloaded, .. } => on_bytes(done + downloaded, total),
    })?;
    Ok(downloaded_any)
}

/// A progress reporter for [`ensure_defaults`] and [`repair_defaults`] run
/// from a command: one line on stderr, drawn over itself, when stderr is a
/// terminal; nothing otherwise, where the log lines already say what
/// happens.
pub fn terminal_progress() -> impl FnMut(u64, u64) {
    let terminal = io::stderr().is_terminal();
    move |done, total| {
        if terminal {
            let end = if done >= total { "\n" } else { "" };
            eprint!("\r{}{end}", progress_line(done, total));
            let _ = io::stderr().flush();
        }
    }
}

fn progress_line(done: u64, total: u64) -> String {
    const MB: u64 = 1_000_000;
    format!(
        "downloading the default models: {:3}% ({} of {} MB)",
        percent(done, total),
        done / MB,
        total.div_ceil(MB)
    )
}

fn percent(done: u64, total: u64) -> u64 {
    done.saturating_mul(100) / total.max(1)
}

/// Reports [`fetch_models`]' events on stderr, for `spokenpad
/// fetch-models`: one line per file, and on a terminal a percentage drawn
/// over itself while it downloads.
pub fn report_on_stderr(event: FetchEvent<'_>) {
    let terminal = io::stderr().is_terminal();
    match event {
        FetchEvent::Present(f) => eprintln!("  {}: present", f.relative_path.display()),
        FetchEvent::Downloading(f) => {
            eprint!(
                "  {}: downloading ({} bytes)",
                f.relative_path.display(),
                f.size
            );
            let _ = io::stderr().flush();
        }
        FetchEvent::Progress { file, downloaded } if terminal => {
            eprint!(
                "\r  {}: downloading {:3}%",
                file.relative_path.display(),
                percent(downloaded, file.size)
            );
            let _ = io::stderr().flush();
        }
        FetchEvent::Progress { .. } => {}
        FetchEvent::Verified(f) if terminal => {
            eprintln!("\r  {}: done              ", f.relative_path.display());
        }
        FetchEvent::Verified(_) => eprintln!(": done"),
    }
}

/// Whether every one of `files` sits under `dest_dir` at its pinned size.
fn all_sized(dest_dir: &Path, files: &[ModelFile]) -> Result<bool> {
    for file in files {
        let path = dest_dir.join(&file.relative_path);
        match fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() == file.size => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
        }
    }
    Ok(true)
}

fn verified(path: &Path, file: &ModelFile) -> Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    if !metadata.is_file() || metadata.len() != file.size {
        return Ok(false);
    }
    let digest = sha256_hex(path)?;
    Ok(crate::core::models::matches(file, metadata.len(), &digest))
}

fn sha256_hex(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("write! to a String never fails");
    }
    s
}

/// Bytes between progress callbacks: frequent enough to show life on a slow
/// link, rare enough not to flood a log file over a 650 MB download.
const PROGRESS_STEP: u64 = 4 * 1024 * 1024;

/// Downloads `file` to `<dest>.part`, and renames it to `dest` once it
/// verifies. The `.part` file is removed on every failure.
fn download(
    agent: &ureq::Agent,
    dest: &Path,
    file: &ModelFile,
    on_progress: &mut dyn FnMut(u64),
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut part_name = dest
        .file_name()
        .context("model destination has no file name")?
        .to_os_string();
    part_name.push(".part");
    let part = dest.with_file_name(part_name);
    if let Err(error) = download_part(agent, &part, file, on_progress) {
        if let Err(removing) = fs::remove_file(&part)
            && removing.kind() != io::ErrorKind::NotFound
        {
            log::warn!("could not remove {}: {removing}", part.display());
        }
        return Err(error);
    }
    fs::rename(&part, dest)
        .with_context(|| format!("rename {} to {}", part.display(), dest.display()))
}

/// Writes `file`'s body to `part`, reading no byte past its pinned size, and
/// checks it against the pin.
fn download_part(
    agent: &ureq::Agent,
    part: &Path,
    file: &ModelFile,
    on_progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let url = &file.url;
    let mut response = agent
        .get(url.as_str())
        .config()
        .timeout_recv_body(Some(body_budget(file.size)))
        .build()
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut out = fs::File::create(part).with_context(|| format!("create {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut since_progress = 0u64;
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("download {url}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > file.size {
            bail!(
                "{url} sent more than the pinned {} bytes; stopped reading",
                file.size
            );
        }
        out.write_all(&buf[..n])
            .with_context(|| format!("write {}", part.display()))?;
        hasher.update(&buf[..n]);
        since_progress += n as u64;
        if since_progress >= PROGRESS_STEP {
            since_progress = 0;
            on_progress(total);
        }
    }
    out.flush()
        .with_context(|| format!("write {}", part.display()))?;
    let digest = hex_encode(&hasher.finalize());
    if !crate::core::models::matches(file, total, &digest) {
        bail!(
            "{url} does not match the pinned size/sha256 ({total} bytes, sha256 {digest}; \
             expected {} bytes, sha256 {})",
            file.size,
            file.sha256
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::{DEFAULT_VAD, every_default_file};
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
        sync::mpsc,
        thread,
    };

    /// Spawns a one-shot HTTP/1.1 server on 127.0.0.1 that answers exactly
    /// one GET request with `body` under a `Content-Length` of `length`, then
    /// stops. No real network access; the same pinned-URL download path is
    /// exercised end to end.
    fn serve_once_claiming(body: &'static [u8], length: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local test port");
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone test socket"));
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) if line == "\r\n" => break,
                    Ok(_) => {}
                }
            }
            let mut stream = reader.into_inner();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(body);
        });
        format!("http://127.0.0.1:{port}/file")
    }

    fn serve_once(body: &'static [u8]) -> String {
        serve_once_claiming(body, body.len())
    }

    fn test_file(url: String, size: u64, sha256: &'static str) -> ModelFile {
        ModelFile {
            relative_path: "test-model.bin".into(),
            url,
            size,
            sha256,
        }
    }

    /// The download path with an agent that may fetch the plain-HTTP test
    /// server.
    fn fetch_local(
        dir: &Path,
        file: &ModelFile,
        on_event: impl FnMut(FetchEvent<'_>),
    ) -> Result<()> {
        fetch_with(
            &agent(Schemes::AlsoHttp),
            dir,
            std::slice::from_ref(file),
            on_event,
        )
    }

    const BODY: &[u8] = b"pretend model weights";
    // sha256("pretend model weights")
    const BODY_SHA256: &str = "c092f7ea91d072d354f1e73d8be1f7d7aa3a463063c07104c2d214a9af07b030";

    #[test]
    fn downloads_verifies_and_renames_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once(BODY);
        let file = test_file(url, BODY.len() as u64, BODY_SHA256);
        assert!(
            !all_sized(dir.path(), std::slice::from_ref(&file)).unwrap(),
            "missing"
        );

        let mut events = vec![];
        fetch_local(dir.path(), &file, |e| {
            events.push(match e {
                FetchEvent::Present(_) => "present",
                FetchEvent::Downloading(_) => "downloading",
                FetchEvent::Progress { .. } => "progress",
                FetchEvent::Verified(_) => "verified",
            });
        })
        .unwrap();

        let dest = dir.path().join("test-model.bin");
        assert_eq!(fs::read(&dest).unwrap(), BODY);
        assert!(!dest.with_extension("bin.part").exists());
        assert_eq!(events, vec!["downloading", "verified"]);
    }

    #[test]
    fn a_present_and_correct_file_is_never_downloaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("test-model.bin");
        fs::write(&dest, BODY).unwrap();
        // A URL nothing is listening on: reaching it would fail the test.
        let file = test_file(
            "http://127.0.0.1:1/unreachable".into(),
            BODY.len() as u64,
            BODY_SHA256,
        );

        assert!(all_sized(dir.path(), std::slice::from_ref(&file)).unwrap());
        let mut events = vec![];
        fetch_local(dir.path(), &file, |e| {
            events.push(matches!(e, FetchEvent::Present(_)));
        })
        .unwrap();
        assert_eq!(events, vec![true]);
        assert_eq!(fs::read(&dest).unwrap(), BODY, "left untouched");
    }

    #[test]
    fn a_corrupt_existing_file_is_redownloaded() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("test-model.bin");
        fs::write(&dest, b"wrong bytes entirely!").unwrap();
        let url = serve_once(BODY);
        let file = test_file(url, BODY.len() as u64, BODY_SHA256);

        assert!(
            all_sized(dir.path(), std::slice::from_ref(&file)).unwrap(),
            "the size alone cannot tell"
        );
        fetch_local(dir.path(), &file, |_| {}).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), BODY);
    }

    #[test]
    fn a_download_that_does_not_match_the_pin_is_rejected_and_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once(BODY);
        // Pin a sha256 that does not match what the server actually sends.
        let file = test_file(url, BODY.len() as u64, "0".repeat(64).leak());

        let error = fetch_local(dir.path(), &file, |_| {}).unwrap_err();
        assert!(
            format!("{error:#}").contains("does not match the pinned"),
            "{error}"
        );
        assert!(!dir.path().join("test-model.bin").exists());
        assert!(!dir.path().join("test-model.bin.part").exists());
    }

    /// A server that sends more than the pin is cut off at the pin, and
    /// nothing it sent stays on disk.
    #[test]
    fn no_byte_past_the_pinned_size_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once(BODY);
        let file = test_file(url, BODY.len() as u64 - 5, BODY_SHA256);

        let error = fetch_local(dir.path(), &file, |_| {}).unwrap_err();
        assert!(
            format!("{error:#}").contains("more than the pinned"),
            "{error:#}"
        );
        assert!(!dir.path().join("test-model.bin").exists());
        assert!(!dir.path().join("test-model.bin.part").exists());
    }

    /// A connection that ends before its body does leaves no `.part` file.
    #[test]
    fn a_download_cut_short_is_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once_claiming(BODY, BODY.len() + 100);
        let file = test_file(url, BODY.len() as u64 + 100, BODY_SHA256);

        let error = fetch_local(dir.path(), &file, |_| {}).unwrap_err();
        assert!(format!("{error:#}").contains("download"), "{error:#}");
        assert!(!dir.path().join("test-model.bin").exists());
        assert!(!dir.path().join("test-model.bin.part").exists());
    }

    /// spokenpad's own agent refuses a URL that is not HTTPS. ureq applies
    /// the same check on every redirect hop, so a redirect to plain HTTP is
    /// refused the same way.
    #[test]
    fn plain_http_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing may answer: the refusal comes before any connection.
        let file = test_file(
            "http://127.0.0.1:1/model".into(),
            BODY.len() as u64,
            BODY_SHA256,
        );
        let error = fetch_models(dir.path(), &[file], |_| {}).unwrap_err();
        assert!(format!("{error:#}").contains("https only"), "{error:#}");
        assert!(!dir.path().join("test-model.bin.part").exists());
    }

    /// Two processes downloading into one directory would write the same
    /// `.part` file: the second waits for the first, then finds the file in
    /// place and downloads nothing.
    #[test]
    fn a_second_download_into_the_same_directory_waits_for_the_first() {
        let dir = tempfile::tempdir().unwrap();
        // `flock` locks an open file description, so a second open in this
        // process conflicts exactly as another process's would.
        let held = lock_dir(dir.path()).unwrap();
        let url = serve_once(BODY);
        let file = test_file(url, BODY.len() as u64, BODY_SHA256);
        let (events, received) = mpsc::channel();
        let path = dir.path().to_owned();
        let second = thread::spawn(move || {
            fetch_local(&path, &file, |event| {
                let _ = events.send(matches!(event, FetchEvent::Present(_)));
            })
        });
        thread::sleep(Duration::from_millis(300));
        assert!(!second.is_finished(), "the second download waits");
        assert!(received.try_recv().is_err(), "and does nothing meanwhile");
        // What the first process would have left once it is done.
        fs::write(dir.path().join("test-model.bin"), BODY).unwrap();
        drop(held);
        second.join().unwrap().unwrap();
        assert_eq!(received.try_iter().collect::<Vec<_>>(), vec![true]);
    }

    #[test]
    fn the_progress_line_names_the_share_and_the_size() {
        assert_eq!(
            progress_line(245_000_000, 670_000_000),
            "downloading the default models:  36% (245 of 670 MB)"
        );
        assert_eq!(
            progress_line(670_000_000, 670_000_000),
            "downloading the default models: 100% (670 of 670 MB)"
        );
    }

    #[test]
    fn a_slow_link_gets_time_for_the_whole_file() {
        let encoder = 652_184_281;
        let budget = body_budget(encoder);
        assert!(budget > Duration::from_secs(2 * 3600), "{budget:?}");
        assert!(budget < Duration::from_secs(3 * 3600), "{budget:?}");
        assert_eq!(body_budget(0), PHASE_TIMEOUT);
    }

    /// Exercises the real pinned URLs over real TLS, through the redirects
    /// both hosts answer with, not the local plaintext server the tests
    /// above use -- the one thing they cannot cover. One small file from
    /// each host. Run with `cargo test --locked -- --ignored
    /// a_real_download`; needs a network.
    #[test]
    #[ignore = "hits the real, pinned Parakeet and Silero URLs over the network"]
    fn a_real_download_verifies_against_the_pinned_hash() {
        let files: Vec<_> = every_default_file()
            .into_iter()
            .filter(|f| {
                f.relative_path.ends_with("tokens.txt")
                    || f.relative_path == Path::new(DEFAULT_VAD.file.name)
            })
            .collect();
        assert_eq!(files.len(), 2);
        let dir = tempfile::tempdir().unwrap();
        fetch_models(dir.path(), &files, |_| {}).expect("download and verify the real files");
        for file in files {
            assert!(dir.path().join(&file.relative_path).is_file());
        }
    }
}
