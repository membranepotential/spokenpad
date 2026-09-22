//! Four owners: main/session, control socket, inference, and editor; recorder
//! owns disk I/O.
//!
//! [`run`] is the imperative shell: it takes the per-user lock, listens on the
//! control socket, registers signals and opens PortAudio, and hands the
//! inference thread a loader that downloads and loads the models while
//! presses are already accepted. [`serve`] is the event loop over whatever
//! devices it is handed, which is what the headless end-to-end tests drive.
use crate::{
    config::{Config, Source},
    core::{
        control::{Received, Reply, Request},
        decode::{
            Commit, Pipeline, Preview, Recognizer, Segmenter, TickKind, Utterance, UtteranceId,
            Worker,
        },
        frames::Frames,
        session::{Notice, Preparing, Recognition, RecordingStatus, Rest, Session},
        state::{Cause, Command, DiscardReason, Event, State},
        text::Processor,
    },
    shell::{
        audio::{
            AudioCapture, CaptureEvent, Captured, InputBackend, MAX_UTTERANCE_SECONDS, PostRoll,
        },
        control::{ControlServer, Socket, refuse_until},
        inference::{SpeechSegmenter, Transcriber, load_segmenter},
        models::{Repair, ensure_defaults, repair_defaults},
        nvim::{AppendFailure, CopyOutcome, Detached, IndicatorState, NvimSession, Want},
        recorder::{CaptureReader, Unfinished},
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// How long the shutdown waits for a background thread, and therefore how
/// long queued editor appends have to reach the file.
///
/// It is also the whole budget a pane has to write what is in its buffers and
/// close: when this runs out the process exits, and a pane thread stops
/// wherever it had got to. `shell::pane` divides it and checks that its
/// pieces fit.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const LOOP_INTERVAL: Duration = Duration::from_millis(20);
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Below this a capture that is much shorter than the hold is reported.
const INCOMPLETE_HOLD: Duration = Duration::from_secs(1);
const NEARLY_SILENT_PEAK: f32 = 0.01;

/// Everything the event loop reads from the outside world.
pub struct Devices<B: InputBackend, R, S> {
    pub capture: AudioCapture<B>,
    /// Control requests. The sender being dropped means the socket closed.
    pub requests: Receiver<Received>,
    pub pipeline: PipelineSource<R, S>,
    /// The config file, read again for each new dictation window.
    pub reload: Reload,
}

/// Builds the recognizer and the segmenter on the inference thread, telling
/// it each step as it begins. Called again, at the next press, after it
/// failed.
pub type Loader<R, S> = Box<dyn FnMut(&mut dyn FnMut(Preparing)) -> Result<Pipeline<R, S>> + Send>;

/// Where the event loop's pipeline comes from.
pub enum PipelineSource<R, S> {
    /// Built already: the loop is ready from its first press.
    Ready(Pipeline<R, S>),
    /// Built by the inference thread while the loop already accepts presses.
    /// Until it is, captures are kept on disk and transcribed afterwards.
    Load(Loader<R, S>),
}

/// A recording whose text is not all written yet: its recovery WAV, the
/// utterance it is, how far into the WAV its text reaches, where this
/// daemon's attempt at it began, and where it is now.
struct Transcription {
    path: PathBuf,
    id: UtteranceId,
    through: Frames,
    from: Frames,
    stage: Stage,
}

/// Where a [`Transcription`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Still being captured.
    Capturing,
    /// Released, and decoded as it was captured: only its tail is left.
    Finishing,
    /// Captured before the speech model was ready, or left untranscribed by
    /// the last daemon: kept on disk until the model is ready.
    Kept,
    /// Sent to the engine as a recording.
    Sent,
    /// Its transcription failed after getting further than the attempt
    /// before; the next start tries the rest again.
    Retry,
}

impl Stage {
    /// Whether a crash, which writes no list, should leave it listed. A
    /// capture decoded as it was captured is listed only at an orderly stop:
    /// its text since the last list would be written twice.
    fn listed_while_running(self) -> bool {
        match self {
            Self::Kept | Self::Sent | Self::Retry => true,
            Self::Capturing | Self::Finishing => false,
        }
    }
}

/// Every recording whose text is not all written yet, in the order they
/// were made.
#[derive(Default)]
struct Transcriptions(Vec<Transcription>);

impl Transcriptions {
    /// The recordings made while the speech model was not ready that it has
    /// yet to transcribe, as the window counts them.
    fn waiting(&self) -> usize {
        self.0
            .iter()
            .filter(|t| matches!(t.stage, Stage::Kept | Stage::Sent))
            .count()
    }
    fn get_mut(&mut self, id: UtteranceId) -> Option<&mut Transcription> {
        self.0.iter_mut().find(|t| t.id == id)
    }
    fn remove(&mut self, id: UtteranceId) -> Option<Transcription> {
        let at = self.0.iter().position(|t| t.id == id)?;
        Some(self.0.remove(at))
    }
    /// Notes how far a recording's text reaches.
    fn advance(&mut self, commit: &Commit) {
        if let Some(t) = self.get_mut(commit.utterance.id) {
            t.through = t.through.max(commit.through);
        }
    }
    /// Those the list the next start reads holds while this daemon runs.
    fn listed_while_running(&self) -> impl Iterator<Item = &Transcription> {
        self.0.iter().filter(|t| t.stage.listed_while_running())
    }
}

/// A recording ends, or stops being readable, before the text already written
/// from it does: it was cut or replaced since, and its rest is gone.
#[derive(Debug)]
struct ShorterThanItsText {
    recorded: f64,
    through: f64,
}

impl std::fmt::Display for ShorterThanItsText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "it ends at {:.2}s, before the {:.2}s its text already reaches",
            self.recorded, self.through
        )
    }
}

impl std::error::Error for ShorterThanItsText {}

/// Control requests in arrival order, each preceded by the clock at its own
/// stamp, so a repeat window that closed before a request arrived is closed
/// before the request is read (see [`crate::core::state`]).
///
/// A release's post-roll ends as soon as a request that would start a
/// capture is waiting; it looks by moving every received request into
/// `early`, which is drained before anything newer, so nothing is lost or
/// reordered.
struct Requests {
    receiver: Receiver<Received>,
    early: VecDeque<Received>,
    /// The clock at the head request's stamp was already handed out.
    head_clocked: bool,
    /// This drain already ended with the clock at the current time.
    drained: bool,
    /// False once the sender is gone: the control socket closed.
    alive: bool,
}

impl Requests {
    fn new(receiver: Receiver<Received>) -> Self {
        Self {
            receiver,
            early: VecDeque::new(),
            head_clocked: false,
            drained: false,
            alive: true,
        }
    }

    /// The next event of one drain: the clock and then the request, for each
    /// waiting request; then the clock at the current time, once; then
    /// `None`, and the next call begins a new drain.
    fn next(&mut self) -> Option<Event> {
        if self.early.is_empty()
            && let Some(request) = self.receive()
        {
            self.early.push_back(request);
        }
        if let Some(&head) = self.early.front() {
            self.drained = false;
            if std::mem::replace(&mut self.head_clocked, true) {
                self.head_clocked = false;
                self.early.pop_front();
                return Some(Event::Request(head));
            }
            return Some(Event::Clock { now: head.at });
        }
        if std::mem::replace(&mut self.drained, true) {
            self.drained = false;
            return None;
        }
        let now = Instant::now();
        // A request stamped before `now` may have arrived meanwhile; it goes
        // first, or the clock could close a window that it continues.
        match self.receive() {
            Some(request) => {
                self.drained = false;
                self.early.push_back(request);
                self.next()
            }
            None => Some(Event::Clock { now }),
        }
    }

    /// Whether a request that would start a capture is waiting, or no request
    /// can ever arrive again. A `stop` or a `cancel` must not shorten the
    /// post-roll of the capture that is ending: the `stop` of the press that
    /// ended a latched capture follows it within the post-roll.
    fn start_waiting(&mut self) -> bool {
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
    fn exhausted(&self) -> bool {
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

enum Work {
    Tick {
        audio: Vec<f32>,
        start: Frames,
        utterance: Arc<Utterance>,
        kind: TickKind,
    },
    Finish(Tail),
    /// Transcribe a recording made before the model was ready, or left by
    /// the last daemon, from `from` on: its text before that is written.
    Recording {
        path: PathBuf,
        utterance: Arc<Utterance>,
        from: Frames,
    },
    /// Try the loader again: the last attempt failed.
    Load,
    Quit,
}
/// What a released capture still holds for its final decode: the audio
/// after `start`, everything before it having been committed.
struct Tail {
    audio: Vec<f32>,
    start: Frames,
    utterance: Arc<Utterance>,
}
enum ResultEvent {
    Preparing(Preparing),
    Ready {
        segmenter: bool,
        elapsed: Duration,
    },
    Unavailable(String),
    /// The config file did not load when a window opened, for this reason.
    /// Sent by the editor thread.
    ConfigInvalid(String),
    /// The user closed the dictation pane at `at`. Sent by the editor thread,
    /// which asks the pane after every piece of work and every 66 ms.
    WindowClosed {
        at: Instant,
    },
    Commit(Commit),
    Tick {
        id: UtteranceId,
        result: Result<Option<Preview>>,
        elapsed: Duration,
    },
    Finished {
        id: UtteranceId,
        result: Result<(String, usize)>,
        elapsed: Duration,
    },
}

/// Everything the editor's winbar shows, replaced wholesale, never merged.
/// `status` is the speech model's notice, shown when it outranks the
/// capture's own.
fn indicator_of(session: &Session, level: f32, status: Option<Notice>) -> IndicatorState {
    IndicatorState {
        phase: session.state.indicator_phase(),
        level: f64::from(level),
        preview: session.preview().to_owned(),
        notice: session.shown_notice(status).as_ref().map(Notice::text),
        latched: session.state.latched(),
        previewing: session.previewing(),
    }
}

enum EditorWork {
    Ensure,
    Append {
        utterance: UtteranceId,
        text: String,
    },
    Indicator(IndicatorState),
    /// Copy the whole dictation buffer to `+`. Sent once per utterance, after
    /// its `Finished` result -- see the comment where it is sent, in
    /// `serve`, for why every `Append` of that utterance is already ahead of
    /// it in this same queue.
    Copy,
    /// Closing the window cancelled the capture it showed: say so, since the
    /// window that would have is gone.
    ClosedMidCapture,
    Quit,
}

fn send<T>(tx: &Sender<T>, value: T) -> Result<()> {
    tx.send(value)
        .map_err(|_| anyhow::anyhow!("a background thread stopped unexpectedly"))
}

fn join_bounded(handle: JoinHandle<()>, name: &str) {
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        if handle.join().is_err() {
            log::error!("{name} thread panicked");
        }
    } else {
        log::warn!(
            "{name} did not stop within {}s; process exit will release it",
            SHUTDOWN_GRACE.as_secs()
        );
    }
}

/// Where an utterance's last commit went. A later commit of the same
/// utterance continues its line only in the same place: joined onto another
/// file's last line, it would run into unrelated text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    Editor,
    Passage,
}

/// Reads the configuration again. The editor thread calls it before each
/// new dictation window, so that `[nvim]` changes apply to that window
/// without a restart.
pub type Reload = Box<dyn FnMut() -> Result<Config> + Send>;

/// The `[nvim]` settings the next window opens with: what the file says now.
/// A file that no longer loads keeps the settings in use, and the window
/// says why. Changes to any other section only take effect at a restart,
/// which is logged once per distinct set of them.
fn reconfigure(
    nvim: &mut NvimSession,
    running: &Config,
    reload: &mut Reload,
    reported: &mut Vec<&'static str>,
    events: &Sender<ResultEvent>,
) {
    match reload() {
        Ok(fresh) => {
            let restart = running.restart_needed(&fresh);
            if restart != *reported && !restart.is_empty() {
                log::warn!(
                    "config changes to [{}] take effect after `systemctl --user restart spokenpad`; [nvim] applies to this window",
                    restart.join("], [")
                );
            }
            *reported = restart;
            nvim.reconfigure(fresh.nvim);
        }
        Err(e) => {
            let reason = format!("{e:#}")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            log::error!("config not reloaded: {reason}; the window keeps the settings in use");
            let _ = events.send(ResultEvent::ConfigInvalid(reason));
        }
    }
}

/// Owns the editor connection. It runs until its channel says to stop, never
/// on a shared flag: text already queued must reach a file even while the
/// daemon is shutting down or the capture it came from was cancelled.
///
/// The session's [`quitting`](NvimSession::quitting) flag changes only how:
/// once it is set, an append that is not answered within a quarter second —
/// held behind a half-typed command, or sent to an editor that stopped
/// answering — is given up on without reconnecting and repeating it, and
/// its text goes to the pending passage like any unconfirmed append; an
/// editor that is not connected is not reattached or opened, so what is
/// still queued goes there too, within the shutdown grace instead of
/// waiting on an editor.
fn editor_thread(
    mut nvim: NvimSession,
    running: Config,
    mut reload: Reload,
    rx: Receiver<EditorWork>,
    events: Sender<ResultEvent>,
) {
    let quitting = nvim.quitting();
    let mut reported = Vec::new();
    let mut paragraph: Option<(UtteranceId, Sink)> = None;
    let mut shown: Option<IndicatorState> = None;
    let mut quit = false;
    while !quit {
        let first = match rx.recv_timeout(Duration::from_millis(66)) {
            Ok(work) => Some(work),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        report_user_close(&mut nvim, &events);
        // Drain what is already queued, keeping only the newest indicator:
        // the loop offers one 50 times a second and only the last is true.
        let mut indicator = None;
        for work in first.into_iter().chain(rx.try_iter()) {
            // Before each piece of work, so that text arriving just after
            // the user closed the pane does not open another.
            report_user_close(&mut nvim, &events);
            match work {
                EditorWork::Indicator(next) => indicator = Some(next),
                EditorWork::Ensure if quitting.load(Ordering::Acquire) => {}
                EditorWork::Ensure => {
                    // A window about to open, or an editor about to be
                    // attached, takes the settings the file has now.
                    if !nvim.attached() {
                        reconfigure(&mut nvim, &running, &mut reload, &mut reported, &events);
                    }
                    match nvim.ensure(Want::Press) {
                        Ok(Some(path)) => {
                            log::info!("dictating into {}", path.display());
                            shown = None;
                        }
                        Ok(None) => log::info!(
                            "no dictation editor is open; text goes to the dictation file until `spokenpad editor` opens it"
                        ),
                        Err(e) => log::error!("could not open dictation window: {e:#}"),
                    }
                }
                EditorWork::Append { utterance, text } => {
                    // A failed request elsewhere (a clipboard copy that timed
                    // out, an indicator push) drops the connection; text that
                    // is owed to the file reattaches rather than waiting for
                    // the next key-down to do it.
                    if !nvim.connected()
                        && !quitting.load(Ordering::Acquire)
                        && let Err(e) = nvim.ensure(Want::Text)
                    {
                        log::warn!("could not reattach to the dictation window: {e:#}");
                    }
                    // Whether the editor may have this text after all, so
                    // the pending passage is a second copy rather than the
                    // only one.
                    let mut maybe_landed = false;
                    if nvim.connected() {
                        let now = Instant::now();
                        let continued = paragraph == Some((utterance, Sink::Editor));
                        match nvim.append(&text, continued) {
                            Ok(line) => {
                                paragraph = Some((utterance, Sink::Editor));
                                shown = None;
                                log::info!(
                                    "appended in {:.0}ms (line {line})",
                                    now.elapsed().as_secs_f64() * 1000.
                                );
                                continue;
                            }
                            // The editor went away before it got the text —
                            // closed while the tail was decoding — so the
                            // text is certainly not there and goes to the
                            // file below.
                            Err(AppendFailure::NotSent(e)) => {
                                log::warn!("the dictation editor did not receive the text: {e:#}");
                            }
                            // Not confirmed even by the repeat with the same
                            // operation id. Kept only in the log, the text
                            // would be lost to the user if the editor never
                            // took it, which is the usual case: Neovim drops
                            // a request whose connection closed before it ran.
                            // So it goes to the pending passage as well, and
                            // the notification says it may be in both.
                            Err(AppendFailure::Unconfirmed(e)) => {
                                log::error!(
                                    "append not confirmed: {e:#}; writing the text to the pending passage too, so if the editor took it after all it is in both"
                                );
                                maybe_landed = true;
                            }
                        }
                    }
                    // With no editor at all, the text goes to the file the
                    // next editor will open on, rather than nowhere.
                    let continued = paragraph == Some((utterance, Sink::Passage));
                    match nvim.append_detached(&text, continued) {
                        Ok(write) => {
                            paragraph = Some((utterance, Sink::Passage));
                            log::info!("no dictation editor: appended to {}", write.path.display());
                            if maybe_landed {
                                nvim.notify_detached(&write.path, Detached::Unconfirmed);
                            } else if write.started {
                                nvim.notify_detached(&write.path, Detached::NoEditor);
                            }
                        }
                        Err(e) => {
                            paragraph = None;
                            log::error!(
                                "could not write {} characters of transcript to a dictation file: {e:#}; recover from the capture WAV if available",
                                text.chars().count()
                            );
                        }
                    }
                }
                EditorWork::Copy => {
                    // Off by default: with `nvim.copy_to_clipboard` false
                    // nothing is copied. Never on a session with no editor
                    // either: nothing has been spoken into a window that was
                    // never opened, and there is nothing to copy.
                    if nvim.config().copy_to_clipboard && nvim.connected() {
                        match nvim.copy_buffer() {
                            Ok(CopyOutcome::Copied) => {
                                log::debug!("copied the dictation buffer to the clipboard")
                            }
                            Ok(CopyOutcome::Empty) => {}
                            Ok(CopyOutcome::Failed(reason)) => {
                                log::warn!("clipboard copy failed: {reason}")
                            }
                            Err(e) => log::warn!("clipboard copy unavailable: {e:#}"),
                        }
                    }
                }
                EditorWork::ClosedMidCapture => nvim.notify_closed_mid_capture(),
                EditorWork::Quit => {
                    quit = true;
                    break;
                }
            }
        }
        if nvim.connected()
            && let Some(next) = indicator
            && shown.as_ref() != Some(&next)
        {
            if let Err(e) = nvim.set_indicator(&next) {
                log::warn!("editor indicator unavailable: {e:#}");
            }
            shown = Some(next);
        }
    }
    // Nothing can be written after this point; say what was lost.
    for work in rx.try_iter() {
        if let EditorWork::Append { text, .. } = work {
            log::error!(
                "shutting down with {} characters of undelivered transcript; recover from the capture WAV",
                text.chars().count()
            );
        }
    }
    nvim.close();
}

/// Tells the event loop that the user closed the dictation pane, if they did
/// since the last time the pane was asked, so it can cancel the capture that
/// pane showed.
fn report_user_close(nvim: &mut NvimSession, events: &Sender<ResultEvent>) {
    if let Some(at) = nvim.take_user_close() {
        log::info!("the user closed the dictation pane; the next text opens no window of its own");
        // The loop is gone only while this thread is being stopped.
        let _ = events.send(ResultEvent::WindowClosed { at });
    }
}

/// Ends a capture that is thrown away rather than decoded. Its recovery WAV
/// is kept, whatever the reason, but nothing transcribes the rest of it.
fn discard<B: InputBackend>(
    capture: &mut AudioCapture<B>,
    session: &Session,
    transcriptions: &mut Transcriptions,
    reason: DiscardReason,
) {
    capture.stop_capture();
    if let Some(utterance) = &session.current {
        transcriptions.remove(utterance.id);
    }
    match reason {
        DiscardReason::TooShort => log::info!(
            "capture held under {}ms; discarded, WAV retained",
            crate::core::state::MINIMUM_HOLD.as_millis()
        ),
        DiscardReason::Cancelled => {
            log::info!("capture cancelled; settled text and WAV retained")
        }
        DiscardReason::WindowClosed => log::info!(
            "capture cancelled: the user closed the dictation window; settled text and WAV retained"
        ),
    }
}

/// Another daemon of this user holds the lock. A daemon started by hand
/// exits with it, and `main` gives it exit code 3; one that systemd started
/// waits for the lock instead ([`lock_under_activation`]).
#[derive(Debug)]
pub struct AnotherDaemon(std::io::Error);

impl std::fmt::Display for AnotherDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "another spokenpad daemon is running ({})", self.0)
    }
}

impl std::error::Error for AnotherDaemon {}

/// Holds the lock for the daemon lifetime. Never unlink a lock another process may hold.
fn daemon_lock() -> Result<File> {
    let dir = crate::config::state_dir();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("daemon.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    // SAFETY: flock borrows a valid owned file descriptor and retains no pointer.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err(AnotherDaemon(std::io::Error::last_os_error()).into());
    }
    Ok(file)
}

/// The imperative shell: acquire the machine's resources, then serve.
/// `config` was read from `source`, which each new dictation window reads
/// again. `inherited` is the control socket systemd passed in, if it passed
/// one; otherwise the daemon binds its own.
pub fn run(
    config: Config,
    source: Source,
    inherited: Option<Socket>,
    dump_dir: Option<&Path>,
) -> Result<()> {
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stopping))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stopping))?;
    let path = crate::config::control_socket();
    let (_lock, socket) = match inherited {
        Some(socket) => match lock_under_activation(&socket, &stopping)? {
            Some(lock) => (lock, socket),
            // Stopped while it waited.
            None => return Ok(()),
        },
        None => (daemon_lock()?, Socket::bind(&path)?),
    };
    // Served first, before anything slow: under socket activation the press
    // that started this daemon is waiting on the socket, and a request is
    // stamped when it is read. The model loads later, on the inference
    // thread, and what is recorded meanwhile is kept until it has.
    let (requests_tx, requests) = mpsc::channel();
    let _control = ControlServer::start(socket, requests_tx)?;
    log::info!("listening on {}", path.display());
    let capture = AudioCapture::new(config.audio.clone(), config.recording.clone());
    let (asr, vad, rate) = (
        config.asr.clone(),
        config.vad.clone(),
        config.audio.sample_rate,
    );
    let load: Loader<Transcriber, SpeechSegmenter> = Box::new(move |step| {
        ensure_defaults(&asr, &vad, |done, total| {
            step(Preparing::Downloading { done, total });
        })?;
        let build = || -> Result<Pipeline<Transcriber, SpeechSegmenter>> {
            log::info!("loading CPU recognizer");
            let mut recognizer = Transcriber::new(&asr, rate)?;
            recognizer.warm_up()?;
            Ok(Pipeline {
                recognizer,
                segmenter: load_segmenter(&vad, rate),
            })
        };
        load_with_repair(
            build,
            |step| {
                repair_defaults(&asr, &vad, |done, total| {
                    step(Preparing::Downloading { done, total });
                })
            },
            step,
        )
    });
    serve(
        &config,
        Devices {
            capture,
            requests,
            pipeline: PipelineSource::Load(load),
            reload: Box::new(move || source.load()),
        },
        stopping,
        dump_dir,
    )
}

/// The lock of a daemon that systemd started: waited for, not required.
///
/// Exiting without it would leave the presses queued on systemd's socket to
/// start this daemon again, and again, until the unit's start limit fails
/// `spokenpad.socket` and no key works any more. So it answers each press
/// with why it cannot serve ([`Reply::AnotherDaemon`] while a daemon started
/// by hand holds the lock, [`Reply::CannotLock`] while the state directory
/// cannot hold one), tries again before each press and every 100 ms, and
/// once it holds the lock goes on as if it had at once. `None` when told to
/// stop first.
fn lock_under_activation(socket: &Socket, stopping: &AtomicBool) -> Result<Option<File>> {
    let mut logged: Option<String> = None;
    let lock = refuse_until(socket, stopping, || {
        daemon_lock().map_err(|e| {
            let reply = if e.is::<AnotherDaemon>() {
                Reply::AnotherDaemon
            } else {
                Reply::CannotLock
            };
            let reason = format!("{e:#}");
            if logged.as_ref() != Some(&reason) {
                log::error!(
                    "cannot take the daemon lock: {reason}; answering presses with \"{reply}\" until it can"
                );
                logged = Some(reason);
            }
            reply
        })
    })?;
    if lock.is_some() && logged.is_some() {
        log::info!("took the daemon lock");
    }
    Ok(lock)
}

/// Builds the pipeline, and when a model that is in place does not load,
/// repairs it once and builds again. Whether the files are all there is
/// decided by size before this, without reading them; a file of the right
/// size with the wrong bytes is only found here, by `repair` hashing it.
fn load_with_repair<P>(
    mut build: impl FnMut() -> Result<P>,
    repair: impl FnOnce(&mut dyn FnMut(Preparing)) -> Result<Repair>,
    step: &mut dyn FnMut(Preparing),
) -> Result<P> {
    step(Preparing::Loading);
    let failure = match build() {
        Ok(pipeline) => return Ok(pipeline),
        Err(failure) => failure,
    };
    log::warn!("the speech model did not load: {failure:#}");
    let repaired = repair(step).with_context(|| {
        format!(
            "{failure:#}; downloading it again failed too (`spokenpad fetch-models` verifies and repairs the files)"
        )
    })?;
    match repaired {
        Repair::Replaced => {
            step(Preparing::Loading);
            build().context("the speech model was downloaded again and still does not load")
        }
        Repair::Verified => Err(failure.context(
            "the speech model's files match their pinned sha256, so this machine cannot load it",
        )),
        Repair::NotOurs => Err(failure.context("check asr.model_dir and asr.family")),
    }
}

/// Turns previews and the silence timeout on as far as the pipeline that
/// loaded allows.
fn configure(session: &mut Session, config: &Config, segmenter: bool) {
    // Without a segmenter nothing ever settles, so a preview would re-decode
    // the whole growing capture. That is the one thing this project refuses.
    let previews = config.preview.enabled && segmenter;
    if config.preview.enabled && !previews {
        log::warn!("no VAD model: previews are off and the capture is decoded at release");
    }
    // Only a tick can report speech, so without one there is no silence to
    // measure and a latch is bounded by its length alone.
    let silence = previews.then(|| config.capture.silence_timeout()).flatten();
    match silence {
        Some(timeout) => log::info!(
            "a latched capture ends by itself after {:.0}s without speech",
            timeout.as_secs_f64()
        ),
        None => log::info!(
            "no silence timeout: a capture ends at the {}-hour limit or the in-memory ceiling",
            crate::core::state::MAX_CAPTURE.as_secs() / 3600
        ),
    }
    session.configure(previews, silence);
}

/// The event loop. Touches no process-global state: no lock, no signals, no
/// environment, and every path it uses comes from `config`.
pub fn serve<B, R, S>(
    config: &Config,
    devices: Devices<B, R, S>,
    stopping: Arc<AtomicBool>,
    dump_dir: Option<&Path>,
) -> Result<()>
where
    B: InputBackend,
    R: Recognizer + Send + 'static,
    S: Segmenter + Send + 'static,
{
    let Devices {
        mut capture,
        requests,
        pipeline,
        reload,
    } = devices;
    let mut requests = Requests::new(requests);
    let rate = config.audio.sample_rate;
    let processor = Processor::new(&config.text)?;
    // No previews and no silence timeout until a pipeline says what it can do.
    let mut session = Session::new(
        false,
        Duration::from_millis(config.preview.interval_ms),
        None,
    );
    let mut recognition = match &pipeline {
        PipelineSource::Ready(pipeline) => {
            configure(&mut session, config, pipeline.segmenter.is_some());
            Recognition::Ready
        }
        PipelineSource::Load(_) => Recognition::Preparing(Preparing::Loading),
    };
    // First those the last daemon left, from where their text reached.
    let mut transcriptions = Transcriptions(
        capture
            .take_unfinished()
            .into_iter()
            .map(|Unfinished { path, through }| {
                log::info!(
                    "{} was left untranscribed by the last run; it is transcribed from {:.2}s once the speech model is ready",
                    path.display(),
                    through.seconds(rate)
                );
                Transcription {
                    path,
                    id: session.reserve_id(),
                    through,
                    from: through,
                    stage: Stage::Kept,
                }
            })
            .collect(),
    );
    // Whether the capture that is recording, or was last, decodes as it goes.
    // One made before the model was ready does not: it is on disk only.
    let mut live = false;
    let window = ((config.preview.max_seconds * f64::from(rate)) as usize).max(1);

    let (work_tx, work_rx) = mpsc::channel();
    let (result_tx, results) = mpsc::channel();
    let editor_events = result_tx.clone();
    let engine = thread::Builder::new()
        .name("spokenpad-asr".into())
        .spawn(move || engine_thread(pipeline, window, rate, &work_rx, &result_tx))?;
    let (editor_tx, editor_rx) = mpsc::channel();
    let running = config.clone();
    let nvim = NvimSession::new(running.nvim.clone());
    let editor_quitting = nvim.quitting();
    let editor = thread::Builder::new()
        .name("spokenpad-nvim".into())
        .spawn(move || editor_thread(nvim, running, reload, editor_rx, editor_events))?;

    let mut next_device_poll = Instant::now();
    let result = (|| -> Result<()> {
        while !stopping.load(Ordering::Acquire) {
            // What the device has to say comes first: the in-memory ceiling
            // ends the capture, and the command it produces is carried out by
            // the same match as a key press's. Only the ceiling produces one,
            // and only once per capture.
            let at = Instant::now();
            let mut ceiling = Command::Nothing;
            if at >= next_device_poll {
                next_device_poll = at + DEVICE_POLL_INTERVAL;
                for event in capture.poll() {
                    let command = apply_capture_event(event, &mut session, at);
                    if command != Command::Nothing {
                        ceiling = command;
                    }
                }
                // A capture kept on disk holds nothing in memory, so the
                // in-memory ceiling cannot end it: its length does, at the
                // same bound.
                if !live
                    && ceiling == Command::Nothing
                    && session.state.recording()
                    && capture.captured_frames().seconds(rate) >= MAX_UTTERANCE_SECONDS as f64
                {
                    log::warn!(
                        "a capture kept on disk reached {} minutes and ends here",
                        MAX_UTTERANCE_SECONDS / 60
                    );
                    ceiling = session.cap_kept((MAX_UTTERANCE_SECONDS / 60) as u64, at);
                }
            }
            // Then requests and the clock, which have priority over delivery
            // of inference results. The clock is also what ends a capture
            // that has run too long or been quiet too long.
            for _ in 0..256 {
                let command = if ceiling != Command::Nothing {
                    std::mem::replace(&mut ceiling, Command::Nothing)
                } else if let Some(event) = requests.next() {
                    session.event(event)
                } else {
                    break;
                };
                match command {
                    Command::Start => {
                        live = recognition == Recognition::Ready;
                        if let Recognition::Unavailable(_) = recognition {
                            send(&work_tx, Work::Load)?;
                            recognition = Recognition::Preparing(Preparing::Loading);
                        }
                        match capture.start_capture() {
                            Ok(()) => begin(&session, &capture, &mut transcriptions),
                            Err(e) => {
                                // The press cleared the previous notice;
                                // cancel first, then say why nothing is being
                                // recorded.
                                session.event(Event::Request(Received {
                                    request: Request::Cancel,
                                    at: Instant::now(),
                                }));
                                session.notify(Notice::MicrophoneUnavailable);
                                log::error!("capture could not start: {e:#}");
                            }
                        }
                        // Either way: on the first press of a session there is
                        // no editor yet, and a notice pushed at a window that
                        // does not exist leaves the user with a dead key and
                        // no explanation anywhere.
                        send(&editor_tx, EditorWork::Ensure)?;
                    }
                    Command::Decode { released, cause } => {
                        if cause != Cause::KeyPress {
                            log::warn!(
                                "no key ended this capture; spokenpad did: {}",
                                session
                                    .notice()
                                    .map_or_else(|| format!("{cause:?}"), Notice::to_string)
                            );
                        }
                        // The winbar says "transcribing" from here on, not
                        // only once the post-roll has arrived.
                        send(
                            &editor_tx,
                            EditorWork::Indicator(indicator_of(
                                &session,
                                0.,
                                recognition.notice(transcriptions.waiting()),
                            )),
                        )?;
                        let (captured, postroll) = capture.finish_capture(released, || {
                            stopping.load(Ordering::Acquire) || requests.start_waiting()
                        });
                        note_postroll(postroll, &mut session);
                        for event in capture.poll() {
                            let command = apply_capture_event(event, &mut session, Instant::now());
                            // This capture has already ended, so the ceiling
                            // can only add its notice here.
                            debug_assert_eq!(command, Command::Nothing);
                        }
                        // The dump holds what is decoded, and a capture made
                        // before the model was ready decodes from its WAV.
                        let tail = release(
                            &mut session,
                            &capture,
                            captured,
                            config,
                            dump_dir.filter(|_| live),
                        )?;
                        if live {
                            if let Some(t) = transcriptions.get_mut(tail.utterance.id) {
                                t.stage = Stage::Finishing;
                            }
                            send(&work_tx, Work::Finish(tail))?;
                        } else {
                            keep(
                                &mut session,
                                tail.utterance.id,
                                &capture,
                                &mut transcriptions,
                            );
                            let _ = persist(&capture, transcriptions.listed_while_running());
                        }
                    }
                    Command::Discard(reason) => {
                        discard(&mut capture, &session, &mut transcriptions, reason)
                    }
                    Command::Nothing => {}
                }
            }
            for event in results.try_iter().take(128) {
                match event {
                    ResultEvent::Preparing(step) => {
                        recognition = Recognition::Preparing(step);
                    }
                    ResultEvent::Ready { segmenter, elapsed } => {
                        log::info!("speech model ready after {:.1}s", elapsed.as_secs_f64());
                        configure(&mut session, config, segmenter);
                        recognition = Recognition::Ready;
                    }
                    ResultEvent::ConfigInvalid(reason) => {
                        session.notify(Notice::ConfigInvalid(reason));
                    }
                    // As `spokenpad cancel`: what is committed stays, the tail
                    // is not decoded, the WAV is kept. Only a capture that was
                    // running when the window closed.
                    ResultEvent::WindowClosed { at } => {
                        if let Command::Discard(reason) = session.event(Event::WindowClosed { at })
                        {
                            discard(&mut capture, &session, &mut transcriptions, reason);
                            send(&editor_tx, EditorWork::ClosedMidCapture)?;
                        }
                    }
                    ResultEvent::Unavailable(reason) => {
                        log::error!(
                            "no speech model: {reason}; captures are kept, and the next press tries again"
                        );
                        recognition = Recognition::Unavailable(reason);
                    }
                    ResultEvent::Commit(c) => {
                        transcriptions.advance(&c);
                        commit(c, &mut session, &processor, &editor_tx)?;
                    }
                    ResultEvent::Tick {
                        id,
                        result,
                        elapsed,
                    } => {
                        let preview = match result {
                            Ok(Some(preview)) => {
                                log::debug!(
                                    "preview {}: {} characters, committed through {}, decoded in {:.2}s",
                                    id.0,
                                    preview.text.chars().count(),
                                    preview.through,
                                    elapsed.as_secs_f64()
                                );
                                Some(Preview {
                                    text: processor.process(&preview.text),
                                    ..preview
                                })
                            }
                            Ok(None) => {
                                log::debug!(
                                    "preview {} stopped after {:.2}s",
                                    id.0,
                                    elapsed.as_secs_f64()
                                );
                                None
                            }
                            Err(e) => {
                                if session.is_current(id) && session.should_warn_tick_failure() {
                                    log::warn!("preview tick failed: {e:#}; will retry");
                                }
                                None
                            }
                        };
                        session.tick_finished(id, preview, Instant::now());
                    }
                    ResultEvent::Finished {
                        id,
                        result,
                        elapsed,
                    } => {
                        match &result {
                            Ok((text, frames)) => {
                                log::info!(
                                    "decoded the last {:.1}s in {:.2}s (utterance {}, {} characters)",
                                    Frames(*frames).seconds(rate),
                                    elapsed.as_secs_f64(),
                                    id.0,
                                    text.chars().count()
                                );
                            }
                            Err(e) => log::error!("decode failed for utterance {}: {e:#}", id.0),
                        }
                        if finished(
                            id,
                            result.as_ref().err(),
                            rate,
                            &capture,
                            &mut session,
                            &mut transcriptions,
                        ) {
                            log::info!("{} recordings to go", transcriptions.waiting());
                            let _ = persist(&capture, transcriptions.listed_while_running());
                        }
                        session.finish(id);
                        // Every `Commit` of this utterance was sent on this
                        // same `results` channel, synchronously, before the
                        // engine thread sent `Finished` -- see
                        // `engine_thread`'s `Work::Finish` arm. The `for`
                        // loop above therefore already turned each of them
                        // into an `EditorWork::Append` and queued it on
                        // `editor_tx` ahead of the copy queued here, so the
                        // editor always writes the append before it reads
                        // the buffer to copy. Sent on every `Finished`,
                        // whatever the release decode did: progressively
                        // committed text is in the buffer either way. The
                        // editor thread copies only when the window's
                        // `nvim.copy_to_clipboard` says to, off by default.
                        send(&editor_tx, EditorWork::Copy)?;
                    }
                }
            }
            let now = Instant::now();
            if session.tick_due(now) {
                let tail = capture.captured_frames().since(session.committed_hint);
                // A tail too long to redraw cheaply stops the cosmetic decode
                // only. The tick still runs, still commits what has settled,
                // and so still lets the shell drop the audio behind it.
                let kind = if Frames(tail).seconds(rate) > config.preview.max_seconds {
                    if session.previewing() {
                        log::warn!(
                            "preview paused: uncommitted audio exceeds preview.max_seconds; still recording and committing"
                        );
                    }
                    session.defer_previews();
                    TickKind::Commits
                } else {
                    session.resume_previews();
                    TickKind::Preview
                };
                let utterance = session
                    .current
                    .as_ref()
                    .context("recording has no utterance")?
                    .clone();
                let mut audio = capture.snapshot_capture(session.committed_hint);
                // One tick chews on at most `preview.max_seconds` of audio,
                // whatever it is for. The detector pass alone costs 0.29 s
                // over 30 s of tail and 2.4 s over 270 s, and a release
                // queued behind it waits for all of it
                // (docs/experiments/2026-09-21-constant-ram-recording.md).
                // The rest of the tail is the next tick's business; a preview
                // tick never reaches this, because it only runs while the
                // tail is under the same limit.
                audio
                    .samples
                    .truncate(((config.preview.max_seconds * f64::from(rate)) as usize).max(1));
                session.requested(now);
                log::debug!(
                    "preview {} requested: {:.2}s audio starting at {}",
                    utterance.id.0,
                    Frames(audio.samples.len()).seconds(rate),
                    audio.start
                );
                send(
                    &work_tx,
                    Work::Tick {
                        audio: audio.samples,
                        start: audio.start,
                        utterance,
                        kind,
                    },
                )?;
            }
            if recognition == Recognition::Ready {
                for recording in transcriptions
                    .0
                    .iter_mut()
                    .filter(|t| t.stage == Stage::Kept)
                {
                    // Kept since its release; see `keep`.
                    send(
                        &work_tx,
                        Work::Recording {
                            path: recording.path.clone(),
                            utterance: Utterance::new(recording.id.0),
                            from: recording.through,
                        },
                    )?;
                    recording.stage = Stage::Sent;
                }
            }
            // Audio the worker has committed is never read again, and a
            // capture that does not decode as it goes is read back from its
            // recording: the recovery WAV keeps the whole capture either way.
            capture.discard_before(if live {
                session.committed_hint
            } else {
                capture.captured_frames()
            });
            let level = if session.state.recording() {
                capture.level()
            } else {
                0.
            };
            send(
                &editor_tx,
                EditorWork::Indicator(indicator_of(
                    &session,
                    level,
                    recognition.notice(transcriptions.waiting()),
                )),
            )?;
            ensure!(
                !requests.exhausted() && !engine.is_finished() && !editor.is_finished(),
                "a required worker stopped unexpectedly"
            );
            thread::sleep(LOOP_INTERVAL);
        }
        Ok(())
    })();
    // A capture still being held has nothing the user is waiting for now: it
    // is cancelled, and listed below for the next start. One already released
    // is inside its final decode, and that text is owed to the user: the
    // bounded join below, not a cancel, is what limits the wait.
    if let Some(u) = &session.current
        && u.ticking()
    {
        u.cancel();
    }
    stopping.store(true, Ordering::Release);
    capture.shutdown();
    let _ = work_tx.send(Work::Quit);
    join_bounded(engine, "inference");
    // The engine may have committed, and finished, while the loop was
    // already leaving or during the join above; neither is dropped here.
    for event in results.try_iter() {
        match event {
            ResultEvent::Commit(c) => {
                transcriptions.advance(&c);
                if let Err(e) = commit(c, &mut session, &processor, &editor_tx) {
                    log::error!("could not queue a commit made during shutdown: {e:#}");
                }
            }
            // Same ordering guarantee as in the main loop: this utterance's
            // commits were drained from the same channel, in order, by the
            // arm above, on an earlier pass of this same `for`.
            ResultEvent::Finished { id, result, .. } => {
                finished(
                    id,
                    result.as_ref().err(),
                    rate,
                    &capture,
                    &mut session,
                    &mut transcriptions,
                );
                if let Err(e) = send(&editor_tx, EditorWork::Copy) {
                    log::error!("could not queue a shutdown clipboard copy: {e:#}");
                }
            }
            ResultEvent::Tick { .. }
            | ResultEvent::Preparing(_)
            | ResultEvent::Ready { .. }
            | ResultEvent::Unavailable(_)
            | ResultEvent::ConfigInvalid(_)
            | ResultEvent::WindowClosed { .. } => {}
        }
    }
    // What is left is not all written: captures the stop cut short, and
    // recordings still waiting. The next start transcribes the rest; only if
    // it cannot know about them does the user have to.
    let saved = persist(&capture, transcriptions.0.iter());
    match saved {
        Ok(()) => {
            for Transcription { path, through, .. } in &transcriptions.0 {
                log::warn!(
                    "{} is not transcribed past {:.2}s; the next start transcribes the rest",
                    path.display(),
                    through.seconds(rate)
                );
            }
        }
        Err(()) => {
            for Transcription { path, through, .. } in &transcriptions.0 {
                if *through == Frames::ZERO {
                    log::warn!(
                        "{} is not transcribed; recover it with `spokenpad transcribe {}`",
                        path.display(),
                        path.display()
                    );
                } else {
                    // What was committed is in the dictation file already;
                    // the rest is recovered from where it stopped, or it is
                    // written twice.
                    let seconds = through.seconds(rate);
                    log::warn!(
                        "{} was transcribed through {seconds:.2}s only; recover the rest with `spokenpad transcribe --from {seconds:.2} {}`",
                        path.display(),
                        path.display()
                    );
                }
            }
        }
    }
    // Sent last: everything the editor still has to write is already queued
    // ahead of it, and the bounded join gives it time to land. The flag first,
    // so a call Neovim is holding behind a half-typed command is given up on
    // rather than waited out past the grace.
    editor_quitting.store(true, Ordering::Release);
    let _ = editor_tx.send(EditorWork::Quit);
    join_bounded(editor, "editor");
    result
}

/// Rewrites the list the next start reads (`waiting.tsv`): every recording
/// not transcribed yet, with how far its text reaches. Called when one is
/// added or finished and at the stop, never per commit, so after a crash the
/// next start may write again the text committed since the last call.
fn persist<'a, B: InputBackend>(
    capture: &AudioCapture<B>,
    recordings: impl Iterator<Item = &'a Transcription>,
) -> Result<(), ()> {
    let unfinished: Vec<Unfinished> = recordings
        .map(|recording| Unfinished {
            path: recording.path.clone(),
            through: recording.through,
        })
        .collect();
    capture.save_unfinished(&unfinished).map_err(|e| {
        log::error!("the next start cannot know what is left to transcribe: {e:#}");
    })
}

/// A decode of utterance `id` ended, with `error` if it failed: a live
/// capture's tail, whose text is all written now, or a recording's
/// transcription, which [`settle`] judges. True when the list the next start
/// reads changed.
fn finished<B: InputBackend>(
    id: UtteranceId,
    error: Option<&anyhow::Error>,
    rate: u32,
    capture: &AudioCapture<B>,
    session: &mut Session,
    transcriptions: &mut Transcriptions,
) -> bool {
    let Some(recording) = transcriptions.get_mut(id) else {
        return false;
    };
    match recording.stage {
        Stage::Sent => {
            if settle(recording, error, rate, capture, session) {
                recording.stage = Stage::Retry;
            } else {
                transcriptions.remove(id);
            }
            true
        }
        Stage::Finishing => {
            transcriptions.remove(id);
            false
        }
        Stage::Capturing | Stage::Kept | Stage::Retry => false,
    }
}

/// Says what became of a recording's transcription, and whether the next
/// start should try the rest again: true for one that failed after getting
/// further than its last attempt. Every other one is released from the
/// pruning's keeping, except one the user is told to recover by hand.
fn settle<B: InputBackend>(
    recording: &Transcription,
    error: Option<&anyhow::Error>,
    rate: u32,
    capture: &AudioCapture<B>,
    session: &mut Session,
) -> bool {
    let path = &recording.path;
    let seconds = recording.through.seconds(rate);
    match error {
        None => {
            capture.release_recording(path);
            log::info!("transcribed {}", path.display());
            false
        }
        Some(e) if e.is::<ShorterThanItsText>() => {
            capture.release_recording(path);
            log::error!(
                "{} is shorter than the text already written from it; the rest is gone",
                path.display()
            );
            session.notify(Notice::RecordingShortened(path.clone()));
            false
        }
        Some(_) if recording.through == Frames::ZERO => {
            capture.release_recording(path);
            log::error!("{} was not transcribed", path.display());
            session.notify(Notice::RecordingLost(path.clone()));
            false
        }
        Some(_) if recording.through > recording.from => {
            log::error!(
                "{} was transcribed through {seconds:.2}s only; the next start tries the rest again",
                path.display()
            );
            session.notify(Notice::RecordingPartlyTranscribed {
                path: path.clone(),
                through: Duration::from_secs_f64(seconds),
                rest: Rest::NextStart,
            });
            true
        }
        // It failed where it failed before: trying again would too. Kept
        // from pruning, since the notice sends the user to it.
        Some(_) => {
            log::error!(
                "{} failed at {seconds:.2}s again; recover the rest with `spokenpad transcribe --from {seconds:.2} {}`",
                path.display(),
                path.display()
            );
            session.notify(Notice::RecordingPartlyTranscribed {
                path: path.clone(),
                through: Duration::from_secs_f64(seconds),
                rest: Rest::ByHand,
            });
            false
        }
    }
}

/// Hands one decoded commit to the editor thread. The event loop and the
/// shutdown drain share it, so text cannot take a different path depending on
/// when the recognizer produced it.
fn commit(
    c: Commit,
    session: &mut Session,
    processor: &Processor,
    editor_tx: &Sender<EditorWork>,
) -> Result<()> {
    session.note_commit(&c, Instant::now());
    let text = processor.process(&c.text);
    log::debug!(
        "utterance {} committed through {}: {} characters",
        c.utterance.id.0,
        c.through,
        text.chars().count()
    );
    if text.trim().is_empty() {
        return Ok(());
    }
    send(
        editor_tx,
        EditorWork::Append {
            utterance: c.utterance.id,
            text,
        },
    )
}

/// The inference thread: builds the pipeline if it was not handed one ready,
/// then decodes what the event loop sends it. `window` is the most audio one
/// step of a recording's transcription reads, as a preview tick does.
fn engine_thread<R: Recognizer, S: Segmenter>(
    pipeline: PipelineSource<R, S>,
    window: usize,
    rate: u32,
    work_rx: &Receiver<Work>,
    result_tx: &Sender<ResultEvent>,
) {
    let mut worker = match pipeline {
        PipelineSource::Ready(pipeline) => Worker::new(pipeline),
        PipelineSource::Load(loader) => match load(loader, work_rx, result_tx) {
            Some(worker) => worker,
            None => return,
        },
    };
    while let Ok(work) = work_rx.recv() {
        let sent = match work {
            Work::Tick {
                audio,
                start,
                utterance,
                kind,
            } => {
                let started = Instant::now();
                let result = worker.tick(&audio, start, &utterance, kind, |c| {
                    let _ = result_tx.send(ResultEvent::Commit(c));
                });
                result_tx.send(ResultEvent::Tick {
                    id: utterance.id,
                    result,
                    elapsed: started.elapsed(),
                })
            }
            Work::Finish(Tail {
                audio,
                start,
                utterance,
            }) => {
                let started = Instant::now();
                let result = worker.finish(&audio, start, &utterance, |c| {
                    let _ = result_tx.send(ResultEvent::Commit(c));
                });
                result_tx.send(ResultEvent::Finished {
                    id: utterance.id,
                    result,
                    elapsed: started.elapsed(),
                })
            }
            Work::Recording {
                path,
                utterance,
                from,
            } => {
                let started = Instant::now();
                let result =
                    decode_recording(&mut worker, &path, &utterance, from, window, rate, |c| {
                        let _ = result_tx.send(ResultEvent::Commit(c));
                    })
                    .with_context(|| format!("transcribe {}", path.display()));
                result_tx.send(ResultEvent::Finished {
                    id: utterance.id,
                    result,
                    elapsed: started.elapsed(),
                })
            }
            // Loaded already.
            Work::Load => Ok(()),
            Work::Quit => break,
        };
        if sent.is_err() {
            break;
        }
    }
}

/// Runs `loader` until it builds the pipeline: once now, and again at every
/// [`Work::Load`] after a failure. `None` when told to stop first.
fn load<R: Recognizer, S: Segmenter>(
    mut loader: Loader<R, S>,
    work_rx: &Receiver<Work>,
    result_tx: &Sender<ResultEvent>,
) -> Option<Worker<R, S>> {
    loop {
        let started = Instant::now();
        let loaded = loader(&mut |step| {
            let _ = result_tx.send(ResultEvent::Preparing(step));
        });
        match loaded {
            Ok(pipeline) => {
                let ready = ResultEvent::Ready {
                    segmenter: pipeline.segmenter.is_some(),
                    elapsed: started.elapsed(),
                };
                return result_tx.send(ready).is_ok().then(|| Worker::new(pipeline));
            }
            Err(e) => result_tx
                .send(ResultEvent::Unavailable(format!("{e:#}")))
                .ok()?,
        }
        // Nothing to decode can come before the pipeline is built: the event
        // loop keeps every capture on disk until it hears `Ready`.
        loop {
            match work_rx.recv().ok()? {
                Work::Load => break,
                Work::Quit => return None,
                Work::Tick { .. } | Work::Finish(_) | Work::Recording { .. } => {
                    log::error!("inference work arrived before the speech model loaded; dropped");
                }
            }
        }
    }
}

/// Transcribes a recording made before the model was ready, the way a live
/// capture of it would have been: settled chunks committed a window at a
/// time, then the tail decoded as the release decodes it. Only a window and
/// the open tail are ever in memory. Everything before `from` is committed
/// already, and not read.
fn decode_recording<R: Recognizer, S: Segmenter>(
    worker: &mut Worker<R, S>,
    path: &Path,
    utterance: &Arc<Utterance>,
    from: Frames,
    window: usize,
    rate: u32,
    mut commit: impl FnMut(Commit),
) -> Result<(String, usize)> {
    let mut reader = CaptureReader::open(path, rate)?;
    let skipped = reader.skip(from.get());
    if skipped < from.get() {
        return Err(ShorterThanItsText {
            recorded: Frames(skipped).seconds(rate),
            through: from.seconds(rate),
        }
        .into());
    }
    worker.resume(utterance, from);
    let mut held = Vec::with_capacity(window);
    let mut start = from;
    loop {
        let wanted = window.saturating_sub(held.len());
        if reader.read(&mut held, wanted)? < wanted {
            break;
        }
        // A full window, as a live tick past `preview.max_seconds` has: one
        // with nothing settled in it -- no VAD model, or speech the detector
        // never breaks -- is committed whole, which keeps this to a window.
        let Some(tail) = worker.tick(&held, start, utterance, TickKind::Commits, &mut commit)?
        else {
            break;
        };
        let settled = tail.through.since(start).min(held.len());
        ensure!(settled > 0, "a window of the recording settled nothing");
        held.drain(..settled);
        start += settled;
    }
    utterance.release();
    worker.finish(&held, start, utterance, commit)
}

/// Everything that happens between the key release and the decode request:
/// the measurements, the notices they call for, and the dump.
fn release<B: InputBackend>(
    session: &mut Session,
    capture: &AudioCapture<B>,
    taken: Captured,
    config: &Config,
    dump_dir: Option<&Path>,
) -> Result<Tail> {
    let rate = config.audio.sample_rate;
    let held = match session.state {
        State::Transcribing { started, released } => released.saturating_duration_since(started),
        _ => Duration::ZERO,
    };
    let Captured {
        samples,
        start,
        frames,
        peak,
    } = taken;
    let captured = frames.seconds(rate);
    log::info!(
        "captured {:.1}s held -> {captured:.1}s audio ({frames} samples, {} still held), peak={peak:.4}",
        held.as_secs_f64(),
        samples.len()
    );
    log_recording(capture.recording_status());
    // Neither of these can displace the memory-cap notice, which outranks them
    // (`Notice::priority`): past the in-memory ceiling a capture is *expected*
    // to be far shorter than the hold, and the cap notice already says what to
    // do about it. The measurements are still logged, because they are true.
    if held > INCOMPLETE_HOLD && captured < held.as_secs_f64() * 0.5 {
        log::error!(
            "microphone delivered less than half the expected audio; capture is incomplete"
        );
        session.notify(Notice::CaptureIncomplete);
    } else if peak < NEARLY_SILENT_PEAK {
        log::warn!("capture is nearly silent; check microphone gain/device");
        session.notify(Notice::NearlySilent);
    }
    if let Some(dir) = dump_dir {
        let path = dir.join(format!(
            "capture-{}.wav",
            chrono::Local::now().format("%Y-%m-%d-%H%M%S-%f")
        ));
        if start > Frames::ZERO {
            log::info!(
                "dump holds the last {:.1}s only; the whole capture is in the recovery WAV",
                Frames(samples.len()).seconds(rate)
            );
        }
        if let Err(e) = crate::shell::recorder::dump_capture(&path, &samples, rate) {
            log::error!("could not dump capture: {e:#}");
        }
    }
    let utterance = session
        .current
        .as_ref()
        .context("capture has no utterance")?
        .clone();
    Ok(Tail {
        audio: samples,
        start,
        utterance,
    })
}

/// Releases a capture made before the speech model was ready. There is
/// nothing to decode now, so the session goes back to idle; the recording is
/// the capture from here on, transcribed once the model is ready.
fn keep<B: InputBackend>(
    session: &mut Session,
    id: UtteranceId,
    capture: &AudioCapture<B>,
    transcriptions: &mut Transcriptions,
) {
    session.finish(id);
    match transcriptions.get_mut(id) {
        Some(recording) => {
            log::info!(
                "the speech model is not ready; {} is transcribed once it is",
                recording.path.display()
            );
            // The only copy of this capture: pruning must pass it by until
            // it is transcribed.
            capture.keep_recording(&recording.path);
            recording.stage = Stage::Kept;
        }
        None => {
            log::error!(
                "the speech model is not ready and nothing was recorded: this capture is lost"
            );
            session.notify(Notice::NotKept);
        }
    }
}

/// Lists the capture that just started, by its recovery WAV: from here on
/// its text is owed to the user, and a stop before all of it is written
/// leaves it to the next start. A capture with no recording cannot be
/// listed.
fn begin<B: InputBackend>(
    session: &Session,
    capture: &AudioCapture<B>,
    transcriptions: &mut Transcriptions,
) {
    let (Some(utterance), RecordingStatus::Recorded(path) | RecordingStatus::Truncated(path)) =
        (&session.current, capture.recording_status())
    else {
        return;
    };
    transcriptions.0.push(Transcription {
        path,
        id: utterance.id,
        through: Frames::ZERO,
        from: Frames::ZERO,
        stage: Stage::Capturing,
    });
}

fn note_postroll(postroll: PostRoll, session: &mut Session) {
    match postroll {
        PostRoll::Complete => {}
        PostRoll::Interrupted => {
            log::debug!("post-roll ended early by a new capture or shutdown")
        }
        PostRoll::TimedOut => {
            log::warn!(
                "the device delivered less than the post-roll in time; decoding what arrived"
            )
        }
        // The watchdog's own report comes after the capture has ended, when
        // it can no longer tell that this capture lost audio; this can.
        PostRoll::StreamStale => {
            log::warn!(
                "input stream stopped delivering before the post-roll arrived; the end of this capture may be missing"
            );
            session.notify(Notice::MicrophoneGap);
        }
    }
}

/// Hands the device's news to the session. Only the in-memory ceiling ends a
/// capture, so only it can return anything but [`Command::Nothing`], and the
/// caller has to carry that command out.
#[must_use]
fn apply_capture_event(event: CaptureEvent, session: &mut Session, now: Instant) -> Command {
    match event {
        CaptureEvent::StreamRestarted {
            gap,
            during_capture,
        } => {
            log::warn!(
                "microphone restarted after {:.1}s without audio",
                gap.as_secs_f64()
            );
            if during_capture {
                session.notify(Notice::MicrophoneGap);
            }
            Command::Nothing
        }
        // The capture logged why, once for the whole streak of failures.
        CaptureEvent::StreamUnavailable { during_capture } => {
            if during_capture {
                session.notify(Notice::MicrophoneUnavailable);
            }
            Command::Nothing
        }
        CaptureEvent::MemoryCapReached { recovery } => {
            // The notice names the recovery WAV by file name, so that a narrow
            // window still has room for it; the whole path goes here instead.
            let notice = Notice::MemoryCap(recovery.clone());
            match &recovery {
                RecordingStatus::Recorded(path) | RecordingStatus::Truncated(path) => {
                    log::warn!("{notice} ({})", path.display())
                }
                RecordingStatus::NotRecorded => log::warn!("{notice}"),
            }
            session.cap(recovery, now)
        }
        CaptureEvent::Flags(flags) => {
            log::warn!("PortAudio: {flags}");
            Command::Nothing
        }
    }
}

fn log_recording(status: RecordingStatus) {
    match status {
        RecordingStatus::Recorded(p) => log::info!("recorded to {}", p.display()),
        RecordingStatus::Truncated(p) => log::error!("recording {} is INCOMPLETE", p.display()),
        RecordingStatus::NotRecorded => log::warn!("no recovery WAV for this capture"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::decode::{Segment, Split, TrailingSilence};

    /// Counts the samples it is given.
    struct Count;
    impl Recognizer for Count {
        fn transcribe(&mut self, samples: &[f32], _: TrailingSilence) -> Result<String> {
            Ok(samples.len().to_string())
        }
    }
    /// Blocks of a fixed size, each settled once audio follows it.
    struct Blocks(usize);
    impl Segmenter for Blocks {
        fn split(&mut self, samples: &[f32]) -> Result<Split> {
            let segments = (0..samples.len())
                .step_by(self.0)
                .map(|start| {
                    let end = (start + self.0).min(samples.len());
                    Segment {
                        window: start..end,
                        speech_end: end,
                        settled: end < samples.len(),
                    }
                })
                .collect();
            Ok(Split {
                segments,
                silent_through: 0,
            })
        }
    }

    /// A recording made before the model was ready is committed a window at
    /// a time, never read whole, and every sample is decoded exactly once.
    #[test]
    fn a_recording_is_transcribed_a_window_at_a_time_every_sample_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.wav");
        crate::shell::recorder::dump_capture(&path, &[0.5; 10_000], 16_000).unwrap();
        let mut worker = Worker::new(Pipeline {
            recognizer: Count,
            segmenter: Some(Blocks(1_000)),
        });
        let mut commits = Vec::new();
        decode_recording(
            &mut worker,
            &path,
            &Utterance::new(1),
            Frames::ZERO,
            3_000,
            16_000,
            |c| commits.push((c.text.parse::<usize>().unwrap(), c.through)),
        )
        .unwrap();
        assert_eq!(
            commits,
            (1..=10)
                .map(|block| (1_000, Frames(block * 1_000)))
                .collect::<Vec<_>>(),
            "one commit per block, each once, each reaching as far as its block"
        );
    }

    /// A recording whose text reaches partway already is transcribed from
    /// there: the rest once, nothing before it again.
    #[test]
    fn a_recording_is_transcribed_from_where_its_text_reaches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.wav");
        crate::shell::recorder::dump_capture(&path, &[0.5; 10_000], 16_000).unwrap();
        let mut worker = Worker::new(Pipeline {
            recognizer: Count,
            segmenter: Some(Blocks(1_000)),
        });
        let mut commits = Vec::new();
        decode_recording(
            &mut worker,
            &path,
            &Utterance::new(1),
            Frames(4_000),
            3_000,
            16_000,
            |c| commits.push((c.text.parse::<usize>().unwrap(), c.through)),
        )
        .unwrap();
        assert_eq!(commits.iter().map(|c| c.0).sum::<usize>(), 6_000);
        assert_eq!(commits.first().map(|c| c.1), Some(Frames(5_000)));
        assert_eq!(commits.last().map(|c| c.1), Some(Frames(10_000)));

        let beyond = decode_recording(
            &mut worker,
            &path,
            &Utterance::new(2),
            Frames(20_000),
            3_000,
            16_000,
            |_| panic!("nothing to commit"),
        );
        assert!(beyond.is_err(), "a recording shorter than its offset");
    }

    /// Every request is preceded by the clock at its own stamp, and a drain
    /// ends with the clock at the current time, once.
    /// With nothing ever settling -- no VAD model, or speech the detector
    /// never breaks -- a recording is still read and decoded a window at a
    /// time, never whole, and every sample is still decoded once.
    #[test]
    fn a_recording_with_nothing_settled_is_held_a_window_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.wav");
        crate::shell::recorder::dump_capture(&path, &[0.5; 10_000], 16_000).unwrap();
        let mut worker = Worker::new(Pipeline {
            recognizer: Count,
            segmenter: None::<Blocks>,
        });
        let mut commits = Vec::new();
        decode_recording(
            &mut worker,
            &path,
            &Utterance::new(1),
            Frames::ZERO,
            3_000,
            16_000,
            |c| commits.push(c.text.parse::<usize>().unwrap()),
        )
        .unwrap();
        assert!(
            commits.iter().all(|&decoded| decoded <= 3_000),
            "a decode larger than the window: {commits:?}"
        );
        assert_eq!(commits.iter().sum::<usize>(), 10_000, "{commits:?}");
    }

    /// A model in place that does not load is checked and downloaded again
    /// once; one whose files verify is reported as such, and a configured
    /// one is never touched.
    #[test]
    fn a_model_that_does_not_load_is_repaired_once() {
        let mut steps = Vec::new();
        let mut builds = 0;
        let loaded = load_with_repair(
            || {
                builds += 1;
                if builds == 1 {
                    anyhow::bail!("bad weights")
                } else {
                    Ok("pipeline")
                }
            },
            |step| {
                step(Preparing::Downloading { done: 1, total: 2 });
                Ok(Repair::Replaced)
            },
            &mut |step| steps.push(step),
        );
        assert_eq!(loaded.unwrap(), "pipeline");
        assert_eq!(builds, 2);
        assert_eq!(
            steps,
            [
                Preparing::Loading,
                Preparing::Downloading { done: 1, total: 2 },
                Preparing::Loading
            ]
        );

        for (repair, says) in [
            (Repair::Verified, "match their pinned sha256"),
            (Repair::NotOurs, "check asr.model_dir"),
        ] {
            let error = load_with_repair(
                || -> Result<()> { anyhow::bail!("bad weights") },
                |_| Ok(repair),
                &mut |_| {},
            )
            .unwrap_err();
            let error = format!("{error:#}");
            assert!(
                error.contains(says) && error.contains("bad weights"),
                "{error}"
            );
        }
        let error = load_with_repair(
            || -> Result<()> { anyhow::bail!("bad weights") },
            |_| anyhow::bail!("offline"),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("spokenpad fetch-models"),
            "{error:#}"
        );
    }

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
