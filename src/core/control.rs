//! The control protocol: what `spokenpad start|stop|toggle|cancel` tells the
//! daemon, as values.
//!
//! One exchange per connection. The client writes one request line, the
//! daemon answers with one reply line and closes. Both lines are a single
//! ASCII word or two, ending in `\n`, so the protocol can be spoken with
//! `socat` as well as with the CLI. The socket itself lives in
//! [`shell::control`](crate::shell::control).

use std::{fmt, time::Instant};

/// Longest line either side accepts, newline included. Far above any line the
/// protocol defines, far below anything worth buffering.
pub const MAX_LINE: usize = 64;

/// What a key binding asks of the daemon. The four subcommands of the same
/// names send exactly these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// The push-to-talk key went down: begin a capture.
    Start,
    /// The push-to-talk key came up: end the held capture.
    Stop,
    /// Begin a latched capture, or end the capture that is running.
    Toggle,
    /// Throw away the capture being recorded.
    Cancel,
}

impl Request {
    pub const ALL: [Self; 4] = [Self::Start, Self::Stop, Self::Toggle, Self::Cancel];

    /// The word on the wire, which is also the subcommand's name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Toggle => "toggle",
            Self::Cancel => "cancel",
        }
    }

    /// The request line a client sends.
    pub fn encode(self) -> String {
        format!("{}\n", self.as_str())
    }

    /// Reads one request line, newline included.
    pub fn decode(line: &[u8]) -> Result<Self, Reply> {
        let word = line.strip_suffix(b"\n").ok_or(Reply::Malformed)?;
        Self::ALL
            .into_iter()
            .find(|request| request.as_str().as_bytes() == word)
            .ok_or(Reply::UnknownRequest)
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The daemon's answer. `Accepted` means the request was queued for the
/// session, not that it changed anything: a `stop` with no capture running
/// is accepted and ignored, because the binding that sent it cannot know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Accepted,
    /// No newline within [`MAX_LINE`] bytes, or the connection closed first.
    Malformed,
    UnknownRequest,
    /// The daemon is shutting down and will not act on anything more.
    ShuttingDown,
    /// systemd started a daemon for this press, but another spokenpad
    /// daemon of this user is running (one started by hand) and holds the
    /// lock. This one waits for it and takes over once it stops.
    AnotherDaemon,
    /// systemd started a daemon for this press, but it cannot create or lock
    /// its lock file: the state directory is not writable, or the disk is
    /// full. It keeps trying, and its log says why.
    CannotLock,
}

impl Reply {
    const ALL: [Self; 6] = [
        Self::Accepted,
        Self::Malformed,
        Self::UnknownRequest,
        Self::ShuttingDown,
        Self::AnotherDaemon,
        Self::CannotLock,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "ok",
            Self::Malformed => "error malformed request",
            Self::UnknownRequest => "error unknown request",
            Self::ShuttingDown => "error shutting down",
            Self::AnotherDaemon => "error another daemon is running",
            Self::CannotLock => "error cannot lock the state directory",
        }
    }

    pub fn encode(self) -> String {
        format!("{}\n", self.as_str())
    }

    /// Reads one reply line, newline included; `None` for anything this
    /// version of the protocol does not define.
    pub fn decode(line: &[u8]) -> Option<Self> {
        let text = line.strip_suffix(b"\n")?;
        Self::ALL
            .into_iter()
            .find(|reply| reply.as_str().as_bytes() == text)
    }
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A request as the session sees it: stamped with the moment the daemon read
/// it, never with a time the client claims. Two processes a key binding
/// spawns can reach the daemon in either order, so only the daemon's clock
/// orders them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    pub request: Request,
    pub at: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_round_trips() {
        for request in Request::ALL {
            assert_eq!(Request::decode(request.encode().as_bytes()), Ok(request));
            assert!(request.encode().len() <= MAX_LINE);
        }
    }

    #[test]
    fn every_reply_round_trips() {
        for reply in Reply::ALL {
            assert_eq!(Reply::decode(reply.encode().as_bytes()), Some(reply));
            assert!(reply.encode().len() <= MAX_LINE);
        }
    }

    #[test]
    fn a_request_is_one_exact_word_and_a_newline() {
        for line in [&b"start"[..], b"", b"\n\n"] {
            let expected = if line.ends_with(b"\n") {
                Reply::UnknownRequest
            } else {
                Reply::Malformed
            };
            assert_eq!(Request::decode(line), Err(expected), "{line:?}");
        }
        for line in [
            &b"START\n"[..],
            b" start\n",
            b"start \n",
            b"start\r\n",
            b"start\nstop\n",
            b"\n",
            b"\xff\n",
        ] {
            assert_eq!(
                Request::decode(line),
                Err(Reply::UnknownRequest),
                "{line:?}"
            );
        }
    }

    #[test]
    fn an_unknown_reply_is_not_mistaken_for_one() {
        for line in [&b"ok"[..], b"OK\n", b"error\n", b""] {
            assert_eq!(Reply::decode(line), None, "{line:?}");
        }
    }
}
