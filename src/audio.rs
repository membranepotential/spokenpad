//! Mono float32 microphone capture with pre-roll and stream recovery.

use crate::{
    config::{Audio, Recording},
    recorder::{CaptureRecorder, RecordingStatus},
};
use anyhow::{Context, Result, anyhow, bail};
use portaudio as pa;
use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub const MAX_UTTERANCE_SECONDS: usize = 3_600;
const FIRST_CALLBACK_TIMEOUT: Duration = Duration::from_millis(500);
const STALE_STREAM: Duration = Duration::from_secs(1);

type InputStream = pa::Stream<pa::NonBlocking, pa::Input<f32>>;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StreamHealth {
    Idle,
    Live {
        seconds_since_callback: f64,
    },
    Stale {
        seconds_since_callback: f64,
        capturing: bool,
    },
    /// A mid-capture gap remains visible until the next capture begins, even
    /// after the replacement stream has started delivering audio.
    Restarted {
        gap_seconds: f64,
        during_capture: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Watchdog {
    Idle,
    Healthy,
    Restarted {
        gap_seconds: f64,
        during_capture: bool,
    },
}

#[derive(Debug)]
struct RingBuffer {
    samples: Vec<f32>,
    write_position: usize,
    filled: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            samples: vec![0.0; capacity],
            write_position: 0,
            filled: 0,
        }
    }

    fn write(&mut self, samples: &[f32]) {
        let capacity = self.samples.len();
        debug_assert!(capacity > 0);
        if samples.len() >= capacity {
            self.samples
                .copy_from_slice(&samples[samples.len() - capacity..]);
            self.write_position = 0;
            self.filled = capacity;
            return;
        }
        let first = samples.len().min(capacity - self.write_position);
        self.samples[self.write_position..self.write_position + first]
            .copy_from_slice(&samples[..first]);
        let remainder = samples.len() - first;
        if remainder > 0 {
            self.samples[..remainder].copy_from_slice(&samples[first..]);
        }
        self.write_position = (self.write_position + samples.len()) % capacity;
        self.filled = (self.filled + samples.len()).min(capacity);
    }

    fn snapshot(&self) -> Vec<f32> {
        if self.filled < self.samples.len() {
            return self.samples[..self.filled].to_vec();
        }
        let mut result = Vec::with_capacity(self.filled);
        result.extend_from_slice(&self.samples[self.write_position..]);
        result.extend_from_slice(&self.samples[..self.write_position]);
        result
    }
}

#[derive(Debug)]
struct Chunk {
    samples: Arc<[f32]>,
    kept: usize,
}

struct CaptureState {
    ring: Option<RingBuffer>,
    capturing: bool,
    chunks: Vec<Chunk>,
    frames: usize,
    capped: bool,
    cap_reported: bool,
}

struct CallbackCore {
    state: Mutex<CaptureState>,
    recorder: Arc<CaptureRecorder>,
    maximum_frames: usize,
    clock_origin: Instant,
    last_callback_nanos: AtomicU64,
    callback_count: AtomicU64,
    level_bits: AtomicU32,
    stream_status_bits: AtomicU64,
}

impl CallbackCore {
    fn process(&self, samples: &[f32], status_bits: u64) {
        let peak = samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        self.level_bits.store(peak.to_bits(), Ordering::Relaxed);
        self.last_callback_nanos
            .store(self.now_nanos(), Ordering::Release);
        self.callback_count.fetch_add(1, Ordering::AcqRel);
        if status_bits != 0 {
            self.stream_status_bits
                .fetch_or(status_bits, Ordering::AcqRel);
        }

        let mut state = lock(&self.state);
        if state.capturing {
            // One copy of PortAudio's reused buffer, shared by the disk and
            // memory sinks. Disk receives it before the memory ceiling is
            // consulted, so the one-hour cap can never truncate recovery.
            let captured: Arc<[f32]> = Arc::from(samples);
            self.recorder.write_shared(Arc::clone(&captured));
            let remaining = self.maximum_frames.saturating_sub(state.frames);
            if remaining == 0 {
                return;
            }
            let kept = captured.len().min(remaining);
            state.chunks.push(Chunk {
                samples: captured,
                kept,
            });
            state.frames += kept;
        } else if let Some(ring) = state.ring.as_mut() {
            ring.write(samples);
        }
    }

    fn now_nanos(&self) -> u64 {
        self.clock_origin
            .elapsed()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64
    }

    fn seconds_since_callback(&self) -> f64 {
        let now = self.now_nanos();
        let then = self.last_callback_nanos.load(Ordering::Acquire);
        now.saturating_sub(then) as f64 / 1_000_000_000.0
    }

    fn take_cap_notice(&self) -> bool {
        let mut state = lock(&self.state);
        if state.cap_reported {
            return false;
        }
        let capped = state.capped || state.frames >= self.maximum_frames;
        state.cap_reported = capped;
        capped
    }
}

pub struct AudioCapture {
    config: Audio,
    _portaudio: pa::PortAudio,
    stream: Option<InputStream>,
    core: Arc<CallbackCore>,
    restart_notice: Option<(f64, bool)>,
}

impl AudioCapture {
    pub fn new(audio: Audio, recording: Recording) -> Result<Self> {
        let portaudio = pa::PortAudio::new().context("initialise PortAudio")?;
        let recorder = Arc::new(CaptureRecorder::new(recording, audio.sample_rate));
        let preroll_frames = audio.preroll_frames();
        let maximum_frames = audio.sample_rate as usize * MAX_UTTERANCE_SECONDS;
        let clock_origin = Instant::now();
        let core = Arc::new(CallbackCore {
            state: Mutex::new(CaptureState {
                ring: (preroll_frames > 0).then(|| RingBuffer::new(preroll_frames)),
                capturing: false,
                chunks: Vec::new(),
                frames: 0,
                capped: false,
                cap_reported: false,
            }),
            recorder,
            maximum_frames,
            clock_origin,
            // Zero means "since this AudioCapture was constructed" until the
            // first callback. Opening a stream refreshes it before start().
            last_callback_nanos: AtomicU64::new(0),
            callback_count: AtomicU64::new(0),
            level_bits: AtomicU32::new(0.0_f32.to_bits()),
            stream_status_bits: AtomicU64::new(0),
        });
        let mut capture = Self {
            config: audio,
            _portaudio: portaudio,
            stream: None,
            core,
            restart_notice: None,
        };
        if preroll_frames > 0 {
            capture.open_stream()?;
        }
        Ok(capture)
    }

    pub fn start_capture(&mut self) -> Result<()> {
        self.restart_notice = None;
        if self.config.preroll_frames() == 0 {
            self.close_stream();
            self.open_stream()?;
        } else {
            self.ensure_idle_stream_alive()?;
        }

        self.core.recorder.start();
        let mut state = lock(&self.core.state);
        let preroll = state
            .ring
            .as_ref()
            .map(RingBuffer::snapshot)
            .unwrap_or_default();
        state.chunks.clear();
        state.frames = preroll.len();
        state.capped = false;
        state.cap_reported = false;
        if !preroll.is_empty() {
            let preroll: Arc<[f32]> = Arc::from(preroll);
            self.core.recorder.write_shared(Arc::clone(&preroll));
            let kept = preroll.len();
            state.chunks.push(Chunk {
                samples: preroll,
                kept,
            });
        }
        state.capturing = true;
        Ok(())
    }

    pub fn stop_capture(&mut self) -> Vec<f32> {
        let chunks = {
            let mut state = lock(&self.core.state);
            state.capped |= state.frames >= self.core.maximum_frames;
            state.capturing = false;
            state.frames = 0;
            std::mem::take(&mut state.chunks)
        };
        self.core.recorder.stop();
        if self.config.preroll_frames() == 0 {
            self.close_stream();
        }

        let length = chunks.iter().map(|chunk| chunk.kept).sum();
        let mut samples = Vec::with_capacity(length);
        for chunk in chunks {
            samples.extend_from_slice(&chunk.samples[..chunk.kept]);
        }
        samples
    }

    pub fn snapshot_capture(&self, since: usize) -> Vec<f32> {
        let (chunks, needed) = {
            let state = lock(&self.core.state);
            if !state.capturing || since >= state.frames {
                return Vec::new();
            }
            let needed = state.frames - since;
            let mut selected = Vec::new();
            let mut selected_frames = 0;
            for chunk in state.chunks.iter().rev() {
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

    pub fn frames(&self) -> usize {
        lock(&self.core.state).frames
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.core.level_bits.load(Ordering::Relaxed))
    }

    pub fn seconds_since_callback(&self) -> f64 {
        self.core.seconds_since_callback()
    }

    pub fn health(&self) -> StreamHealth {
        if let Some((gap_seconds, during_capture)) = self.restart_notice {
            return StreamHealth::Restarted {
                gap_seconds,
                during_capture,
            };
        }
        let capturing = lock(&self.core.state).capturing;
        let Some(stream) = self.stream.as_ref() else {
            return if capturing {
                StreamHealth::Stale {
                    seconds_since_callback: self.seconds_since_callback(),
                    capturing,
                }
            } else {
                StreamHealth::Idle
            };
        };
        let seconds = self.seconds_since_callback();
        if stream.is_active().unwrap_or(false) && seconds < STALE_STREAM.as_secs_f64() {
            StreamHealth::Live {
                seconds_since_callback: seconds,
            }
        } else {
            StreamHealth::Stale {
                seconds_since_callback: seconds,
                capturing,
            }
        }
    }

    /// Repair a stream that died during capture, retaining all audio already
    /// received. The returned event is edge-triggered; `health()` keeps the
    /// restart visible until the next capture begins.
    pub fn watchdog(&mut self) -> Result<Watchdog> {
        let capturing = lock(&self.core.state).capturing;
        if !capturing {
            return Ok(Watchdog::Idle);
        }
        let seconds = self.seconds_since_callback();
        let active = self
            .stream
            .as_ref()
            .is_some_and(|stream| stream.is_active().unwrap_or(false));
        if active && seconds < STALE_STREAM.as_secs_f64() {
            return Ok(Watchdog::Healthy);
        }

        log::error!(
            "input stream stopped delivering audio {seconds:.1}s ago mid-capture; reopening; audio spoken during the gap is lost"
        );
        self.close_stream();
        if let Some(ring) = lock(&self.core.state).ring.as_mut() {
            *ring = RingBuffer::new(self.config.preroll_frames());
        }
        self.open_stream()?;
        self.restart_notice = Some((seconds, true));
        Ok(Watchdog::Restarted {
            gap_seconds: seconds,
            during_capture: true,
        })
    }

    pub fn take_cap_notice(&self) -> bool {
        self.core.take_cap_notice()
    }

    pub fn recording_status(&self) -> RecordingStatus {
        self.core.recorder.status()
    }

    pub fn take_stream_status(&self) -> Option<String> {
        let bits = self.core.stream_status_bits.swap(0, Ordering::AcqRel);
        if bits == 0 {
            return None;
        }
        let flags = pa::StreamCallbackFlags::from_bits_truncate(bits as _);
        Some(format!("{flags:?}"))
    }

    pub fn shutdown(&mut self) {
        {
            let mut state = lock(&self.core.state);
            state.capturing = false;
        }
        self.core.recorder.stop();
        self.close_stream();
    }

    fn ensure_idle_stream_alive(&mut self) -> Result<()> {
        let seconds = self.seconds_since_callback();
        let active = self
            .stream
            .as_ref()
            .is_some_and(|stream| stream.is_active().unwrap_or(false));
        if active && seconds < STALE_STREAM.as_secs_f64() {
            return Ok(());
        }
        if self.stream.is_some() {
            log::warn!("input stream looks dead after {seconds:.1}s without a callback; reopening");
        }
        self.close_stream();
        if let Some(ring) = lock(&self.core.state).ring.as_mut() {
            *ring = RingBuffer::new(self.config.preroll_frames());
        }
        self.open_stream()
    }

    fn open_stream(&mut self) -> Result<()> {
        let callback_count = self.core.callback_count.load(Ordering::Acquire);
        let device = select_input_device(&self._portaudio, self.config.device.as_deref())?;
        let info = self
            ._portaudio
            .device_info(device)
            .context("inspect input device")?;
        if info.max_input_channels < 1 {
            bail!("audio device {:?} has no input channels", info.name);
        }
        let params =
            pa::StreamParameters::<f32>::new(device, 1, true, info.default_low_input_latency);
        self._portaudio
            .is_input_format_supported(params, f64::from(self.config.sample_rate))
            .context("input device does not support mono float32 at the configured sample rate")?;
        let settings = pa::InputStreamSettings::new(
            params,
            f64::from(self.config.sample_rate),
            pa::FRAMES_PER_BUFFER_UNSPECIFIED,
        );
        let core = Arc::clone(&self.core);
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
            ._portaudio
            .open_non_blocking_stream(settings, callback)
            .context("open PortAudio input stream")?;
        // Sample before start: PortAudio may invoke the callback from start().
        self.core
            .last_callback_nanos
            .store(self.core.now_nanos(), Ordering::Release);
        stream.start().context("start PortAudio input stream")?;
        self.stream = Some(stream);

        let deadline = Instant::now() + FIRST_CALLBACK_TIMEOUT;
        while Instant::now() < deadline {
            if self.core.callback_count.load(Ordering::Acquire) > callback_count {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        log::error!(
            "input stream opened but delivered no audio within 500ms; the device may be suspended or held by another client"
        );
        Ok(())
    }

    fn close_stream(&mut self) {
        if let Some(mut stream) = self.stream.take()
            && let Err(error) = stream.stop()
        {
            log::debug!("stopping input stream failed; continuing: {error}");
        }
        // Stream::drop closes the native handle. Calling close() explicitly
        // would make its Drop implementation close the same pointer twice.
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.shutdown();
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

    fn core(preroll: usize, maximum: usize) -> Arc<CallbackCore> {
        let recorder = Arc::new(CaptureRecorder::new(
            Recording {
                enabled: false,
                dir: "/unused".into(),
                max_total_bytes: 1,
            },
            1_000,
        ));
        core_with_recorder(preroll, maximum, recorder)
    }

    fn core_with_recorder(
        preroll: usize,
        maximum: usize,
        recorder: Arc<CaptureRecorder>,
    ) -> Arc<CallbackCore> {
        Arc::new(CallbackCore {
            state: Mutex::new(CaptureState {
                ring: (preroll > 0).then(|| RingBuffer::new(preroll)),
                capturing: false,
                chunks: Vec::new(),
                frames: 0,
                capped: false,
                cap_reported: false,
            }),
            recorder,
            maximum_frames: maximum,
            clock_origin: Instant::now(),
            last_callback_nanos: AtomicU64::new(0),
            callback_count: AtomicU64::new(0),
            level_bits: AtomicU32::new(0.0_f32.to_bits()),
            stream_status_bits: AtomicU64::new(0),
        })
    }

    #[test]
    fn ring_snapshot_is_chronological_after_wrap() {
        let mut ring = RingBuffer::new(5);
        ring.write(&[1.0, 2.0, 3.0]);
        assert_eq!(ring.snapshot(), [1.0, 2.0, 3.0]);
        ring.write(&[4.0, 5.0, 6.0]);
        assert_eq!(ring.snapshot(), [2.0, 3.0, 4.0, 5.0, 6.0]);
        ring.write(&[7.0, 8.0, 9.0, 10.0, 11.0, 12.0]);
        assert_eq!(ring.snapshot(), [8.0, 9.0, 10.0, 11.0, 12.0]);
    }

    #[test]
    fn disk_sink_precedes_memory_cap() {
        let temporary = tempfile::tempdir().unwrap();
        let recorder = Arc::new(CaptureRecorder::new(
            Recording {
                enabled: true,
                dir: temporary.path().join("audio"),
                max_total_bytes: 1024 * 1024,
            },
            1_000,
        ));
        recorder.start();
        let core = core_with_recorder(0, 5, Arc::clone(&recorder));
        lock(&core.state).capturing = true;
        core.process(&[1.0, 2.0, 3.0], 0);
        core.process(&[4.0, 5.0, 6.0], 0);
        core.process(&[7.0, 8.0], 0);
        {
            let state = lock(&core.state);
            assert_eq!(state.frames, 5);
            assert_eq!(
                state.chunks.iter().map(|chunk| chunk.kept).sum::<usize>(),
                5
            );
            assert_eq!(
                &state.chunks[1].samples[..state.chunks[1].kept],
                &[4.0, 5.0]
            );
        }
        recorder.stop();
        let RecordingStatus::Recorded(path) = recorder.status() else {
            panic!("capture was not recorded")
        };
        let (written, sample_rate) = crate::recorder::read_capture(&path).unwrap();
        assert_eq!(sample_rate, 1_000);
        assert_eq!(written.len(), 8, "disk keeps frames past the RAM cap");
        assert!(core.take_cap_notice());
        assert!(!core.take_cap_notice(), "notice is once per capture");
        {
            let mut state = lock(&core.state);
            state.capturing = false;
            state.frames = 0;
        }
        {
            let mut state = lock(&core.state);
            state.capturing = true;
            state.capped = false;
            state.cap_reported = false;
        }
        core.process(&[9.0; 5], 0);
        {
            let mut state = lock(&core.state);
            state.capped = state.frames >= core.maximum_frames;
            state.capturing = false;
            state.frames = 0;
        }
        assert!(core.take_cap_notice(), "an unread notice survives stop");
        assert!(!core.take_cap_notice(), "the new capture also reports once");
    }

    #[test]
    fn snapshot_tail_math_handles_partial_first_chunk() {
        let core = core(0, 100);
        {
            let mut state = lock(&core.state);
            state.capturing = true;
            for samples in [
                [0.0, 1.0, 2.0, 3.0],
                [4.0, 5.0, 6.0, 7.0],
                [8.0, 9.0, 10.0, 11.0],
            ] {
                let samples: Arc<[f32]> = Arc::from(samples);
                state.frames += samples.len();
                state.chunks.push(Chunk {
                    kept: samples.len(),
                    samples,
                });
            }
        }
        let selected = {
            let state = lock(&core.state);
            let needed = state.frames - 5;
            let mut chunks = Vec::new();
            let mut frames = 0;
            for chunk in state.chunks.iter().rev() {
                chunks.push((Arc::clone(&chunk.samples), chunk.kept));
                frames += chunk.kept;
                if frames >= needed {
                    break;
                }
            }
            chunks.reverse();
            let skip = frames - needed;
            let mut result = Vec::new();
            let mut skipped = 0;
            for (samples, kept) in chunks {
                let chunk_skip = (skip - skipped).min(kept);
                result.extend_from_slice(&samples[chunk_skip..kept]);
                skipped += chunk_skip;
            }
            result
        };
        assert_eq!(selected, [5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0]);
    }

    #[test]
    fn callback_tracks_peak_and_status_without_allocating_status_text() {
        let core = core(4, 100);
        core.process(&[-0.25, 0.75], 4);
        assert_eq!(
            f32::from_bits(core.level_bits.load(Ordering::Relaxed)),
            0.75
        );
        assert_eq!(core.stream_status_bits.load(Ordering::Acquire), 4);
        assert_eq!(
            lock(&core.state).ring.as_ref().unwrap().snapshot(),
            [-0.25, 0.75]
        );
    }
}
