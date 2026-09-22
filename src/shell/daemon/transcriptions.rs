//! Every recording whose text is not all written yet, and what becomes of
//! each: the daemon's bookkeeping for the list the next start reads.
use super::engine::ShorterThanItsText;
use crate::{
    core::{
        decode::{Commit, UtteranceId},
        frames::Frames,
        session::{Notice, RecordingStatus, Rest, Session},
    },
    shell::{
        audio::{AudioCapture, InputBackend},
        recorder::Unfinished,
    },
};
use std::{path::PathBuf, time::Duration};

/// A recording whose text is not all written yet: its recovery WAV, the
/// utterance it is, how far into the WAV its text reaches, where this
/// daemon's attempt at it began, and where it is now.
pub(super) struct Transcription {
    pub(super) path: PathBuf,
    pub(super) id: UtteranceId,
    pub(super) through: Frames,
    pub(super) from: Frames,
    pub(super) stage: Stage,
}

/// Where a [`Transcription`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
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
    /// before, or its release decode failed; the next start tries the rest
    /// again.
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
pub(super) struct Transcriptions(pub(super) Vec<Transcription>);

impl Transcriptions {
    /// The recordings made while the speech model was not ready that it has
    /// yet to transcribe, as the window counts them.
    pub(super) fn waiting(&self) -> usize {
        self.0
            .iter()
            .filter(|t| matches!(t.stage, Stage::Kept | Stage::Sent))
            .count()
    }
    pub(super) fn get_mut(&mut self, id: UtteranceId) -> Option<&mut Transcription> {
        self.0.iter_mut().find(|t| t.id == id)
    }
    pub(super) fn remove(&mut self, id: UtteranceId) -> Option<Transcription> {
        let at = self.0.iter().position(|t| t.id == id)?;
        Some(self.0.remove(at))
    }
    /// Notes how far a recording's text reaches.
    pub(super) fn advance(&mut self, commit: &Commit) {
        if let Some(t) = self.get_mut(commit.utterance.id) {
            t.through = t.through.max(commit.through);
        }
    }
    /// Those the list the next start reads holds while this daemon runs.
    pub(super) fn listed_while_running(&self) -> impl Iterator<Item = &Transcription> {
        self.0.iter().filter(|t| t.stage.listed_while_running())
    }
}

/// Rewrites the list the next start reads (`waiting.tsv`): every recording
/// not transcribed yet, with how far its text reaches. Called when one is
/// added or finished and at the stop, never per commit, so after a crash the
/// next start may write again the text committed since the last call.
pub(super) fn persist<'a, B: InputBackend>(
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
/// capture's tail, whose text is all written now unless it failed, or a
/// recording's transcription, which [`settle`] judges. True when the list
/// the next start reads changed.
pub(super) fn finished<B: InputBackend>(
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
        Stage::Finishing => match error {
            None => {
                transcriptions.remove(id);
                false
            }
            // This daemon's only attempt at it: the next start transcribes
            // the rest from its recording, from where its text reaches.
            Some(_) => {
                retry_at_next_start(recording, rate, capture, session);
                recording.stage = Stage::Retry;
                true
            }
        },
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
            retry_at_next_start(recording, rate, capture, session);
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

/// Leaves the rest of a recording whose transcription failed to the next
/// start, from where its text reaches, and says so. Kept from pruning until
/// then: it is the only copy of that rest.
fn retry_at_next_start<B: InputBackend>(
    recording: &Transcription,
    rate: u32,
    capture: &AudioCapture<B>,
    session: &mut Session,
) {
    let (path, seconds) = (&recording.path, recording.through.seconds(rate));
    capture.keep_recording(path);
    log::error!(
        "{} was transcribed through {seconds:.2}s only; the next start tries the rest again",
        path.display()
    );
    session.notify(Notice::RecordingPartlyTranscribed {
        path: path.clone(),
        through: Duration::from_secs_f64(seconds),
        rest: Rest::NextStart,
    });
}

/// Releases a capture made before the speech model was ready. There is
/// nothing to decode now, so the session goes back to idle; the recording is
/// the capture from here on, transcribed once the model is ready.
pub(super) fn keep<B: InputBackend>(
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
pub(super) fn begin<B: InputBackend>(
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
