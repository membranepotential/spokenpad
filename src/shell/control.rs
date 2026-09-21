//! The control socket: how key bindings reach the daemon.
//!
//! The daemon listens on a Unix socket ([`config::control_socket`]) and
//! `spokenpad start|stop|toggle|cancel` each send one request over it. The
//! daemon reads no input device: which key does what is the window manager's
//! business, configured by the user. The protocol is
//! [`core::control`](crate::core::control).
//!
//! [`config::control_socket`]: crate::config::control_socket

use crate::core::control::{MAX_LINE, Received, Reply, Request};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// How long either side waits for the other's line. A client is a process
/// that writes its request right after connecting, so anything slower is
/// stuck, and the daemon must not wait on it: requests are served one at a
/// time, in arrival order.
const IO_TIMEOUT: Duration = Duration::from_millis(500);
/// How often the listener looks at its stop flag while no client connects.
const STOP_POLL: Duration = Duration::from_millis(100);

/// Listens on the control socket and forwards each request, stamped, to the
/// session. Stops, and removes the socket, when dropped.
pub struct ControlServer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ControlServer {
    /// Binds `path`, replacing a socket file no process is listening on any
    /// more, and refusing when one is. The socket is made private to the
    /// user: whoever can connect can start the microphone.
    pub fn bind(path: &Path, sender: Sender<Received>) -> Result<Self> {
        remove_stale(path)?;
        let listener = UnixListener::bind(path)
            .with_context(|| format!("listen on control socket {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("make {} private", path.display()))?;
        listener
            .set_nonblocking(true)
            .context("make the control socket nonblocking")?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("spokenpad-control".into())
            .spawn(move || serve(&listener, &sender, &stopping))
            .context("start the control socket thread")?;
        Ok(Self {
            path: path.to_owned(),
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::error!("control socket thread panicked");
        }
        // Only this process can be listening here: nothing binds a path
        // another listener answers on (see `remove_stale`).
        if let Err(e) = fs::remove_file(&self.path) {
            log::debug!("could not remove {}: {e}", self.path.display());
        }
    }
}

/// A socket file nobody answers on is what a crashed daemon leaves behind.
/// Anything else at the path is not ours to delete.
fn remove_stale(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "{} exists and is not a socket; refusing to replace it",
            path.display()
        );
    }
    match UnixStream::connect(path) {
        Ok(_) => bail!(
            "another spokenpad daemon is listening on {}",
            path.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            log::info!("removing stale control socket {}", path.display());
            fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("probe {}", path.display())),
    }
}

fn serve(listener: &UnixListener, sender: &Sender<Received>, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = answer(stream, sender) {
                    log::warn!("control request failed: {e:#}");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_readable(listener),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                log::error!("control socket failed: {e}");
                return;
            }
        }
    }
}

/// Blocks until a client connects or [`STOP_POLL`] passes.
fn wait_readable(listener: &UnixListener) {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `descriptor` is one initialized pollfd that outlives the call,
    // matching the count of 1; its fd is owned by `listener`, borrowed for
    // the whole call. poll retains no pointer. Any error, EINTR included,
    // only sends the caller round its loop again.
    unsafe {
        libc::poll(&mut descriptor, 1, STOP_POLL.as_millis() as libc::c_int);
    }
}

/// Serves one connection: read the request, stamp it, queue it, reply.
fn answer(mut stream: UnixStream, sender: &Sender<Received>) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let line = read_line(&mut stream)?;
    let at = Instant::now();
    let reply = match Request::decode(&line) {
        Ok(request) => match sender.send(Received { request, at }) {
            Ok(()) => {
                log::debug!("control: {request}");
                Reply::Accepted
            }
            Err(_) => Reply::ShuttingDown,
        },
        Err(reply) => {
            log::warn!(
                "control: rejected {:?} ({reply})",
                String::from_utf8_lossy(&line)
            );
            reply
        }
    };
    stream.write_all(reply.encode().as_bytes())?;
    Ok(())
}

/// Reads up to and including the first newline, at most [`MAX_LINE`] bytes.
/// Returns what arrived when the peer stops early; the decoder rejects it.
fn read_line(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut line = Vec::with_capacity(MAX_LINE);
    let mut byte = [0_u8];
    while line.len() < MAX_LINE && !line.ends_with(b"\n") {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(line)
}

/// Why a request did not reach the daemon.
#[derive(Debug)]
pub enum SendError {
    /// Nothing is listening: the daemon is not running, or is still loading
    /// its model.
    NoDaemon { path: PathBuf, cause: io::Error },
    /// The daemon answered, but not with `ok`.
    Refused(Reply),
    /// The connection failed after it was made, or the reply was not one this
    /// version knows.
    Failed(anyhow::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDaemon { path, cause } => write!(
                f,
                "no spokenpad daemon is listening on {} ({cause}); start it with `systemctl --user start spokenpad`, or wait until it has loaded its model",
                path.display()
            ),
            Self::Refused(reply) => write!(f, "the daemon refused the request: {reply}"),
            Self::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SendError {}

/// Sends one request and waits for the daemon's acknowledgement: what
/// `spokenpad start|stop|toggle|cancel` do.
pub fn send(path: &Path, request: Request) -> Result<(), SendError> {
    let mut stream = UnixStream::connect(path).map_err(|cause| SendError::NoDaemon {
        path: path.to_owned(),
        cause,
    })?;
    match exchange(&mut stream, request).map_err(SendError::Failed)? {
        Reply::Accepted => Ok(()),
        reply => Err(SendError::Refused(reply)),
    }
}

fn exchange(stream: &mut UnixStream, request: Request) -> Result<Reply> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(request.encode().as_bytes())?;
    let line = read_line(stream).context("read the daemon's reply")?;
    Reply::decode(&line).with_context(|| {
        format!(
            "unexpected reply {:?}; is the daemon the same version as this command?",
            String::from_utf8_lossy(&line)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_request_reaches_the_session_stamped_and_is_acknowledged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let (sender, receiver) = mpsc::channel();
        let server = ControlServer::bind(&path, sender).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the socket is private");
        for request in Request::ALL {
            let before = Instant::now();
            send(&path, request).unwrap();
            let received = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(received.request, request);
            assert!(received.at >= before && received.at <= Instant::now());
        }
        drop(server);
        assert!(!path.exists(), "the socket is removed on shutdown");
        assert!(matches!(
            send(&path, Request::Start),
            Err(SendError::NoDaemon { .. })
        ));
    }

    #[test]
    fn garbage_is_rejected_and_not_forwarded() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let (sender, receiver) = mpsc::channel();
        let _server = ControlServer::bind(&path, sender).unwrap();
        for (line, reply) in [
            (&b"record\n"[..], Reply::UnknownRequest),
            (b"start", Reply::Malformed),
            (&[b'x'; MAX_LINE][..], Reply::Malformed),
        ] {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.write_all(line).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut answer = Vec::new();
            stream.read_to_end(&mut answer).unwrap();
            assert_eq!(Reply::decode(&answer), Some(reply), "{line:?}");
        }
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_not() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "a crashed daemon leaves its socket behind");
        let (sender, _receiver) = mpsc::channel();
        let live = ControlServer::bind(&path, sender.clone()).unwrap();
        let error = ControlServer::bind(&path, sender.clone())
            .err()
            .expect("a second listener on a live socket");
        assert!(
            error.to_string().contains("another spokenpad daemon"),
            "{error}"
        );
        drop(live);

        fs::write(&path, "not a socket").unwrap();
        let error = ControlServer::bind(&path, sender)
            .err()
            .expect("a plain file is not replaced");
        assert!(error.to_string().contains("not a socket"), "{error}");
    }

    #[test]
    fn a_daemon_that_is_shutting_down_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let (sender, receiver) = mpsc::channel();
        let _server = ControlServer::bind(&path, sender).unwrap();
        drop(receiver);
        assert!(matches!(
            send(&path, Request::Stop),
            Err(SendError::Refused(Reply::ShuttingDown))
        ));
    }
}
