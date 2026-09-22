//! Mono float32 microphone capture with pre-roll and stream recovery.
//!
//! [`AudioCapture`] is generic over the device backend so the capture
//! arithmetic — pre-roll, immutable chunks, the memory ceiling, the watchdog
//! and snapshot slicing — can be driven deterministically in tests. The
//! production backend is [`PortAudioBackend`].

use crate::{
    config::{Audio, Recording},
    core::{frames::Frames, session::RecordingStatus},
    shell::recorder::{CaptureRecorder, Unfinished},
};
use anyhow::{Context, Result, anyhow, bail};
use portaudio as pa;
use std::{
    collections::VecDeque,
    fmt,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// The most audio one capture may hold in memory at once. Audio the decoder
/// has finished with is dropped as the capture runs, by
/// [`AudioCapture::discard_before`], so this is a ceiling on the *retained*
/// window rather than on how long a capture may be; with a VAD model loaded
/// nothing comes near it. Without one nothing ever settles, and this is what
/// stops a forgotten capture from taking the machine's memory.
pub const MAX_UTTERANCE_SECONDS: usize = 3_600;
const FIRST_CALLBACK_TIMEOUT: Duration = Duration::from_millis(500);
const STALE_STREAM: Duration = Duration::from_secs(1);
/// How long a stop waits, beyond its post-roll, for the device to deliver: at
/// least the callback already in flight, so the partial device buffer at the
/// moment of the key event reaches capture and WAV.
const FINAL_CALLBACK_WAIT: Duration = Duration::from_millis(100);
/// How often a stop re-checks delivery, liveness and its interrupt.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(2);
/// How long the watchdog waits after the first failed open before it tries
/// again; each further failure in a row doubles the wait, up to
/// [`MAX_REOPEN_BACKOFF`]. An attempt costs a PortAudio initialisation and up
/// to [`FIRST_CALLBACK_TIMEOUT`] on the event loop's thread, so a microphone
/// that stays missing must not be looked for every two seconds for good.
const REOPEN_BACKOFF: Duration = Duration::from_secs(2);
/// The longest wait between two attempts of the watchdog. A press does not
/// wait for it: it tries the microphone at once.
const MAX_REOPEN_BACKOFF: Duration = Duration::from_secs(60);
/// Chunk vector capacity reserved up front, in seconds of audio.
const RESERVE_SECONDS: usize = 60;
/// Smallest device buffer the reserve above assumes. Before the first
/// callback nothing is known about the device, and reserving room for
/// one-frame buffers would allocate tens of megabytes of chunk headers.
const MINIMUM_CALLBACK_FRAMES: usize = 64;

/// How an input stream is torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Teardown {
    /// Orderly: let the device finish what it is doing. Used at shutdown.
    Drain,
    /// Immediate: a wedged device must never block the event loop.
    Abort,
}

/// What opening a stream achieved. A device that opens but never calls back
/// is as unusable as one that refuses to open, and is backed off the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opened {
    Delivering,
    Silent,
}

impl Opened {
    /// `Err` for an open but mute device, carrying the reason the event loop
    /// shows the user.
    fn delivering(self) -> Result<()> {
        match self {
            Self::Delivering => Ok(()),
            Self::Silent => bail!(
                "input stream opened but delivered no audio within {}ms; the device may be suspended or held by another client",
                FIRST_CALLBACK_TIMEOUT.as_millis()
            ),
        }
    }
}

/// A running input stream owned by [`AudioCapture`].
pub trait InputStream {
    /// Whether the device still considers the stream running.
    fn is_active(&self) -> bool;
    /// Stops delivering audio and releases the device.
    fn close(self, teardown: Teardown);
}

/// Opens input streams that feed a [`CallbackCore`].
pub trait InputBackend {
    type Stream: InputStream;
    fn open(&mut self, config: &Audio, core: Arc<CallbackCore>) -> Result<Self::Stream>;
}

/// Failed attempts to open the microphone in a row, since it last worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Failing {
    /// 1 at the first failure.
    attempts: u32,
    /// When the watchdog may try again.
    retry_at: Instant,
}

impl Failing {
    /// The streak after one more failure, at `now`.
    fn after(previous: Option<Self>, now: Instant) -> Self {
        let attempts = previous.map_or(1, |failing| failing.attempts.saturating_add(1));
        Self {
            attempts,
            retry_at: now + reopen_backoff(attempts),
        }
    }
}

/// The watchdog's wait after `attempts` failures in a row.
fn reopen_backoff(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(u32::BITS - 1);
    REOPEN_BACKOFF
        .saturating_mul(1 << doublings)
        .min(MAX_REOPEN_BACKOFF)
}

/// Non-fatal conditions the device reported to the capture callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFlags(u64);

impl StreamFlags {
    const NAMES: [(u64, &'static str); 5] = [
        (1, "input underflow"),
        (2, "input overflow"),
        (4, "output underflow"),
        (8, "output overflow"),
        (16, "priming output"),
    ];

    pub fn bits(self) -> u64 {
        self.0
    }
}

impl fmt::Display for StreamFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        let mut rest = self.0;
        for (bit, name) in Self::NAMES {
            if self.0 & bit != 0 {
                if !first {
                    f.write_str(", ")?;
                }
                f.write_str(name)?;
                first = false;
                rest &= !bit;
            }
        }
        if rest != 0 {
            if !first {
                f.write_str(", ")?;
            }
            write!(f, "unknown flags {rest:#x}")?;
        } else if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

/// Everything the event loop needs to learn about the device, delivered once
/// each by [`AudioCapture::poll`].
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureEvent {
    /// The stream stopped delivering audio and a replacement is running.
    StreamRestarted { gap: Duration, during_capture: bool },
    /// The stream stopped delivering audio and could not be reopened. Why is
    /// logged by the capture, once per streak of failures.
    StreamUnavailable { during_capture: bool },
    /// The in-memory ceiling was reached; later audio exists only on disk.
    MemoryCapReached { recovery: RecordingStatus },
    /// The device reported overflow/underflow to the callback.
    Flags(StreamFlags),
}

/// How the wait for audio after a release ended. The capture is taken
/// whatever the outcome; only [`PostRoll::StreamStale`] means audio is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostRoll {
    /// Every post-roll frame arrived, or no capture was running to owe any.
    Complete,
    /// The caller's interrupt ended the wait early: a key event is waiting,
    /// or the daemon is stopping.
    Interrupted,
    /// The device delivered too little before the deadline.
    TimedOut,
    /// The stream was dead, or died, before the post-roll arrived. The stream
    /// is not reopened here; the next [`AudioCapture::poll`] does that.
    StreamStale,
}

#[derive(Debug)]
struct Chunk {
    samples: Arc<[f32]>,
    kept: usize,
}

/// Part of a running capture, handed to the inference worker. `start` is
/// where `samples` begins in the capture.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub start: Frames,
    pub samples: Vec<f32>,
}

impl Snapshot {
    fn empty(start: Frames) -> Self {
        Self {
            start,
            samples: Vec::new(),
        }
    }
}

/// A capture that has ended: the audio still held, and the facts about the
/// rest of it that outlive the samples themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct Captured {
    /// The retained audio, beginning at `start`.
    pub samples: Vec<f32>,
    pub start: Frames,
    /// Where the capture ended, counted from its first frame.
    pub frames: Frames,
    /// The loudest sample of the whole capture, dropped audio included.
    pub peak: f32,
}

/// Either the pre-roll ring is filling, or a capture is accumulating chunks.
enum CaptureState {
    Idle {
        ring: VecDeque<f32>,
    },
    Capturing {
        /// The audio still held, covering `start..frames` of the capture.
        chunks: Vec<Chunk>,
        /// Where `chunks` begins in the capture: what
        /// [`AudioCapture::discard_before`] has dropped.
        start: Frames,
        /// End of the accepted audio, counted from the first frame of the
        /// capture and never moved back, because it is what a snapshot and
        /// the committed offset are measured against.
        frames: Frames,
        /// Device frames delivered since the capture started, counted after
        /// they were recorded and whether or not the memory ceiling let them
        /// into `chunks`. A stop measures its post-roll with this: `frames`
        /// stops growing at the ceiling.
        delivered: Frames,
        /// The loudest sample of the whole capture, which outlives the audio
        /// that was dropped.
        peak: f32,
        /// The retained window reached the memory ceiling and audio was
        /// dropped. Final for the rest of the capture: accepting again after
        /// a gap would splice audio that is not contiguous.
        capped: bool,
    },
}

impl CaptureState {
    fn capturing(&self) -> bool {
        matches!(self, Self::Capturing { .. })
    }
}

/// Cap notice for the current or most recent capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapNotice {
    None,
    Pending,
    Reported,
}

/// The state a capture callback touches. Backends own an `Arc` of this and
/// call [`CallbackCore::process`] for every device buffer.
pub struct CallbackCore {
    state: Mutex<CaptureState>,
    recorder: Arc<CaptureRecorder>,
    preroll_frames: usize,
    maximum_frames: usize,
    clock_origin: Instant,
    // Monotone, single-location counters: Relaxed is sufficient, no data is
    // published through them. `callback_count` only tells a fresh stream's
    // first callback apart; capture progress is `CaptureState`'s `delivered`.
    last_callback_nanos: AtomicU64,
    callback_count: AtomicU64,
    callback_frames: AtomicUsize,
    level_bits: AtomicU32,
    flag_bits: AtomicU64,
}

impl CallbackCore {
    /// Accepts one device buffer. Runs on the device's callback thread and
    /// does bounded work: one allocation, one lock, no I/O.
    pub fn process(&self, samples: &[f32], status_bits: u64) {
        let peak = samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        self.level_bits.store(peak.to_bits(), Ordering::Relaxed);
        self.last_callback_nanos
            .store(self.now_nanos(), Ordering::Relaxed);
        self.callback_count.fetch_add(1, Ordering::Relaxed);
        self.callback_frames.store(samples.len(), Ordering::Relaxed);
        if status_bits != 0 {
            self.flag_bits.fetch_or(status_bits, Ordering::Relaxed);
        }

        let mut state = lock(&self.state);
        match &mut *state {
            CaptureState::Capturing {
                chunks,
                start,
                frames,
                delivered,
                peak: highest,
                capped,
            } => {
                // One copy of the device's reused buffer, shared by the disk
                // and memory sinks. Disk receives it before the memory ceiling
                // is consulted, so the cap cannot truncate recovery.
                let captured: Arc<[f32]> = Arc::from(samples);
                self.recorder.write_shared(Arc::clone(&captured));
                *delivered += captured.len();
                *highest = highest.max(peak);
                if *capped {
                    return;
                }
                let remaining = self.maximum_frames.saturating_sub(frames.since(*start));
                let kept = captured.len().min(remaining);
                *capped = kept < captured.len();
                if kept == 0 {
                    return;
                }
                *frames += kept;
                chunks.push(Chunk {
                    samples: captured,
                    kept,
                });
            }
            CaptureState::Idle { ring } => write_ring(ring, samples, self.preroll_frames),
        }
    }

    fn now_nanos(&self) -> u64 {
        self.clock_origin
            .elapsed()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64
    }

    fn since_last_callback(&self) -> Duration {
        let now = self.now_nanos();
        let then = self.last_callback_nanos.load(Ordering::Relaxed);
        Duration::from_nanos(now.saturating_sub(then))
    }
}

/// Appends to a fixed-capacity ring, discarding the oldest samples.
fn write_ring(ring: &mut VecDeque<f32>, samples: &[f32], capacity: usize) {
    if capacity == 0 {
        return;
    }
    if samples.len() >= capacity {
        ring.clear();
        ring.extend(&samples[samples.len() - capacity..]);
        return;
    }
    let overflow = (ring.len() + samples.len()).saturating_sub(capacity);
    ring.drain(..overflow);
    ring.extend(samples);
}

pub struct AudioCapture<B: InputBackend = PortAudioBackend> {
    config: Audio,
    backend: B,
    stream: Option<B::Stream>,
    core: Arc<CallbackCore>,
    cap: CapNotice,
    /// `Some` while the microphone cannot be opened; the watchdog backs off.
    failing: Option<Failing>,
}

impl AudioCapture<PortAudioBackend> {
    /// The microphone through PortAudio, opened once something needs it.
    pub fn new(audio: Audio, recording: Recording) -> Self {
        Self::with_backend(PortAudioBackend::default(), audio, recording)
    }
}

impl<B: InputBackend> AudioCapture<B> {
    /// Never fails: a device that cannot be opened now (unplugged, or a
    /// sound server that is not up yet at login) is retried by
    /// [`poll`](Self::poll) and at the next [`start_capture`](Self::start_capture),
    /// and a press meanwhile gets the microphone-unavailable notice. The
    /// daemon has taken presses by then, and must not exit over it.
    pub fn with_backend(backend: B, audio: Audio, recording: Recording) -> Self {
        let maximum_frames = audio.sample_rate as usize * MAX_UTTERANCE_SECONDS;
        Self::build(backend, audio, recording, maximum_frames)
    }

    fn build(backend: B, audio: Audio, recording: Recording, maximum_frames: usize) -> Self {
        let recorder = Arc::new(CaptureRecorder::new(recording, audio.sample_rate));
        let preroll_frames = audio.preroll_frames();
        let core = Arc::new(CallbackCore {
            state: Mutex::new(CaptureState::Idle {
                ring: VecDeque::with_capacity(preroll_frames),
            }),
            recorder,
            preroll_frames,
            maximum_frames,
            clock_origin: Instant::now(),
            // Zero means "since this AudioCapture was constructed" until the
            // first callback. Opening a stream refreshes it before start().
            last_callback_nanos: AtomicU64::new(0),
            callback_count: AtomicU64::new(0),
            callback_frames: AtomicUsize::new(0),
            level_bits: AtomicU32::new(0.0_f32.to_bits()),
            flag_bits: AtomicU64::new(0),
        });
        let mut capture = Self {
            config: audio,
            backend,
            stream: None,
            core,
            cap: CapNotice::None,
            failing: None,
        };
        if preroll_frames > 0 {
            // A failure is logged, and retried, by `reopen` and the watchdog.
            let _ = capture.reopen("start-up");
        }
        capture
    }

    pub fn start_capture(&mut self) -> Result<()> {
        self.cap = CapNotice::None;
        // A press tries the microphone at once, whatever the watchdog's
        // back-off says: the user is waiting on this one.
        if self.config.preroll_frames() == 0 {
            self.reopen("key press")?;
        } else if !self.stream_is_live() {
            self.reopen("input stream looks dead at the key press")?;
        }

        self.core.recorder.start();
        let per_callback = self
            .core
            .callback_frames
            .load(Ordering::Relaxed)
            .max(MINIMUM_CALLBACK_FRAMES);
        let reserve = (self.config.sample_rate as usize * RESERVE_SECONDS).div_ceil(per_callback);
        let mut state = lock(&self.core.state);
        // Decision: the ring is consumed and dropped here, so audio recorded
        // before the *previous* capture can never be spliced into this one.
        let preroll = match &mut *state {
            CaptureState::Idle { ring } => Vec::from_iter(ring.drain(..)),
            CaptureState::Capturing { .. } => Vec::new(),
        };
        let mut chunks = Vec::with_capacity(reserve);
        let mut frames = Frames::ZERO;
        let mut peak = 0.0_f32;
        if !preroll.is_empty() {
            let preroll: Arc<[f32]> = Arc::from(preroll);
            self.core.recorder.write_shared(Arc::clone(&preroll));
            frames += preroll.len();
            peak = preroll.iter().fold(0.0_f32, |p, s| p.max(s.abs()));
            chunks.push(Chunk {
                kept: preroll.len(),
                samples: preroll,
            });
        }
        *state = CaptureState::Capturing {
            chunks,
            start: Frames::ZERO,
            frames,
            delivered: Frames::ZERO,
            peak,
            capped: false,
        };
        Ok(())
    }

    /// Ends a capture that is being thrown away: no post-roll, only the
    /// buffer in flight at the key event, which the recovery WAV still owes.
    pub fn stop_capture(&mut self) -> Captured {
        match self.await_delivery(1, FINAL_CALLBACK_WAIT, &mut || false) {
            PostRoll::TimedOut => {
                log::debug!("no further callback arrived within the release window")
            }
            PostRoll::Complete | PostRoll::Interrupted | PostRoll::StreamStale => {}
        }
        self.take_capture()
    }

    /// Ends a capture that is going to be decoded, after its post-roll: the
    /// speech still sounding when the key came up at `released`. Audio
    /// captured since then counts towards it, so this blocks for at most what
    /// is left of `postroll_ms` plus [`FINAL_CALLBACK_WAIT`], and returns as
    /// soon as `interrupted` is true, which it is asked between polls of the
    /// device. The state lock is never held while waiting.
    pub fn finish_capture(
        &mut self,
        released: Instant,
        mut interrupted: impl FnMut() -> bool,
    ) -> (Captured, PostRoll) {
        let remaining = self.config.postroll().saturating_sub(released.elapsed());
        let postroll = self.await_delivery(
            self.config.frames_in(remaining).max(1),
            remaining + FINAL_CALLBACK_WAIT,
            &mut interrupted,
        );
        (self.take_capture(), postroll)
    }

    fn take_capture(&mut self) -> Captured {
        let (chunks, start, frames, peak, capped) = {
            let mut state = lock(&self.core.state);
            match &mut *state {
                CaptureState::Idle { .. } => (Vec::new(), Frames::ZERO, Frames::ZERO, 0.0, false),
                CaptureState::Capturing {
                    chunks,
                    start,
                    frames,
                    peak,
                    capped,
                    ..
                } => {
                    let taken = (std::mem::take(chunks), *start, *frames, *peak, *capped);
                    *state = CaptureState::Idle {
                        ring: VecDeque::with_capacity(self.core.preroll_frames),
                    };
                    taken
                }
            }
        };
        // The ceiling may be reached between two polls and the release that
        // follows; the notice still belongs to this capture.
        if capped && self.cap == CapNotice::None {
            self.cap = CapNotice::Pending;
        }
        self.core.recorder.stop();
        if self.config.preroll_frames() == 0 {
            self.close_stream(Teardown::Drain);
        }

        let length = chunks.iter().map(|chunk| chunk.kept).sum();
        let mut samples = Vec::with_capacity(length);
        for chunk in chunks {
            samples.extend_from_slice(&chunk.samples[..chunk.kept]);
        }
        Captured {
            samples,
            start,
            frames,
            peak,
        }
    }

    /// Drops capture audio the decoder is finished with: everything before
    /// `through`, to whole chunks. No window of any later decode reaches back
    /// across the committed offset, and `through` never runs ahead of it, so
    /// what goes here is audio nothing will read again. The recovery WAV has
    /// it all either way.
    pub fn discard_before(&self, through: Frames) {
        let mut state = lock(&self.core.state);
        let CaptureState::Capturing { chunks, start, .. } = &mut *state else {
            return;
        };
        let mut dropped = 0;
        let mut whole = 0;
        for chunk in chunks.iter() {
            if *start + (dropped + chunk.kept) > through {
                break;
            }
            dropped += chunk.kept;
            whole += 1;
        }
        if whole == 0 {
            return;
        }
        chunks.drain(..whole);
        *start += dropped;
    }

    /// Samples of the current capture after `since`, empty when idle. The
    /// snapshot says where it begins: audio already dropped cannot be handed
    /// out, however far back `since` reaches.
    pub fn snapshot_capture(&self, since: Frames) -> Snapshot {
        let (chunks, needed, start) = {
            let state = lock(&self.core.state);
            let CaptureState::Capturing {
                chunks,
                start,
                frames,
                ..
            } = &*state
            else {
                return Snapshot::empty(since);
            };
            let start = since.max(*start);
            let needed = frames.since(start);
            if needed == 0 {
                return Snapshot::empty(start);
            }
            let mut selected = Vec::new();
            let mut selected_frames = 0;
            for chunk in chunks.iter().rev() {
                selected.push((Arc::clone(&chunk.samples), chunk.kept));
                selected_frames += chunk.kept;
                if selected_frames >= needed {
                    break;
                }
            }
            selected.reverse();
            (selected, needed, start)
        };

        let selected_frames = chunks.iter().map(|(_, kept)| kept).sum::<usize>();
        let skip = selected_frames - needed;
        let mut samples = Vec::with_capacity(needed);
        let mut skipped = 0;
        for (chunk, kept) in chunks {
            let chunk_skip = (skip - skipped).min(kept);
            samples.extend_from_slice(&chunk[chunk_skip..kept]);
            skipped += chunk_skip;
        }
        Snapshot { start, samples }
    }

    /// End of the current capture, counted from its first frame; zero while
    /// idle. The event loop uses this to size the open tail without copying
    /// it.
    pub fn captured_frames(&self) -> Frames {
        match &*lock(&self.core.state) {
            CaptureState::Capturing { frames, .. } => *frames,
            CaptureState::Idle { .. } => Frames::ZERO,
        }
    }

    /// Samples of the current capture held in memory; zero while idle. Only
    /// the tests ask: the loop drops what it drops without measuring it.
    #[cfg(test)]
    pub fn retained_frames(&self) -> usize {
        match &*lock(&self.core.state) {
            CaptureState::Capturing { start, frames, .. } => frames.since(*start),
            CaptureState::Idle { .. } => 0,
        }
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.core.level_bits.load(Ordering::Relaxed))
    }

    pub fn recording_status(&self) -> RecordingStatus {
        self.core.recorder.status()
    }

    /// Keeps a finished recording from being pruned while it waits to be
    /// transcribed; see [`CaptureRecorder::keep`](crate::shell::recorder::CaptureRecorder::keep).
    pub fn keep_recording(&self, path: &std::path::Path) {
        self.core.recorder.keep(path);
    }

    pub fn release_recording(&self, path: &std::path::Path) {
        self.core.recorder.release(path);
    }

    /// See [`CaptureRecorder::take_unfinished`].
    pub fn take_unfinished(&self) -> Vec<Unfinished> {
        self.core.recorder.take_unfinished()
    }

    /// See [`CaptureRecorder::save_unfinished`].
    pub fn save_unfinished(&self, unfinished: &[Unfinished]) -> Result<()> {
        self.core.recorder.save_unfinished(unfinished)
    }

    /// Repairs the stream if it died and reports every device condition
    /// observed since the previous call. Each event is delivered once.
    pub fn poll(&mut self) -> Vec<CaptureEvent> {
        let mut events = Vec::new();
        let capturing = lock(&self.core.state).capturing();

        // A stream that opened mute and delivers after all ends the streak,
        // so that losing it later is reported, and retried, afresh.
        if self.failing.is_some() && self.stream_is_live() {
            self.recovered();
        }
        if self.stream_expected() && !self.stream_is_live() {
            let now = Instant::now();
            if self.failing.is_none_or(|failing| now >= failing.retry_at) {
                let gap = self.core.since_last_callback();
                // A microphone that has not come back since it was lost was
                // reported by the first attempt of the streak.
                match (self.failing, capturing) {
                    (Some(_), _) => {}
                    (None, true) => log::error!(
                        "input stream stopped delivering audio {:.1}s ago mid-capture; reopening; audio spoken during the gap is lost",
                        gap.as_secs_f64()
                    ),
                    (None, false) => log::warn!(
                        "idle input stream stopped delivering audio {:.1}s ago; reopening so the pre-roll is ready",
                        gap.as_secs_f64()
                    ),
                }
                events.push(match self.reopen("watchdog").and_then(Opened::delivering) {
                    Ok(()) => CaptureEvent::StreamRestarted {
                        gap,
                        during_capture: capturing,
                    },
                    Err(_) => CaptureEvent::StreamUnavailable {
                        during_capture: capturing,
                    },
                });
            }
        }

        if self.cap == CapNotice::None
            && let CaptureState::Capturing { capped: true, .. } = &*lock(&self.core.state)
        {
            self.cap = CapNotice::Pending;
        }
        if self.cap == CapNotice::Pending {
            self.cap = CapNotice::Reported;
            events.push(CaptureEvent::MemoryCapReached {
                recovery: self.recording_status(),
            });
        }

        let bits = self.core.flag_bits.swap(0, Ordering::Relaxed);
        if bits != 0 {
            events.push(CaptureEvent::Flags(StreamFlags(bits)));
        }
        events
    }

    pub fn shutdown(&mut self) {
        let mut state = lock(&self.core.state);
        if state.capturing() {
            *state = CaptureState::Idle {
                ring: VecDeque::new(),
            };
        }
        drop(state);
        self.core.recorder.stop();
        self.close_stream(Teardown::Drain);
    }

    /// A stream is only expected while capturing, or when a pre-roll ring has
    /// to stay warm between captures.
    fn stream_expected(&self) -> bool {
        lock(&self.core.state).capturing() || self.config.preroll_frames() > 0
    }

    fn stream_is_live(&self) -> bool {
        self.stream.as_ref().is_some_and(InputStream::is_active)
            && self.core.since_last_callback() < STALE_STREAM
    }

    /// Replaces the stream, dead or missing, and counts the failures in a
    /// row: the first is logged as an error, the rest at debug level, and the
    /// recovery once. The old handle is aborted rather than stopped: a wedged
    /// device must not block the event loop while it drains.
    fn reopen(&mut self, reason: &str) -> Result<Opened> {
        log::debug!("opening the input stream ({reason})");
        self.close_stream(Teardown::Abort);
        if let CaptureState::Idle { ring } = &mut *lock(&self.core.state) {
            ring.clear();
        }
        let opened = self.open_stream();
        let outcome = match &opened {
            Ok(opened) => opened.delivering(),
            Err(error) => Err(anyhow!("{error:#}")),
        };
        match outcome {
            Ok(()) => self.recovered(),
            Err(error) => {
                let failing = Failing::after(self.failing, Instant::now());
                if failing.attempts == 1 {
                    log::error!(
                        "microphone unavailable: {error:#}; looked for again at each press, and in between after a wait that doubles up to {}s",
                        MAX_REOPEN_BACKOFF.as_secs()
                    );
                } else {
                    log::debug!(
                        "microphone still unavailable after {} attempts: {error:#}",
                        failing.attempts
                    );
                }
                self.failing = Some(failing);
            }
        }
        opened
    }

    /// Ends the streak of failures, if there was one, and says so once.
    fn recovered(&mut self) {
        if let Some(failing) = self.failing.take() {
            log::info!(
                "microphone available again after {} failed attempts",
                failing.attempts
            );
        }
    }

    fn open_stream(&mut self) -> Result<Opened> {
        let seen = self.core.callback_count.load(Ordering::Relaxed);
        // Sample before start: the backend may invoke the callback from open().
        self.core
            .last_callback_nanos
            .store(self.core.now_nanos(), Ordering::Relaxed);
        self.stream = Some(self.backend.open(&self.config, Arc::clone(&self.core))?);

        let deadline = Instant::now() + FIRST_CALLBACK_TIMEOUT;
        while Instant::now() < deadline {
            if self.core.callback_count.load(Ordering::Relaxed) > seen {
                return Ok(Opened::Delivering);
            }
            thread::sleep(Duration::from_millis(10));
        }
        // The device is open but mute. `reopen`, every caller's way in,
        // reports it and backs off, so that the stall above costs one attempt
        // per back-off period rather than one on every poll of the event loop.
        Ok(Opened::Silent)
    }

    fn close_stream(&mut self, teardown: Teardown) {
        if let Some(stream) = self.stream.take() {
            stream.close(teardown);
        }
    }

    /// Frames the device has delivered to the current capture; `None` while
    /// idle.
    fn delivered(&self) -> Option<Frames> {
        match &*lock(&self.core.state) {
            CaptureState::Capturing { delivered, .. } => Some(*delivered),
            CaptureState::Idle { .. } => None,
        }
    }

    /// Waits until the device has delivered `wanted` more frames to the
    /// current capture, the stream is found dead, `interrupted` says to stop,
    /// or `limit` passes, whichever comes first. Sleeps without the lock.
    fn await_delivery(
        &self,
        wanted: usize,
        limit: Duration,
        interrupted: &mut dyn FnMut() -> bool,
    ) -> PostRoll {
        let Some(start) = self.delivered() else {
            return PostRoll::Complete;
        };
        let target = start + wanted;
        let deadline = Instant::now() + limit;
        loop {
            if self.delivered().is_none_or(|delivered| delivered >= target) {
                return PostRoll::Complete;
            }
            if !self.stream_is_live() {
                return PostRoll::StreamStale;
            }
            if interrupted() {
                return PostRoll::Interrupted;
            }
            if Instant::now() >= deadline {
                return PostRoll::TimedOut;
            }
            thread::sleep(STOP_POLL_INTERVAL);
        }
    }
}

impl<B: InputBackend> Drop for AudioCapture<B> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The production backend: PortAudio's non-blocking input stream.
///
/// PortAudio is initialised at the first open, and again after an open that
/// failed: it lists the devices when it is initialised, so a sound server
/// that came up since, or a microphone plugged in since, is only found by a
/// fresh one.
#[derive(Default)]
pub struct PortAudioBackend {
    portaudio: Option<pa::PortAudio>,
}

pub struct PortAudioStream(pa::Stream<pa::NonBlocking, pa::Input<f32>>);

impl InputStream for PortAudioStream {
    fn is_active(&self) -> bool {
        self.0.is_active().unwrap_or(false)
    }

    fn close(mut self, teardown: Teardown) {
        let result = match teardown {
            Teardown::Drain => self.0.stop(),
            Teardown::Abort => self.0.abort(),
        };
        if let Err(error) = result {
            log::debug!("closing the input stream failed; continuing: {error}");
        }
        // Stream::drop closes the native handle. Calling close() explicitly
        // would make its Drop implementation close the same pointer twice.
    }
}

impl InputBackend for PortAudioBackend {
    type Stream = PortAudioStream;

    fn open(&mut self, config: &Audio, core: Arc<CallbackCore>) -> Result<PortAudioStream> {
        let portaudio = match self.portaudio.take() {
            Some(portaudio) => portaudio,
            None => pa::PortAudio::new().context("initialise PortAudio")?,
        };
        // Kept only when the open works; dropped otherwise, which ends
        // PortAudio (no stream holds it: every caller closed the old one
        // first), so the next attempt lists the devices afresh.
        let stream = open_stream(&portaudio, config, core)?;
        self.portaudio = Some(portaudio);
        Ok(stream)
    }
}

fn open_stream(
    portaudio: &pa::PortAudio,
    config: &Audio,
    core: Arc<CallbackCore>,
) -> Result<PortAudioStream> {
    let device = select_input_device(portaudio, config.device.as_deref())?;
    let info = portaudio
        .device_info(device)
        .context("inspect input device")?;
    if info.max_input_channels < 1 {
        bail!("audio device {:?} has no input channels", info.name);
    }
    let params = pa::StreamParameters::<f32>::new(device, 1, true, info.default_low_input_latency);
    portaudio
        .is_input_format_supported(params, f64::from(config.sample_rate))
        .context("input device does not support mono float32 at the configured sample rate")?;
    let settings = pa::InputStreamSettings::new(
        params,
        f64::from(config.sample_rate),
        pa::FRAMES_PER_BUFFER_UNSPECIFIED,
    );
    let callback = move |pa::InputStreamCallbackArgs {
                             buffer,
                             frames,
                             flags,
                             ..
                         }| {
        core.process(&buffer[..frames], flags.bits());
        pa::Continue
    };
    let mut stream = portaudio
        .open_non_blocking_stream(settings, callback)
        .context("open PortAudio input stream")?;
    stream.start().context("start PortAudio input stream")?;
    Ok(PortAudioStream(stream))
}

fn select_input_device(portaudio: &pa::PortAudio, query: Option<&str>) -> Result<pa::DeviceIndex> {
    let Some(query) = query else {
        return portaudio
            .default_input_device()
            .context("find default input device");
    };
    let devices = portaudio
        .devices()
        .context("enumerate audio devices")?
        .filter_map(|entry| match entry {
            Ok((index, info)) if info.max_input_channels > 0 => {
                let host_api = portaudio.host_api_info(info.host_api).ok_or_else(|| {
                    anyhow!(
                        "inspect host API {} for device {:?}",
                        info.host_api,
                        info.name
                    )
                });
                Some(host_api.map(|host_api| DeviceCandidate {
                    index,
                    name: info.name.to_owned(),
                    host_api: host_api.name.to_owned(),
                }))
            }
            Ok(_) => None,
            Err(error) => Some(Err(anyhow!(error))),
        })
        .collect::<Result<Vec<_>>>()?;
    match_device_query(query, &devices)
}

#[derive(Debug)]
struct DeviceCandidate<I> {
    index: I,
    name: String,
    host_api: String,
}

/// Match sounddevice's `_get_device_id`: case-insensitive, whitespace-split
/// substrings in order across `device, host API`, with a unique exact match
/// resolving an otherwise ambiguous query.
fn match_device_query<I: Copy>(query: &str, devices: &[DeviceCandidate<I>]) -> Result<I> {
    let query_lower = query.to_lowercase();
    let substrings = query_lower.split_whitespace().collect::<Vec<_>>();
    let mut matches = Vec::new();
    let mut exact = Vec::new();
    for device in devices {
        let full = format!("{}, {}", device.name, device.host_api);
        let full_lower = full.to_lowercase();
        let mut position = 0;
        let matched = substrings.iter().all(|substring| {
            let Some(found) = full_lower[position..].find(substring) else {
                return false;
            };
            position += found + substring.len();
            true
        });
        if matched {
            matches.push((device.index, full));
            if query_lower == device.name.to_lowercase() || query_lower == full_lower {
                exact.push(device.index);
            }
        }
    }
    match matches.as_slice() {
        [(index, _)] => Ok(*index),
        [] => bail!("no input audio device matches {query:?}"),
        _ if exact.len() == 1 => Ok(exact[0]),
        matches => {
            let choices = matches
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            bail!("multiple input audio devices match {query:?}:\n{choices}")
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn device(index: usize, name: &str, host_api: &str) -> DeviceCandidate<usize> {
        DeviceCandidate {
            index,
            name: name.into(),
            host_api: host_api.into(),
        }
    }

    #[test]
    fn device_query_matches_case_insensitive_ordered_terms_and_host_api() {
        let devices = [
            device(1, "Built-in Audio", "ALSA"),
            device(2, "Scarlett USB Microphone", "PipeWire ALSA"),
        ];
        assert_eq!(match_device_query("usb PIPE", &devices).unwrap(), 2);
        assert!(match_device_query("pipe usb", &devices).is_err());
    }

    #[test]
    fn exact_device_query_disambiguates_substring_ties() {
        let devices = [
            device(1, "USB", "ALSA"),
            device(2, "USB Microphone", "ALSA"),
        ];
        assert_eq!(match_device_query("usb", &devices).unwrap(), 1);
        assert!(match_device_query("usb alsa", &devices).is_err());
        assert_eq!(match_device_query("USB, ALSA", &devices).unwrap(), 1);
    }

    /// A backend the test drives by hand: `feed` delivers one device buffer,
    /// `kill` makes the current stream look dead to the watchdog, `refuse`
    /// fails every open and `silent` opens streams that never call back.
    #[derive(Clone, Default)]
    struct TestBackend {
        core: Arc<Mutex<Option<Arc<CallbackCore>>>>,
        alive: Arc<AtomicBool>,
        opens: Arc<AtomicUsize>,
        refuse: Arc<AtomicBool>,
        silent: Arc<AtomicBool>,
    }

    struct TestStream {
        alive: Arc<AtomicBool>,
    }

    impl InputStream for TestStream {
        fn is_active(&self) -> bool {
            self.alive.load(Ordering::Relaxed)
        }
        fn close(self, _teardown: Teardown) {}
    }

    impl InputBackend for TestBackend {
        type Stream = TestStream;
        fn open(&mut self, _config: &Audio, core: Arc<CallbackCore>) -> Result<TestStream> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            if self.refuse.load(Ordering::Relaxed) {
                bail!("no such device");
            }
            self.alive.store(true, Ordering::Relaxed);
            *lock(&self.core) = Some(Arc::clone(&core));
            if !self.silent.load(Ordering::Relaxed) {
                // A real device delivers immediately; skip the open timeout.
                core.process(&[], 0);
            }
            Ok(TestStream {
                alive: Arc::clone(&self.alive),
            })
        }
    }

    impl TestBackend {
        fn feed(&self, samples: &[f32]) {
            let core = lock(&self.core).clone().expect("stream opened");
            core.process(samples, 0);
        }
        fn kill(&self) {
            self.alive.store(false, Ordering::Relaxed);
        }
    }

    /// 250 frames at the tests' 1 kHz.
    const TEST_POSTROLL_MS: u32 = 250;

    /// A device on its own thread: delivers ten frames of `1.0` every two
    /// milliseconds until dropped.
    struct Feeder {
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Feeder {
        fn start(backend: &TestBackend) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let backend = backend.clone();
            let thread = thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    backend.feed(&[1.0; 10]);
                    thread::sleep(Duration::from_millis(2));
                }
            });
            Self {
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for Feeder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("feeder thread");
            }
        }
    }

    fn capture(preroll_ms: u32) -> (TestBackend, AudioCapture<TestBackend>) {
        capped_capture(preroll_ms, usize::MAX)
    }

    fn capped_capture(
        preroll_ms: u32,
        maximum_frames: usize,
    ) -> (TestBackend, AudioCapture<TestBackend>) {
        let backend = TestBackend::default();
        let capture = AudioCapture::build(
            backend.clone(),
            Audio {
                sample_rate: 1_000,
                preroll_ms,
                postroll_ms: TEST_POSTROLL_MS,
                device: None,
            },
            Recording {
                enabled: false,
                dir: "/unused".into(),
                max_total_bytes: 1,
            },
            maximum_frames,
        );
        (backend, capture)
    }

    /// A microphone that cannot be opened when the daemon starts (a sound
    /// server not up yet at login) does not stop it: the capture is built
    /// without a stream, a press says the microphone is unavailable, and a
    /// later press opens it.
    #[test]
    fn a_microphone_missing_at_start_is_opened_at_a_later_press() {
        let backend = TestBackend::default();
        backend.refuse.store(true, Ordering::Relaxed);
        let mut capture = AudioCapture::build(
            backend.clone(),
            Audio {
                sample_rate: 1_000,
                preroll_ms: 100,
                postroll_ms: TEST_POSTROLL_MS,
                device: None,
            },
            Recording {
                enabled: false,
                dir: "/unused".into(),
                max_total_bytes: 1,
            },
            10_000,
        );
        assert!(capture.start_capture().is_err(), "still unavailable");
        backend.refuse.store(false, Ordering::Relaxed);
        capture.start_capture().expect("opened at the next press");
    }

    #[test]
    fn ring_keeps_the_newest_samples_after_wrapping() {
        let mut ring = VecDeque::new();
        write_ring(&mut ring, &[1.0, 2.0, 3.0], 5);
        assert_eq!(Vec::from_iter(ring.iter().copied()), [1.0, 2.0, 3.0]);
        write_ring(&mut ring, &[4.0, 5.0, 6.0], 5);
        assert_eq!(
            Vec::from_iter(ring.iter().copied()),
            [2.0, 3.0, 4.0, 5.0, 6.0]
        );
        write_ring(&mut ring, &[7.0, 8.0, 9.0, 10.0, 11.0, 12.0], 5);
        assert_eq!(
            Vec::from_iter(ring.iter().copied()),
            [8.0, 9.0, 10.0, 11.0, 12.0]
        );
    }

    #[test]
    fn capture_begins_with_the_preroll_and_never_with_older_audio() {
        // 5 frames of pre-roll at 1kHz.
        let (backend, mut capture) = capture(5);
        backend.feed(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        capture.start_capture().unwrap();
        backend.feed(&[8.0, 9.0]);
        assert_eq!(
            capture.stop_capture().samples,
            [3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0],
            "capture starts with exactly the pre-roll before the press"
        );

        // A second press immediately afterwards must not see the first
        // capture's audio, even though less than preroll_ms has passed.
        capture.start_capture().unwrap();
        backend.feed(&[10.0]);
        assert_eq!(capture.stop_capture().samples, [10.0]);

        backend.feed(&[11.0, 12.0]);
        capture.start_capture().unwrap();
        assert_eq!(capture.stop_capture().samples, [11.0, 12.0]);
    }

    /// Audio the worker has committed is dropped to whole device buffers,
    /// and what is left says where it begins, so nothing is ever spliced from
    /// the wrong offset.
    #[test]
    fn committed_audio_is_dropped_and_the_snapshot_says_where_it_begins() {
        let (backend, mut capture) = capture(0);
        capture.start_capture().unwrap();
        for samples in [
            [0.0, 1.0, 2.0, 3.0],
            [4.0, 5.0, 6.0, 7.0],
            [8.0, 9.0, 10.0, 11.0],
        ] {
            backend.feed(&samples);
        }
        assert_eq!(capture.retained_frames(), 12);

        // Half a buffer is not dropped: the chunk it is in is still needed.
        capture.discard_before(Frames(6));
        assert_eq!(capture.retained_frames(), 8);
        let snapshot = capture.snapshot_capture(Frames(6));
        assert_eq!(snapshot.start, Frames(6));
        assert_eq!(snapshot.samples, [6.0, 7.0, 8.0, 9.0, 10.0, 11.0]);
        assert_eq!(
            capture.captured_frames(),
            Frames(12),
            "the capture is still twelve frames long"
        );

        capture.discard_before(Frames(8));
        assert_eq!(capture.retained_frames(), 4);
        let dropped = capture.snapshot_capture(Frames(2));
        assert_eq!(
            (dropped.start, dropped.samples.as_slice()),
            (Frames(8), [8.0, 9.0, 10.0, 11.0].as_slice()),
            "a snapshot cannot reach into audio that is gone"
        );

        backend.feed(&[12.0, 13.0]);
        assert_eq!(
            capture.snapshot_capture(Frames(11)).samples,
            [11.0, 12.0, 13.0],
            "later audio still lands at its own offset"
        );
        let taken = capture.stop_capture();
        assert_eq!(taken.start, Frames(8));
        assert_eq!(taken.frames, Frames(14));
        assert_eq!(taken.samples, [8.0, 9.0, 10.0, 11.0, 12.0, 13.0]);
    }

    /// The ceiling is on the audio held at once. With the decoder keeping up
    /// it is never reached, however long the capture runs; the recovery WAV
    /// holds all of it either way.
    #[test]
    fn the_ceiling_bounds_what_is_held_not_how_long_a_capture_runs() {
        let (backend, mut capture) = capped_capture(0, 8);
        capture.start_capture().unwrap();
        for round in 0..20 {
            backend.feed(&[0.5; 4]);
            capture.discard_before(Frames(4 * round));
            assert!(
                capture.retained_frames() <= 8,
                "held {} frames in round {round}",
                capture.retained_frames()
            );
        }
        let taken = capture.stop_capture();
        assert_eq!(taken.frames, Frames(80), "every frame was accepted");
        assert!(
            capture
                .poll()
                .iter()
                .all(|e| !matches!(e, CaptureEvent::MemoryCapReached { .. }))
        );
    }

    /// Once the ceiling drops audio the capture has a hole in it, so it never
    /// accepts again: splicing what came after the hole onto what came before
    /// would decode speech that was never spoken in that order.
    #[test]
    fn the_ceiling_is_final_for_the_rest_of_the_capture() {
        let (backend, mut capture) = capped_capture(0, 4);
        capture.start_capture().unwrap();
        backend.feed(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(capture.retained_frames(), 4);
        capture.discard_before(Frames(4));
        backend.feed(&[6.0, 7.0]);
        let taken = capture.stop_capture();
        assert!(
            taken.samples.is_empty(),
            "audio after the hole is not spliced on: {:?}",
            taken.samples
        );
        assert!(
            capture
                .poll()
                .iter()
                .any(|e| matches!(e, CaptureEvent::MemoryCapReached { .. }))
        );
    }

    /// The loudest sample of the capture decides the "nearly silent" notice,
    /// so it has to survive the audio it was measured in.
    #[test]
    fn the_capture_peak_outlives_the_audio_that_was_dropped() {
        let (backend, mut capture) = capture(0);
        capture.start_capture().unwrap();
        backend.feed(&[0.9, -0.2]);
        capture.discard_before(Frames(2));
        backend.feed(&[0.001, 0.001]);
        let taken = capture.stop_capture();
        assert!(taken.samples.iter().all(|s| s.abs() < 0.01));
        assert_eq!(taken.peak, 0.9);
    }

    #[test]
    fn snapshot_returns_the_tail_after_a_partial_chunk() {
        let (backend, mut capture) = capture(0);
        capture.start_capture().unwrap();
        for samples in [
            [0.0, 1.0, 2.0, 3.0],
            [4.0, 5.0, 6.0, 7.0],
            [8.0, 9.0, 10.0, 11.0],
        ] {
            backend.feed(&samples);
        }
        assert_eq!(capture.captured_frames(), Frames(12));
        assert_eq!(
            capture.snapshot_capture(Frames(5)).samples,
            [5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0]
        );
        assert!(capture.snapshot_capture(Frames(12)).samples.is_empty());
        assert!(capture.snapshot_capture(Frames(99)).samples.is_empty());
        capture.stop_capture();
        assert_eq!(capture.captured_frames(), Frames::ZERO);
        assert!(capture.snapshot_capture(Frames::ZERO).samples.is_empty());
    }

    #[test]
    fn disk_keeps_audio_past_the_memory_cap_and_the_notice_arrives_once() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = TestBackend::default();
        let mut capture = AudioCapture::build(
            backend.clone(),
            Audio {
                sample_rate: 1_000,
                preroll_ms: 0,
                postroll_ms: TEST_POSTROLL_MS,
                device: None,
            },
            Recording {
                enabled: true,
                dir: temporary.path().join("audio"),
                max_total_bytes: 1024 * 1024,
            },
            5,
        );

        capture.start_capture().unwrap();
        backend.feed(&[1.0, 2.0, 3.0]);
        backend.feed(&[4.0, 5.0, 6.0]);
        backend.feed(&[7.0, 8.0]);
        let events = capture.poll();
        assert!(
            matches!(
                events.as_slice(),
                [CaptureEvent::MemoryCapReached { .. }, ..]
            ),
            "cap must be reported while still capturing: {events:?}"
        );
        assert!(
            !capture
                .poll()
                .iter()
                .any(|e| matches!(e, CaptureEvent::MemoryCapReached { .. })),
            "the notice is delivered once per capture"
        );
        let samples = capture.stop_capture().samples;
        assert_eq!(samples, [1.0, 2.0, 3.0, 4.0, 5.0]);

        let RecordingStatus::Recorded(path) = capture.recording_status() else {
            panic!("capture was not recorded")
        };
        let (written, sample_rate) = crate::shell::recorder::read_capture(&path).unwrap();
        assert_eq!(sample_rate, 1_000);
        assert_eq!(written.len(), 8, "disk keeps frames past the RAM cap");
    }

    #[test]
    fn a_cap_reached_just_before_release_is_still_reported() {
        let (backend, mut capture) = capped_capture(0, 2);
        capture.start_capture().unwrap();
        backend.feed(&[1.0, 2.0, 3.0]);
        assert_eq!(capture.stop_capture().samples, [1.0, 2.0]);
        assert!(
            capture
                .poll()
                .iter()
                .any(|e| matches!(e, CaptureEvent::MemoryCapReached { .. })),
            "an unread notice survives stop_capture"
        );
        capture.start_capture().unwrap();
        assert!(
            !capture
                .poll()
                .iter()
                .any(|e| matches!(e, CaptureEvent::MemoryCapReached { .. })),
            "a new capture starts without the previous notice"
        );
    }

    #[test]
    fn callback_tracks_peak_and_flags_without_allocating_status_text() {
        let (backend, mut capture) = capture(4);
        backend.feed(&[-0.25, 0.75]);
        capture.core.flag_bits.store(6, Ordering::Relaxed);
        assert_eq!(capture.level(), 0.75);
        assert_eq!(
            capture.core.flag_bits.load(Ordering::Relaxed),
            6,
            "flags accumulate until polled"
        );
        let events = capture.poll();
        assert_eq!(
            events,
            vec![CaptureEvent::Flags(StreamFlags(6))],
            "each flag set is reported once"
        );
        assert_eq!(
            StreamFlags(6).to_string(),
            "input overflow, output underflow"
        );
        assert_eq!(StreamFlags(0).to_string(), "none");
        assert_eq!(StreamFlags(64).to_string(), "unknown flags 0x40");
        assert!(capture.poll().is_empty());
    }

    #[test]
    fn a_dead_stream_is_repaired_while_idle_and_while_capturing() {
        let (backend, mut capture) = capture(5);
        assert!(capture.poll().is_empty(), "a live stream reports nothing");

        backend.kill();
        let events = capture.poll();
        assert!(
            matches!(
                events.as_slice(),
                [CaptureEvent::StreamRestarted {
                    during_capture: false,
                    ..
                }]
            ),
            "an idle stream is repaired so the pre-roll is ready: {events:?}"
        );

        capture.start_capture().unwrap();
        backend.feed(&[1.0]);
        backend.kill();
        let events = capture.poll();
        assert!(
            matches!(
                events.as_slice(),
                [CaptureEvent::StreamRestarted {
                    during_capture: true,
                    ..
                }]
            ),
            "{events:?}"
        );
        backend.feed(&[2.0]);
        assert_eq!(
            capture.stop_capture().samples,
            [1.0, 2.0],
            "audio received before the gap is kept"
        );
    }

    #[test]
    fn a_device_that_opens_but_delivers_nothing_is_reported_and_backed_off() {
        let (backend, mut capture) = capture(5);
        // The replacement stream opens, but the device stays mute: held by
        // another client, or suspended.
        backend.silent.store(true, Ordering::Relaxed);
        backend.kill();
        let opens = backend.opens.load(Ordering::Relaxed);
        let events = capture.poll();
        assert!(
            matches!(
                events.as_slice(),
                [CaptureEvent::StreamUnavailable {
                    during_capture: false,
                    ..
                }]
            ),
            "an open but mute device is a failed repair: {events:?}"
        );
        // Dead again, so only the back-off can stop the next poll from paying
        // another first-callback stall.
        backend.kill();
        assert!(capture.poll().is_empty(), "the retry is backed off");
        assert_eq!(
            backend.opens.load(Ordering::Relaxed),
            opens + 1,
            "a mute device is reopened once per back-off period, not per poll"
        );
    }

    #[test]
    fn a_device_that_will_not_reopen_is_reported_and_backed_off() {
        let (backend, mut capture) = capture(5);
        backend.refuse.store(true, Ordering::Relaxed);
        backend.kill();
        let opens = backend.opens.load(Ordering::Relaxed);
        let events = capture.poll();
        assert!(
            matches!(
                events.as_slice(),
                [CaptureEvent::StreamUnavailable {
                    during_capture: false,
                    ..
                }]
            ),
            "{events:?}"
        );
        assert!(capture.poll().is_empty(), "the retry is backed off");
        assert_eq!(
            backend.opens.load(Ordering::Relaxed),
            opens + 1,
            "the device is not hammered once per poll"
        );
    }

    #[test]
    fn the_wait_between_attempts_doubles_up_to_a_minute() {
        let waits: Vec<u64> = (1..=8).map(|n| reopen_backoff(n).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(reopen_backoff(u32::MAX), MAX_REOPEN_BACKOFF);
    }

    /// A microphone that stays missing is looked for less and less often,
    /// and one that comes back ends the streak, so the next loss starts the
    /// wait from two seconds again.
    #[test]
    fn a_microphone_that_stays_missing_is_looked_for_less_often() {
        let (backend, mut capture) = capture(5);
        backend.refuse.store(true, Ordering::Relaxed);
        backend.kill();
        let mut waits = Vec::new();
        for _ in 0..3 {
            // Due now: what the clock would reach after the wait.
            if let Some(failing) = &mut capture.failing {
                failing.retry_at = Instant::now();
            }
            let before = Instant::now();
            assert!(matches!(
                capture.poll().as_slice(),
                [CaptureEvent::StreamUnavailable { .. }]
            ));
            let failing = capture.failing.expect("still failing");
            waits.push((failing.attempts, failing.retry_at - before));
        }
        let attempts: Vec<u32> = waits.iter().map(|(n, _)| *n).collect();
        assert_eq!(attempts, [1, 2, 3]);
        for ((_, wait), expected) in waits.iter().zip([2, 4, 8]) {
            let expected = Duration::from_secs(expected);
            assert!(
                *wait >= expected && *wait < expected + Duration::from_secs(1),
                "{wait:?}, expected {expected:?}"
            );
        }

        backend.refuse.store(false, Ordering::Relaxed);
        capture.failing.as_mut().expect("failing").retry_at = Instant::now();
        assert!(matches!(
            capture.poll().as_slice(),
            [CaptureEvent::StreamRestarted { .. }]
        ));
        assert_eq!(capture.failing, None, "the streak ends with the recovery");
    }

    /// A stream that opened mute and then delivers ends the streak, so a
    /// later loss is reported and retried from the first wait again.
    #[test]
    fn a_mute_stream_that_starts_delivering_ends_the_streak() {
        let (backend, mut capture) = capture(5);
        backend.silent.store(true, Ordering::Relaxed);
        backend.kill();
        assert!(matches!(
            capture.poll().as_slice(),
            [CaptureEvent::StreamUnavailable { .. }]
        ));
        assert_eq!(capture.failing.map(|f| f.attempts), Some(1));
        backend.feed(&[0.5]);
        assert!(capture.poll().is_empty());
        assert_eq!(capture.failing, None, "delivering, it works");

        backend.silent.store(false, Ordering::Relaxed);
        backend.kill();
        assert!(
            matches!(
                capture.poll().as_slice(),
                [CaptureEvent::StreamRestarted { .. }]
            ),
            "a later loss is repaired at once, not after a back-off"
        );
    }

    /// The back-off is the watchdog's: a press tries the microphone at once.
    #[test]
    fn a_press_does_not_wait_for_the_back_off() {
        let (backend, mut capture) = capture(5);
        backend.refuse.store(true, Ordering::Relaxed);
        backend.kill();
        capture.poll();
        let retry_at = capture.failing.expect("failing").retry_at;
        assert!(retry_at > Instant::now() + Duration::from_secs(1));
        let opens = backend.opens.load(Ordering::Relaxed);
        assert!(capture.start_capture().is_err(), "still missing");
        assert_eq!(backend.opens.load(Ordering::Relaxed), opens + 1);
        assert_eq!(capture.failing.expect("failing").attempts, 2);
        backend.refuse.store(false, Ordering::Relaxed);
        capture.start_capture().expect("back at the next press");
        assert_eq!(capture.failing, None);
    }

    #[test]
    fn the_postroll_collects_the_configured_frames_after_the_release() {
        let (backend, mut capture) = capture(0);
        capture.start_capture().unwrap();
        backend.feed(&[0.5; 3]);
        let _device = Feeder::start(&backend);
        let (captured, postroll) = capture.finish_capture(Instant::now(), || false);
        let samples = captured.samples;
        assert_eq!(postroll, PostRoll::Complete);
        assert_eq!(samples[..3], [0.5; 3], "audio before the release is kept");
        assert!(
            samples.len() >= 3 + 250,
            "the post-roll holds at least postroll_ms of audio: {} frames",
            samples.len()
        );
    }

    #[test]
    fn a_postroll_without_audio_ends_at_its_deadline() {
        let (backend, mut capture) = capture(0);
        capture.start_capture().unwrap();
        backend.feed(&[0.5; 3]);
        let started = Instant::now();
        let (captured, postroll) = capture.finish_capture(Instant::now(), || false);
        let samples = captured.samples;
        let waited = started.elapsed();
        assert_eq!(postroll, PostRoll::TimedOut);
        assert_eq!(samples, [0.5; 3], "what arrived is still returned");
        let deadline = Duration::from_millis(u64::from(TEST_POSTROLL_MS)) + FINAL_CALLBACK_WAIT;
        assert!(
            waited >= deadline && waited < deadline + Duration::from_millis(200),
            "waited {waited:?}, deadline {deadline:?}"
        );
    }

    #[test]
    fn an_interrupt_ends_the_postroll_at_once() {
        let (backend, mut capture) = capture(0);
        capture.start_capture().unwrap();
        backend.feed(&[0.5; 3]);
        let started = Instant::now();
        let mut asked = 0;
        let (captured, postroll) = capture.finish_capture(Instant::now(), || {
            asked += 1;
            asked > 2
        });
        let samples = captured.samples;
        assert_eq!(postroll, PostRoll::Interrupted);
        assert_eq!(asked, 3, "the interrupt is asked between polls");
        assert_eq!(samples, [0.5; 3]);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_capped_capture_completes_its_postroll_on_delivery() {
        let (backend, mut capture) = capped_capture(0, 5);
        capture.start_capture().unwrap();
        backend.feed(&[0.5; 10]);
        let _device = Feeder::start(&backend);
        let started = Instant::now();
        let (captured, postroll) = capture.finish_capture(Instant::now(), || false);
        let samples = captured.samples;
        assert_eq!(
            postroll,
            PostRoll::Complete,
            "past the memory ceiling, delivery still counts"
        );
        assert_eq!(samples, [0.5; 5]);
        assert!(
            started.elapsed()
                < Duration::from_millis(u64::from(TEST_POSTROLL_MS)) + FINAL_CALLBACK_WAIT,
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_dead_stream_ends_the_postroll_and_says_so() {
        let (backend, mut capture) = capture(5);
        backend.feed(&[0.25; 5]);
        capture.start_capture().unwrap();
        backend.feed(&[0.5; 3]);
        backend.kill();
        let (captured, postroll) = capture.finish_capture(Instant::now(), || false);
        let samples = captured.samples;
        assert_eq!(postroll, PostRoll::StreamStale);
        assert_eq!(
            samples,
            [0.25, 0.25, 0.25, 0.25, 0.25, 0.5, 0.5, 0.5],
            "pre-roll and speech are kept"
        );
    }
}
