//! TOML is validated once, before starting threads or loading native code.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

/// Both models are 16kHz: Silero's window is 512 samples at that rate and
/// every supported ASR family's features assume it. Nothing resamples in
/// between.
pub const REQUIRED_SAMPLE_RATE: u32 = 16_000;
/// Upper bound for `audio.postroll_ms`: every release blocks the event loop
/// this long at most, and a key pressed meanwhile ends it early.
pub const MAX_POSTROLL_MS: u32 = 1_000;

/// How a transducer searches. Hotwords exist only inside
/// `ModifiedBeamSearch`, the one sherpa-onnx search that applies them.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoding {
    GreedySearch,
    ModifiedBeamSearch {
        /// Phrases to bias toward; empty means no biasing.
        vocabulary: Vec<String>,
        /// Bias added per token of a matching phrase.
        hotwords_score: f32,
    },
}
impl Decoding {
    pub fn method(&self) -> &'static str {
        match self {
            Self::GreedySearch => "greedy_search",
            Self::ModifiedBeamSearch { .. } => "modified_beam_search",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modifier {
    Shift,
    Ctrl,
    Alt,
    Super,
    None,
}
impl Modifier {
    pub fn codes(self) -> &'static [u16] {
        match self {
            Self::Shift => &[42, 54],
            Self::Ctrl => &[29, 97],
            Self::Alt => &[56, 100],
            Self::Super => &[125, 126],
            Self::None => &[],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hotkey {
    pub key_code: u16,
    pub cancel_key_code: Option<u16>,
    pub latch_modifier: Modifier,
}
impl Default for Hotkey {
    fn default() -> Self {
        Self {
            key_code: 186,
            cancel_key_code: Some(1),
            latch_modifier: Modifier::Shift,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    pub sample_rate: u32,
    pub preroll_ms: u32,
    /// Audio still captured after a release, because speech is often still
    /// sounding when the key comes up.
    pub postroll_ms: u32,
    pub device: Option<String>,
}
impl Default for Audio {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            preroll_ms: 250,
            postroll_ms: 250,
            device: None,
        }
    }
}
impl Audio {
    pub fn preroll_frames(&self) -> usize {
        self.frames_in(self.preroll_ms)
    }
    pub fn postroll_frames(&self) -> usize {
        self.frames_in(self.postroll_ms)
    }
    pub fn postroll(&self) -> Duration {
        Duration::from_millis(u64::from(self.postroll_ms))
    }
    fn frames_in(&self, milliseconds: u32) -> usize {
        (u64::from(self.sample_rate) * u64::from(milliseconds) / 1000) as usize
    }
}

/// Where `scripts/fetch-models.sh` puts the default models:
/// `$XDG_DATA_HOME/spokenpad/models`, or `~/.local/share/spokenpad/models`.
pub fn models_dir() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
        .join("spokenpad/models")
}

/// The ASR model, one variant per model family sherpa-onnx loads offline.
#[derive(Debug, Clone, PartialEq)]
pub enum Model {
    /// A NeMo transducer such as Parakeet TDT, the default. The only family
    /// with hotwords.
    Parakeet { decoding: Decoding },
    /// OpenAI Whisper. `None` detects the language.
    Whisper { language: Option<String> },
    /// SenseVoice. `None` detects the language.
    SenseVoice { language: Option<String> },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "RawAsr")]
pub struct Asr {
    /// A configured relative path is resolved against the config file's own
    /// directory.
    pub model_dir: PathBuf,
    pub num_threads: u16,
    pub model: Model,
}
impl Default for Asr {
    fn default() -> Self {
        Self {
            model_dir: models_dir().join("parakeet-tdt-0.6b-v3-int8"),
            num_threads: 6,
            model: Model::Parakeet {
                decoding: Decoding::ModifiedBeamSearch {
                    vocabulary: vec![],
                    hotwords_score: 1.5,
                },
            },
        }
    }
}

/// `[asr]` as written: one flat table, whose valid keys depend on `family`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAsr {
    #[serde(default)]
    family: Family,
    model_dir: Option<PathBuf>,
    num_threads: Option<u16>,
    decoding: Option<SearchMethod>,
    hotwords_score: Option<f32>,
    vocabulary: Option<Vec<String>>,
    language: Option<String>,
}
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Family {
    #[default]
    Parakeet,
    Whisper,
    SenseVoice,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchMethod {
    GreedySearch,
    ModifiedBeamSearch,
}

impl TryFrom<RawAsr> for Asr {
    type Error = anyhow::Error;
    fn try_from(raw: RawAsr) -> Result<Self> {
        let default = Self::default();
        let family = raw.family;
        let name = match family {
            Family::Parakeet => "parakeet",
            Family::Whisper => "whisper",
            Family::SenseVoice => "sense_voice",
        };
        if family != Family::Parakeet {
            ensure!(
                raw.vocabulary.is_none() && raw.hotwords_score.is_none(),
                "asr.vocabulary and asr.hotwords_score need family = \"parakeet\": \
                 sherpa-onnx applies hotwords only in a transducer's modified_beam_search"
            );
            ensure!(
                raw.decoding.is_none(),
                "asr.decoding applies only to family = \"parakeet\""
            );
        }
        ensure!(
            raw.language.is_none() || family != Family::Parakeet,
            "asr.language does not apply to family = \"{name}\""
        );
        if let Some(language) = &raw.language {
            ensure!(
                !language.is_empty()
                    && language
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "asr.language must be a language code such as \"en\"; leave it out to detect"
            );
        }
        let model = match family {
            Family::Parakeet => Model::Parakeet {
                decoding: match raw.decoding {
                    Some(SearchMethod::GreedySearch) => {
                        ensure!(
                            raw.vocabulary.is_none_or(|v| v.is_empty()),
                            "asr.vocabulary requires decoding = \"modified_beam_search\""
                        );
                        Decoding::GreedySearch
                    }
                    Some(SearchMethod::ModifiedBeamSearch) | None => {
                        let vocabulary = raw.vocabulary.unwrap_or_default();
                        let hotwords_score = raw.hotwords_score.unwrap_or(1.5);
                        ensure!(
                            hotwords_score.is_finite(),
                            "asr.hotwords_score must be finite"
                        );
                        for word in &vocabulary {
                            ensure!(
                                !word.trim().is_empty() && !word.contains(['\n', '\r', '\0']),
                                "invalid vocabulary phrase"
                            );
                        }
                        Decoding::ModifiedBeamSearch {
                            vocabulary,
                            hotwords_score,
                        }
                    }
                },
            },
            Family::Whisper => Model::Whisper {
                language: raw.language,
            },
            Family::SenseVoice => Model::SenseVoice {
                language: raw.language,
            },
        };
        let model_dir = match raw.model_dir {
            Some(dir) => dir,
            None if family == Family::Parakeet => default.model_dir,
            None => bail!("asr.model_dir is required for family = \"{name}\""),
        };
        let num_threads = raw.num_threads.unwrap_or(default.num_threads);
        ensure!(num_threads > 0, "asr.num_threads must be positive");
        Ok(Self {
            model_dir,
            num_threads,
            model,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Vad {
    pub enabled: bool,
    /// Resolved like `asr.model_dir`.
    pub model: PathBuf,
    pub threshold: f64,
    pub min_silence_seconds: f64,
    pub min_speech_seconds: f64,
    pub max_speech_seconds: f64,
    pub chunk_seconds: f64,
    pub pad_seconds: f64,
    pub edge_pad_seconds: f64,
}
impl Default for Vad {
    fn default() -> Self {
        Self {
            enabled: true,
            model: models_dir().join("silero_vad.onnx"),
            threshold: 0.5,
            min_silence_seconds: 0.35,
            min_speech_seconds: 0.15,
            max_speech_seconds: 20.,
            chunk_seconds: 10.,
            pad_seconds: 0.5,
            edge_pad_seconds: 2.,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Text {
    pub strip_fillers: bool,
    pub fillers: Vec<String>,
    pub replacements: indexmap::IndexMap<String, String>,
    pub trailing_space: bool,
}
impl Default for Text {
    fn default() -> Self {
        Self {
            strip_fillers: true,
            fillers: ["uh", "um", "erm", "hmm"].map(str::to_owned).into(),
            replacements: indexmap::IndexMap::new(),
            trailing_space: false,
        }
    }
}

pub fn state_home() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
}
fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
pub fn state_dir() -> PathBuf {
    state_home().join("spokenpad")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Recording {
    pub enabled: bool,
    pub dir: PathBuf,
    pub max_total_bytes: u64,
}
impl Default for Recording {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: state_dir().join("audio"),
            max_total_bytes: 5 * 1024_u64.pow(3),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Nvim {
    pub terminal: Vec<String>,
    pub editor: Vec<String>,
    pub init: Option<PathBuf>,
    pub colorscheme: Option<String>,
    pub transparent: bool,
    pub window_instance: String,
    pub socket_path: PathBuf,
    pub dictation_dir: PathBuf,
    pub file_template: String,
    pub window_fraction: f64,
    pub startup_timeout_s: f64,
}
impl Default for Nvim {
    fn default() -> Self {
        Self {
            terminal: [
                "alacritty",
                "--class",
                "Floating,{instance}",
                "-o",
                "window.position.x={x}",
                "-o",
                "window.position.y={y}",
                "-e",
            ]
            .map(str::to_owned)
            .into(),
            editor: vec!["nvim".into()],
            init: None,
            colorscheme: None,
            transparent: true,
            window_instance: "spokenpad".into(),
            socket_path: env::var_os("XDG_RUNTIME_DIR")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(state_home)
                .join("spokenpad-nvim.sock"),
            dictation_dir: state_dir().join("dictation"),
            file_template: "dictation-%Y-%m-%d-%H%M%S.md".into(),
            window_fraction: 0.33,
            startup_timeout_s: 20.,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preview {
    pub enabled: bool,
    pub interval_ms: u64,
    pub max_seconds: f64,
}
impl Default for Preview {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 1100,
            max_seconds: 30.,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub hotkey: Hotkey,
    pub audio: Audio,
    pub recording: Recording,
    pub asr: Asr,
    pub vad: Vad,
    pub text: Text,
    pub nvim: Nvim,
    pub preview: Preview,
}
impl Config {
    /// An explicit path must exist: silently running on defaults because a
    /// `--config` typo pointed nowhere is how a user loses their settings.
    /// Only the default location may be absent.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(p) = path else {
            let default = env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".config"))
                .join("spokenpad/config.toml");
            if !default.exists() {
                let c = Self::default();
                c.validate()?;
                return Ok(c);
            }
            return Self::read(&default);
        };
        ensure!(p.exists(), "configuration file {} not found", p.display());
        Self::read(p)
    }
    fn read(path: &Path) -> Result<Self> {
        Self::parse(
            &std::fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?,
            path.parent(),
        )
    }
    pub fn parse(input: &str, base: Option<&Path>) -> Result<Self> {
        let mut c: Self = toml::from_str(input).context("invalid configuration")?;
        // A relative model path is relative to the config file. The defaults
        // are absolute, so joining leaves them alone.
        for p in [&mut c.asr.model_dir, &mut c.vad.model] {
            *p = expand_path(p)?;
            if p.is_relative()
                && let Some(b) = base
            {
                *p = b.join(&*p);
            }
        }
        for p in [
            &mut c.nvim.socket_path,
            &mut c.nvim.dictation_dir,
            &mut c.recording.dir,
        ] {
            *p = expand_path(p)?;
        }
        if let Some(p) = &mut c.nvim.init {
            *p = expand_path(p)?;
        }
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..0x300).contains(&self.hotkey.key_code),
            "hotkey.key_code out of range"
        );
        if let Some(k) = self.hotkey.cancel_key_code {
            ensure!(
                (1..0x300).contains(&k) && k != self.hotkey.key_code,
                "invalid or conflicting cancel key"
            );
        }
        ensure!(
            self.audio.sample_rate == REQUIRED_SAMPLE_RATE,
            "audio.sample_rate must be {REQUIRED_SAMPLE_RATE}: the Silero VAD window is 512 samples at 16kHz and the ASR models expect 16kHz input"
        );
        ensure!(
            self.audio.preroll_ms <= 60_000,
            "audio.preroll_ms must be <= 60000"
        );
        ensure!(
            self.audio.postroll_ms <= MAX_POSTROLL_MS,
            "audio.postroll_ms must be <= {MAX_POSTROLL_MS}: the release waits this long before decoding"
        );
        ensure!(
            self.vad.threshold.is_finite() && self.vad.threshold > 0. && self.vad.threshold < 1.,
            "vad.threshold must be in (0,1)"
        );
        for (name, v) in [
            ("min_silence_seconds", self.vad.min_silence_seconds),
            ("min_speech_seconds", self.vad.min_speech_seconds),
            ("max_speech_seconds", self.vad.max_speech_seconds),
        ] {
            ensure!(
                v.is_finite() && v > 0. && v <= 3600.,
                "vad.{name} must be in (0,3600]"
            );
        }
        ensure!(
            self.vad.chunk_seconds.is_finite()
                && self.vad.chunk_seconds > 0.
                && self.vad.chunk_seconds <= 3600.,
            "vad.chunk_seconds must be in (0,3600]"
        );
        ensure!(
            self.vad.max_speech_seconds > self.vad.min_speech_seconds,
            "vad.max_speech_seconds must exceed min_speech_seconds"
        );
        for v in [self.vad.pad_seconds, self.vad.edge_pad_seconds] {
            ensure!(
                v.is_finite() && (0.0..=3600.).contains(&v),
                "invalid VAD padding"
            );
        }
        ensure!(
            self.recording.max_total_bytes > 0,
            "recording.max_total_bytes must be positive"
        );
        ensure!(
            !self.nvim.editor.is_empty() && !self.nvim.editor[0].is_empty(),
            "nvim.editor must name an executable"
        );
        ensure!(
            !self.nvim.window_instance.is_empty()
                && self
                    .nvim
                    .window_instance
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "invalid nvim.window_instance"
        );
        ensure!(
            self.nvim.window_fraction.is_finite()
                && self.nvim.window_fraction > 0.
                && self.nvim.window_fraction <= 1.,
            "nvim.window_fraction must be in (0,1]"
        );
        ensure!(
            self.nvim.startup_timeout_s.is_finite()
                && self.nvim.startup_timeout_s > 0.
                && self.nvim.startup_timeout_s <= 3600.,
            "invalid nvim.startup_timeout_s"
        );
        let items: Vec<_> = chrono::format::StrftimeItems::new(&self.nvim.file_template).collect();
        ensure!(
            !items.contains(&chrono::format::Item::Error),
            "invalid nvim.file_template date format"
        );
        let rendered = chrono::Local::now()
            .format_with_items(items.iter())
            .to_string();
        ensure!(
            !rendered.is_empty()
                && !rendered.contains(['/', '\0'])
                && rendered != "."
                && rendered != "..",
            "nvim.file_template must be a single filename"
        );
        ensure!(
            self.preview.interval_ms >= 200 && self.preview.interval_ms <= 3_600_000,
            "preview.interval_ms must be in [200,3600000]"
        );
        ensure!(
            self.preview.max_seconds.is_finite()
                && self.preview.max_seconds > 0.
                && self.preview.max_seconds <= 3600.,
            "preview.max_seconds must be in (0,3600]"
        );
        if let Some(name) = &self.nvim.colorscheme {
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
                "nvim.colorscheme must match [A-Za-z0-9_.-]+"
            );
        }
        for s in self
            .text
            .fillers
            .iter()
            .chain(self.text.replacements.keys())
        {
            ensure!(!s.is_empty(), "empty filler/replacement key");
        }
        // The native wrapper builds CStrings; reject interior NULs at this boundary.
        for p in [
            &self.asr.model_dir,
            &self.vad.model,
            &self.nvim.socket_path,
            &self.nvim.dictation_dir,
            &self.recording.dir,
        ] {
            ensure!(
                !p.as_os_str().as_encoded_bytes().contains(&0),
                "NUL in path"
            );
        }
        Ok(())
    }
}

pub fn expand_path(path: &Path) -> Result<PathBuf> {
    let raw = path.to_str().context("path must be UTF-8")?;
    let expanded = if raw == "~" {
        home().to_string_lossy().into_owned()
    } else if let Some(tail) = raw.strip_prefix("~/") {
        home().join(tail).to_string_lossy().into_owned()
    } else {
        raw.to_owned()
    };
    let re = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)")?;
    let mut failure = None;
    let value = re.replace_all(&expanded, |caps: &regex::Captures<'_>| {
        let name = caps
            .get(1)
            .or_else(|| caps.get(2))
            .expect("capture")
            .as_str();
        match env::var(name) {
            Ok(v) => v,
            Err(_) => {
                failure = Some(name.to_owned());
                String::new()
            }
        }
    });
    if let Some(name) = failure {
        bail!("unset environment variable {name}");
    }
    ensure!(
        !value.contains('$'),
        "unresolved environment variable in path"
    );
    Ok(PathBuf::from(value.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_example() {
        Config::default().validate().unwrap();
        Config::parse(
            include_str!("../config.example.toml"),
            Some(Path::new("/tmp")),
        )
        .unwrap();
    }
    #[test]
    fn reject_invalid_boundaries() {
        for bad in [
            "[wat]",
            "[audio]\nprerol_ms=2",
            "[audio]\nsample_rate=0",
            "[audio]\nsample_rate=44100",
            "[vad]\nchunk_seconds=0",
            "[preview]\nmax_seconds=3601",
            "[nvim]\ncolorscheme='ha ha; !'",
            "[nvim]\ncolorscheme=''",
            "[audio]\npreroll_ms=-1",
            "[audio]\npostroll_ms=-1",
            "[audio]\npostroll_ms=1001",
            "[preview]\ninterval_ms=199",
            "[preview]\nmax_seconds=nan",
            "[vad]\nthreshold=nan",
            "[nvim]\nwindow_fraction=inf",
            "[nvim]\nfile_template='../x'",
            "[nvim]\nfile_template='%Q'",
            "[nvim]\neditor=[]",
            "[nvim]\nwindow_instance='x\" ]'",
            "[recording]\nmax_total_bytes=0",
            "[asr]\ndecoding='typo'",
            "[asr]\ndecoding='greedy_search'\nvocabulary=['rust']",
            "[asr]\nvocabulary=['']",
            "[asr]\nhotwords_score=nan",
            "[asr]\nnum_threads=0",
            "[asr]\nlanguage='en'",
            "[asr]\nfamily='moonshine'\nmodel_dir='m'",
            "[asr]\nfamily='whisper'",
            "[asr]\nfamily='whisper'\nmodel_dir='w'\ndecoding='greedy_search'",
            "[asr]\nfamily='sense_voice'\nmodel_dir='s'\nhotwords_score=1.5",
            "[asr]\nfamily='whisper'\nmodel_dir='w'\nlanguage='e n'",
            "[text]\nfillers=['']",
        ] {
            assert!(Config::parse(bad, None).is_err(), "accepted {bad}");
        }
    }
    #[test]
    fn a_missing_explicit_config_is_an_error() {
        // Only the default location may be absent; a --config typo must not
        // silently run the daemon on built-in defaults.
        let temporary = tempfile::tempdir().unwrap();
        let absent = temporary.path().join("absent.toml");
        let error = Config::load(Some(&absent)).unwrap_err().to_string();
        assert!(error.contains("not found"), "{error}");

        let c = Config::default();
        c.validate().unwrap();
        assert_eq!(c.audio.sample_rate, REQUIRED_SAMPLE_RATE);
        assert_eq!(c.preview.max_seconds, 30.);
    }
    #[test]
    fn the_models_only_accept_sixteen_kilohertz() {
        let error = Config::parse("[audio]\nsample_rate=48000", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Silero") && error.contains("ASR"), "{error}");
        assert_eq!(
            Config::parse("[audio]\npreroll_ms=0", None)
                .unwrap()
                .audio
                .preroll_frames(),
            0
        );
    }
    #[test]
    fn a_family_takes_only_its_own_keys() {
        let c = Config::parse(
            "[asr]\nfamily='whisper'\nmodel_dir='w'\nlanguage='de'\nnum_threads=2",
            Some(Path::new("/tmp/conf")),
        )
        .unwrap();
        assert_eq!(c.asr.model_dir, Path::new("/tmp/conf/w"));
        assert_eq!(c.asr.num_threads, 2);
        assert_eq!(
            c.asr.model,
            Model::Whisper {
                language: Some("de".into())
            }
        );
        let c = Config::parse("[asr]\nfamily='sense_voice'\nmodel_dir='s'", None).unwrap();
        assert_eq!(c.asr.model, Model::SenseVoice { language: None });
        let c = Config::parse("[asr]\ndecoding='greedy_search'\nhotwords_score=3.0", None).unwrap();
        assert_eq!(
            c.asr.model,
            Model::Parakeet {
                decoding: Decoding::GreedySearch
            }
        );
        assert_eq!(
            Config::parse("[asr]\nvocabulary=['mkdir']", None)
                .unwrap()
                .asr
                .model,
            Model::Parakeet {
                decoding: Decoding::ModifiedBeamSearch {
                    vocabulary: vec!["mkdir".into()],
                    hotwords_score: 1.5
                }
            }
        );

        let error = format!(
            "{:#}",
            Config::parse(
                "[asr]\nfamily='whisper'\nmodel_dir='w'\nvocabulary=['mkdir']",
                None
            )
            .unwrap_err()
        );
        assert!(
            error.contains("asr.vocabulary") && error.contains("family = \"parakeet\""),
            "{error}"
        );
        let error = format!(
            "{:#}",
            Config::parse("[asr]\nfamily='whisper'", None).unwrap_err()
        );
        assert!(error.contains("asr.model_dir is required"), "{error}");
    }
    #[test]
    fn explicit_model_paths_use_config_directory() {
        let c = Config::parse(
            "[asr]\nmodel_dir='weights'\n[vad]\nmodel='vad.onnx'",
            Some(Path::new("/tmp/conf")),
        )
        .unwrap();
        assert_eq!(c.asr.model_dir, Path::new("/tmp/conf/weights"));
        assert_eq!(c.vad.model, Path::new("/tmp/conf/vad.onnx"));
        // An unconfigured path keeps the default under the XDG data home,
        // whatever directory the config file happens to live in.
        let c = Config::parse("", Some(Path::new("/tmp"))).unwrap();
        assert!(models_dir().is_absolute());
        assert_eq!(
            c.asr.model_dir,
            models_dir().join("parakeet-tdt-0.6b-v3-int8")
        );
        assert_eq!(c.vad.model, models_dir().join("silero_vad.onnx"));
    }
    #[test]
    fn unset_environment_rejected() {
        assert!(expand_path(Path::new("$SPOKENPAD_UNSET_TEST_VARIABLE/foo")).is_err());
    }
}
