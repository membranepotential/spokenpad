//! The msgpack-RPC transport to a running Neovim, and nothing else.
//!
//! Every call carries an absolute deadline rather than a per-operation
//! timeout: one request can involve several reads, and a peer that dribbles
//! bytes must not be able to extend the call indefinitely by staying just
//! inside a per-read timeout. The editor thread is on the dictation path, so
//! no call here may block without a way out — with one exception, chosen per
//! call: a [`Patience::WhileTyping`] call outlives its deadline while Neovim
//! says it is holding the call until its user finishes typing a command,
//! because the call is not lost then, only late. Its caller still bounds that
//! wait.
use anyhow::{Result, anyhow, ensure};
use rmpv::Value;
use std::{
    io::{BufReader, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    time::{Duration, Instant},
};

/// Depth limit for a decoded reply, so a hostile or broken peer cannot make
/// the decoder recurse without bound.
pub(crate) const RPC_MAX_DEPTH: usize = 32;
/// Byte budget for one message on the wire: a request is refused if its
/// encoding exceeds this, and a reply is abandoned once it has read this
/// much. It is per message, not per session — a long dictation sends many
/// appends, each of which is bounded on its own.
pub(crate) const RPC_MAX_BYTES: usize = 8 * 1024 * 1024;

/// One msgpack-RPC message, in either direction.
///
/// The transport is not part of this: the daemon speaks to the editor over a
/// Unix socket ([`RpcClient`] below), and the pane speaks to the Neovim it
/// embedded over that process's stdin and stdout
/// ([`shell::pane::ui`](crate::shell::pane::ui)). Both frame messages the
/// same way, so both build them here.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Message {
    Request {
        id: u64,
        method: String,
        arguments: Vec<Value>,
    },
    Response {
        id: u64,
        /// `Nil` when the call succeeded.
        error: Value,
        result: Value,
    },
    Notification {
        method: String,
        arguments: Vec<Value>,
    },
}

impl Message {
    /// The message as msgpack, or an error when it would exceed the per
    /// message budget.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let value = match self {
            Self::Request {
                id,
                method,
                arguments,
            } => Value::Array(vec![
                Value::from(0),
                Value::from(*id),
                Value::from(method.as_str()),
                Value::Array(arguments.clone()),
            ]),
            Self::Response { id, error, result } => Value::Array(vec![
                Value::from(1),
                Value::from(*id),
                error.clone(),
                result.clone(),
            ]),
            Self::Notification { method, arguments } => Value::Array(vec![
                Value::from(2),
                Value::from(method.as_str()),
                Value::Array(arguments.clone()),
            ]),
        };
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &value)?;
        ensure!(
            bytes.len() <= RPC_MAX_BYTES,
            "nvim RPC message exceeded {RPC_MAX_BYTES} bytes"
        );
        Ok(bytes)
    }

    /// A decoded msgpack value as a message, or `None` when it is not one.
    ///
    /// Anything else on the channel is the peer's business, not a protocol
    /// error to abort on: the caller skips it and reads the next message.
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        let parts = value.as_array()?;
        match (parts.first()?.as_i64()?, parts.len()) {
            (0, 4) => Some(Self::Request {
                id: parts[1].as_u64()?,
                method: parts[2].as_str()?.to_owned(),
                arguments: parts[3].as_array()?.clone(),
            }),
            (1, 4) => Some(Self::Response {
                id: parts[1].as_u64()?,
                error: parts[2].clone(),
                result: parts[3].clone(),
            }),
            (2, 3) => Some(Self::Notification {
                method: parts[1].as_str()?.to_owned(),
                arguments: parts[2].as_array()?.clone(),
            }),
            _ => None,
        }
    }
}

/// A failure of one RPC call, classified by what the caller can do about it.
#[derive(Debug)]
pub(super) enum RpcFailure {
    /// The deadline expired. For a request, the outcome is *unknown*: the
    /// call may still complete inside the editor after the reply was lost.
    Timeout(String),
    /// The socket file exists but nothing is listening on it.
    StaleSocket(std::io::Error),
    /// The caller stopped waiting for a call Neovim was holding, because the
    /// daemon is stopping. Like a timeout, the outcome is unknown: the call is
    /// still in Neovim's queue.
    Abandoned(String),
    /// The connection was accepted, then the peer closed it before answering.
    /// Linux queues a Unix-socket connect on the listener before anyone calls
    /// accept, so a listener that is closed in that window (an editor exiting,
    /// or another process still holding the descriptor across a fork) yields
    /// a broken pipe or a reset on the first exchange rather than a refusal.
    PeerGone(std::io::Error),
    Other(anyhow::Error),
}

impl RpcFailure {
    /// A socket path with no live listener behind it.
    pub(super) fn is_stale_socket(&self) -> bool {
        matches!(
            self,
            Self::StaleSocket(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionReset
                )
        )
    }

    fn from_io(error: std::io::Error, timed_out: &str) -> Self {
        match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                Self::Timeout(timed_out.to_owned())
            }
            std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof => Self::PeerGone(error),
            _ => Self::Other(error.into()),
        }
    }
}

impl std::fmt::Display for RpcFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(message) | Self::Abandoned(message) => formatter.write_str(message),
            Self::StaleSocket(error) => error.fmt(formatter),
            Self::PeerGone(error) => write!(formatter, "nvim closed the connection: {error}"),
            Self::Other(error) => error.fmt(formatter),
        }
    }
}

impl From<RpcFailure> for anyhow::Error {
    fn from(value: RpcFailure) -> Self {
        match value {
            RpcFailure::Timeout(message) | RpcFailure::Abandoned(message) => anyhow!(message),
            RpcFailure::StaleSocket(error) => error.into(),
            RpcFailure::PeerGone(error) => {
                anyhow::Error::from(error).context("nvim closed the connection before answering")
            }
            RpcFailure::Other(error) => error,
        }
    }
}

/// How long a live Neovim takes, at most, to answer `nvim_get_mode`.
const MODE_ANSWER: Duration = Duration::from_millis(250);
/// How often a call held back by a half-typed command asks again whether it
/// still is.
const MODE_POLL: Duration = Duration::from_millis(500);

/// What a call whose reply misses its deadline has to conclude.
pub(super) enum Patience<'a> {
    /// The call timed out, and its outcome is unknown.
    Deadline,
    /// Ask Neovim whether it is only waiting for the rest of a command the
    /// user is typing in its window: the call runs the moment that command
    /// is finished or cancelled. For calls on an editor already attached to,
    /// where timing out would divert text from a window that is alive and
    /// only waiting for its user.
    ///
    /// The call asks this every half second while it waits, before its
    /// deadline too: `Ok` waits on, and an error ends the wait with that
    /// error. The transport has no opinion on how long is too long; the
    /// session does, and can also end the wait for reasons of its own.
    WhileTyping(&'a mut dyn FnMut(Waiting) -> Result<(), RpcFailure>),
}

/// Where a patient call is, each time it asks its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Waiting {
    /// Unanswered, and its deadline has not passed yet.
    Unanswered,
    /// Neovim says it is holding the call behind keys it waits for, and has
    /// for this long.
    Held(Duration),
}

/// Why no response arrived.
enum Late {
    /// The deadline passed between two messages.
    Between,
    /// The deadline passed in the middle of one, which leaves the channel out
    /// of step.
    Within,
    Failed(RpcFailure),
}

/// Whether an `nvim_get_mode` answer says Neovim is blocked on input: in the
/// middle of a command, a count, a register name or a prompt.
pub(crate) fn waiting_for_keys(mode: &Value) -> bool {
    mode.as_map().is_some_and(|fields| {
        fields
            .iter()
            .any(|(key, value)| key.as_str() == Some("blocking") && value.as_bool() == Some(true))
    })
}

pub(super) struct RpcClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl RpcClient {
    pub(super) fn connect(path: &Path, deadline: Instant) -> Result<Self, RpcFailure> {
        let writer = connect_unix(path, deadline)?;
        let reader = BufReader::new(
            writer
                .try_clone()
                .map_err(|error| RpcFailure::Other(error.into()))?,
        );
        Ok(Self {
            reader,
            writer,
            next_id: 1,
        })
    }

    /// Sends a request and waits for its reply, discarding anything else the
    /// editor says on the channel meanwhile.
    pub(super) fn request(
        &mut self,
        method: &str,
        arguments: Vec<Value>,
        deadline: Instant,
        patience: Patience<'_>,
    ) -> Result<Value, RpcFailure> {
        let id = self.send_request(method, arguments, deadline)?;
        self.reply(id, method, deadline, patience)
    }

    /// The first half of [`request`](Self::request): writes the request and
    /// returns its id. A failure here means the editor never received the
    /// whole message, so it cannot have acted on it — unlike a failure while
    /// waiting for the reply, whose outcome is unknown.
    pub(super) fn send_request(
        &mut self,
        method: &str,
        arguments: Vec<Value>,
        deadline: Instant,
    ) -> Result<u64, RpcFailure> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.send(
            Message::Request {
                id,
                method: method.to_owned(),
                arguments,
            },
            deadline,
        )?;
        Ok(id)
    }

    /// The second half of [`request`](Self::request): waits for the reply to
    /// request `id`.
    pub(super) fn reply(
        &mut self,
        id: u64,
        method: &str,
        deadline: Instant,
        mut patience: Patience<'_>,
    ) -> Result<Value, RpcFailure> {
        let timed_out = || RpcFailure::Timeout(format!("nvim RPC {method} timed out"));
        let patient = matches!(patience, Patience::WhileTyping(_));
        let mut deadline = deadline;
        // The `nvim_get_mode` asked once the deadline passed, while it is
        // unanswered.
        let mut asked: Option<u64> = None;
        // When Neovim first said it was holding the call.
        let mut held_since: Option<Instant> = None;
        loop {
            // A patient call wakes every half second before its deadline, so
            // its caller can end the wait early.
            let until = match patient && asked.is_none() {
                true => deadline.min(Instant::now() + MODE_POLL),
                false => deadline,
            };
            let (answered, outcome) = match self.next_response(until) {
                Ok(response) => response,
                Err(Late::Between) if patient && asked.is_none() && Instant::now() < deadline => {
                    if let Patience::WhileTyping(watch) = &mut patience {
                        watch(Waiting::Unanswered)?;
                    }
                    continue;
                }
                Err(Late::Between) if patient && asked.is_none() => {
                    // `nvim_get_mode` is one of the few calls Neovim answers
                    // at once even while it waits for a key, so a live editor
                    // answers it within milliseconds whatever it is doing.
                    deadline = Instant::now() + MODE_ANSWER;
                    asked = Some(self.send_request("nvim_get_mode", Vec::new(), deadline)?);
                    continue;
                }
                Err(Late::Between | Late::Within) => return Err(timed_out()),
                Err(Late::Failed(failure)) => return Err(failure),
            };
            if answered == id {
                return outcome.map_err(|error| {
                    RpcFailure::Other(anyhow!("nvim RPC {method} failed: {error}"))
                });
            }
            if asked != Some(answered) {
                continue;
            }
            asked = None;
            if !outcome.is_ok_and(|mode| waiting_for_keys(&mode)) {
                return Err(timed_out());
            }
            // Neovim holds every other call while the user has half a
            // command typed in its window -- a count, `g`, `"`, `f` -- and
            // runs them the moment the command is finished. The call is not
            // lost, so giving up on it would only make its outcome unknown;
            // the caller decides how long that is worth.
            let since = *held_since.get_or_insert_with(Instant::now);
            if let Patience::WhileTyping(watch) = &mut patience {
                watch(Waiting::Held(since.elapsed()))?;
            }
            deadline = Instant::now() + MODE_POLL;
        }
    }

    /// The next response on the channel, skipping anything that is not one.
    fn next_response(&mut self, deadline: Instant) -> Result<(u64, Result<Value, Value>), Late> {
        loop {
            let mut reader = DeadlineRead::new(&mut self.reader, deadline, RPC_MAX_BYTES);
            let decoded = rmpv::decode::read_value_with_max_depth(&mut reader, RPC_MAX_DEPTH);
            let untouched = reader.remaining == RPC_MAX_BYTES;
            let message = match decoded {
                Ok(message) => message,
                Err(error) => {
                    return Err(
                        match RpcFailure::from_io(error.into(), "nvim RPC read timed out") {
                            // Nothing of the next message was read, so the
                            // channel is still in step and can carry more.
                            RpcFailure::Timeout(_) if untouched => Late::Between,
                            RpcFailure::Timeout(_) => Late::Within,
                            failure => Late::Failed(failure),
                        },
                    );
                }
            };
            if let Some(Message::Response { id, error, result }) = Message::parse(&message) {
                let outcome = if error.is_nil() {
                    Ok(result)
                } else {
                    Err(error)
                };
                return Ok((id, outcome));
            }
        }
    }

    /// Sends a notification: no reply, so the deadline bounds the write only.
    pub(super) fn notify(
        &mut self,
        method: &str,
        arguments: Vec<Value>,
        deadline: Instant,
    ) -> Result<(), RpcFailure> {
        self.send(
            Message::Notification {
                method: method.to_owned(),
                arguments,
            },
            deadline,
        )
    }

    fn send(&mut self, message: Message, deadline: Instant) -> Result<(), RpcFailure> {
        let bytes = message.encode().map_err(RpcFailure::Other)?;
        let mut writer = DeadlineWrite::new(&mut self.writer, deadline);
        writer
            .write_all(&bytes)
            .map_err(|error| RpcFailure::from_io(error, "nvim RPC write timed out"))?;
        writer
            .flush()
            .map_err(|error| RpcFailure::from_io(error, "nvim RPC flush timed out"))
    }
}

/// A reader that enforces one absolute deadline and one byte budget across
/// however many `read` calls the decoder makes.
struct DeadlineRead<'a> {
    reader: &'a mut BufReader<UnixStream>,
    deadline: Instant,
    remaining: usize,
}

impl<'a> DeadlineRead<'a> {
    fn new(reader: &'a mut BufReader<UnixStream>, deadline: Instant, budget: usize) -> Self {
        Self {
            reader,
            deadline,
            remaining: budget,
        }
    }
}

impl Read for DeadlineRead<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining_time = self.deadline.saturating_duration_since(Instant::now());
        if remaining_time.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "nvim RPC read deadline expired",
            ));
        }
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "nvim RPC response exceeded byte budget",
            ));
        }
        self.reader
            .get_ref()
            .set_read_timeout(Some(remaining_time))?;
        let capacity = buffer.len().min(self.remaining);
        let count = self.reader.read(&mut buffer[..capacity])?;
        self.remaining -= count;
        Ok(count)
    }
}

struct DeadlineWrite<'a> {
    writer: &'a mut UnixStream,
    deadline: Instant,
}

impl<'a> DeadlineWrite<'a> {
    fn new(writer: &'a mut UnixStream, deadline: Instant) -> Self {
        Self { writer, deadline }
    }
}

impl Write for DeadlineWrite<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.writer
            .set_write_timeout(Some(self.remaining("nvim RPC write deadline expired")?))?;
        self.writer.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer
            .set_write_timeout(Some(self.remaining("nvim RPC flush deadline expired")?))?;
        self.writer.flush()
    }
}

impl DeadlineWrite<'_> {
    fn remaining(&self, expired: &'static str) -> std::io::Result<std::time::Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, expired));
        }
        Ok(remaining)
    }
}

/// Connects to a Unix socket under a deadline.
///
/// `UnixStream::connect` has no timeout, so this opens a non-blocking socket,
/// polls it for writability, and only then turns blocking I/O back on: a
/// socket file left behind by a killed editor must fail fast rather than hang
/// the editor thread.
fn connect_unix(path: &Path, deadline: Instant) -> Result<UnixStream, RpcFailure> {
    let bytes = path.as_os_str().as_bytes();
    ensure_socket_path(bytes).map_err(RpcFailure::Other)?;
    // SAFETY: socket has no pointer arguments and returns a new owned descriptor.
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw_fd == -1 {
        return Err(RpcFailure::Other(std::io::Error::last_os_error().into()));
    }
    // SAFETY: `raw_fd` was just returned by socket and has no other owner.
    let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    // SAFETY: zero is a valid initial representation for sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(bytes.iter().copied()) {
        *target = source as libc::c_char;
    }
    let address_length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: address points to an initialized sockaddr_un of `address_length`
    // bytes, and the descriptor is owned and live for the call.
    let result = unsafe {
        libc::connect(
            owned.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_length,
        )
    };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(RpcFailure::StaleSocket(error));
        }
        wait_for_connect(owned.as_raw_fd(), deadline)?;
    }
    set_blocking(owned.as_raw_fd()).map_err(|error| RpcFailure::Other(error.into()))?;
    Ok(UnixStream::from(owned))
}

fn ensure_socket_path(path: &[u8]) -> Result<()> {
    ensure!(!path.contains(&0), "nvim socket path contains NUL");
    ensure!(
        path.len()
            < std::mem::size_of::<libc::sockaddr_un>()
                - std::mem::offset_of!(libc::sockaddr_un, sun_path),
        "nvim socket path is too long"
    );
    Ok(())
}

fn wait_for_connect(fd: libc::c_int, deadline: Instant) -> Result<(), RpcFailure> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RpcFailure::Timeout(
                "nvim socket connect timed out".to_owned(),
            ));
        }
        let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: descriptor is valid for one pollfd element for this call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready == 0 {
            return Err(RpcFailure::Timeout(
                "nvim socket connect timed out".to_owned(),
            ));
        }
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(RpcFailure::Other(error.into()));
        }
        let mut socket_error: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
        // SAFETY: the descriptor is live and both pointers reference writable
        // objects of exactly the sizes passed alongside them.
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut socket_error).cast(),
                &mut length,
            )
        } == -1
        {
            return Err(RpcFailure::Other(std::io::Error::last_os_error().into()));
        }
        if socket_error != 0 {
            return Err(RpcFailure::StaleSocket(std::io::Error::from_raw_os_error(
                socket_error,
            )));
        }
        return Ok(());
    }
}

fn set_blocking(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: fd is owned and live; fcntl does not retain pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above; flags came from F_GETFL for this descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixListener, thread, time::Duration};

    #[test]
    fn socket_path_must_be_nul_free_and_short_enough() {
        assert!(ensure_socket_path(b"/run/user/1000/spokenpad-nvim.sock").is_ok());
        let nul = ensure_socket_path(b"/run/\0/sock").unwrap_err().to_string();
        assert!(nul.contains("NUL"), "{nul}");
        let long = ensure_socket_path(&[b'a'; 512]).unwrap_err().to_string();
        assert!(long.contains("too long"), "{long}");
    }

    #[test]
    fn a_socket_file_with_no_listener_is_reported_as_stale() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("abandoned.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        drop(listener);
        let failure = RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1))
            .err()
            .expect("nothing is listening");
        assert!(failure.is_stale_socket(), "{failure}");
        assert!(socket.exists(), "the abandoned socket file must remain");
    }

    #[test]
    fn request_has_one_deadline_while_bytes_dribble_in() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("dribble.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            let mut response = Vec::new();
            rmpv::encode::write_value(
                &mut response,
                &Value::Array(vec![
                    Value::from(1),
                    Value::from(1),
                    Value::Nil,
                    Value::from(1),
                ]),
            )
            .unwrap();
            for byte in response {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(30));
            }
        });
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = client
            .request(
                "nvim_eval",
                vec![Value::from("1")],
                Instant::now() + Duration::from_millis(100),
                Patience::Deadline,
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Timeout(_)), "{error}");
        assert!(started.elapsed() < Duration::from_millis(300));
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn request_times_out_when_peer_never_replies() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("stalled.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(200));
        });
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = client
            .request(
                "nvim_eval",
                vec![Value::from("1")],
                Instant::now() + Duration::from_millis(50),
                Patience::Deadline,
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Timeout(_)), "{error}");
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn a_reply_to_another_request_is_skipped_rather_than_returned() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("interleaved.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            let mut response = Vec::new();
            // A notification from the editor, then a reply carrying a
            // different request id, then the one this caller is waiting for.
            for message in [
                Value::Array(vec![
                    Value::from(2),
                    Value::from("redraw"),
                    Value::Array(Vec::new()),
                ]),
                Value::Array(vec![
                    Value::from(1),
                    Value::from(99),
                    Value::Nil,
                    Value::from("stale"),
                ]),
                Value::Array(vec![
                    Value::from(1),
                    Value::from(1),
                    Value::Nil,
                    Value::from("mine"),
                ]),
            ] {
                rmpv::encode::write_value(&mut response, &message).unwrap();
            }
            let _ = stream.write_all(&response);
            thread::sleep(Duration::from_millis(50));
        });
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let value = client
            .request(
                "nvim_eval",
                vec![Value::from("1")],
                Instant::now() + Duration::from_secs(1),
                Patience::Deadline,
            )
            .unwrap();
        assert_eq!(value.as_str(), Some("mine"));
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn an_error_reply_becomes_an_error_not_a_value() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("failing.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            let mut response = Vec::new();
            rmpv::encode::write_value(
                &mut response,
                &Value::Array(vec![
                    Value::from(1),
                    Value::from(1),
                    Value::Array(vec![Value::from(0), Value::from("Vim:E5108")]),
                    Value::Nil,
                ]),
            )
            .unwrap();
            let _ = stream.write_all(&response);
            thread::sleep(Duration::from_millis(50));
        });
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let error = client
            .request(
                "nvim_exec_lua",
                vec![Value::from("error('boom')")],
                Instant::now() + Duration::from_secs(1),
                Patience::Deadline,
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Other(_)), "{error}");
        assert!(error.to_string().contains("E5108"), "{error}");
        drop(client);
        peer.join().unwrap();
    }

    /// A stand-in editor that answers `nvim_get_mode` with `blocking` and
    /// every other request only after `hold` has passed since it arrived:
    /// Neovim with half a command typed into it, finished `hold` later.
    fn held_editor(
        socket: &Path,
        blocking: bool,
        hold: Duration,
    ) -> thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(socket).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut seen = Vec::new();
            let mut held: Option<(u64, Instant)> = None;
            let answer = |writer: &mut UnixStream, id: u64, result: Value| {
                let mut bytes = Vec::new();
                rmpv::encode::write_value(
                    &mut bytes,
                    &Value::Array(vec![Value::from(1), Value::from(id), Value::Nil, result]),
                )
                .unwrap();
                writer.write_all(&bytes).is_ok()
            };
            loop {
                if let Some((id, since)) = held
                    && since.elapsed() >= hold
                {
                    let _ = answer(&mut writer, id, Value::from("done"));
                    return seen;
                }
                let message = match rmpv::decode::read_value(&mut reader) {
                    Ok(message) => message,
                    Err(error) => match std::io::Error::from(error).kind() {
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => continue,
                        _ => return seen,
                    },
                };
                let Some(Message::Request { id, method, .. }) = Message::parse(&message) else {
                    continue;
                };
                seen.push(method.clone());
                if method == "nvim_get_mode" {
                    let mode = Value::Map(vec![
                        (Value::from("mode"), Value::from("n")),
                        (Value::from("blocking"), Value::from(blocking)),
                    ]);
                    if !answer(&mut writer, id, mode) {
                        return seen;
                    }
                } else {
                    held = Some((id, Instant::now()));
                }
            }
        })
    }

    #[test]
    fn a_patient_call_outlasts_a_half_typed_command() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("typing.sock");
        let peer = held_editor(&socket, true, Duration::from_millis(1_200));
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let mut asked = Vec::new();
        let value = client
            .request(
                "nvim_exec_lua",
                vec![Value::from("return 1")],
                Instant::now() + Duration::from_millis(100),
                Patience::WhileTyping(&mut |waiting| {
                    if let Waiting::Held(held) = waiting {
                        asked.push(held);
                    }
                    Ok(())
                }),
            )
            .expect("an editor waiting for its user is waited for");
        assert_eq!(value.as_str(), Some("done"));
        assert!(started.elapsed() >= Duration::from_millis(1_200));
        assert!(
            asked.len() >= 2 && asked.windows(2).all(|pair| pair[0] < pair[1]),
            "the caller should be asked on every repeat, with the time held so far: {asked:?}"
        );
        drop(client);
        let seen = peer.join().unwrap();
        assert_eq!(seen.first().map(String::as_str), Some("nvim_exec_lua"));
        assert!(
            seen.iter()
                .filter(|method| *method == "nvim_get_mode")
                .count()
                >= 2,
            "the client should keep asking while the command is half typed: {seen:?}"
        );
    }

    #[test]
    fn a_patient_call_still_times_out_on_an_editor_that_is_not_waiting_for_keys() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("busy.sock");
        let peer = held_editor(&socket, false, Duration::from_secs(2));
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = client
            .request(
                "nvim_exec_lua",
                vec![Value::from("return 1")],
                Instant::now() + Duration::from_millis(100),
                Patience::WhileTyping(&mut |waiting| match waiting {
                    Waiting::Held(_) => panic!("an editor that is not held is never held"),
                    Waiting::Unanswered => Ok(()),
                }),
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Timeout(_)), "{error}");
        assert!(started.elapsed() < Duration::from_millis(100) + MODE_ANSWER);
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn a_patient_call_ends_when_its_caller_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("given-up.sock");
        let peer = held_editor(&socket, true, Duration::from_secs(3));
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = client
            .request(
                "nvim_exec_lua",
                vec![Value::from("return 1")],
                Instant::now() + Duration::from_millis(100),
                Patience::WhileTyping(&mut |waiting| match waiting {
                    Waiting::Held(held) if held >= Duration::from_millis(600) => {
                        Err(RpcFailure::Abandoned("stopping".to_owned()))
                    }
                    _ => Ok(()),
                }),
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Abandoned(_)), "{error}");
        assert!(started.elapsed() < Duration::from_millis(1_500));
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn a_patient_call_can_be_ended_before_its_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("frozen.sock");
        let peer = held_editor(&socket, true, Duration::from_secs(3));
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let mut asked = 0;
        let error = client
            .request(
                "nvim_exec_lua",
                vec![Value::from("return 1")],
                Instant::now() + Duration::from_secs(10),
                Patience::WhileTyping(&mut |waiting| {
                    assert_eq!(waiting, Waiting::Unanswered);
                    asked += 1;
                    match asked {
                        2 => Err(RpcFailure::Abandoned("stopping".to_owned())),
                        _ => Ok(()),
                    }
                }),
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Abandoned(_)), "{error}");
        let took = started.elapsed();
        assert!(
            took >= 2 * MODE_POLL && took < 3 * MODE_POLL,
            "asked every half second: {took:?}"
        );
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn a_strict_call_does_not_ask_why_it_is_late() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("strict.sock");
        let peer = held_editor(&socket, true, Duration::from_millis(600));
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let error = client
            .request(
                "nvim_exec_lua",
                vec![Value::from("return 1")],
                Instant::now() + Duration::from_millis(100),
                Patience::Deadline,
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Timeout(_)), "{error}");
        drop(client);
        assert_eq!(peer.join().unwrap(), vec!["nvim_exec_lua".to_owned()]);
    }
}
