//! Downloads spokenpad's own default model set (Parakeet TDT + Silero VAD)
//! from the pinned URLs in [`core::models`](crate::core::models), verifying
//! the pinned size and sha256 of every file -- already on disk, or freshly
//! downloaded -- before it is used. This is the only network access
//! anywhere in spokenpad: the URLs are fixed literals in `core::models`,
//! never built from configuration or user input.
use crate::{
    config::{Asr, Vad, models_dir},
    core::models::{ModelFile, files_to_ensure},
};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
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
    files: &[&ModelFile],
    mut on_event: impl FnMut(FetchEvent<'_>),
) -> Result<()> {
    for file in files {
        let dest = dest_dir.join(file.relative_path);
        if verified(&dest, file)? {
            on_event(FetchEvent::Present(file));
            continue;
        }
        on_event(FetchEvent::Downloading(file));
        download(&dest, file, &mut |downloaded| {
            on_event(FetchEvent::Progress { file, downloaded });
        })?;
        on_event(FetchEvent::Verified(file));
    }
    Ok(())
}

/// Downloads the default model files this configuration would load and
/// does not have yet (see [`files_to_ensure`]): nothing for a
/// user-configured `model_dir`, `asr.family` or `vad.model`. Reports
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
    let files = files_to_ensure(asr, vad);
    if !files
        .iter()
        .any(|f| f.kind == crate::core::models::ModelKind::Asr)
    {
        return Ok(Repair::NotOurs);
    }
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
    files: &[&ModelFile],
    mut on_bytes: impl FnMut(u64, u64),
) -> Result<bool> {
    let mut downloaded_any = false;
    let total = files.iter().map(|f| f.size).sum();
    let mut done = 0;
    fetch_models(dest, files, |event| match event {
        FetchEvent::Present(file) | FetchEvent::Verified(file) => {
            log::info!("model {} is in place", file.relative_path);
            done += file.size;
            on_bytes(done, total);
        }
        FetchEvent::Downloading(file) => {
            downloaded_any = true;
            log::info!("downloading {} ({} bytes)", file.relative_path, file.size);
        }
        FetchEvent::Progress { downloaded, .. } => on_bytes(done + downloaded, total),
    })?;
    Ok(downloaded_any)
}

/// Whether every one of `files` sits under `dest_dir` at its pinned size.
fn all_sized(dest_dir: &Path, files: &[&ModelFile]) -> Result<bool> {
    for file in files {
        let path = dest_dir.join(file.relative_path);
        match fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() == file.size => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
        }
    }
    Ok(true)
}

fn verified(path: &Path, file: &ModelFile) -> Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
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

fn download(dest: &Path, file: &ModelFile, on_progress: &mut dyn FnMut(u64)) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut part_name = dest
        .file_name()
        .context("model destination has no file name")?
        .to_os_string();
    part_name.push(".part");
    let part = dest.with_file_name(part_name);

    let url = file.url();
    let mut response = ureq::get(url.as_str())
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut out = fs::File::create(&part).with_context(|| format!("create {}", part.display()))?;
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
        out.write_all(&buf[..n])
            .with_context(|| format!("write {}", part.display()))?;
        hasher.update(&buf[..n]);
        total += n as u64;
        since_progress += n as u64;
        if since_progress >= PROGRESS_STEP {
            since_progress = 0;
            on_progress(total);
        }
    }
    out.flush()
        .with_context(|| format!("write {}", part.display()))?;
    drop(out);
    let digest = hex_encode(&hasher.finalize());
    if !crate::core::models::matches(file, total, &digest) {
        let _ = fs::remove_file(&part);
        bail!(
            "{url} does not match the pinned size/sha256 ({total} bytes, sha256 {digest}; \
             expected {} bytes, sha256 {})",
            file.size,
            file.sha256
        );
    }
    fs::rename(&part, dest)
        .with_context(|| format!("rename {} to {}", part.display(), dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::ModelKind;
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
        thread,
    };

    /// Spawns a one-shot HTTP/1.1 server on 127.0.0.1 that answers exactly
    /// one GET request with `body`, then stops. No real network access; the
    /// same pinned-URL download path is exercised end to end.
    fn serve_once(body: &'static [u8]) -> String {
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
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(body);
        });
        format!("http://127.0.0.1:{port}/file")
    }

    fn test_file(url: String, size: u64, sha256: &'static str) -> ModelFile {
        // `base_url`/`url_suffix` are only `pub(crate)`, so this test builds
        // the whole URL as the base and leaves the suffix empty.
        ModelFile {
            kind: ModelKind::Asr,
            required_for_load: true,
            relative_path: "test-model.bin",
            base_url: Box::leak(url.into_boxed_str()),
            url_suffix: "",
            size,
            sha256,
        }
    }

    const BODY: &[u8] = b"pretend model weights";
    // sha256("pretend model weights")
    const BODY_SHA256: &str = "c092f7ea91d072d354f1e73d8be1f7d7aa3a463063c07104c2d214a9af07b030";

    #[test]
    fn downloads_verifies_and_renames_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once(BODY);
        let file = test_file(url, BODY.len() as u64, BODY_SHA256);
        assert!(!all_sized(dir.path(), &[&file]).unwrap(), "missing");

        let mut events = vec![];
        fetch_models(dir.path(), &[&file], |e| {
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

        assert!(all_sized(dir.path(), &[&file]).unwrap());
        let mut events = vec![];
        fetch_models(dir.path(), &[&file], |e| {
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
            all_sized(dir.path(), &[&file]).unwrap(),
            "the size alone cannot tell"
        );
        fetch_models(dir.path(), &[&file], |_| {}).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), BODY);
    }

    #[test]
    fn a_download_that_does_not_match_the_pin_is_rejected_and_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once(BODY);
        // Pin a sha256 that does not match what the server actually sends.
        let file = test_file(url, BODY.len() as u64, "0".repeat(64).leak());

        let error = fetch_models(dir.path(), &[&file], |_| {}).unwrap_err();
        assert!(
            format!("{error:#}").contains("does not match the pinned"),
            "{error}"
        );
        assert!(!dir.path().join("test-model.bin").exists());
        assert!(!dir.path().join("test-model.bin.part").exists());
    }

    /// Exercises the real pinned URL over real TLS, not the local plaintext
    /// server the tests above use -- the one thing they cannot cover. Run
    /// with `cargo test --locked -- --ignored real_pinned_url`; needs a
    /// network.
    #[test]
    #[ignore = "hits the real, pinned Parakeet URL over the network"]
    fn a_real_download_verifies_against_the_pinned_hash() {
        let file = crate::core::models::DEFAULT_MODEL_FILES
            .iter()
            .find(|f| f.relative_path.ends_with("tokens.txt"))
            .expect("tokens.txt is in the default set");
        let dir = tempfile::tempdir().unwrap();
        fetch_models(dir.path(), &[file], |_| {}).expect("download and verify the real file");
        assert!(dir.path().join(file.relative_path).is_file());
    }
}
