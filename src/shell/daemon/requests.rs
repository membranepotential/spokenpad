//! The control requests, as the event loop reads them.
use crate::core::{
    control::{Received, Request},
    state::Event,
};
use std::{
    collections::VecDeque,
    sync::mpsc::{self, Receiver},
    time::Instant,
};

/// Control requests in arrival order, each preceded by the clock at its own
/// stamp, so a repeat window that closed before a request arrived is closed
/// before the request is read (see [`crate::core::state`]).
///
/// A release's post-roll ends as soon as a request that would start a
/// capture is waiting; it looks by moving every received request into
/// `early`, which is drained before anything newer, so nothing is lost or
/// reordered.
pub(super) struct Requests {
    receiver: Receiver<Received>,
    early: VecDeque<Received>,
    drain: Drain,
    /// False once the sender is gone: the control socket closed.
    alive: bool,
}

/// Where one drain of [`Requests`] stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drain {
    /// Next is the clock: at the head request's stamp, or, with none
    /// waiting, at the current time.
    Clock,
    /// The clock at the head request's stamp was handed out; the request
    /// itself is next.
    Request,
    /// The drain ended with the clock at the current time; the next call
    /// returns `None` and begins a new drain.
    Done,
}

impl Requests {
    pub(super) fn new(receiver: Receiver<Received>) -> Self {
        Self {
            receiver,
            early: VecDeque::new(),
            drain: Drain::Clock,
            alive: true,
        }
    }

    /// The next event of one drain: the clock and then the request, for each
    /// waiting request; then the clock at the current time, once; then
    /// `None`, and the next call begins a new drain.
    pub(super) fn next(&mut self) -> Option<Event> {
        if self.early.is_empty()
            && let Some(request) = self.receive()
        {
            self.early.push_back(request);
        }
        if let Some(&head) = self.early.front() {
            return Some(match self.drain {
                Drain::Request => {
                    self.drain = Drain::Clock;
                    self.early.pop_front();
                    Event::Request(head)
                }
                Drain::Clock | Drain::Done => {
                    self.drain = Drain::Request;
                    Event::Clock { now: head.at }
                }
            });
        }
        if self.drain == Drain::Done {
            self.drain = Drain::Clock;
            return None;
        }
        let now = Instant::now();
        // A request stamped before `now` may have arrived meanwhile; it goes
        // first, or the clock could close a window that it continues.
        match self.receive() {
            Some(request) => {
                self.early.push_back(request);
                self.next()
            }
            None => {
                self.drain = Drain::Done;
                Some(Event::Clock { now })
            }
        }
    }

    /// Whether a request that would start a capture is waiting, or no request
    /// can ever arrive again. A `stop` or a `cancel` must not shorten the
    /// post-roll of the capture that is ending: the `stop` of the press that
    /// ended a latched capture follows it within the post-roll.
    pub(super) fn start_waiting(&mut self) -> bool {
        while let Some(request) = self.receive() {
            self.early.push_back(request);
        }
        self.early
            .iter()
            .any(|r| matches!(r.request, Request::Start | Request::Toggle))
            || !self.alive
    }

    /// No request is queued and none can arrive again. Disconnection alone is
    /// not enough: `start_waiting` may have seen it while requests it moved
    /// into `early` are still owed to the loop.
    pub(super) fn exhausted(&self) -> bool {
        !self.alive && self.early.is_empty()
    }

    fn receive(&mut self) -> Option<Received> {
        match self.receiver.try_recv() {
            Ok(request) => Some(request),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.alive = false;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Every request is preceded by the clock at its own stamp, and a drain
    /// ends with the clock at the current time, once.
    #[test]
    fn requests_are_clocked_at_their_stamps() {
        let (sender, receiver) = mpsc::channel();
        let mut requests = Requests::new(receiver);
        let t = Instant::now();
        let stop = Received {
            request: Request::Stop,
            at: t,
        };
        let start = Received {
            request: Request::Start,
            at: t + Duration::from_millis(40),
        };
        sender.send(stop).unwrap();
        sender.send(start).unwrap();
        assert_eq!(requests.next(), Some(Event::Clock { now: t }));
        assert!(requests.start_waiting(), "the start is waiting behind it");
        assert_eq!(requests.next(), Some(Event::Request(stop)));
        assert_eq!(requests.next(), Some(Event::Clock { now: start.at }));
        assert_eq!(requests.next(), Some(Event::Request(start)));
        let before = Instant::now();
        assert!(matches!(
            requests.next(),
            Some(Event::Clock { now }) if now >= before
        ));
        assert_eq!(requests.next(), None, "the drain is over");
        assert!(matches!(requests.next(), Some(Event::Clock { .. })));
        assert!(!requests.exhausted());
        drop(sender);
        assert_eq!(requests.next(), None);
        assert!(requests.exhausted());
    }
}
