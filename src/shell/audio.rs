//! Mono float32 microphone capture with pre-roll and stream recovery.
//!
//! [`AudioCapture`] is generic over the device backend so the capture
//! arithmetic — pre-roll, immutable chunks, the memory ceiling, the watchdog
//! and snapshot slicing — can be driven deterministically in tests. The
//! production backend is [`PortAudioBackend`].

use crate::{
    config::{Audio, Recording},
    core::{frames::Frames, session::RecordingStatus},
    shell::recorder::CaptureRecorder,
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

pub const MAX_UTTERANCE_SECONDS: usize = 3_600;
const FIRST_CALLBACK_TIMEOUT: Duration = Duration::from_millis(500);
const STALE_STREAM: Duration = Duration::from_secs(1);
/// How long a release waits for the callback that is already in flight, so the
/// partial device buffer at the moment of release reaches capture and WAV.
const FINAL_CALLBACK_WAIT: Duration = Duration::from_millis(100);
/// A device that will not reopen must not be retried on every poll.
const REOPEN_BACKOFF: Duration = Duration::from_secs(2);
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
    fn open(&self, config: &Audio, core: Arc<CallbackCore>) -> Result<Self::Stream>;
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
    /// The stream stopped delivering audio and could not be reopened.
    StreamUnavailable {
        reason: String,
        during_capture: bool,
    },
    /// The in-memory ceiling was reached; later audio exists only on disk.
    MemoryCapReached { recovery: RecordingStatus },
    /// The device reported overflow/underflow to the callback.
    Flags(StreamFlags),
}

#[derive(Debug)]
struct Chunk {
    samples: Arc<[f32]>,
    kept: usize,
}

/// Either the pre-roll ring is filling, or a capture is accumulating chunks.
enum CaptureState {
    Idle {
        ring: VecDeque<f32>,
    },
    Capturing {
        chunks: Vec<Chunk>,
        /// Sum of `kept` over `chunks`, cached because the callback needs it.
        frames: Frames,
        /// Samples were dropped because the memory ceiling was reached.
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
    // published through them.
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
                frames,
                capped,
            } => {
                // One copy of the device's reused buffer, shared by the disk
                // and memory sinks. Disk receives it before the memory ceiling
                // is consulted, so the one-hour cap cannot truncate recovery.
                let captured: Arc<[f32]> = Arc::from(samples);
                self.recorder.write_shared(Arc::clone(&captured));
                let remaining = self.maximum_frames.saturating_sub(frames.get());
                let kept = captured.len().min(remaining);
                *capped |= kept < captured.len();
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
    next_reopen: Option<Instant>,
}

impl AudioCapture<PortAudioBackend> {
    /// Initialises PortAudio and opens the configured input device.
    pub fn new(audio: Audio, recording: Recording) -> Result<Self> {
        Self::with_backend(PortAudioBackend::new()?, audio, recording)
    }
}

impl<B: InputBackend> AudioCapture<B> {
    pub fn with_backend(backend: B, audio: Audio, recording: Recording) -> Result<Self> {
        let maximum_frames = audio.sample_rate as usize * MAX_UTTERANCE_SECONDS;
        Self::build(backend, audio, recording, maximum_frames)
    }

    fn build(
        backend: B,
        audio: Audio,
        recording: Recording,
        maximum_frames: usize,
    ) -> Result<Self> {
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
            next_reopen: None,
        };
        if preroll_frames > 0 {
            capture.open_stream()?;
        }
        Ok(capture)
    }

    pub fn start_capture(&mut self) -> Result<()> {
        self.cap = CapNotice::None;
        self.next_reopen = None;
        if self.config.preroll_frames() == 0 {
            self.close_stream(Teardown::Abort);
            self.open_stream()?;
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
        if !preroll.is_empty() {
            let preroll: Arc<[f32]> = Arc::from(preroll);
            self.core.recorder.write_shared(Arc::clone(&preroll));
            frames += preroll.len();
            chunks.push(Chunk {
                kept: preroll.len(),
                samples: preroll,
            });
        }
        *state = CaptureState::Capturing {
            chunks,
            frames,
            capped: false,
        };
        Ok(())
    }

    pub fn stop_capture(&mut self) -> Vec<f32> {
        // The buffer in flight at the moment of release still belongs to this
        // capture; give the device one callback to hand it over.
        self.await_final_callback();
        let (chunks, capped) = {
            let mut state = lock(&self.core.state);
            match &mut *state {
                CaptureState::Idle { .. } => (Vec::new(), false),
                CaptureState::Capturing { chunks, capped, .. } => {
                    let taken = std::mem::take(chunks);
                    let capped = *capped;
                    *state = CaptureState::Idle {
                        ring: VecDeque::with_capacity(self.core.preroll_frames),
                    };
                    (taken, capped)
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
        samples
    }

    /// Samples of the current capture after `since`, empty when idle.
    pub fn snapshot_capture(&self, since: Frames) -> Vec<f32> {
        let (chunks, needed) = {
            let state = lock(&self.core.state);
            let CaptureState::Capturing { chunks, frames, .. } = &*state else {
                return Vec::new();
            };
            let needed = frames.since(since);
            if needed == 0 {
                return Vec::new();
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
            (selected, needed)
        };

        let selected_frames = chunks.iter().map(|(_, kept)| kept).sum::<usize>();
        let skip = selected_frames - needed;
        let mut result = Vec::with_capacity(needed);
        let mut skipped = 0;
        for (samples, kept) in chunks {
            let chunk_skip = (skip - skipped).min(kept);
            result.extend_from_slice(&samples[chunk_skip..kept]);
            skipped += chunk_skip;
        }
        result
    }

    /// Frames of the current capture held in memory; zero while idle. The
    /// event loop uses this to size the open tail without copying it.
    pub fn captured_frames(&self) -> Frames {
        match &*lock(&self.core.state) {
            CaptureState::Capturing { frames, .. } => *frames,
            CaptureState::Idle { .. } => Frames::ZERO,
        }
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.core.level_bits.load(Ordering::Relaxed))
    }

    pub fn recording_status(&self) -> RecordingStatus {
        self.core.recorder.status()
    }

    /// Repairs the stream if it died and reports every device condition
    /// observed since the previous call. Each event is delivered once.
    pub fn poll(&mut self) -> Vec<CaptureEvent> {
        let mut events = Vec::new();
        let capturing = lock(&self.core.state).capturing();

        if self.stream_expected() && !self.stream_is_live() {
            let now = Instant::now();
            if self.next_reopen.is_none_or(|at| now >= at) {
                let gap = self.core.since_last_callback();
                if capturing {
                    log::error!(
                        "input stream stopped delivering audio {:.1}s ago mid-capture; reopening; audio spoken during the gap is lost",
                        gap.as_secs_f64()
                    );
                } else {
                    log::warn!(
                        "idle input stream stopped delivering audio {:.1}s ago; reopening so the pre-roll is ready",
                        gap.as_secs_f64()
                    );
                }
                events.push(match self.reopen("watchdog").and_then(Opened::delivering) {
                    Ok(()) => {
                        self.next_reopen = None;
                        CaptureEvent::StreamRestarted {
                            gap,
                            during_capture: capturing,
                        }
                    }
                    Err(error) => {
                        self.next_reopen = Some(now + REOPEN_BACKOFF);
                        CaptureEvent::StreamUnavailable {
                            reason: format!("{error:#}"),
                            during_capture: capturing,
                        }
                    }
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

    /// Replaces a dead stream. The old handle is aborted rather than stopped:
    /// a wedged device must not block the event loop while it drains.
    fn reopen(&mut self, reason: &str) -> Result<Opened> {
        log::debug!("reopening the input stream ({reason})");
        self.close_stream(Teardown::Abort);
        if let CaptureState::Idle { ring } = &mut *lock(&self.core.state) {
            ring.clear();
        }
        self.open_stream()
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
        // The device is open but mute. The back-off is set here, for every
        // caller, so that the stall above costs one attempt per back-off
        // period rather than one on every poll of the event loop.
        self.next_reopen = Some(Instant::now() + REOPEN_BACKOFF);
        log::error!(
            "input stream opened but delivered no audio within {}ms; the device may be suspended or held by another client",
            FIRST_CALLBACK_TIMEOUT.as_millis()
        );
        Ok(Opened::Silent)
    }

    fn close_stream(&mut self, teardown: Teardown) {
        if let Some(stream) = self.stream.take() {
            stream.close(teardown);
        }
    }

    fn await_final_callback(&self) {
        if !self.stream_is_live() {
            return;
        }
        let seen = self.core.callback_count.load(Ordering::Relaxed);
        let deadline = Instant::now() + FINAL_CALLBACK_WAIT;
        while Instant::now() < deadline {
            if self.core.callback_count.load(Ordering::Relaxed) > seen {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        log::debug!("no further callback arrived within the release window");
    }
}

impl<B: InputBackend> Drop for AudioCapture<B> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The production backend: PortAudio's non-blocking input stream.
pub struct PortAudioBackend {
    portaudio: pa::PortAudio,
}

impl PortAudioBackend {
    pub fn new() -> Result<Self> {
        Ok(Self {
            portaudio: pa::PortAudio::new().context("initialise PortAudio")?,
        })
    }
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

    fn open(&self, config: &Audio, core: Arc<CallbackCore>) -> Result<PortAudioStream> {
        let device = select_input_device(&self.portaudio, config.device.as_deref())?;
        let info = self
            .portaudio
            .device_info(device)
            .context("inspect input device")?;
        if info.max_input_channels < 1 {
            bail!("audio device {:?} has no input channels", info.name);
        }
        let params =
            pa::StreamParameters::<f32>::new(device, 1, true, info.default_low_input_latency);
        self.portaudio
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
        let mut stream = self
            .portaudio
            .open_non_blocking_stream(settings, callback)
            .context("open PortAudio input stream")?;
        stream.start().context("start PortAudio input stream")?;
        Ok(PortAudioStream(stream))
    }
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
        fn open(&self, _config: &Audio, core: Arc<CallbackCore>) -> Result<TestStream> {
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
                device: None,
            },
            Recording {
                enabled: false,
                dir: "/unused".into(),
                max_total_bytes: 1,
            },
            maximum_frames,
        )
        .unwrap();
        (backend, capture)
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
            capture.stop_capture(),
            [3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0],
            "capture starts with exactly the pre-roll before the press"
        );

        // A second press immediately afterwards must not see the first
        // capture's audio, even though less than preroll_ms has passed.
        capture.start_capture().unwrap();
        backend.feed(&[10.0]);
        assert_eq!(capture.stop_capture(), [10.0]);

        backend.feed(&[11.0, 12.0]);
        capture.start_capture().unwrap();
        assert_eq!(capture.stop_capture(), [11.0, 12.0]);
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
            capture.snapshot_capture(Frames(5)),
            [5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0]
        );
        assert!(capture.snapshot_capture(Frames(12)).is_empty());
        assert!(capture.snapshot_capture(Frames(99)).is_empty());
        capture.stop_capture();
        assert_eq!(capture.captured_frames(), Frames::ZERO);
        assert!(capture.snapshot_capture(Frames::ZERO).is_empty());
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
                device: None,
            },
            Recording {
                enabled: true,
                dir: temporary.path().join("audio"),
                max_total_bytes: 1024 * 1024,
            },
            5,
        )
        .unwrap();

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
        let samples = capture.stop_capture();
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
        assert_eq!(capture.stop_capture(), [1.0, 2.0]);
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
            capture.stop_capture(),
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
}
