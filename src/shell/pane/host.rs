//! The pane, on a thread of its own, as the daemon's editor thread sees it.
//!
//! A pane has to be *driven*: Neovim reports its display whenever it has
//! something to say, and nothing is drawn until [`Pane::step`] applies it. The
//! editor thread cannot do that, because it blocks on its own channel waiting
//! for transcripts. So the pane gets a thread, and this is the handle to it.
//!
//! The daemon says only two things: open one, and stop — and, while Neovim
//! holds one of its calls behind a command the user left half typed, that it
//! is waiting ([`PaneHost::show_held`]). A window that closes needs no word
//! to end the passage: the editor inside the pane dies with it, its socket
//! goes with it, and the next key-down finds a dead socket and asks for a new
//! pane. What the daemon *can* ask is whether one is open at all
//! ([`PaneHost::alive`]), which is how a wait for a dead editor ends early
//! instead of running out its clock, and whether the user closed the last
//! one ([`PaneHost::take_closed_by_user`]), which cancels the capture it was
//! showing.
//!
//! The thread blocks with no deadline. A command does not wait to be noticed:
//! whoever sends one also knocks on the window ([`x11::Waker`]), which makes
//! the blocked `step` return at once.
use super::{Ending, Options, Pane, Status, x11};
use crate::shell::sync::lock;
use anyhow::{Context, Result, bail};
use std::{
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{
            Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, channel, sync_channel,
        },
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// A backstop, not a poll: every command wakes the loop, so this only bounds
/// how long a wake that was somehow lost could go unnoticed.
const BACKSTOP: Duration = Duration::from_secs(3600);

/// Where a pane should open, and what to run in it.
pub struct Opening {
    pub command: Command,
    pub options: Options,
}

enum Work {
    Open {
        opening: Box<Opening>,
        answer: SyncSender<Result<(u16, u16)>>,
    },
    Close,
}

/// What the thread publishes for its handle to read. Its mutexes are taken
/// with [`lock`], also after a thread panicked holding one: every critical
/// section is one assignment, `take` or read of an `Option` (the waker's
/// `wake` reads it), so a panic inside one cannot leave the value half
/// written, and the daemon's editor thread, which asks on every press, keeps
/// working.
#[derive(Default)]
struct Shared {
    /// Set while a pane is open, so a caller waiting on that pane's editor
    /// can tell "still starting" from "gone".
    alive: Arc<AtomicBool>,
    /// How to interrupt the open pane's loop.
    waker: Mutex<Option<x11::Waker>>,
    /// Whether the daemon is waiting for a call Neovim holds behind a
    /// half-typed command, which the pane then says in its last row.
    held: AtomicBool,
    /// When the user closed the pane that is open or last was, until the
    /// daemon asks. Set before `alive` goes false, so that a caller who finds
    /// the pane gone and no close recorded knows it did not end by the
    /// user's hand; cleared when the next pane is asked for, so a close is
    /// never taken for the next pane's.
    closed_by_user: Mutex<Option<Instant>>,
}

impl Shared {
    fn took(&self, pane: &Pane) {
        *lock(&self.waker) = Some(pane.waker());
        self.alive.store(true, Ordering::Release);
    }

    fn gave_up(&self) {
        self.alive.store(false, Ordering::Release);
        *lock(&self.waker) = None;
    }

    /// Make the pane's loop come round, so a command just sent is acted on.
    fn knock(&self) {
        if let Some(waker) = lock(&self.waker).as_ref()
            && let Err(error) = waker.wake()
        {
            log::debug!("could not wake the pane: {error:#}");
        }
    }
}

pub struct PaneHost {
    work: Sender<Work>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl PaneHost {
    pub fn start() -> Result<Self> {
        let (work, shared, thread) = spawn()?;
        Ok(Self {
            work,
            shared,
            thread: Some(thread),
        })
    }

    /// Whether a pane is open right now, as a flag another thread can read.
    ///
    /// The daemon waits for the editor inside a pane on that editor's own
    /// socket, which cannot tell a slow start from a dead one. This can: a
    /// pane whose editor exited is dropped by the thread, and the flag goes
    /// false with it. Take it after [`Self::open`] has succeeded — a restart
    /// makes a new one.
    pub fn alive(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.alive)
    }

    /// Open a pane and wait until its window is up, or say why it is not.
    /// Returns the grid it opened with, columns and rows: what was asked
    /// for, cut to the monitor.
    ///
    /// `deadline` is the caller's, for the whole operation; the editor inside
    /// gets what is left of it through `options.attach_timeout`. There is one
    /// deadline and one owner: if this returns an error, no window is left
    /// behind, because the thread drops a pane nobody is waiting for.
    pub fn open(&mut self, opening: Opening, deadline: Instant) -> Result<(u16, u16)> {
        // A panic on the pane thread used to disable pane mode for the life
        // of the daemon. It costs one window, not the feature.
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            log::warn!("the pane thread stopped unexpectedly; starting another");
            self.restart()?;
        }
        let (answer, reply) = sync_channel(1);
        self.work
            .send(Work::Open {
                opening: Box::new(opening),
                answer,
            })
            .ok()
            .context("the pane thread has stopped")?;
        self.shared.knock();
        let waiting = deadline.saturating_duration_since(Instant::now());
        match reply.recv_timeout(waiting) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => bail!("the pane did not open within {waiting:?}"),
            Err(RecvTimeoutError::Disconnected) => bail!("the pane thread stopped while opening"),
        }
    }

    /// When the user closed the last pane — its window, or `:q` in it — if
    /// they did since the last time this was asked: the moment the window
    /// manager's request or Neovim's word that it is quitting arrived.
    pub fn take_closed_by_user(&self) -> Option<Instant> {
        lock(&self.shared.closed_by_user).take()
    }

    /// Whether a close by the user is recorded and not taken yet.
    pub fn closed_by_user_pending(&self) -> bool {
        lock(&self.shared.closed_by_user).is_some()
    }

    /// Say in the pane, or stop saying, that the daemon is waiting for a call
    /// Neovim holds behind a command half typed in it. Neovim cannot draw
    /// that itself: it is the one not running.
    pub fn show_held(&self, held: bool) {
        self.shared.held.store(held, Ordering::Release);
        self.shared.knock();
    }

    /// Close the pane, if one is open. The editor inside it is asked to write
    /// what it has and quit.
    pub fn close(&self) {
        let _ = self.work.send(Work::Close);
        self.shared.knock();
    }

    fn restart(&mut self) -> Result<()> {
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::error!("the pane thread panicked");
        }
        let (work, shared, thread) = spawn()?;
        self.work = work;
        self.shared = shared;
        self.thread = Some(thread);
        Ok(())
    }
}

impl Drop for PaneHost {
    fn drop(&mut self) {
        // Dropping the sender is the signal to stop: the thread sees its
        // channel close, drops the pane — which writes the buffer and reaps
        // nvim — and returns. The knock is what makes it look.
        let (work, _) = channel();
        drop(std::mem::replace(&mut self.work, work));
        self.shared.knock();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::error!("the pane thread panicked");
        }
    }
}

fn spawn() -> Result<(Sender<Work>, Arc<Shared>, JoinHandle<()>)> {
    let (work, receiver) = channel();
    let shared = Arc::new(Shared::default());
    let theirs = Arc::clone(&shared);
    let thread = std::thread::Builder::new()
        .name("spokenpad-pane".to_owned())
        .spawn(move || serve(receiver, theirs))
        .context("start the pane thread")?;
    Ok((work, shared, thread))
}

fn serve(work: Receiver<Work>, shared: Arc<Shared>) {
    let mut pane: Option<Pane> = None;
    loop {
        let next = match pane.as_mut() {
            Some(open) => {
                let stepped = open.step(BACKSTOP).and_then(|status| match status {
                    Status::Running => open
                        .show_held(shared.held.load(Ordering::Acquire))
                        .map(|()| status),
                    Status::Finished(_) => Ok(status),
                });
                match stepped {
                    Ok(Status::Running) => {}
                    Ok(Status::Finished(ending)) => {
                        match ending {
                            Ending::ByUser { at } => {
                                log::info!("the dictation pane closed: the user closed it");
                                *lock(&shared.closed_by_user) = Some(at);
                            }
                            Ending::EditorDied => log::warn!(
                                "the dictation pane closed: its editor went away without being told to quit"
                            ),
                        }
                        // Dropping it writes every modified buffer and reaps
                        // the editor; the next key-down opens a new one.
                        pane = None;
                        shared.gave_up();
                    }
                    Err(error) => {
                        log::warn!("the dictation pane stopped: {error:#}");
                        pane = None;
                        shared.gave_up();
                    }
                }
                match work.try_recv() {
                    Ok(next) => next,
                    Err(TryRecvError::Empty) => continue,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            None => match work.recv() {
                Ok(next) => next,
                Err(_) => break,
            },
        };
        match next {
            Work::Open { opening, answer } => {
                // A close recorded for the pane before this one is that
                // pane's; the one about to open must not inherit it.
                *lock(&shared.closed_by_user) = None;
                // Any pane still open is replaced, not stacked: one window is
                // one passage, and the caller only asks when it has none.
                pane = None;
                shared.gave_up();
                match open(*opening) {
                    Ok(opened) => {
                        shared.took(&opened);
                        if answer.send(Ok(opened.size())).is_err() {
                            // The caller gave up waiting. Keeping this window
                            // would orphan it: a live editor on the dictation
                            // socket that no session owns, which every later
                            // key-down would refuse rather than replace.
                            log::warn!(
                                "the pane opened after the daemon stopped waiting for it; closing it"
                            );
                            drop(opened);
                            shared.gave_up();
                        } else {
                            pane = Some(opened);
                        }
                    }
                    Err(error) => {
                        let _ = answer.send(Err(error));
                    }
                }
            }
            Work::Close => {
                pane = None;
                shared.gave_up();
            }
        }
    }
    drop(pane);
    shared.gave_up();
}

fn open(opening: Opening) -> Result<Pane> {
    let Opening { command, options } = opening;
    let mut pane = Pane::open(&options, command)?;
    pane.show()?;
    Ok(pane)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread that panics while it holds the close record does not take
    /// the daemon's editor thread down with it the next time it asks.
    #[test]
    fn a_poisoned_close_record_is_still_read() {
        let shared = Arc::new(Shared::default());
        let at = Instant::now();
        let holder = Arc::clone(&shared);
        let panicked = std::thread::spawn(move || {
            let mut closed = holder.closed_by_user.lock().unwrap();
            *closed = Some(at);
            panic!("while holding the lock");
        })
        .join();
        assert!(panicked.is_err());
        assert!(shared.closed_by_user.is_poisoned());
        assert_eq!(lock(&shared.closed_by_user).take(), Some(at));
        assert_eq!(*lock(&shared.closed_by_user), None);
    }
}
