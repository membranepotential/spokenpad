//! Only this adapter knows sherpa. Model objects stay on one inference thread.
//!
//! The span merging policy this wraps is pure and lives in
//! [`core::segments`](crate::core::segments).
use crate::{
    config::{Asr, Decoding, Model, Vad},
    core::{
        decode::{Recognizer, Segment, Segmenter, TrailingSilence},
        segments::merge_spans,
    },
};
use anyhow::{Context, Result, bail, ensure};
use sherpa_onnx::{
    OfflineModelConfig, OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig,
    OfflineTransducerModelConfig, OfflineWhisperModelConfig, SileroVadModelConfig, VadModelConfig,
    VoiceActivityDetector,
};
use std::{io::Write, ops::Range, path::Path};

pub struct Transcriber {
    recognizer: OfflineRecognizer,
    rate: u32,
    /// The most audio one native decode receives; see [`piece_len`].
    max_piece: usize,
    input: Vec<f32>,
}
fn path_string(p: &Path) -> Result<String> {
    Ok(p.to_str().context("model path is not UTF-8")?.to_owned())
}

/// Extensions of a weights file, preferred first: int8 weights are smaller
/// and faster on a CPU.
const WEIGHTS: &[&str] = &[".int8.onnx", ".onnx"];
const TEXT: &[&str] = &[".txt"];

/// The file in `names` that holds `role`: `<role><ext>`, or
/// `<prefix>-<role><ext>` as Whisper releases name them
/// (`tiny.en-encoder.onnx`), for the first of `extensions` that matches.
fn pick<'a>(names: &'a [String], role: &str, extensions: &[&str]) -> Result<Option<&'a str>> {
    for extension in extensions {
        let file = format!("{role}{extension}");
        let prefixed = format!("-{file}");
        let found: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|n| *n == file || n.ends_with(&prefixed))
            .collect();
        match found.as_slice() {
            [] => {}
            [one] => return Ok(Some(one)),
            several => bail!("several {role} files: {}", several.join(", ")),
        }
    }
    Ok(None)
}

/// Finds the files of `asr.model` in `asr.model_dir` and describes them to
/// sherpa. An error here means the model is missing or incomplete.
pub fn model_config(asr: &Asr) -> Result<OfflineModelConfig> {
    let dir = &asr.model_dir;
    let names: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read ASR model directory {}", dir.display()))?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    let hint = match asr.model {
        Model::Parakeet { .. } => " (run scripts/fetch-models.sh)",
        _ => "",
    };
    let find = |role: &str, extensions: &[&str]| -> Result<Option<String>> {
        let name = pick(&names, role, extensions)
            .with_context(|| format!("ambiguous ASR model in {}", dir.display()))?
            .with_context(|| {
                let expected: Vec<String> =
                    extensions.iter().map(|e| format!("{role}{e}")).collect();
                format!(
                    "missing ASR model: no {} in {}{hint}",
                    expected.join(" or "),
                    dir.display()
                )
            })?;
        Ok(Some(path_string(&dir.join(name))?))
    };
    let weights = |role: &str| find(role, WEIGHTS);
    let mut c = OfflineModelConfig {
        tokens: find("tokens", TEXT)?,
        num_threads: i32::from(asr.num_threads),
        provider: Some("cpu".into()),
        ..Default::default()
    };
    match &asr.model {
        Model::Parakeet { .. } => {
            c.transducer = OfflineTransducerModelConfig {
                encoder: weights("encoder")?,
                decoder: weights("decoder")?,
                joiner: weights("joiner")?,
            };
            c.model_type = Some("nemo_transducer".into());
        }
        Model::Whisper { language } => {
            c.whisper = OfflineWhisperModelConfig {
                encoder: weights("encoder")?,
                decoder: weights("decoder")?,
                language: language.clone(),
                ..Default::default()
            }
        }
        Model::SenseVoice { language } => {
            c.sense_voice = OfflineSenseVoiceModelConfig {
                model: weights("model")?,
                language: language.clone(),
                // Punctuation and written-form numbers, as dictation wants.
                use_itn: true,
            }
        }
    }
    Ok(c)
}

impl Transcriber {
    pub fn new(config: &Asr, rate: u32) -> Result<Self> {
        ensure!(
            sherpa_onnx::version().trim_start_matches('v') == "1.13.6",
            "native sherpa version {} does not match Rust bindings 1.13.6",
            sherpa_onnx::version()
        );
        let mut c = OfflineRecognizerConfig {
            model_config: model_config(config)?,
            ..Default::default()
        };
        c.decoding_method = Some(
            match &config.model {
                Model::Parakeet { decoding } => decoding.method(),
                Model::Whisper { .. } | Model::SenseVoice { .. } => "greedy_search",
            }
            .into(),
        );
        // Keep temporary files alive until native construction has read them.
        let mut temporary = vec![];
        if let Model::Parakeet {
            decoding:
                Decoding::ModifiedBeamSearch {
                    vocabulary,
                    hotwords_score,
                },
        } = &config.model
            && !vocabulary.is_empty()
        {
            let tokens = c.model_config.tokens.as_deref().context("no tokens file")?;
            let mut h = tempfile::NamedTempFile::new()?;
            writeln!(h, "{}", vocabulary.join("\n"))?;
            let mut b = tempfile::NamedTempFile::new()?;
            b.write_all(generate_bpe(&std::fs::read_to_string(tokens)?)?.as_bytes())?;
            c.hotwords_file = Some(path_string(h.path())?);
            c.hotwords_score = *hotwords_score;
            c.model_config.modeling_unit = Some("bpe".into());
            c.model_config.bpe_vocab = Some(path_string(b.path())?);
            temporary = vec![h, b];
        }
        let recognizer =
            OfflineRecognizer::create(&c).context("could not construct the CPU recognizer")?;
        drop(temporary);
        Ok(Self {
            recognizer,
            rate,
            max_piece: match config.model {
                Model::Whisper { .. } => WHISPER_PIECE_SECONDS * rate as usize,
                Model::Parakeet { .. } | Model::SenseVoice { .. } => usize::MAX,
            },
            input: Vec::with_capacity(rate as usize),
        })
    }
}
impl Transcriber {
    /// One second of silence through the model, so the first real decode does
    /// not pay for lazy native initialisation.
    pub fn warm_up(&mut self) -> Result<()> {
        self.transcribe(&vec![0.; self.rate as usize], TrailingSilence::Padded)
            .map(drop)
    }
}
impl Recognizer for Transcriber {
    /// `Padded` appends one second of zeros; `Bare` submits `samples` as is.
    fn transcribe(&mut self, samples: &[f32], trailing: TrailingSilence) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let silence = match trailing {
            TrailingSilence::Padded => self.rate as usize,
            TrailingSilence::Bare => 0,
        };
        let texts = samples
            .chunks(piece_len(samples.len(), self.max_piece))
            .map(|piece| self.decode_piece(piece, silence))
            .collect::<Result<Vec<_>>>()?;
        Ok(match texts.as_slice() {
            [one] => one.clone(),
            _ => texts.iter().map(|t| t.trim()).collect::<Vec<_>>().join(" "),
        })
    }
}
impl Transcriber {
    fn decode_piece(&mut self, samples: &[f32], silence: usize) -> Result<String> {
        prepare_padded_input(&mut self.input, samples, silence)?;
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

/// Whisper reads at most 30 s and drops the rest without an error. A piece of
/// 28 s plus the second of trailing silence stays below that.
const WHISPER_PIECE_SECONDS: usize = 28;

/// Length of the equal consecutive pieces a window of `len` samples is
/// decoded in, none longer than `max`. A VAD window has no length limit, so
/// for Whisper a long one is cut, possibly inside a word, rather than
/// truncated; every sample is still decoded once.
fn piece_len(len: usize, max: usize) -> usize {
    len.div_ceil(len.div_ceil(max).max(1)).max(1)
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
            config.model.is_file(),
            "missing VAD model {}",
            config.model.display()
        );
        let c = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(path_string(&config.model)?),
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
    fn long_windows_are_cut_into_equal_pieces_within_the_limit() {
        assert_eq!(piece_len(10, usize::MAX), 10);
        assert_eq!(piece_len(10, 10), 10);
        assert_eq!(piece_len(11, 10), 6);
        assert_eq!(piece_len(37, 28), 19);
        assert_eq!(piece_len(0, 28), 1);
    }

    #[test]
    fn model_files_are_found_by_role_prefix_and_preference() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let whisper = names(&[
            "tiny.en-encoder.onnx",
            "tiny.en-encoder.int8.onnx",
            "tiny.en-decoder.onnx",
            "tiny.en-tokens.txt",
        ]);
        assert_eq!(
            pick(&whisper, "encoder", WEIGHTS).unwrap(),
            Some("tiny.en-encoder.int8.onnx")
        );
        assert_eq!(
            pick(&whisper, "decoder", WEIGHTS).unwrap(),
            Some("tiny.en-decoder.onnx")
        );
        assert_eq!(
            pick(&whisper, "tokens", TEXT).unwrap(),
            Some("tiny.en-tokens.txt")
        );
        assert_eq!(pick(&whisper, "joiner", WEIGHTS).unwrap(), None);
        // A role name inside a longer word is not that role.
        let words = names(&["uncached_decoder.onnx", "predecoder.onnx"]);
        assert_eq!(pick(&words, "decoder", WEIGHTS).unwrap(), None);
        let two = names(&["tiny-encoder.onnx", "base-encoder.onnx"]);
        assert!(pick(&two, "encoder", WEIGHTS).is_err());
    }

    #[test]
    fn bpe_format() {
        assert_eq!(
            generate_bpe("<blk> 0\n▁hello 1\n").unwrap(),
            "<blk>\t0\n▁hello\t-1\n"
        );
    }
}
