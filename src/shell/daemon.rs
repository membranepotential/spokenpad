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
        session::{Notice, Preparing, Recognition, RecordingStatus, Session},
        state::{Cause, Command, DiscardReason, Event, State},
        text::Processor,
    },
    shell::{
        audio::{AudioCapture, CaptureEvent, Captured, InputBackend, PostRoll},
        control::{ControlServer, Socket, refuse_pending},
        inference::{SpeechSegmenter, Transcriber, load_segmenter},
        models::ensure_defaults,
        nvim::{AppendFailure, CopyOutcome, IndicatorState, NvimSession},
        recorder::CaptureReader,
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

/// A capture made before the speech model was ready: its recovery WAV, and
/// the utterance it was.
struct Waiting {
    path: PathBuf,
    id: UtteranceId,
}

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
    /// Transcribe a recording made before the model was ready.
    Recording {
        path: PathBuf,
        utterance: Arc<Utterance>,
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
/// on a shared flag: text already queued must reach the file even while the
/// daemon is shutting down or the capture it came from was cancelled.
fn editor_thread(
    running: Config,
    mut reload: Reload,
    rx: Receiver<EditorWork>,
    events: Sender<ResultEvent>,
) {
    let mut nvim = NvimSession::new(running.nvim.clone());
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
        // Drain what is already queued, keeping only the newest indicator:
        // the loop offers one 50 times a second and only the last is true.
        let mut indicator = None;
        for work in first.into_iter().chain(rx.try_iter()) {
            match work {
                EditorWork::Indicator(next) => indicator = Some(next),
                EditorWork::Ensure => {
                    // A window about to open, or an editor about to be
                    // attached, takes the settings the file has now.
                    if !nvim.attached() {
                        reconfigure(&mut nvim, &running, &mut reload, &mut reported, &events);
                    }
                    match nvim.ensure() {
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
                        && let Err(e) = nvim.ensure()
                    {
                        log::warn!("could not reattach to the dictation window: {e:#}");
                    }
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
                            // The editor may hold the text already; writing
                            // it anywhere else could write it twice.
                            Err(AppendFailure::Unconfirmed(e)) => {
                                paragraph = None;
                                log::error!(
                                    "append failed: {e:#}; transcript retained in diagnostic log; recover from the capture WAV if available"
                                );
                                log::debug!("undelivered text: {text:?}");
                                continue;
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
                            if write.started {
                                nvim.notify_detached(&write.path);
                            }
                        }
                        Err(e) => {
                            paragraph = None;
                            log::error!(
                                "could not write the transcript to a dictation file: {e:#}; recover from the capture WAV if available"
                            );
                            log::debug!("undelivered text: {text:?}");
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
            log::debug!("undelivered text: {text:?}");
        }
    }
    nvim.close();
}

/// Another daemon of this user holds the lock: the one exit of a daemon that
/// has taken over its socket. `main` gives it exit code 3, which the unit
/// does not restart on.
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
        .create(&dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join("daemon.lock"))?;
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
    let _lock = match daemon_lock() {
        Ok(lock) => lock,
        Err(e) => {
            // A press queued on systemd's socket would start this daemon
            // again the moment it exits, and again: each is answered first,
            // so that a press starts it at most once.
            if let Some(socket) = inherited
                && e.is::<AnotherDaemon>()
            {
                refuse_pending(socket, Reply::AnotherDaemon);
            }
            return Err(e);
        }
    };
    // Served first, before anything slow: under socket activation the press
    // that started this daemon is waiting on the socket, and a request is
    // stamped when it is read. The model loads later, on the inference
    // thread, and what is recorded meanwhile is kept until it has.
    let (requests_tx, requests) = mpsc::channel();
    let path = crate::config::control_socket();
    let socket = match inherited {
        Some(socket) => socket,
        None => Socket::bind(&path)?,
    };
    let _control = ControlServer::start(socket, requests_tx)?;
    log::info!("listening on {}", path.display());
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stopping))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stopping))?;
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
        step(Preparing::Loading);
        log::info!("loading CPU recognizer");
        let mut recognizer = Transcriber::new(&asr, rate)?;
        recognizer.warm_up()?;
        Ok(Pipeline {
            recognizer,
            segmenter: load_segmenter(&vad, rate),
        })
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
    // Captures made before the model was ready, not yet sent to it; and those
    // sent to it and not transcribed yet.
    let mut waiting: VecDeque<Waiting> = VecDeque::new();
    let mut transcribing: Vec<Waiting> = Vec::new();
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
    let editor = thread::Builder::new()
        .name("spokenpad-nvim".into())
        .spawn(move || editor_thread(running, reload, editor_rx, editor_events))?;

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
                        if let Err(e) = capture.start_capture() {
                            // The press cleared the previous notice; cancel
                            // first, then say why nothing is being recorded.
                            session.event(Event::Request(Received {
                                request: Request::Cancel,
                                at: Instant::now(),
                            }));
                            session.notify(Notice::MicrophoneUnavailable);
                            log::error!("capture could not start: {e:#}");
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
                                recognition.notice(waiting.len() + transcribing.len()),
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
                            send(&work_tx, Work::Finish(tail))?;
                        } else {
                            keep(
                                &mut session,
                                tail.utterance.id,
                                capture.recording_status(),
                                &mut waiting,
                            );
                        }
                    }
                    Command::Discard(reason) => {
                        capture.stop_capture();
                        match reason {
                            DiscardReason::TooShort => log::info!(
                                "capture held under {}ms; discarded, WAV retained",
                                crate::core::state::MINIMUM_HOLD.as_millis()
                            ),
                            DiscardReason::Cancelled => {
                                log::info!("capture cancelled; settled text and WAV retained")
                            }
                        }
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
                    ResultEvent::Unavailable(reason) => {
                        log::error!(
                            "no speech model: {reason}; captures are kept, and the next press tries again"
                        );
                        recognition = Recognition::Unavailable(reason);
                    }
                    ResultEvent::Commit(c) => commit(c, &mut session, &processor, &editor_tx)?,
                    ResultEvent::Tick {
                        id,
                        result,
                        elapsed,
                    } => {
                        let preview = match result {
                            Ok(Some(Preview { text, through })) => {
                                log::debug!(
                                    "preview {}: {} characters, committed through {through}, decoded in {:.2}s",
                                    id.0,
                                    text.chars().count(),
                                    elapsed.as_secs_f64()
                                );
                                Some(Preview {
                                    text: processor.process(&text),
                                    through,
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
                        match result {
                            Ok((text, frames)) => {
                                log::info!(
                                    "decoded the last {:.1}s in {:.2}s (utterance {})",
                                    Frames(frames).seconds(rate),
                                    elapsed.as_secs_f64(),
                                    id.0
                                );
                                log::debug!("release transcript: {text:?}");
                            }
                            Err(e) => log::error!("decode failed for utterance {}: {e:#}", id.0),
                        }
                        if let Some(at) = transcribing.iter().position(|t| t.id == id) {
                            transcribing.remove(at);
                            log::info!(
                                "transcribed the recording of utterance {}; {} to go",
                                id.0,
                                waiting.len() + transcribing.len()
                            );
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
                for recording in waiting.drain(..) {
                    send(
                        &work_tx,
                        Work::Recording {
                            path: recording.path.clone(),
                            utterance: Utterance::new(recording.id.0),
                        },
                    )?;
                    transcribing.push(recording);
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
                    recognition.notice(waiting.len() + transcribing.len()),
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
    // A capture still being held has nothing the user is waiting for. One
    // already released is inside its final decode, and that text is owed to
    // the user: the bounded join below, not a cancel, is what limits the wait.
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
                if let Err(e) = commit(c, &mut session, &processor, &editor_tx) {
                    log::error!("could not queue a commit made during shutdown: {e:#}");
                }
            }
            // Same ordering guarantee as in the main loop: this utterance's
            // commits were drained from the same channel, in order, by the
            // arm above, on an earlier pass of this same `for`.
            ResultEvent::Finished { id, .. } => {
                transcribing.retain(|t| t.id != id);
                if let Err(e) = send(&editor_tx, EditorWork::Copy) {
                    log::error!("could not queue a shutdown clipboard copy: {e:#}");
                }
            }
            ResultEvent::Tick { .. }
            | ResultEvent::Preparing(_)
            | ResultEvent::Ready { .. }
            | ResultEvent::Unavailable(_)
            | ResultEvent::ConfigInvalid(_) => {}
        }
    }
    for Waiting { path, .. } in waiting.into_iter().chain(transcribing) {
        log::warn!(
            "{} was recorded before the speech model was ready and is not transcribed; recover it with `spokenpad transcribe`",
            path.display()
        );
    }
    // Sent last: everything the editor still has to write is already queued
    // ahead of it, and the bounded join gives it time to land.
    let _ = editor_tx.send(EditorWork::Quit);
    join_bounded(editor, "editor");
    result
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
        "utterance {} committed through {}: {text:?}",
        c.utterance.id.0,
        c.through
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
            Work::Recording { path, utterance } => {
                let started = Instant::now();
                let result = decode_recording(&mut worker, &path, &utterance, window, rate, |c| {
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
/// the open tail are ever in memory.
fn decode_recording<R: Recognizer, S: Segmenter>(
    worker: &mut Worker<R, S>,
    path: &Path,
    utterance: &Arc<Utterance>,
    window: usize,
    rate: u32,
    mut commit: impl FnMut(Commit),
) -> Result<(String, usize)> {
    let mut reader = CaptureReader::open(path, rate)?;
    let mut held = Vec::with_capacity(window);
    let mut start = Frames::ZERO;
    loop {
        let wanted = window.saturating_sub(held.len());
        if reader.read(&mut held, wanted)? < wanted {
            break;
        }
        let through = worker
            .tick(&held, start, utterance, TickKind::Commits, &mut commit)?
            .map_or(start, |tail| tail.through);
        let settled = through.since(start).min(held.len());
        if settled == 0 {
            // A window of unbroken speech: like a live capture's open tail,
            // the rest is decoded at the end, as a whole.
            reader.read(&mut held, usize::MAX)?;
            break;
        }
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
fn keep(
    session: &mut Session,
    id: UtteranceId,
    recording: RecordingStatus,
    waiting: &mut VecDeque<Waiting>,
) {
    session.finish(id);
    match recording {
        RecordingStatus::Recorded(path) | RecordingStatus::Truncated(path) => {
            log::info!(
                "the speech model is not ready; {} is transcribed once it is",
                path.display()
            );
            waiting.push_back(Waiting { path, id });
        }
        RecordingStatus::NotRecorded => {
            log::error!(
                "the speech model is not ready and nothing was recorded: this capture is lost"
            );
            session.notify(Notice::NotKept);
        }
    }
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
        CaptureEvent::StreamUnavailable {
            reason,
            during_capture,
        } => {
            log::error!("microphone recovery failed: {reason}");
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
        decode_recording(&mut worker, &path, &Utterance::new(1), 3_000, 16_000, |c| {
            commits.push(c.text.parse::<usize>().unwrap())
        })
        .unwrap();
        assert_eq!(commits, vec![1_000; 10], "one commit per block, each once");
    }

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
