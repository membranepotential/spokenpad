//! What the event loop does with the microphone's news: a capture thrown
//! away, a capture released for its final decode, and what the device
//! reports in between.
use super::{engine::Tail, transcriptions::Transcriptions};
use crate::{
    config::Config,
    core::{
        frames::Frames,
        session::{Notice, RecordingStatus, Session},
        state::{Command, DiscardReason, State},
    },
    shell::audio::{
        AudioCapture, CaptureEvent, Captured, InputBackend, MAX_UTTERANCE_SECONDS, PostRoll,
    },
};
use anyhow::{Context, Result};
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// Below this a capture that is much shorter than the hold is reported.
const INCOMPLETE_HOLD: Duration = Duration::from_secs(1);
/// Below this peak a capture is reported as nearly silent.
const NEARLY_SILENT_PEAK: f32 = 0.01;

/// Ends a capture that is thrown away rather than decoded. Its recovery WAV
/// is kept, whatever the reason, but nothing transcribes the rest of it.
pub(super) fn discard<B: InputBackend>(
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

/// Everything that happens between the key release and the decode request:
/// the measurements, the notices they call for, and the dump.
pub(super) fn release<B: InputBackend>(
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

pub(super) fn note_postroll(postroll: PostRoll, session: &mut Session) {
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
pub(super) fn apply_capture_event(
    event: CaptureEvent,
    session: &mut Session,
    now: Instant,
) -> Command {
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
            let minutes = (MAX_UTTERANCE_SECONDS / 60) as u64;
            let notice = Notice::MemoryCap {
                recovery: recovery.clone(),
                minutes,
            };
            match &recovery {
                RecordingStatus::Recorded(path) | RecordingStatus::Truncated(path) => {
                    log::warn!("{notice} ({})", path.display())
                }
                RecordingStatus::NotRecorded => log::warn!("{notice}"),
            }
            session.cap(recovery, minutes, now)
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
