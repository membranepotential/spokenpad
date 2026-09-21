//! The pane, on a thread of its own, as the daemon's editor thread sees it.
//!
//! A pane has to be *driven*: Neovim reports its display whenever it has
//! something to say, and nothing is drawn until [`Pane::step`] applies it. The
//! editor thread cannot do that, because it blocks on its own channel waiting
//! for transcripts. So the pane gets a thread, and this is the handle to it.
//!
//! The daemon says only two things: open one, and stop. It is never told that
//! a window closed, because it does not need to be — the editor inside the
//! pane dies with it, its socket goes with it, and the next key-down finds a
//! dead socket and asks for a new pane. That is exactly what managed mode
//! does when the user closes the terminal, and it is why the passage ends the
//! same way in both. What the daemon *can* ask is whether one is open at all
//! ([`PaneHost::alive`]), which is how a wait for a dead editor ends early
//! instead of running out its clock.
//!
//! The thread blocks with no deadline. A command does not wait to be noticed:
//! whoever sends one also knocks on the window ([`x11::Waker`]), which makes
//! the blocked `step` return at once.
use super::{Options, Pane, Status, x11};
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
    /// The top-left corner the pane should be moved to once the window
    /// manager has taken the window. `None` leaves it where it opened.
    pub correct_to: Option<(i32, i32)>,
}

enum Work {
    Open {
        opening: Box<Opening>,
        answer: SyncSender<Result<()>>,
    },
    Close,
}

/// What the thread publishes for its handle to read.
#[derive(Default)]
struct Shared {
    /// Set while a pane is open, so a caller waiting on that pane's editor
    /// can tell "still starting" from "gone".
    alive: Arc<AtomicBool>,
    /// How to interrupt the open pane's loop.
    waker: Mutex<Option<x11::Waker>>,
}

impl Shared {
    fn took(&self, pane: &Pane) {
        *self.waker.lock().expect("the pane waker") = Some(pane.waker());
        self.alive.store(true, Ordering::Release);
    }

    fn gave_up(&self) {
        self.alive.store(false, Ordering::Release);
        *self.waker.lock().expect("the pane waker") = None;
    }

    /// Make the pane's loop come round, so a command just sent is acted on.
    fn knock(&self) {
        if let Some(waker) = self.waker.lock().expect("the pane waker").as_ref()
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
    ///
    /// `deadline` is the caller's, for the whole operation; the editor inside
    /// gets what is left of it through `options.attach_timeout`. There is one
    /// deadline and one owner: if this returns an error, no window is left
    /// behind, because the thread drops a pane nobody is waiting for.
    pub fn open(&mut self, opening: Opening, deadline: Instant) -> Result<()> {
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
                match open.step(BACKSTOP) {
                    Ok(Status::Running) => {}
                    Ok(Status::Finished) => {
                        log::info!("the dictation pane closed");
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
                // Any pane still open is replaced, not stacked: one window is
                // one passage, and the caller only asks when it has none.
                pane = None;
                shared.gave_up();
                match open(*opening) {
                    Ok(opened) => {
                        shared.took(&opened);
                        if answer.send(Ok(())).is_err() {
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
    let Opening {
        command,
        options,
        correct_to,
    } = opening;
    let mut pane = Pane::open(&options, command)?;
    pane.show()?;
    if let Some((x, y)) = correct_to {
        // The position asked for before the map gets the window close; this
        // corrects for whatever frame the window manager drew around it. It
        // is a few pixels, and it happens before Neovim has drawn anything.
        if let Err(error) = pane.place_at(x, y) {
            log::debug!("could not correct the pane's position: {error:#}");
        }
    }
    Ok(pane)
}
