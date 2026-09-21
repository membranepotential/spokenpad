//! Turning VAD spans into decode windows: merge, padding, settlement.
//!
//! Pure policy shared by the live daemon and the offline tools; the Silero
//! detector that produces the raw spans is
//! [`shell::inference`](crate::shell::inference).
use crate::{
    config::Vad,
    core::decode::{Segment, Split},
};
use std::ops::Range;

/// Silero's analysis window at the project's 16 kHz. The detector consumes a
/// slice in windows of this size from the slice's first sample, so a slice
/// that starts a whole number of windows later covers the audio they share
/// with exactly the same windows.
pub const VAD_WINDOW: usize = 512;

/// A pause at least this long is a safe decode boundary: it closes the pending
/// chunk, and no later window reaches back across it. Both sides of such a
/// split become context edges, so the threshold is twice the widest padding,
/// which keeps the two windows from overlapping, and never below one second.
pub fn settling_silence(config: &Vad, rate: u32) -> usize {
    (rate as usize).max(
        (2.0 * config.edge_pad_seconds.max(config.pad_seconds) * f64::from(rate))
            .ceil()
            .min(usize::MAX as f64) as usize,
    )
}

/// How much of a slice is finished when nothing but silence follows
/// `speech_end`: all of it but the last `keep` samples, rounded down to the
/// detector's window grid so that dropping the prefix leaves the rest covered
/// by the same windows. Zero while the silence is too short to be a decode
/// boundary, because a later window may still reach back into it.
fn settled_prefix(speech_end: usize, len: usize, keep: usize) -> usize {
    if len.saturating_sub(speech_end) < keep {
        return 0;
    }
    let through = (len - keep) / VAD_WINDOW * VAD_WINDOW;
    if through > speech_end { through } else { 0 }
}

/// Identical merge/padding/settlement policy to the reference implementation.
///
/// No spans means no segments: with a VAD model loaded, a capture it hears no
/// speech in is not decoded at all. Returning the whole buffer instead is what
/// let Parakeet hallucinate "Thank you." into the file from a 0.5s empty press
/// (docs/decisions.md, 2026-09-11).
pub fn merge_spans(spans: &[Range<usize>], len: usize, config: &Vad, rate: u32) -> Split {
    let split_silence = settling_silence(config, rate);
    if spans.is_empty() {
        return Split {
            segments: vec![],
            silent_through: settled_prefix(0, len, split_silence),
        };
    }
    struct Chunk {
        start: usize,
        end: usize,
        speech: usize,
        closed: bool,
        edge_lead: bool,
        edge_trail: bool,
    }
    let mut chunks = vec![];
    let mut pending: Option<Chunk> = None;
    let target = config.chunk_seconds * f64::from(rate);
    // A pause of `settling_silence` closes the pending chunk before the
    // speech target is reached. This targets long dead-air gaps that can make
    // the transducer discard later speech, while preserving normal sentence
    // pauses for contextual decoding.
    for span in spans {
        let prior_end = pending
            .as_ref()
            .map(|chunk| chunk.end)
            .or_else(|| chunks.last().map(|chunk: &Chunk| chunk.end));
        let edge_lead =
            prior_end.is_some_and(|end| span.start.saturating_sub(end) >= split_silence);
        if edge_lead {
            if pending.is_some() {
                let mut chunk = pending.take().expect("pending chunk");
                chunk.closed = true;
                chunks.push(chunk);
            }
            chunks.last_mut().expect("prior chunk").edge_trail = true;
        }
        let c = pending.get_or_insert(Chunk {
            start: span.start,
            end: span.start,
            speech: 0,
            closed: false,
            edge_lead,
            edge_trail: false,
        });
        c.end = span.end;
        c.speech += span.end - span.start;
        if c.speech as f64 >= target {
            c.closed = true;
            chunks.push(pending.take().expect("pending chunk"));
        }
    }
    if let Some(mut c) = pending {
        // Trailing silence closes the pending chunk on the same rule a later
        // span would apply to it. Waiting for that span is what used to hold
        // a chunk open for as long as the speaker stayed quiet, and the
        // window is the one the release would have decoded either way.
        c.closed |= len.saturating_sub(c.end) >= split_silence;
        chunks.push(c);
    }
    let pad = (config.pad_seconds * f64::from(rate)) as usize;
    let edge = (config.edge_pad_seconds * f64::from(rate)) as usize;
    let last = chunks.len() - 1;
    let speech_end = chunks[last].end;
    let segments = chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let wide = c.speech >= 3 * rate as usize;
            let lead = if wide && (i == 0 || c.edge_lead) {
                edge
            } else {
                pad
            };
            let trail = if wide && (i == last || c.edge_trail) {
                edge
            } else {
                pad
            };
            Segment {
                window: c.start.saturating_sub(lead)..len.min(c.end + trail),
                speech_end: c.end,
                settled: c.closed && (i < last || len.saturating_sub(c.end) >= rate as usize),
            }
        })
        .collect();
    Split {
        segments,
        silent_through: settled_prefix(speech_end, len, split_silence),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decode windows alone, which is what most of these cases are about.
    fn windows(spans: &[Range<usize>], len: usize, config: &Vad, rate: u32) -> Vec<Segment> {
        merge_spans(spans, len, config, rate).segments
    }

    #[test]
    fn settlement_and_edge_padding() {
        let c = Vad::default();
        let s = windows(&[200..1200, 1500..1800], 1900, &c, 100);
        assert_eq!(
            s[0],
            Segment {
                window: 0..1250,
                speech_end: 1200,
                settled: true
            }
        );
        assert_eq!(
            s[1],
            Segment {
                window: 1450..1900,
                speech_end: 1800,
                settled: false
            }
        );
        assert!(!windows(std::slice::from_ref(&(200..1200)), 1299, &c, 100)[0].settled);
        assert!(windows(std::slice::from_ref(&(200..1200)), 1300, &c, 100)[0].settled);
        assert_eq!(
            windows(std::slice::from_ref(&(500..550)), 1000, &c, 100)[0].window,
            450..600
        );
    }

    #[test]
    fn no_speech_yields_no_segments_so_silence_is_never_decoded() {
        assert!(windows(&[], 16_000, &Vad::default(), 16_000).is_empty());
        assert!(windows(&[], 0, &Vad::default(), 16_000).is_empty());
    }

    #[test]
    fn long_silence_splits_short_speech_without_padding_overlap() {
        let c = Vad::default();
        let split = windows(&[100..300, 700..800], 900, &c, 100);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].window, 50..350);
        assert_eq!(split[0].speech_end, 300);
        assert!(split[0].settled);
        assert_eq!(split[1].window, 650..850);
        assert!(!split[1].settled);

        let merged = windows(&[100..300, 699..800], 900, &c, 100);
        assert_eq!(merged.len(), 1, "ordinary pauses still aggregate");
        assert_eq!(merged[0].window, 0..900);

        let mut wide_pad = c.clone();
        wide_pad.pad_seconds = 3.0;
        assert_eq!(
            windows(&[100..300, 700..800], 900, &wide_pad, 100).len(),
            1,
            "custom inner padding raises the safe split threshold"
        );

        let wide = windows(&[100..500, 900..1300], 1400, &c, 100);
        assert_eq!(wide[0].window, 0..700);
        assert_eq!(wide[1].window, 700..1400);

        let target_then_gap = windows(&[100..1100, 1500..1600], 1700, &c, 100);
        assert_eq!(target_then_gap[0].window, 0..1300);
        assert_eq!(target_then_gap[1].window, 1450..1650);
        let target_then_short_gap = windows(&[100..1100, 1499..1600], 1700, &c, 100);
        assert_eq!(target_then_short_gap[0].window, 0..1150);
        assert_eq!(target_then_short_gap[1].window, 1449..1650);
    }

    /// The pause that a later span would treat as a decode boundary is one
    /// while it is still the end of the slice. Without this a chunk under the
    /// speech target stayed open for as long as the speaker stayed quiet, and
    /// with it the audio behind it.
    #[test]
    fn trailing_silence_closes_a_chunk_on_the_same_rule_a_later_span_would() {
        let c = Vad::default();
        let rate = 16_000;
        let keep = settling_silence(&c, rate);
        assert_eq!(keep, 4 * rate as usize, "twice the 2s edge padding");
        let speech = rate as usize..2 * rate as usize;

        let open = &windows(
            std::slice::from_ref(&speech),
            2 * rate as usize + keep - 1,
            &c,
            rate,
        )[0];
        assert!(!open.settled, "a pause under the threshold leaves it open");

        let len = 2 * rate as usize + keep;
        let closed = &windows(std::slice::from_ref(&speech), len, &c, rate)[0];
        assert!(closed.settled);
        // The window a release would have decoded, had the speaker never
        // spoken again: same audio, committed at the tick instead.
        let at_release = &windows(std::slice::from_ref(&speech), len * 4, &c, rate)[0];
        assert_eq!(closed.window, at_release.window);
        assert_eq!(closed.speech_end, at_release.speech_end);
        // And the same window a later span would have produced by closing it.
        let later = &windows(
            &[speech.clone(), 3 * len..3 * len + rate as usize],
            len * 4,
            &c,
            rate,
        )[0];
        assert_eq!(closed.window, later.window);
    }

    #[test]
    fn settled_silence_keeps_one_split_threshold_and_sits_on_the_window_grid() {
        let c = Vad::default();
        let rate = 16_000;
        let keep = settling_silence(&c, rate);

        // Nothing but silence: everything but the keep-back is finished.
        let quiet = merge_spans(&[], 10 * rate as usize + 7, &c, rate);
        assert!(quiet.segments.is_empty());
        assert_eq!(
            quiet.silent_through,
            (10 * rate as usize + 7 - keep) / VAD_WINDOW * VAD_WINDOW
        );
        assert_eq!(quiet.silent_through % VAD_WINDOW, 0);
        assert_eq!(
            merge_spans(&[], keep, &c, rate).silent_through,
            0,
            "a pause under the threshold is not finished with"
        );

        // Speech, then a long pause: the silence after the chunk settles too.
        let speech = rate as usize..2 * rate as usize;
        let len = 10 * rate as usize;
        let after = merge_spans(std::slice::from_ref(&speech), len, &c, rate);
        assert!(after.segments[0].settled);
        assert_eq!(after.silent_through, (len - keep) / VAD_WINDOW * VAD_WINDOW);
        assert!(
            after.silent_through >= after.segments[0].speech_end,
            "settled silence never moves back over decoded speech"
        );
        assert!(
            after.silent_through + keep <= len,
            "one split threshold of silence stays, so a later window still has its padding"
        );
        assert_eq!(
            merge_spans(
                std::slice::from_ref(&speech),
                2 * rate as usize + keep,
                &c,
                rate
            )
            .silent_through,
            0,
            "the keep-back alone leaves nothing to finish"
        );
    }
}
