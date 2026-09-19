//! TOML is validated once, before starting threads or loading native code.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

/// Both models are 16kHz: Silero's window is 512 samples at that rate and
/// Parakeet's feature extractor assumes it. Nothing resamples in between.
pub const REQUIRED_SAMPLE_RATE: u32 = 16_000;
/// Upper bound for `audio.postroll_ms`: every release blocks the event loop
/// this long at most, and a key pressed meanwhile ends it early.
pub const MAX_POSTROLL_MS: u32 = 1_000;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decoding {
    GreedySearch,
    ModifiedBeamSearch,
}
impl Decoding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GreedySearch => "greedy_search",
            Self::ModifiedBeamSearch => "modified_beam_search",
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

pub const DEFAULT_MODEL_DIR: &str = "models/parakeet-tdt-0.6b-v3-int8";
pub const DEFAULT_VAD_MODEL: &str = "models/silero_vad.onnx";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Asr {
    /// `None` keeps the working-directory-relative default; a configured path
    /// is resolved against the config file's own directory.
    pub model_dir: Option<PathBuf>,
    pub num_threads: u16,
    pub decoding: Decoding,
    pub hotwords_score: f32,
    pub vocabulary: Vec<String>,
}
impl Default for Asr {
    fn default() -> Self {
        Self {
            model_dir: None,
            num_threads: 6,
            decoding: Decoding::ModifiedBeamSearch,
            hotwords_score: 1.5,
            vocabulary: vec![],
        }
    }
}
impl Asr {
    pub fn model_dir(&self) -> &Path {
        self.model_dir
            .as_deref()
            .unwrap_or(Path::new(DEFAULT_MODEL_DIR))
    }
    pub fn model_files(&self) -> [PathBuf; 4] {
        [
            "encoder.int8.onnx",
            "decoder.int8.onnx",
            "joiner.int8.onnx",
            "tokens.txt",
        ]
        .map(|s| self.model_dir().join(s))
    }
    pub fn check_files(&self) -> Result<()> {
        for p in self.model_files() {
            ensure!(
                p.is_file(),
                "missing ASR model: {} (run scripts/fetch_model.py)",
                p.display()
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Vad {
    pub enabled: bool,
    /// `None` keeps the working-directory-relative default.
    pub model: Option<PathBuf>,
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
            model: None,
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

impl Vad {
    pub fn model(&self) -> &Path {
        self.model
            .as_deref()
            .unwrap_or(Path::new(DEFAULT_VAD_MODEL))
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
        // A configured model path is relative to the config file; the built-in
        // defaults stay relative to the working directory, which is exactly
        // what `Option::None` means here.
        for p in [&mut c.asr.model_dir, &mut c.vad.model]
            .into_iter()
            .flatten()
        {
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
            "audio.sample_rate must be {REQUIRED_SAMPLE_RATE}: the Silero VAD window is 512 samples at 16kHz and Parakeet's features assume 16kHz input"
        );
        ensure!(
            self.audio.preroll_ms <= 60_000,
            "audio.preroll_ms must be <= 60000"
        );
        ensure!(
            self.audio.postroll_ms <= MAX_POSTROLL_MS,
            "audio.postroll_ms must be <= {MAX_POSTROLL_MS}: the release waits this long before decoding"
        );
        ensure!(self.asr.num_threads > 0, "asr.num_threads must be positive");
        ensure!(
            self.asr.hotwords_score.is_finite(),
            "asr.hotwords_score must be finite"
        );
        ensure!(
            self.asr.vocabulary.is_empty() || self.asr.decoding == Decoding::ModifiedBeamSearch,
            "asr.vocabulary requires modified_beam_search"
        );
        for word in &self.asr.vocabulary {
            ensure!(
                !word.trim().is_empty() && !word.contains(['\n', '\r', '\0']),
                "invalid vocabulary phrase"
            );
        }
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
            self.asr.model_dir(),
            self.vad.model(),
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
        assert!(
            error.contains("Silero") && error.contains("Parakeet"),
            "{error}"
        );
        assert_eq!(
            Config::parse("[audio]\npreroll_ms=0", None)
                .unwrap()
                .audio
                .preroll_frames(),
            0
        );
    }
    #[test]
    fn explicit_model_paths_use_config_directory() {
        let c = Config::parse(
            "[asr]\nmodel_dir='weights'\n[vad]\nmodel='vad.onnx'",
            Some(Path::new("/tmp/conf")),
        )
        .unwrap();
        assert_eq!(c.asr.model_dir(), Path::new("/tmp/conf/weights"));
        assert_eq!(c.vad.model(), Path::new("/tmp/conf/vad.onnx"));
        // An unconfigured path keeps the working-directory-relative default,
        // whatever directory the config file happens to live in.
        let c = Config::parse("", Some(Path::new("/tmp"))).unwrap();
        assert_eq!(c.asr.model_dir(), Path::new(DEFAULT_MODEL_DIR));
        assert_eq!(c.vad.model(), Path::new(DEFAULT_VAD_MODEL));
    }
    #[test]
    fn unset_environment_rejected() {
        assert!(expand_path(Path::new("$SPOKENPAD_UNSET_TEST_VARIABLE/foo")).is_err());
    }
}
