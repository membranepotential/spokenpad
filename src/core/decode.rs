//! Shared live/recovery pipeline. Preview output has a separate event type.
use crate::core::frames::Frames;
use anyhow::{Result, ensure};
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

/// Whether the recognizer appends its trailing silence to the samples it is
/// given. `Padded` is the normal presentation. Parakeet sometimes decodes a
/// short utterance to "" with the silence and to the right words without it,
/// and sometimes the other way round, so neither is safe on its own; `Bare`
/// exists for the retry in [`Pipeline::transcribe_speech`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailingSilence {
    Padded,
    Bare,
}
pub trait Recognizer {
    fn transcribe(&mut self, samples: &[f32], trailing: TrailingSilence) -> Result<String>;
}
pub trait Segmenter {
    fn split(&mut self, samples: &[f32]) -> Result<Split>;
}
/// What the segmenter made of one slice: the windows to decode, and how far
/// the slice is finished even where it holds no speech.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Split {
    pub segments: Vec<Segment>,
    /// Slice prefix that no later window can reach back into, because only
    /// settled silence follows the last speech in it. Audio before it may be
    /// dropped. Zero while the trailing silence is too short to be a decode
    /// boundary, and always on the detector's window grid.
    pub silent_through: usize,
    /// Where the slice may be cut when none of `segments` settled, with the
    /// windows that decode what comes before the cut; `None` when the
    /// detector heard no pause in it.
    pub pause: Option<Pause>,
}
/// The last pause the detector heard in a slice. `at` is where the speech
/// before it ends, and `segments` decode everything before `at`, padded as
/// a window that ends there is, but never into the speech that follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pause {
    pub at: usize,
    pub segments: Vec<Segment>,
}
impl Pause {
    /// How much of the slice its segments read: `at`, and the padding
    /// after it.
    fn reach(&self) -> usize {
        self.segments
            .last()
            .map_or(self.at, |s| s.window.end.max(self.at))
    }
}
/// One recognizer input. `window` indexes the slice that was segmented;
/// `speech_end` is the unpadded speech boundary inside the same slice, which
/// is what a settled commit advances the offset to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub window: Range<usize>,
    pub speech_end: usize,
    pub settled: bool,
}

/// An utterance moves forwards only: recording, then either released for its
/// final decode or cancelled outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Lifecycle {
    Live = 0,
    Released = 1,
    Cancelled = 2,
}

impl Lifecycle {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Live,
            1 => Self::Released,
            _ => Self::Cancelled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UtteranceId(pub u64);
#[derive(Debug)]
pub struct Utterance {
    pub id: UtteranceId,
    lifecycle: AtomicU8,
}
impl Utterance {
    pub fn new(id: u64) -> Arc<Self> {
        Arc::new(Self {
            id: UtteranceId(id),
            lifecycle: AtomicU8::new(Lifecycle::Live as u8),
        })
    }
    fn advance(&self, to: Lifecycle) {
        self.lifecycle.fetch_max(to as u8, Ordering::AcqRel);
    }
    pub fn release(&self) {
        self.advance(Lifecycle::Released);
    }
    pub fn cancel(&self) {
        self.advance(Lifecycle::Cancelled);
    }
    pub fn lifecycle(&self) -> Lifecycle {
        Lifecycle::from_bits(self.lifecycle.load(Ordering::Acquire))
    }
    pub fn cancelled(&self) -> bool {
        self.lifecycle() == Lifecycle::Cancelled
    }
    /// Still recording: preview ticks may be issued and their text shown.
    pub fn ticking(&self) -> bool {
        self.lifecycle() == Lifecycle::Live
    }
}

/// The recognizer and the voice activity detector that cuts what it hears
/// into its windows. Both are always there: without the detector nothing
/// settles, and silence is decoded, which is where the recognizer invents
/// words.
pub struct Pipeline<R, S> {
    pub recognizer: R,
    pub segmenter: S,
}
impl<R: Recognizer, S: Segmenter> Pipeline<R, S> {
    fn split(&mut self, samples: &[f32]) -> Result<Split> {
        let split = self.segmenter.split(samples)?;
        let end = speech_end(&split.segments, samples.len())?;
        ensure!(
            split.silent_through == 0
                || (split.silent_through >= end && split.silent_through <= samples.len()),
            "settled silence outside the slice"
        );
        if let Some(pause) = &split.pause {
            ensure!(
                pause.at > 0
                    && pause.at <= samples.len()
                    && speech_end(&pause.segments, samples.len())? <= pause.at,
                "a pause outside the slice"
            );
        }
        Ok(split)
    }
    /// Decodes one segment's window. A window the VAD marked as speech that
    /// decodes to nothing is decoded once more without the trailing silence,
    /// and that result stands. This is the only second decode of committed
    /// audio: it runs only when the first decode produced no text, so every
    /// window's output still comes from exactly one decode.
    fn transcribe_speech(&mut self, window: &[f32]) -> Result<String> {
        let text = self
            .recognizer
            .transcribe(window, TrailingSilence::Padded)?;
        if !text.trim().is_empty() {
            return Ok(text);
        }
        let retry = self.recognizer.transcribe(window, TrailingSilence::Bare)?;
        let seconds = Frames(window.len()).seconds(crate::config::REQUIRED_SAMPLE_RATE);
        if retry.trim().is_empty() {
            log::debug!("{seconds:.1}s of speech decoded empty with and without trailing silence");
        } else {
            log::info!(
                "{seconds:.1}s of speech decoded empty; without trailing silence: {} characters",
                retry.chars().count()
            );
        }
        Ok(retry)
    }
    /// Decodes `samples` whole, segment by segment, handing each segment's
    /// text to `on_segment` with where its speech ends in `samples`: what
    /// that text covers, and so how far a commit of it reaches.
    pub fn decode(
        &mut self,
        samples: &[f32],
        abandoned: impl Fn() -> bool,
        on_segment: impl FnMut(String, usize),
    ) -> Result<String> {
        if samples.is_empty() || abandoned() {
            return Ok(String::new());
        }
        let segments = self.split(samples)?.segments;
        self.decode_segments(samples, &segments, &abandoned, &abandoned, on_segment)
    }
    /// [`decode`](Self::decode) over the segments [`split`](Self::split)
    /// already made of `samples`. `stop` is asked before each decode, and
    /// once it says so nothing more is decoded; `discard` is asked after
    /// each, and a decode that ends while it says so is thrown away.
    fn decode_segments(
        &mut self,
        samples: &[f32],
        segments: &[Segment],
        stop: impl Fn() -> bool,
        discard: impl Fn() -> bool,
        mut on_segment: impl FnMut(String, usize),
    ) -> Result<String> {
        if samples.is_empty() || stop() {
            return Ok(String::new());
        }
        // Silence is not decoded: `split` only returns nothing when the VAD
        // found no speech at all. The rate is the validated project-wide
        // constant.
        if segments.is_empty() {
            log::debug!(
                "VAD found no speech in {:.1}s; nothing to decode",
                Frames(samples.len()).seconds(crate::config::REQUIRED_SAMPLE_RATE)
            );
            return Ok(String::new());
        }
        let mut texts = vec![];
        for s in segments {
            if stop() {
                return Ok(texts.join(" "));
            }
            let text = self.transcribe_speech(&samples[s.window.clone()])?;
            log::debug!("chunk {:?}: {} characters", s.window, text.chars().count());
            if discard() {
                return Ok(texts.join(" "));
            }
            if !text.trim().is_empty() {
                texts.push(text.clone());
                on_segment(text, s.speech_end);
            }
        }
        // Explicit failure recovery exception: don't let segmentation erase speech.
        if texts.is_empty() && segments.len() > 1 && !stop() {
            log::warn!(
                "all {} chunks empty; retrying the complete remainder",
                segments.len()
            );
            let text = self
                .recognizer
                .transcribe(samples, TrailingSilence::Padded)?;
            if !text.trim().is_empty() && !discard() {
                texts.push(text.clone());
                on_segment(text, samples.len());
            }
        }
        Ok(texts.join(" "))
    }
}

/// Where the speech of `segments` ends, after checking that they are in
/// order and inside a slice of `len` samples.
fn speech_end(segments: &[Segment], len: usize) -> Result<usize> {
    let mut end = 0;
    for s in segments {
        ensure!(
            s.window.start <= s.speech_end
                && s.speech_end <= s.window.end
                && s.window.end <= len
                && s.speech_end >= end,
            "invalid segment boundaries"
        );
        end = s.speech_end;
    }
    Ok(end)
}

#[derive(Debug)]
pub struct Commit {
    pub utterance: Arc<Utterance>,
    pub text: String,
    pub through: Frames,
}
/// Cosmetic text for the open tail, and what the tick heard. Absence of a
/// `Preview` is the single encoding of "this tick produced nothing to show".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub text: String,
    pub through: Frames,
    /// Where the last speech the detector found in this tick's audio ends,
    /// capture-absolute, settled or not; `None` when it found none. Speech
    /// that ends later than any the capture had before is the user still
    /// talking, whether or not the recognizer has made words of it yet.
    pub heard: Option<Frames>,
}
#[derive(Default)]
struct Progress {
    id: Option<UtteranceId>,
    through: Frames,
}
/// What a tick is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickKind {
    /// The whole open tail: settled chunks commit, and the rest is decoded
    /// for the preview.
    Preview,
    /// A window cut from an open tail longer than it, too long to redraw on
    /// every tick. Settled chunks still commit, so the committed offset keeps
    /// moving and the audio behind it can still be dropped; the rest is not
    /// decoded. A window that begins at the committed offset and settles
    /// nothing is committed through its last pause, or whole when it has
    /// none, so the offset moves anyway.
    Commits,
}

pub struct Worker<R, S> {
    pub pipeline: Pipeline<R, S>,
    progress: Progress,
}
impl<R: Recognizer, S: Segmenter> Worker<R, S> {
    pub fn new(pipeline: Pipeline<R, S>) -> Self {
        Self {
            pipeline,
            progress: Progress::default(),
        }
    }
    /// Takes up `utterance` with its text committed through `through`
    /// already, by an earlier daemon: nothing before it is decoded again.
    pub fn resume(&mut self, utterance: &Utterance, through: Frames) {
        self.progress = Progress {
            id: Some(utterance.id),
            through,
        };
    }
    fn begin(&mut self, utterance: &Utterance) {
        if self.progress.id != Some(utterance.id) {
            self.progress = Progress {
                id: Some(utterance.id),
                through: Frames::ZERO,
            };
        }
    }
    /// Commits every chunk that has settled and returns the open tail's
    /// cosmetic text, or `None` once the utterance stopped ticking.
    pub fn tick(
        &mut self,
        samples: &[f32],
        start: Frames,
        utterance: &Arc<Utterance>,
        kind: TickKind,
        mut commit: impl FnMut(Commit),
    ) -> Result<Option<Preview>> {
        if !utterance.ticking() {
            return Ok(None);
        }
        self.begin(utterance);
        ensure!(
            start <= self.progress.through,
            "snapshot begins after the worker's offset"
        );
        let skip = self.progress.through.since(start).min(samples.len());
        let remainder = &samples[skip..];
        if remainder.is_empty() {
            return Ok(Some(self.open_tail(String::new(), None)));
        }
        let base = self.progress.through;
        let split = self.pipeline.split(remainder)?;
        let heard = split.segments.last().map(|s| base + s.speech_end);
        let mut open = None;
        for segment in &split.segments {
            if !utterance.ticking() {
                return Ok(None);
            }
            if !segment.settled {
                open = Some(segment.window.clone());
                break;
            }
            let text = self
                .pipeline
                .transcribe_speech(&remainder[segment.window.clone()])?;
            if utterance.cancelled() {
                return Ok(None);
            }
            // A release during this decode keeps this completed settled chunk.
            self.progress.through = base + segment.speech_end;
            commit(Commit {
                utterance: Arc::clone(utterance),
                text,
                through: self.progress.through,
            });
        }
        match (open, kind) {
            // The preview is cosmetic and redrawn every tick, so an empty open
            // tail is not retried; only a settled chunk's text is permanent.
            (Some(window), TickKind::Preview) => {
                let text = self
                    .pipeline
                    .recognizer
                    .transcribe(&remainder[window], TrailingSilence::Padded)?;
                return Ok(utterance.ticking().then(|| self.open_tail(text, heard)));
            }
            (Some(_), TickKind::Commits) => {}
            // Every window is decoded and only settled silence follows.
            // Finishing it commits no text and decodes nothing; it is what
            // keeps the open tail, and the audio the shell has to hold for
            // it, bounded while nobody is speaking.
            (None, _) => {
                let silent_through = base + split.silent_through;
                if split.silent_through > 0 && silent_through > self.progress.through {
                    if utterance.cancelled() {
                        return Ok(None);
                    }
                    self.progress.through = silent_through;
                    commit(Commit {
                        utterance: Arc::clone(utterance),
                        text: String::new(),
                        through: silent_through,
                    });
                }
            }
        }
        if kind == TickKind::Commits && skip == 0 && self.progress.through == base {
            self.commit_window(remainder, &split, utterance, &mut commit)?;
        }
        Ok(utterance
            .ticking()
            .then(|| self.open_tail(String::new(), heard)))
    }

    fn open_tail(&self, text: String, heard: Option<Frames>) -> Preview {
        Preview {
            text,
            through: self.progress.through,
            heard,
        }
    }
    /// Commits `window`, which begins at the worker's offset and was split
    /// into `split`, through its last pause, and moves the offset there: a
    /// window cut from a longer tail in which no chunk settled -- pauses too
    /// short to settle one, or speech the detector never breaks -- so that
    /// no more than a window of it is ever held. What follows the pause stays
    /// for the next tick: a word the window's end cuts in two, or one begun
    /// too briefly before it for the detector to call it speech yet. Only a
    /// window with no pause at all is committed whole, and its end may fall
    /// inside a word: the price of the bound, paid only there. Decoded as
    /// [`finish`](Self::finish) decodes a tail, but without ending anything;
    /// a release stops it before the next segment and keeps the one just
    /// decoded, as a settled chunk is kept.
    fn commit_window(
        &mut self,
        window: &[f32],
        split: &Split,
        utterance: &Arc<Utterance>,
        commit: &mut impl FnMut(Commit),
    ) -> Result<()> {
        let base = self.progress.through;
        let (slice, segments, cut) = match &split.pause {
            Some(pause) => (&window[..pause.reach()], &pause.segments, pause.at),
            None => (window, &split.segments, window.len()),
        };
        let end = base + cut;
        let (_, reached) = self.decode_rest(
            slice,
            segments,
            base,
            utterance,
            || !utterance.ticking(),
            commit,
        )?;
        self.progress.through = reached;
        if utterance.ticking() && end > reached {
            // Every segment was decoded: what follows the last one up to the
            // cut is silence, finished with as settled silence is.
            self.progress.through = end;
            commit(Commit {
                utterance: Arc::clone(utterance),
                text: String::new(),
                through: end,
            });
        }
        Ok(())
    }
    /// Decodes `remainder`, which begins at `base` and holds `segments`,
    /// committing each segment's text at its own speech end, so that text
    /// never claims audio that was not decoded. No segment is decoded once
    /// `stop` says so, and one decoded after a cancel is thrown away. Returns
    /// the text and how far the commits reach.
    fn decode_rest(
        &mut self,
        remainder: &[f32],
        segments: &[Segment],
        base: Frames,
        utterance: &Arc<Utterance>,
        stop: impl Fn() -> bool,
        commit: &mut impl FnMut(Commit),
    ) -> Result<(String, Frames)> {
        let mut reached = base;
        let text = self.pipeline.decode_segments(
            remainder,
            segments,
            stop,
            || utterance.cancelled(),
            |text, speech_end| {
                reached = base + speech_end;
                commit(Commit {
                    utterance: Arc::clone(utterance),
                    text,
                    through: reached,
                });
            },
        )?;
        Ok((text, reached))
    }
    /// Decodes what is left of the capture. `samples` is the audio still held,
    /// beginning at `start`; everything before it has been committed and is
    /// gone.
    pub fn finish(
        &mut self,
        samples: &[f32],
        start: Frames,
        utterance: &Arc<Utterance>,
        mut commit: impl FnMut(Commit),
    ) -> Result<(String, usize)> {
        self.begin(utterance);
        ensure!(
            start <= self.progress.through,
            "the held audio begins after the worker's offset"
        );
        let skip = self.progress.through.since(start).min(samples.len());
        let remainder = &samples[skip..];
        let segments = if remainder.is_empty() || utterance.cancelled() {
            Ok(Vec::new())
        } else {
            self.pipeline.split(remainder).map(|split| split.segments)
        };
        let result = segments.and_then(|segments| {
            self.decode_rest(
                remainder,
                &segments,
                start + skip,
                utterance,
                || utterance.cancelled(),
                &mut commit,
            )
        });
        self.progress = Progress::default();
        Ok((result?.0, remainder.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        calls: Vec<Vec<f32>>,
        replies: Vec<String>,
        stop: Option<Arc<Utterance>>,
    }
    impl Recognizer for Fake {
        fn transcribe(&mut self, s: &[f32], _: TrailingSilence) -> Result<String> {
            self.calls.push(s.to_vec());
            if let Some(u) = &self.stop {
                u.release();
            }
            Ok(if self.replies.is_empty() {
                "text".into()
            } else {
                self.replies.remove(0)
            })
        }
    }
    struct Quarters;
    impl Segmenter for Quarters {
        fn split(&mut self, s: &[f32]) -> Result<Split> {
            Ok(Split {
                segments: (0..s.len())
                    .step_by(4)
                    .map(|i| Segment {
                        window: i..(i + 4).min(s.len()),
                        speech_end: (i + 4).min(s.len()),
                        settled: i + 4 < s.len(),
                    })
                    .collect(),
                ..Split::default()
            })
        }
    }
    fn worker() -> Worker<Fake, Quarters> {
        Worker::new(Pipeline {
            recognizer: Fake {
                calls: vec![],
                replies: vec![],
                stop: None,
            },
            segmenter: Quarters,
        })
    }
    #[test]
    fn progressive_release_never_redecodes_committed_speech() {
        let mut w = worker();
        let u = Utterance::new(1);
        let samples: Vec<_> = (0..10).map(|i| i as f32).collect();
        let mut commits = vec![];
        assert_eq!(
            w.tick(&samples, Frames::ZERO, &u, TickKind::Preview, |c| commits
                .push(c))
                .unwrap(),
            Some(Preview {
                text: "text".into(),
                through: Frames(8),
                heard: Some(Frames(10)),
            })
        );
        assert_eq!(
            commits.iter().map(|c| c.through).collect::<Vec<_>>(),
            vec![Frames(4), Frames(8)]
        );
        u.release();
        let (_, tail) = w
            .finish(&samples, Frames::ZERO, &u, |c| commits.push(c))
            .unwrap();
        assert_eq!(tail, 2);
        assert_eq!(
            w.pipeline.recognizer.calls,
            vec![
                vec![0., 1., 2., 3.],
                vec![4., 5., 6., 7.],
                vec![8., 9.],
                vec![8., 9.]
            ]
        );
        assert_eq!(commits.len(), 3);
    }
    #[test]
    fn lagging_snapshot_and_stale_tick() {
        let mut w = worker();
        let a = Utterance::new(1);
        let s: Vec<_> = (0..10).map(|i| i as f32).collect();
        w.tick(&s, Frames::ZERO, &a, TickKind::Preview, |_| {})
            .unwrap();
        w.tick(&s[4..], Frames(4), &a, TickKind::Preview, |_| {
            panic!("duplicate commit")
        })
        .unwrap();
        assert_eq!(w.pipeline.recognizer.calls.last().unwrap(), &vec![8., 9.]);
        a.release();
        let b = Utterance::new(2);
        assert_eq!(
            w.tick(&s, Frames::ZERO, &a, TickKind::Preview, |_| panic!())
                .unwrap(),
            None
        );
        w.finish(&s, Frames::ZERO, &a, |_| {}).unwrap();
        w.tick(&s, Frames::ZERO, &b, TickKind::Preview, |_| {})
            .unwrap();
        assert_eq!(w.progress.through, Frames(8));
    }
    #[test]
    fn release_in_settled_decode_commits_current_chunk_then_stops() {
        let mut w = worker();
        let u = Utterance::new(1);
        w.pipeline.recognizer.stop = Some(u.clone());
        let mut commits = vec![];
        assert_eq!(
            w.tick(&[1.; 10], Frames::ZERO, &u, TickKind::Preview, |c| commits
                .push(c))
                .unwrap(),
            None
        );
        assert_eq!(commits.len(), 1);
        assert_eq!(w.progress.through, Frames(4));
    }
    #[test]
    fn a_cancelled_final_decode_decodes_nothing() {
        let mut w = worker();
        let u = Utterance::new(1);
        u.cancel();
        w.finish(&[1.; 10], Frames::ZERO, &u, |_| panic!()).unwrap();
        assert!(w.pipeline.recognizer.calls.is_empty());
    }
    #[test]
    fn empty_chunks_retry_whole_buffer() {
        let mut w = worker();
        // Two chunks, each decoded padded and then bare, then the remainder.
        w.pipeline.recognizer.replies = vec![
            "".into(),
            "".into(),
            " ".into(),
            "".into(),
            "recovered".into(),
        ];
        let mut commits = vec![];
        let (text, _) = w
            .finish(&[1.; 8], Frames::ZERO, &Utterance::new(1), |c| {
                commits.push(c)
            })
            .unwrap();
        assert_eq!(text, "recovered");
        assert_eq!(commits.len(), 1);
        assert_eq!(w.pipeline.recognizer.calls.len(), 5);
    }
    /// Parakeet's failure mode: nothing with the trailing silence, `bare`
    /// without it.
    struct DeafWhenPadded {
        bare: &'static str,
        calls: Vec<TrailingSilence>,
    }
    impl Recognizer for DeafWhenPadded {
        fn transcribe(&mut self, _: &[f32], trailing: TrailingSilence) -> Result<String> {
            self.calls.push(trailing);
            Ok(match trailing {
                TrailingSilence::Padded => "",
                TrailingSilence::Bare => self.bare,
            }
            .into())
        }
    }
    fn deaf_worker(bare: &'static str) -> Worker<DeafWhenPadded, Quarters> {
        Worker::new(Pipeline {
            recognizer: DeafWhenPadded {
                bare,
                calls: vec![],
            },
            segmenter: Quarters,
        })
    }
    #[test]
    fn empty_speech_is_decoded_again_without_trailing_silence() {
        use TrailingSilence::{Bare, Padded};
        let mut w = deaf_worker("words");
        let u = Utterance::new(1);
        let samples = [1.; 10];
        let mut commits = vec![];
        let preview = w
            .tick(&samples, Frames::ZERO, &u, TickKind::Preview, |c| {
                commits.push(c)
            })
            .unwrap();
        assert_eq!(
            commits
                .iter()
                .map(|c| (c.text.as_str(), c.through))
                .collect::<Vec<_>>(),
            vec![("words", Frames(4)), ("words", Frames(8))],
            "each settled chunk commits its retried text"
        );
        assert_eq!(
            preview.map(|p| p.text),
            Some(String::new()),
            "the preview is not retried"
        );
        assert_eq!(
            w.pipeline.recognizer.calls,
            [Padded, Bare, Padded, Bare, Padded]
        );
        u.release();
        let (text, tail) = w
            .finish(&samples, Frames::ZERO, &u, |c| commits.push(c))
            .unwrap();
        assert_eq!(
            (text.as_str(), tail),
            ("words", 2),
            "the release path retries"
        );
        assert_eq!(commits.len(), 3);
    }
    #[test]
    fn speech_empty_either_way_commits_nothing_and_still_advances() {
        use TrailingSilence::{Bare, Padded};
        let mut w = deaf_worker("");
        let u = Utterance::new(1);
        let mut commits = vec![];
        w.tick(&[1.; 10], Frames::ZERO, &u, TickKind::Preview, |c| {
            commits.push(c)
        })
        .unwrap();
        assert!(commits.iter().all(|c| c.text.is_empty()));
        assert_eq!(w.progress.through, Frames(8));
        let fresh = Utterance::new(2);
        let (text, _) = w
            .finish(&[1.; 8], Frames::ZERO, &fresh, |_| panic!())
            .unwrap();
        assert_eq!(text, "");
        assert_eq!(
            w.pipeline.recognizer.calls[5..],
            [Padded, Bare, Padded, Bare, Padded],
            "both chunks retried once, then the whole remainder once"
        );
    }
    #[test]
    fn a_capture_the_vad_hears_no_speech_in_is_not_decoded() {
        struct Silent;
        impl Segmenter for Silent {
            fn split(&mut self, _: &[f32]) -> Result<Split> {
                Ok(Split::default())
            }
        }
        let mut w = Worker::new(Pipeline {
            recognizer: Fake {
                calls: vec![],
                replies: vec![],
                stop: None,
            },
            segmenter: Silent,
        });
        let u = Utterance::new(1);
        assert_eq!(
            w.tick(&[0.; 10], Frames::ZERO, &u, TickKind::Preview, |_| panic!(
                "committed silence"
            ))
            .unwrap(),
            Some(Preview {
                text: String::new(),
                through: Frames::ZERO,
                heard: None,
            }),
            "an empty preview, and the committed offset stays where it was"
        );
        u.release();
        let (text, tail) = w
            .finish(&[0.; 10], Frames::ZERO, &u, |_| panic!("committed silence"))
            .unwrap();
        assert_eq!((text.as_str(), tail), ("", 10));
        assert!(
            w.pipeline.recognizer.calls.is_empty(),
            "no chunk decode and no whole-buffer retry: {:?}",
            w.pipeline.recognizer.calls
        );
    }
    /// Silence the detector heard nothing in is finished with all the same:
    /// the offset moves, no recognizer is called, and the empty commit is
    /// what lets the shell drop the audio behind it.
    #[test]
    fn settled_silence_advances_the_offset_without_a_decode() {
        struct Quiet(usize);
        impl Segmenter for Quiet {
            fn split(&mut self, s: &[f32]) -> Result<Split> {
                Ok(Split {
                    segments: vec![],
                    silent_through: s.len().saturating_sub(self.0),
                    ..Split::default()
                })
            }
        }
        let mut w = Worker::new(Pipeline {
            recognizer: Fake {
                calls: vec![],
                replies: vec![],
                stop: None,
            },
            segmenter: Quiet(4),
        });
        let u = Utterance::new(1);
        let mut commits = vec![];
        assert_eq!(
            w.tick(&[0.; 10], Frames::ZERO, &u, TickKind::Preview, |c| commits
                .push(c))
                .unwrap(),
            Some(Preview {
                text: String::new(),
                through: Frames(6),
                heard: None,
            })
        );
        assert_eq!(
            commits
                .iter()
                .map(|c| (c.text.as_str(), c.through))
                .collect::<Vec<_>>(),
            vec![("", Frames(6))],
            "an empty commit, which carries no text to the editor"
        );
        assert!(
            w.pipeline.recognizer.calls.is_empty(),
            "silence is never decoded"
        );
        // The next slice begins where the last one finished, and a keep-back
        // that has not grown commits nothing again.
        assert_eq!(
            w.tick(&[0.; 4], Frames(6), &u, TickKind::Preview, |_| panic!(
                "committed the keep-back"
            ))
            .unwrap(),
            Some(Preview {
                text: String::new(),
                through: Frames(6),
                heard: None,
            })
        );
    }

    /// A tail too long to redraw pauses the cosmetic decode only. Stopping
    /// the whole tick would stop the commits that shorten the tail, and the
    /// pause would never end.
    #[test]
    fn a_commits_only_tick_still_commits_and_leaves_the_tail_undecoded() {
        let mut w = worker();
        let u = Utterance::new(1);
        let mut commits = vec![];
        assert_eq!(
            w.tick(&[1.; 10], Frames::ZERO, &u, TickKind::Commits, |c| commits
                .push(c))
                .unwrap(),
            Some(Preview {
                text: String::new(),
                through: Frames(8),
                heard: Some(Frames(10)),
            })
        );
        assert_eq!(commits.len(), 2, "both settled chunks committed");
        assert_eq!(
            w.pipeline.recognizer.calls,
            vec![vec![1.; 4], vec![1.; 4]],
            "the open tail was not decoded"
        );
    }

    /// Speech the detector never breaks: one open chunk over the whole slice,
    /// whatever its length.
    struct Unbroken;
    impl Segmenter for Unbroken {
        fn split(&mut self, s: &[f32]) -> Result<Split> {
            Ok(Split {
                segments: vec![Segment {
                    window: 0..s.len(),
                    speech_end: s.len(),
                    settled: false,
                }],
                ..Split::default()
            })
        }
    }

    /// A tick over a full window of a longer tail that settles nothing commits
    /// the window whole. Otherwise every later tick reads the same window, the
    /// committed offset never moves, and the tail grows until the release.
    #[test]
    fn a_commits_tick_that_settles_nothing_commits_its_window_whole() {
        let mut w = Worker::new(Pipeline {
            recognizer: Fake {
                calls: vec![],
                replies: vec![],
                stop: None,
            },
            segmenter: Unbroken,
        });
        let u = Utterance::new(1);
        let mut commits = vec![];
        let tail = w
            .tick(&[1.; 10], Frames::ZERO, &u, TickKind::Commits, |c| {
                commits.push(c)
            })
            .unwrap()
            .expect("still ticking");
        assert_eq!(tail.through, Frames(10), "the window is finished with");
        assert_eq!(
            commits
                .iter()
                .map(|c| (c.text.as_str(), c.through))
                .collect::<Vec<_>>(),
            vec![("text", Frames(10))]
        );
        // The next window begins where this one ended, and is decoded once.
        w.tick(&[1.; 10], Frames(10), &u, TickKind::Commits, |c| {
            commits.push(c)
        })
        .unwrap();
        assert_eq!(commits.last().map(|c| c.through), Some(Frames(20)));
        assert_eq!(w.pipeline.recognizer.calls, vec![vec![1.; 10]; 2]);
    }

    /// A release while a window is committed whole keeps the segment whose
    /// decode it interrupted, as a settled chunk is kept: the release decodes
    /// only what follows it.
    #[test]
    fn a_release_during_a_window_committed_whole_keeps_the_decoded_segment() {
        let mut w = Worker::new(Pipeline {
            recognizer: Fake {
                calls: vec![],
                replies: vec![],
                stop: None,
            },
            segmenter: Unbroken,
        });
        let u = Utterance::new(1);
        w.pipeline.recognizer.stop = Some(u.clone());
        let mut commits = vec![];
        assert_eq!(
            w.tick(&[1.; 10], Frames::ZERO, &u, TickKind::Commits, |c| {
                commits.push(c)
            })
            .unwrap(),
            None
        );
        assert_eq!(
            commits
                .iter()
                .map(|c| (c.text.as_str(), c.through))
                .collect::<Vec<_>>(),
            vec![("text", Frames(10))]
        );
        w.finish(&[1.; 10], Frames::ZERO, &u, |c| commits.push(c))
            .unwrap();
        assert_eq!(w.pipeline.recognizer.calls.len(), 1, "decoded once");
    }

    /// A preview tick is not a full window: its tail is all there is, and it
    /// waits for the chunk to settle.
    #[test]
    fn a_preview_tick_that_settles_nothing_commits_nothing() {
        let mut w = Worker::new(Pipeline {
            recognizer: Fake {
                calls: vec![],
                replies: vec![],
                stop: None,
            },
            segmenter: Unbroken,
        });
        let u = Utterance::new(1);
        w.tick(&[1.; 10], Frames::ZERO, &u, TickKind::Preview, |_| {
            panic!("committed an open chunk")
        })
        .unwrap();
        assert_eq!(w.progress.through, Frames::ZERO);
    }

    /// Each segment of a final decode commits at its own speech end, so a
    /// decode that fails, or a daemon that stops, after the first segment
    /// leaves the rest to be decoded again rather than skipped.
    #[test]
    fn a_final_decode_commits_each_segment_at_its_own_end() {
        struct FailsSecond(usize);
        impl Recognizer for FailsSecond {
            fn transcribe(&mut self, _: &[f32], _: TrailingSilence) -> Result<String> {
                self.0 += 1;
                anyhow::ensure!(self.0 != 2, "the recognizer failed");
                Ok("text".into())
            }
        }
        let mut w = worker();
        let u = Utterance::new(1);
        w.resume(&u, Frames(100));
        let mut commits = vec![];
        w.finish(&[1.; 10], Frames(100), &u, |c| commits.push(c.through))
            .unwrap();
        assert_eq!(commits, vec![Frames(104), Frames(108), Frames(110)]);

        let mut w = Worker::new(Pipeline {
            recognizer: FailsSecond(0),
            segmenter: Quarters,
        });
        let mut commits = vec![];
        let failed = w.finish(&[1.; 10], Frames::ZERO, &Utterance::new(2), |c| {
            commits.push(c.through)
        });
        assert!(failed.is_err());
        assert_eq!(commits, vec![Frames(4)], "the text reaches the first only");
    }

    #[test]
    fn lifecycle_only_moves_forwards() {
        let u = Utterance::new(1);
        assert_eq!(u.lifecycle(), Lifecycle::Live);
        assert!(u.ticking() && !u.cancelled());
        u.release();
        assert_eq!(u.lifecycle(), Lifecycle::Released);
        assert!(!u.ticking() && !u.cancelled());
        u.cancel();
        assert_eq!(u.lifecycle(), Lifecycle::Cancelled);
        u.release();
        assert_eq!(u.lifecycle(), Lifecycle::Cancelled, "a cancel is final");
        assert!(u.cancelled() && !u.ticking());
    }
    #[test]
    fn cancel_during_decode_suppresses_result() {
        struct Cancel(Arc<Utterance>);
        impl Recognizer for Cancel {
            fn transcribe(&mut self, _: &[f32], _: TrailingSilence) -> Result<String> {
                self.0.cancel();
                Ok("late".into())
            }
        }
        let u = Utterance::new(1);
        let mut p = Pipeline {
            recognizer: Cancel(u.clone()),
            segmenter: Quarters,
        };
        assert_eq!(
            p.decode(&[1.; 8], || u.cancelled(), |_, _| panic!("late commit"))
                .unwrap(),
            ""
        );
    }
}

/// A long capture driven through the real decode path, to show that what the
/// shell has to hold does not grow with it.
///
/// The recognizer counts; the segmenter is the real `merge_spans` policy over
/// spans a stand-in detector reads off the amplitudes, on the same
/// `VAD_WINDOW` grid Silero uses. The capture buffer is modelled the way
/// `shell::audio` holds one: whole device buffers, dropped from the front
/// once the worker has committed past them.
#[cfg(test)]
mod long_capture {
    use super::*;
    use crate::{
        config::Vad,
        core::segments::{VAD_WINDOW, merge_spans, settling_silence},
    };
    use std::{cell::Cell, collections::VecDeque, ops::Range, rc::Rc};

    const RATE: u32 = 16_000;
    /// One device buffer: 20 ms, as PortAudio delivers.
    const BUFFER: usize = RATE as usize / 50;
    const INTERVAL: usize = 1_100 * RATE as usize / 1_000;
    /// `preview.max_seconds`.
    const PREVIEW_MAX: usize = 30 * RATE as usize;
    const SPEECH: f32 = 0.5;

    /// Where the slice the segmenter is about to see begins in the capture,
    /// so the windows can be recorded in capture-absolute coordinates.
    type Base = Rc<Cell<usize>>;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Window {
        range: Range<usize>,
        /// Where this chunk's speech ends: what committing it advances the
        /// offset to, in capture-absolute samples.
        speech_end: usize,
        /// Decoded for keeps: a settled chunk, or anything the release
        /// decoded. A preview is neither, and is redrawn whenever the tick
        /// cadence happens to fall.
        committed: bool,
    }

    /// Which bookkeeping a replay runs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Bookkeeping {
        /// This change: the capture buffer drops what the worker has
        /// committed, and settled silence finishes the offset.
        Dropping,
        /// The same policy, with the whole capture kept in memory. The
        /// reference for "dropping it changes nothing".
        Keeping,
        /// Before this change: a chunk stays open until a later span closes
        /// it, silence finishes nothing, and nothing is dropped.
        Before,
    }

    /// Amplitude VAD on Silero's window grid, cutting an unbroken run at
    /// `max_speech_seconds` as Silero does, then the real merge policy.
    struct Detector {
        config: Vad,
        base: Base,
        releasing: Rc<Cell<bool>>,
        windows: Rc<Cell<Vec<Window>>>,
        /// Put the pre-change closing rule back: a chunk the speech target
        /// and the later spans both left open stays open, and silence
        /// finishes nothing. The windows are untouched — the old code cut
        /// exactly the same ones, it only left this chunk unsettled.
        before: bool,
    }

    impl Segmenter for Detector {
        fn split(&mut self, samples: &[f32]) -> Result<Split> {
            let longest = (self.config.max_speech_seconds * f64::from(RATE)) as usize;
            let mut spans: Vec<Range<usize>> = vec![];
            for (i, window) in samples.as_chunks::<VAD_WINDOW>().0.iter().enumerate() {
                if !window.iter().any(|s| s.abs() > 0.1) {
                    continue;
                }
                let (start, end) = (i * VAD_WINDOW, (i + 1) * VAD_WINDOW);
                match spans.last_mut() {
                    Some(last) if last.end == start && last.end - last.start < longest => {
                        last.end = end
                    }
                    _ => spans.push(start..end),
                }
            }
            let mut split = merge_spans(&spans, samples.len(), &self.config, RATE);
            if self.before {
                split.silent_through = 0;
                if pending_before_the_change(&spans, &self.config)
                    && let Some(last) = split.segments.last_mut()
                {
                    last.settled = false;
                }
            }
            let base = self.base.get();
            let releasing = self.releasing.get();
            let mut seen = self.windows.take();
            seen.extend(split.segments.iter().map(|s| Window {
                range: base + s.window.start..base + s.window.end,
                speech_end: base + s.speech_end,
                committed: s.settled || releasing,
            }));
            self.windows.set(seen);
            Ok(split)
        }
    }

    /// Whether the rule before this change would have left the last chunk
    /// open. It closed a chunk on the speech target, or when a *later* span
    /// stood more than the split threshold away — never on the silence at the
    /// end of the slice. Written out here rather than asked of `merge_spans`,
    /// so that the reference does not move when the policy does.
    fn pending_before_the_change(spans: &[Range<usize>], config: &Vad) -> bool {
        let target = config.chunk_seconds * f64::from(RATE);
        let threshold = settling_silence(config, RATE);
        let mut open: Option<usize> = None;
        let mut previous_end: Option<usize> = None;
        for span in spans {
            if let Some(end) = previous_end
                && span.start.saturating_sub(end) >= threshold
            {
                open = None;
            }
            let speech = open.get_or_insert(0);
            *speech += span.end - span.start;
            if *speech as f64 >= target {
                open = None;
            }
            previous_end = Some(span.end);
        }
        open.is_some()
    }

    /// Counts what it is given, and records how much of it there was.
    struct Counter(Rc<Cell<Vec<usize>>>);

    impl Recognizer for Counter {
        fn transcribe(&mut self, samples: &[f32], _: TrailingSilence) -> Result<String> {
            let mut calls = self.0.take();
            calls.push(samples.len());
            self.0.set(calls);
            Ok(match samples.iter().filter(|s| s.abs() > 0.1).count() {
                0 => String::new(),
                loud => loud.to_string(),
            })
        }
    }

    /// The speech in a capture, as sample ranges. Deterministic, mixing short
    /// and long bursts with pauses on both sides of the split threshold, one
    /// stretch of unbroken speech, and one long forgotten-latch silence.
    fn schedule(seconds: usize) -> Vec<Range<usize>> {
        let len = seconds * RATE as usize;
        let mut spans: Vec<Range<usize>> = vec![];
        let mut at = RATE as usize / 2;
        let mut seed = 0x5eed_u64;
        let mut next = |modulo: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % modulo
        };
        while at < len {
            if spans.len() == 20 {
                // Unbroken speech: the chunk target is the only thing that
                // can cut it.
                let end = (at + 180 * RATE as usize).min(len);
                spans.push(at..end);
                at = end + 8 * RATE as usize;
                continue;
            }
            if spans.len() == 40 {
                // Nobody is speaking and nobody stopped the latch.
                spans.push(at..at + RATE as usize / 2);
                at += 300 * RATE as usize;
                continue;
            }
            let burst = (400 + next(5_600)) as usize * RATE as usize / 1_000;
            let pause = (200 + next(7_800)) as usize * RATE as usize / 1_000;
            let end = (at + burst).min(len);
            spans.push(at..end);
            at = end + pause;
        }
        spans
    }

    /// The audio of one sample range, without holding the capture.
    fn audio(spans: &[Range<usize>], range: Range<usize>) -> Vec<f32> {
        let mut samples = vec![0.0; range.len()];
        let mut hit = spans.partition_point(|s| s.end <= range.start);
        while let Some(span) = spans.get(hit) {
            if span.start >= range.end {
                break;
            }
            for at in span.start.max(range.start)..span.end.min(range.end) {
                samples[at - range.start] = if at % 2 == 0 { SPEECH } else { -SPEECH };
            }
            hit += 1;
        }
        samples
    }

    /// What the shell holds: whole device buffers, dropped from the front.
    struct Held {
        buffers: VecDeque<Vec<f32>>,
        start: usize,
        end: usize,
        /// Generate the slice instead of holding it: the reference run keeps
        /// the whole capture, without paying for it in the test.
        whole: Option<Vec<Range<usize>>>,
    }

    impl Held {
        fn push(&mut self, buffer: Vec<f32>) {
            self.end += buffer.len();
            if self.whole.is_none() {
                self.buffers.push_back(buffer);
            }
        }
        fn retained(&self) -> usize {
            self.end - self.start
        }
        fn discard_before(&mut self, through: usize) {
            while let Some(front) = self.buffers.front() {
                if self.start + front.len() > through {
                    break;
                }
                self.start += front.len();
                self.buffers.pop_front();
            }
        }
        fn snapshot(&self, since: usize) -> (usize, Vec<f32>) {
            if let Some(spans) = &self.whole {
                return (since, audio(spans, since..self.end));
            }
            let start = since.max(self.start);
            let mut samples = Vec::with_capacity(self.end - start);
            let mut at = self.start;
            for buffer in &self.buffers {
                if at + buffer.len() > start {
                    samples.extend_from_slice(&buffer[start.saturating_sub(at)..]);
                }
                at += buffer.len();
            }
            (start, samples)
        }
    }

    struct Run {
        windows: Vec<Window>,
        calls: Vec<usize>,
        peak_retained: usize,
        frames: usize,
    }

    /// Replays `seconds` of capture through a worker, ticking every `step`
    /// samples of audio and releasing at the end.
    fn replay(seconds: usize, step: usize, mode: Bookkeeping) -> Run {
        let spans = schedule(seconds);
        let base: Base = Rc::new(Cell::new(0));
        let releasing = Rc::new(Cell::new(false));
        let windows = Rc::new(Cell::new(vec![]));
        let calls = Rc::new(Cell::new(vec![]));
        let mut worker = Worker::new(Pipeline {
            recognizer: Counter(Rc::clone(&calls)),
            segmenter: Detector {
                config: Vad::default(),
                base: Rc::clone(&base),
                releasing: Rc::clone(&releasing),
                windows: Rc::clone(&windows),
                before: mode == Bookkeeping::Before,
            },
        });
        let mut held = Held {
            buffers: VecDeque::new(),
            start: 0,
            end: 0,
            whole: (mode != Bookkeeping::Dropping).then(|| spans.clone()),
        };
        let utterance = Utterance::new(1);
        let mut through = 0_usize;
        let mut peak_retained = 0;
        let mut next_tick = step;
        let frames = seconds * RATE as usize;
        for start in (0..frames).step_by(BUFFER) {
            held.push(audio(&spans, start..start + BUFFER));
            peak_retained = peak_retained.max(held.retained());
            if held.end < next_tick {
                continue;
            }
            next_tick += step;
            let kind = if held.end - through > PREVIEW_MAX {
                TickKind::Commits
            } else {
                TickKind::Preview
            };
            let (from, samples) = held.snapshot(through);
            base.set(through);
            worker
                .tick(&samples, Frames(from), &utterance, kind, |c| {
                    through = c.through.get()
                })
                .expect("tick");
            if mode == Bookkeeping::Dropping {
                held.discard_before(through);
            }
        }
        utterance.release();
        let (from, samples) = held.snapshot(through);
        base.set(through);
        releasing.set(true);
        worker
            .finish(&samples, Frames(from), &utterance, |c| {
                through = c.through.get()
            })
            .expect("finish");
        Run {
            windows: windows.take(),
            calls: calls.take(),
            peak_retained,
            frames,
        }
    }

    /// Half an hour of capture, and the shell never holds more than the open
    /// tail: one chunk's worth of speech with the pauses inside it, the
    /// keep-back that a later window's padding may still need, and the audio
    /// that arrived while the tick was running.
    #[test]
    fn thirty_minutes_of_capture_holds_a_bounded_tail() {
        let run = replay(1_800, INTERVAL, Bookkeeping::Dropping);
        let bound = 45 * RATE as usize;
        assert!(
            run.peak_retained <= bound,
            "held {:.1}s at once, bound {:.1}s",
            Frames(run.peak_retained).seconds(RATE),
            Frames(bound).seconds(RATE)
        );
        assert_eq!(run.frames, 1_800 * RATE as usize);
        assert!(
            run.windows.iter().filter(|w| w.committed).count() > 20,
            "the capture has to commit progressively for the bound to mean anything"
        );
    }

    /// Dropping the audio behind the committed offset, and finishing settled
    /// silence without decoding it, change no decode: the same windows, over
    /// the same capture-absolute samples, in the same order.
    #[test]
    fn dropping_committed_audio_decodes_exactly_the_same_windows() {
        for step in cadences() {
            let dropping = replay(900, step, Bookkeeping::Dropping);
            let keeping = replay(900, step, Bookkeeping::Keeping);
            assert_eq!(
                committed(&dropping),
                committed(&keeping),
                "the windows moved at a {:.1}s cadence",
                Frames(step).seconds(RATE)
            );
            assert_eq!(dropping.calls, keeping.calls, "the decodes moved");
            assert!(
                !committed(&dropping).is_empty(),
                "the comparison has to have something to compare"
            );
            assert!(
                keeping.peak_retained > 20 * dropping.peak_retained,
                "the run that keeps everything is the one that grows: {} vs {}",
                keeping.peak_retained,
                dropping.peak_retained
            );
        }
    }

    /// The three tick cadences, in samples. 6 s and 9 s are longer than the
    /// 4 s settle threshold, which is where a chunk can settle with a gap
    /// behind it — the regime a worker that has fallen behind runs in.
    fn cadences() -> [usize; 3] {
        [INTERVAL, 6 * RATE as usize, 9 * RATE as usize]
    }

    fn committed(run: &Run) -> Vec<Range<usize>> {
        run.windows
            .iter()
            .filter(|w| w.committed)
            .map(|w| w.range.clone())
            .collect()
    }

    /// No window ever reaches back into audio that was dropped, and every
    /// settled window moves the capture forwards.
    #[test]
    fn no_window_reaches_behind_the_committed_offset() {
        for mode in [Bookkeeping::Dropping, Bookkeeping::Before] {
            for step in cadences() {
                let run = replay(600, step, mode);
                // The offset a commit leaves behind is the chunk's speech
                // end, not where its window began: the window reaches further
                // back, into the silence before the speech.
                let mut offset = 0;
                let mut seen = 0;
                for window in &run.windows {
                    assert!(
                        window.range.start >= offset,
                        "window {:?} reaches back over speech committed through \
                         {offset} ({mode:?}, {:.1}s cadence)",
                        window.range,
                        Frames(step).seconds(RATE)
                    );
                    if window.committed {
                        offset = window.speech_end;
                        seen += 1;
                    }
                }
                assert!(seen > 10, "{mode:?} committed only {seen} windows");
            }
        }
    }

    /// Unbroken speech: nothing in the merge policy cuts it, because Silero
    /// itself ends a span at `vad.max_speech_seconds`. That is what settles
    /// and so what bounds the tail; `chunk_seconds` only merges shorter runs.
    #[test]
    fn continuous_speech_settles_on_the_detector_span_limit() {
        let seconds = 300;
        let unbroken = 0..seconds * RATE as usize;
        let spans = std::slice::from_ref(&unbroken);
        let base: Base = Rc::new(Cell::new(0));
        let windows = Rc::new(Cell::new(vec![]));
        let calls = Rc::new(Cell::new(vec![]));
        let config = Vad::default();
        let mut worker = Worker::new(Pipeline {
            recognizer: Counter(Rc::clone(&calls)),
            segmenter: Detector {
                config: config.clone(),
                base: Rc::clone(&base),
                releasing: Rc::new(Cell::new(false)),
                windows: Rc::clone(&windows),
                before: false,
            },
        });
        let utterance = Utterance::new(1);
        let mut through = 0_usize;
        let mut peak = 0;
        for end in (INTERVAL..seconds * RATE as usize).step_by(INTERVAL) {
            peak = peak.max(end - through);
            base.set(through);
            let samples = audio(spans, through..end);
            worker
                .tick(
                    &samples,
                    Frames(through),
                    &utterance,
                    TickKind::Preview,
                    |c| through = c.through.get(),
                )
                .expect("tick");
        }
        // One detector span, the second of audio that settles the chunk it
        // closes, and the tick interval in which both are noticed.
        let bound = ((config.max_speech_seconds + 1.0) * f64::from(RATE)) as usize + 2 * INTERVAL;
        assert!(
            peak <= bound,
            "held {:.1}s, bound {:.1}s",
            Frames(peak).seconds(RATE),
            Frames(bound).seconds(RATE)
        );
    }

    /// Amplitude VAD that, as Silero, calls a run speech only once it lasts
    /// `vad.min_speech_seconds`, then the real merge policy.
    struct MinSpeech(Vad);

    impl Segmenter for MinSpeech {
        fn split(&mut self, samples: &[f32]) -> Result<Split> {
            let shortest = (self.0.min_speech_seconds * f64::from(RATE)) as usize;
            let mut spans: Vec<Range<usize>> = vec![];
            for (i, window) in samples.as_chunks::<VAD_WINDOW>().0.iter().enumerate() {
                if !window.iter().any(|s| s.abs() > 0.1) {
                    continue;
                }
                let (start, end) = (i * VAD_WINDOW, (i + 1) * VAD_WINDOW);
                match spans.last_mut() {
                    Some(last) if last.end == start => last.end = end,
                    _ => spans.push(start..end),
                }
            }
            spans.retain(|span| span.len() >= shortest);
            Ok(merge_spans(&spans, samples.len(), &self.0, RATE))
        }
    }

    /// A window that settles nothing -- short phrases, pauses too short to
    /// settle them -- is committed through its last pause, not its end. A
    /// word that begins a tenth of a second before the window ends is too
    /// short there to be speech; committed whole, the window claimed it, and
    /// the release decoded only the rest of the word. Every loud sample is
    /// decoded exactly once.
    #[test]
    fn a_window_that_settles_nothing_is_cut_at_its_last_pause() {
        let second = RATE as usize;
        let window = 12 * second;
        let spans = [
            second..3 * second,
            4 * second..6 * second,
            7 * second..17 * second / 2,
            window - second / 10..window + second / 2,
        ];
        let samples = audio(&spans, 0..13 * second);
        let loud = samples.iter().filter(|s| s.abs() > 0.1).count();
        let calls = Rc::new(Cell::new(vec![]));
        let mut worker = Worker::new(Pipeline {
            recognizer: Counter(Rc::clone(&calls)),
            segmenter: MinSpeech(Vad::default()),
        });
        let utterance = Utterance::new(1);
        let mut commits = vec![];
        worker
            .tick(
                &samples[..window],
                Frames::ZERO,
                &utterance,
                TickKind::Commits,
                |c| commits.push(c),
            )
            .expect("tick");
        let through = commits.last().expect("the window committed").through;
        // Where the detector's windows end the third phrase.
        let pause = (17 * second / 2).next_multiple_of(VAD_WINDOW);
        assert_eq!(through, Frames(pause), "cut at the last pause");
        utterance.release();
        worker
            .finish(&samples[through.get()..], through, &utterance, |c| {
                commits.push(c)
            })
            .expect("finish");
        let decoded: usize = commits
            .iter()
            .map(|c| c.text.parse::<usize>().unwrap_or(0))
            .sum();
        assert_eq!(decoded, loud, "every loud sample, once");
    }

    /// The keep-back is what a later window's padding may still need, so it
    /// is never smaller than the widest lead padding.
    #[test]
    fn the_keep_back_covers_the_widest_lead_padding() {
        let config = Vad::default();
        let edge = (config.edge_pad_seconds * f64::from(RATE)) as usize;
        assert!(settling_silence(&config, RATE) >= edge);
    }
}
