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
//! same way in both.
use super::{Options, Pane, Status};
use anyhow::{Context, Result, bail};
use std::{
    process::Command,
    sync::mpsc::{
        Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, channel, sync_channel,
    },
    thread::JoinHandle,
    time::Duration,
};

/// How long the thread waits on the pane before looking at its own channel.
///
/// It is not a frame rate: the pane blocks for this whole period unless
/// something happens. It is how long a `close` or a daemon shutdown may sit
/// unanswered, traded against waking four times a second to find nothing.
const STEP: Duration = Duration::from_millis(250);

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

pub struct PaneHost {
    work: Sender<Work>,
    thread: Option<JoinHandle<()>>,
}

impl PaneHost {
    pub fn start() -> Result<Self> {
        let (work, rx) = channel();
        let thread = std::thread::Builder::new()
            .name("spokenpad-pane".to_owned())
            .spawn(move || serve(rx))
            .context("start the pane thread")?;
        Ok(Self {
            work,
            thread: Some(thread),
        })
    }

    /// Open a pane and wait until its window is up, or say why it is not.
    ///
    /// The editor behind it may still be starting: the caller waits for that
    /// on the editor's own socket, exactly as it does for a spawned terminal.
    pub fn open(&self, opening: Opening, within: Duration) -> Result<()> {
        let (answer, reply) = sync_channel(1);
        self.work
            .send(Work::Open {
                opening: Box::new(opening),
                answer,
            })
            .ok()
            .context("the pane thread has stopped")?;
        match reply.recv_timeout(within + STEP) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => bail!("the pane did not open within {within:?}"),
            Err(RecvTimeoutError::Disconnected) => bail!("the pane thread stopped while opening"),
        }
    }

    /// Close the pane, if one is open. The editor inside it is asked to write
    /// what it has and quit.
    pub fn close(&self) {
        let _ = self.work.send(Work::Close);
    }
}

impl Drop for PaneHost {
    fn drop(&mut self) {
        // Dropping the sender is the signal to stop: the thread sees its
        // channel close, drops the pane — which writes the buffer and reaps
        // nvim — and returns.
        let (work, _) = channel();
        drop(std::mem::replace(&mut self.work, work));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(work: Receiver<Work>) {
    let mut pane: Option<Pane> = None;
    loop {
        let next = match pane.as_mut() {
            Some(open) => {
                match open.step(STEP) {
                    Ok(Status::Running) => {}
                    Ok(Status::Finished) => {
                        log::info!("the dictation pane closed");
                        // Dropping it writes every modified buffer and reaps
                        // the editor; the next key-down opens a new one.
                        pane = None;
                    }
                    Err(error) => {
                        log::warn!("the dictation pane stopped: {error:#}");
                        pane = None;
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
                match open(*opening) {
                    Ok(opened) => {
                        pane = Some(opened);
                        let _ = answer.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = answer.send(Err(error));
                    }
                }
            }
            Work::Close => pane = None,
        }
    }
    drop(pane);
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
