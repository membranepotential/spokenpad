//! Only this adapter knows sherpa. Model objects stay on one inference thread.
//!
//! The span merging policy this wraps is pure and lives in
//! [`core::segments`](crate::core::segments).
use crate::{
    config::{Asr, Vad},
    core::{
        decode::{Recognizer, Segment, Segmenter},
        segments::merge_spans,
    },
};
use anyhow::{Context, Result, ensure};
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig, SileroVadModelConfig,
    VadModelConfig, VoiceActivityDetector,
};
use std::{io::Write, ops::Range, path::Path};

pub struct Transcriber {
    recognizer: OfflineRecognizer,
    rate: u32,
    input: Vec<f32>,
}
fn path_string(p: &Path) -> Result<String> {
    Ok(p.to_str().context("model path is not UTF-8")?.to_owned())
}
impl Transcriber {
    pub fn new(config: &Asr, rate: u32) -> Result<Self> {
        ensure!(
            sherpa_onnx::version().trim_start_matches('v') == "1.13.6",
            "native sherpa version {} does not match Rust bindings 1.13.6",
            sherpa_onnx::version()
        );
        config.check_files()?;
        let [encoder, decoder, joiner, tokens] = config.model_files();
        let mut c = OfflineRecognizerConfig::default();
        c.model_config.transducer = OfflineTransducerModelConfig {
            encoder: Some(path_string(&encoder)?),
            decoder: Some(path_string(&decoder)?),
            joiner: Some(path_string(&joiner)?),
        };
        c.model_config.tokens = Some(path_string(&tokens)?);
        c.model_config.num_threads = i32::from(config.num_threads);
        c.model_config.provider = Some("cpu".into());
        c.model_config.model_type = Some("nemo_transducer".into());
        c.decoding_method = Some(config.decoding.as_str().into());
        // Keep temporary files alive until native construction has read them.
        let mut hotwords = None;
        let mut vocab = None;
        if !config.vocabulary.is_empty() {
            let mut h = tempfile::NamedTempFile::new()?;
            writeln!(h, "{}", config.vocabulary.join("\n"))?;
            c.hotwords_file = Some(path_string(h.path())?);
            c.hotwords_score = config.hotwords_score;
            c.model_config.modeling_unit = Some("bpe".into());
            let mut b = tempfile::NamedTempFile::new()?;
            b.write_all(generate_bpe(&std::fs::read_to_string(&tokens)?)?.as_bytes())?;
            c.model_config.bpe_vocab = Some(path_string(b.path())?);
            hotwords = Some(h);
            vocab = Some(b);
        }
        let recognizer =
            OfflineRecognizer::create(&c).context("could not construct the CPU recognizer")?;
        drop((hotwords, vocab));
        Ok(Self {
            recognizer,
            rate,
            input: Vec::with_capacity(rate as usize),
        })
    }
}
impl Transcriber {
    /// One second of silence through the model, so the first real decode does
    /// not pay for lazy native initialisation.
    pub fn warm_up(&mut self) -> Result<()> {
        self.transcribe(&vec![0.; self.rate as usize]).map(drop)
    }
}
impl Recognizer for Transcriber {
    fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        prepare_padded_input(&mut self.input, samples, self.rate as usize)?;
        let stream = self.recognizer.create_stream();
        // One call is essential: sherpa's offline AcceptWaveform finalizes
        // feature extraction on every call despite the wrapper saying append.
        stream.accept_waveform(self.rate as i32, &self.input);
        self.recognizer.decode(&stream);
        Ok(stream
            .get_result()
            .context("recognizer returned no result")?
            .text)
    }
}

fn padded_length(samples: usize, silence: usize, maximum: usize) -> Result<usize> {
    let total = samples
        .checked_add(silence)
        .context("padded audio length overflow")?;
    ensure!(
        total <= maximum,
        "audio exceeds the native interface's sample limit"
    );
    Ok(total)
}

fn prepare_padded_input<'a>(
    buffer: &'a mut Vec<f32>,
    samples: &[f32],
    silence: usize,
) -> Result<&'a [f32]> {
    let total = padded_length(samples.len(), silence, i32::MAX as usize)?;
    buffer.clear();
    buffer.extend_from_slice(samples);
    buffer.resize(total, 0.0);
    Ok(buffer)
}
pub fn generate_bpe(tokens: &str) -> Result<String> {
    let mut output = String::new();
    for line in tokens.lines().filter(|s| !s.is_empty()) {
        let (piece, id) = line
            .rsplit_once(char::is_whitespace)
            .context("invalid tokens.txt line")?;
        let id: i64 = id.parse().context("invalid token index")?;
        use std::fmt::Write;
        writeln!(
            output,
            "{}\t{}",
            piece.trim_end(),
            id.checked_neg().context("token index overflow")?
        )?;
    }
    Ok(output)
}

pub struct SpeechSegmenter {
    detector: VoiceActivityDetector,
    config: Vad,
    rate: u32,
}
impl SpeechSegmenter {
    pub fn new(config: &Vad, rate: u32) -> Result<Self> {
        ensure!(
            config.model().is_file(),
            "missing VAD model {}",
            config.model().display()
        );
        let c = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(path_string(config.model())?),
                threshold: config.threshold as f32,
                min_silence_duration: config.min_silence_seconds as f32,
                min_speech_duration: config.min_speech_seconds as f32,
                max_speech_duration: config.max_speech_seconds as f32,
                window_size: 512,
            },
            sample_rate: rate as i32,
            num_threads: 1,
            provider: Some("cpu".into()),
            ..Default::default()
        };
        Ok(Self {
            detector: VoiceActivityDetector::create(&c, (config.max_speech_seconds * 2.) as f32)
                .context("could not load Silero")?,
            config: config.clone(),
            rate,
        })
    }
    fn drain(&self, spans: &mut Vec<Range<usize>>, len: usize) -> Result<()> {
        while let Some(segment) = self.detector.front() {
            let start = usize::try_from(segment.start()).context("negative VAD offset")?;
            let end = start + usize::try_from(segment.n()).context("negative VAD length")?;
            ensure!(
                start <= end && end <= len,
                "VAD returned an out-of-bounds span {start}..{end}/{len}"
            );
            spans.push(start..end);
            self.detector.pop();
        }
        Ok(())
    }
}
impl Segmenter for SpeechSegmenter {
    fn split(&mut self, samples: &[f32]) -> Result<Vec<Segment>> {
        ensure!(
            samples.len() <= i32::MAX as usize,
            "capture exceeds the VAD offset limit"
        );
        self.detector.reset();
        let mut spans = vec![];
        for chunk in samples.chunks_exact(512) {
            self.detector.accept_waveform(chunk);
            self.drain(&mut spans, samples.len())?;
        }
        self.detector.flush();
        self.drain(&mut spans, samples.len())?;
        Ok(merge_spans(&spans, samples.len(), &self.config, self.rate))
    }
}
pub fn load_segmenter(config: &Vad, rate: u32) -> Option<SpeechSegmenter> {
    if !config.enabled {
        log::info!("VAD disabled; decoding whole captures");
        return None;
    }
    match SpeechSegmenter::new(config, rate) {
        Ok(s) => Some(s),
        Err(e) => {
            log::warn!("VAD unavailable: {e:#}; decoding whole captures");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_padding_preserves_input_and_respects_native_limit() {
        let samples = vec![0.25, -0.5];
        let original = samples.clone();
        let mut buffer = vec![9.0];
        assert_eq!(
            prepare_padded_input(&mut buffer, &samples, 3).unwrap(),
            &[0.25, -0.5, 0.0, 0.0, 0.0]
        );
        assert_eq!(samples, original);
        assert!(padded_length(8, 3, 10).is_err());
        assert!(padded_length(usize::MAX, 1, usize::MAX).is_err());
    }

    #[test]
    fn bpe_format() {
        assert_eq!(
            generate_bpe("<blk> 0\n▁hello 1\n").unwrap(),
            "<blk>\t0\n▁hello\t-1\n"
        );
    }
}
