//! TOML is validated once, before starting threads or loading native code.
use crate::core::{font::Points, geometry::Dimensions, terminal::Terminal};
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
/// Upper bound for `audio.preroll_ms`. Together with [`MAX_POSTROLL_MS`] it
/// bounds how much a recovery WAV holds beyond the capture itself, which is
/// what `shell::recorder`'s own limit has to leave room for.
pub const MAX_PREROLL_MS: u32 = 60_000;

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

#[derive(Debug, Clone, PartialEq, Deserialize)]
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
        self.frames_in(Duration::from_millis(u64::from(self.preroll_ms)))
    }
    pub fn postroll(&self) -> Duration {
        Duration::from_millis(u64::from(self.postroll_ms))
    }
    /// Whole frames in `span` at the sample rate.
    pub fn frames_in(&self, span: Duration) -> usize {
        (u128::from(self.sample_rate) * span.as_millis() / 1000) as usize
    }
}

/// When a capture nobody is ending ends by itself. The absolute limit that
/// bounds every capture is `core::state::MAX_CAPTURE`, which is not a
/// setting: it is what keeps a recovery WAV readable.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capture {
    /// Seconds a latched capture may hear no speech before it ends itself,
    /// or `0` to never end one for silence.
    pub silence_timeout_s: f64,
}
impl Default for Capture {
    fn default() -> Self {
        Self {
            silence_timeout_s: 300.,
        }
    }
}
impl Capture {
    /// The timeout as a duration, or `None` where it is off.
    pub fn silence_timeout(&self) -> Option<Duration> {
        (self.silence_timeout_s > 0.).then(|| Duration::from_secs_f64(self.silence_timeout_s))
    }
}

/// Where `spokenpad fetch-models` puts the default models:
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
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
                decoding: Decoding::GreedySearch,
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
                // Greedy unless hotwords are asked for: sherpa-onnx's beam
                // search on Parakeet TDT returns "" or "Yeah." for clear
                // speech about one time in five (k2-fsa/sherpa-onnx#3267).
                decoding: match raw.decoding.unwrap_or(
                    if raw.vocabulary.as_ref().is_some_and(|v| !v.is_empty())
                        || raw.hotwords_score.is_some()
                    {
                        SearchMethod::ModifiedBeamSearch
                    } else {
                        SearchMethod::GreedySearch
                    },
                ) {
                    SearchMethod::GreedySearch => {
                        ensure!(
                            raw.vocabulary.is_none_or(|v| v.is_empty()),
                            "asr.vocabulary requires decoding = \"modified_beam_search\""
                        );
                        ensure!(
                            raw.hotwords_score.is_none(),
                            "asr.hotwords_score requires decoding = \"modified_beam_search\": \
                             greedy_search applies no hotwords"
                        );
                        Decoding::GreedySearch
                    }
                    SearchMethod::ModifiedBeamSearch => {
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
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
/// The daemon's control socket, which `spokenpad start|stop|toggle|cancel`
/// connect to: `$XDG_RUNTIME_DIR/spokenpad.sock`, or the private state
/// directory when there is no runtime directory. Deliberately not a config
/// key, so the CLI a key binding spawns reads no file to find it.
pub fn control_socket() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(state_dir)
        .join("spokenpad.sock")
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
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

/// Who opens the dictation editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The user does, with `spokenpad editor`, in any terminal on any desktop.
    /// The daemon never opens a window, so it can never take focus.
    Attach,
    /// The daemon does, in `terminal`, on the first key-down, after proving
    /// the running i3 or sway refuses that window focus.
    Managed,
    /// The daemon does, in a window it draws itself, on the first key-down;
    /// the default. Needs no rule in the user's configuration and no
    /// terminal: the window carries the properties that make a window
    /// manager refuse it focus, and on sway the daemon adds a `no_focus` rule
    /// over IPC. X11, and Wayland through Xwayland.
    Pane,
}

/// How the pane sits among the other windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneLayout {
    /// Above the others, at the pointer (`_NET_WM_WINDOW_TYPE_UTILITY`):
    /// tiling window managers float it.
    #[default]
    Floating,
    /// An ordinary window (`_NET_WM_WINDOW_TYPE_NORMAL`): tiling window
    /// managers tile it beside the window you are typing in. Stacking window
    /// managers have no tiles, and treat it as an ordinary window at the
    /// pointer.
    Tiled,
}

/// A fontconfig family name, checked once where it enters the program.
///
/// Not a pattern: the pane appends `:bold` and `:charset=…` to it, so a value
/// carrying its own colon, comma or backslash would become fontconfig syntax
/// rather than a name. Parsing it here means the pane cannot be handed one
/// that would — the check used to live in both places and the two disagreed
/// about `-`, so `font_family = "JetBrains-Mono"` passed the configuration
/// and failed when the window opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFamily(String);

impl FontFamily {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for FontFamily {
    /// The alias every desktop defines, pointing at whatever the user has
    /// already chosen as their monospace font.
    fn default() -> Self {
        Self("monospace".to_owned())
    }
}

impl std::fmt::Display for FontFamily {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for FontFamily {
    type Error = anyhow::Error;

    fn try_from(name: String) -> Result<Self> {
        ensure!(
            !name.is_empty() && !name.contains([':', ',', '\\']),
            "nvim.font_family must be a plain fontconfig family name, \
             such as \"monospace\" or \"JetBrains Mono\", not a pattern"
        );
        Ok(Self(name))
    }
}

impl<'de> Deserialize<'de> for FontFamily {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Nvim {
    pub mode: Mode,
    /// The terminal a spawned editor runs in (managed mode); see [`Terminal`].
    pub terminal: Terminal,
    pub editor: Vec<String>,
    pub init: Option<PathBuf>,
    pub colorscheme: Option<String>,
    pub transparent: bool,
    pub window_instance: String,
    pub socket_path: PathBuf,
    pub dictation_dir: PathBuf,
    pub file_template: String,
    /// Managed mode: the terminal window's size, as a fraction of each axis
    /// of the monitor it opens on. The daemon cannot know the terminal's
    /// cell size, so it sizes that window in pixels.
    pub window_fraction: f64,
    /// Pane mode: the pane's size in cells, as Alacritty's
    /// `window.dimensions`, cut down to what fits on the monitor.
    pub pane_dimensions: Dimensions,
    /// Pane mode: floating (the default) or tiled.
    pub pane_layout: PaneLayout,
    /// The pane's font, as a fontconfig family name. `monospace` is the alias
    /// every desktop defines and most people have already pointed at the font
    /// they want; naming one here overrides it, which is worth having because
    /// what `monospace` resolves to may have no bold or italic face at all.
    pub font_family: FontFamily,
    /// The pane's font size, in points, meaning exactly what Alacritty's
    /// `font.size` means: the display's `Xft.dpi` turns it into pixels, so
    /// the same number gives the same cells in both, on any screen.
    pub font_size: Points,
    /// The X display a pane opens on, read from `$DISPLAY` when the
    /// configuration is loaded rather than looked up when a window is wanted.
    ///
    /// Not a configuration key: it is the session's, not the user's, and
    /// reading it once at the boundary is what keeps the environment out of
    /// the middle of the program. `None` means there is no display, which is
    /// an ordinary state — the daemon says so and dictation goes to the
    /// pending passage.
    #[serde(skip)]
    pub display: Option<String>,
    /// `$SWAYSOCK`, read alongside `display`. A pane that finds sway running
    /// its display adds its own `no_focus` rule over sway's IPC before it
    /// maps, since sway reads none of the properties that keep other window
    /// managers from focusing it; this is one of the two places it looks for
    /// the socket. Unset is an ordinary state.
    #[serde(skip)]
    pub sway_socket: Option<PathBuf>,
    /// `$XDG_RUNTIME_DIR`, read alongside `display`: the other place, where
    /// sway puts `sway-ipc.<uid>.<pid>.sock` for the sway whose process runs
    /// the display.
    #[serde(skip)]
    pub runtime_dir: Option<PathBuf>,
    pub startup_timeout_s: f64,
    /// Send a desktop notification when dictated text has to go to the
    /// dictation file because no editor is open.
    pub notify: bool,
    /// After every release, copy the whole dictation buffer to the `+`
    /// register through the editor's own clipboard provider. Off by
    /// default: dictation already lands in the file, and a clipboard write
    /// is a side effect worth opting into rather than assuming. When off,
    /// no copy request is ever sent.
    pub copy_to_clipboard: bool,
}
impl Default for Nvim {
    fn default() -> Self {
        Self {
            mode: Mode::Pane,
            terminal: Terminal::Alacritty,
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
            pane_dimensions: Dimensions::DEFAULT,
            pane_layout: PaneLayout::Floating,
            font_family: FontFamily::default(),
            font_size: Points::DEFAULT,
            // Filled in by `Config::load`; `Default` is what a test builds,
            // and a test says which display it means.
            display: None,
            sway_socket: None,
            runtime_dir: None,
            startup_timeout_s: 20.,
            notify: true,
            copy_to_clipboard: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
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
    pub audio: Audio,
    pub capture: Capture,
    pub recording: Recording,
    pub asr: Asr,
    pub vad: Vad,
    pub text: Text,
    pub nvim: Nvim,
    pub preview: Preview,
}
/// Where the configuration comes from, kept so that the daemon can read it
/// again: `-c/--config` if given, else the default location, and
/// `--model-dir` on top.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: Option<PathBuf>,
    pub model_dir: Option<PathBuf>,
}

impl Source {
    /// Reads the file, applies `--model-dir` and validates the result.
    pub fn load(&self) -> Result<Config> {
        let mut config = Config::load(self.path.as_deref())?;
        if let Some(dir) = &self.model_dir {
            config.asr.model_dir = expand_path(dir)?;
        }
        config.validate()?;
        Ok(config)
    }

    /// What the daemon runs on: the file, or the defaults and the reason
    /// when the file does not load. The daemon has taken presses by the time
    /// it reads the file, and must not exit over it; each new dictation
    /// window reads the file again and says why it failed.
    pub fn load_or_defaults(&self) -> (Config, Option<anyhow::Error>) {
        match self.load() {
            Ok(config) => (config, None),
            Err(error) => {
                let mut config = Config::default();
                config.nvim.read_session();
                (config, Some(error))
            }
        }
    }
}

impl Nvim {
    /// The session's X display, sway socket and runtime directory, read once, when the config
    /// is: they are environment, not file, and everything after takes them
    /// as values, so nothing has to ask the environment at the moment it
    /// wants a window.
    fn read_session(&mut self) {
        self.display = env::var("DISPLAY").ok().filter(|name| !name.is_empty());
        let path = |name: &str| {
            env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        self.sway_socket = path("SWAYSOCK");
        self.runtime_dir = path("XDG_RUNTIME_DIR");
    }
}

impl Config {
    /// The sections that differ in `fresh` and take effect only when the
    /// daemon restarts: all but `[nvim]`, which applies to the next window.
    pub fn restart_needed(&self, fresh: &Self) -> Vec<&'static str> {
        [
            ("audio", self.audio == fresh.audio),
            ("capture", self.capture == fresh.capture),
            ("recording", self.recording == fresh.recording),
            ("asr", self.asr == fresh.asr),
            ("vad", self.vad == fresh.vad),
            ("text", self.text == fresh.text),
            ("preview", self.preview == fresh.preview),
        ]
        .into_iter()
        .filter_map(|(section, same)| (!same).then_some(section))
        .collect()
    }

    /// An explicit path must exist: silently running on defaults because a
    /// `--config` typo pointed nowhere is how a user loses their settings.
    /// Only the default location may be absent.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut config = Self::read_from(path)?;
        config.nvim.read_session();
        Ok(config)
    }

    fn read_from(path: Option<&Path>) -> Result<Self> {
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
        // Removed rather than unknown: say what replaced it. Input that is
        // not TOML at all is reported by the typed parse below.
        if input
            .parse::<toml::Table>()
            .is_ok_and(|table| table.contains_key("hotkey"))
        {
            bail!(
                "[hotkey] was removed: spokenpad no longer reads the keyboard. Delete the [hotkey] table and bind keys in your window manager to `spokenpad start`, `spokenpad stop`, `spokenpad toggle` and `spokenpad cancel` (see \"Bind your keys\" in the README)"
            );
        }
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
            self.audio.sample_rate == REQUIRED_SAMPLE_RATE,
            "audio.sample_rate must be {REQUIRED_SAMPLE_RATE}: the Silero VAD window is 512 samples at 16kHz and the ASR models expect 16kHz input"
        );
        ensure!(
            self.audio.preroll_ms <= MAX_PREROLL_MS,
            "audio.preroll_ms must be <= {MAX_PREROLL_MS}"
        );
        let quiet = self.capture.silence_timeout_s;
        ensure!(
            quiet == 0. || (quiet.is_finite() && (1.0..=3600.).contains(&quiet)),
            "capture.silence_timeout_s must be 0 (off) or in [1,3600]: it is how long a latched capture may hear no speech before it ends itself"
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
        // A terminal takes the editor as trailing arguments; a first word
        // starting with `-` would be read as one of the terminal's options.
        ensure!(
            self.nvim
                .editor
                .first()
                .is_some_and(|program| !program.is_empty() && !program.starts_with('-')),
            "nvim.editor must name an executable"
        );
        // Interpolated into window-manager criteria, and the last element of
        // ghostty's GTK application id, which may not start with a digit.
        ensure!(
            self.nvim
                .window_instance
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic())
                && self
                    .nvim
                    .window_instance
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "nvim.window_instance must match [A-Za-z][A-Za-z0-9_-]*"
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
        // The silence timeout measures the time since the recognizer last
        // produced text, and only a tick produces any: the earliest a capture
        // can report speech is one tick after the press. A timeout shorter
        // than two ticks would end a capture the user is talking into, having
        // given it no chance to say so, so the two keys are validated against
        // each other rather than only against their own ranges.
        if self.preview.enabled
            && self.vad.enabled
            && let Some(timeout) = self.capture.silence_timeout()
        {
            let tick = Duration::from_millis(self.preview.interval_ms);
            ensure!(
                timeout >= tick * 2,
                "capture.silence_timeout_s ({:.3}s) must be at least twice preview.interval_ms ({}ms): the first text a capture can produce arrives one tick after the key press, so a shorter timeout would end a capture before anything had the chance to report speech",
                timeout.as_secs_f64(),
                self.preview.interval_ms
            );
        }
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

    /// `[nvim]` applies to the next window; every other section is named as
    /// needing a restart, and only when it changed.
    #[test]
    fn only_sections_other_than_nvim_need_a_restart() {
        let running = Config::default();
        let fresh = Config::parse(
            "[nvim]\nfont_size = 14.0\ncopy_to_clipboard = true\n[vad]\nthreshold = 0.6\n[asr]\nnum_threads = 2\n",
            None,
        )
        .unwrap();
        assert_eq!(running.restart_needed(&fresh), vec!["asr", "vad"]);
        assert!(running.restart_needed(&running.clone()).is_empty());
    }

    #[test]
    fn a_source_reads_its_file_again_and_applies_the_model_dir_on_top() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let source = Source {
            path: Some(path.clone()),
            model_dir: Some(directory.path().join("models")),
        };
        std::fs::write(&path, "[nvim]\ncopy_to_clipboard = false\n").unwrap();
        let first = source.load().unwrap();
        assert!(!first.nvim.copy_to_clipboard);
        assert_eq!(first.asr.model_dir, directory.path().join("models"));
        std::fs::write(&path, "[nvim]\ncopy_to_clipboard = true\n").unwrap();
        assert!(source.load().unwrap().nvim.copy_to_clipboard);
        std::fs::write(&path, "[nvim]\ncopy_to_clipboard = 3\n").unwrap();
        assert!(source.load().is_err());
        let (config, error) = source.load_or_defaults();
        assert!(!config.nvim.copy_to_clipboard, "the defaults");
        assert!(
            format!("{:#}", error.expect("and why")).contains("copy_to_clipboard"),
            "the error names the key"
        );
    }

    #[test]
    fn defaults_and_example() {
        Config::default().validate().unwrap();
        Config::parse(
            include_str!("../config.example.toml"),
            Some(Path::new("/tmp")),
        )
        .unwrap();
    }
    /// `0` is the one value outside the range, and it means off: the type
    /// the daemon reads says so rather than a sentinel number travelling on.
    #[test]
    fn the_silence_timeout_is_a_duration_or_nothing() {
        let timeout = |toml: &str| {
            Config::parse(toml, None)
                .expect("valid")
                .capture
                .silence_timeout()
        };
        assert_eq!(timeout(""), Some(Duration::from_secs(300)));
        assert_eq!(
            timeout("[capture]\nsilence_timeout_s=3600"),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(timeout("[capture]\nsilence_timeout_s=0"), None);
        // Exactly two ticks is allowed; the rejected side is in
        // `reject_invalid_boundaries`.
        assert_eq!(
            timeout("[capture]\nsilence_timeout_s=1\n[preview]\ninterval_ms=500"),
            Some(Duration::from_secs(1))
        );
        // A tick that can never land is only a contradiction while the tick
        // exists: with the progressive decode off, the silence rule is off
        // too and the pair says nothing about each other.
        for off in [
            "[capture]\nsilence_timeout_s=300\n[preview]\ninterval_ms=600000\nenabled=false",
            "[capture]\nsilence_timeout_s=300\n[preview]\ninterval_ms=600000\n[vad]\nenabled=false",
        ] {
            assert_eq!(timeout(off), Some(Duration::from_secs(300)), "{off}");
        }
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
            "[nvim]\npane_dimensions = { columns = 0, lines = 20 }",
            "[nvim]\npane_dimensions = { columns = 72 }",
            "[nvim]\npane_dimensions = { columns = 72, lines = 20, rows = 3 }",
            "[nvim]\nfile_template='../x'",
            "[nvim]\nfile_template='%Q'",
            "[nvim]\neditor=[]",
            "[nvim]\neditor=['--headless']",
            "[nvim]\nwindow_instance='x\" ]'",
            "[nvim]\nwindow_instance='1st'",
            "[nvim]\nwindow_instance=''",
            "[nvim]\nterminal='xterm'",
            "[nvim]\nmode='spawn'",
            "[nvim]\nmode=true",
            "[nvim]\nterminal=['alacritty', '-e']",
            "[recording]\nmax_total_bytes=0",
            "[capture]\nsilence_timeout_s=0.5",
            // Shorter than two preview ticks: it would end a capture the
            // user is talking into, before any tick could say so.
            "[capture]\nsilence_timeout_s=300\n[preview]\ninterval_ms=600000",
            "[capture]\nsilence_timeout_s=2\n[preview]\ninterval_ms=5000",
            "[capture]\nsilence_timeout_s=1\n[preview]\ninterval_ms=501",
            "[capture]\nsilence_timeout_s=3601",
            "[capture]\nsilence_timeout_s=-1",
            "[capture]\nsilence_timeout_s=nan",
            "[capture]\nsilence_timeou_s=300",
            "[asr]\ndecoding='typo'",
            "[asr]\ndecoding='greedy_search'\nvocabulary=['rust']",
            "[asr]\ndecoding='greedy_search'\nhotwords_score=3.0",
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
    fn a_hotkey_table_says_what_replaced_it() {
        for old in ["[hotkey]\nkey_code = 186", "[hotkey]", "hotkey = {}"] {
            let error = format!("{:#}", Config::parse(old, None).unwrap_err());
            assert!(
                error.contains("[hotkey] was removed") && error.contains("spokenpad start"),
                "{error}"
            );
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
        // Greedy by default: beam search is taken only for hotwords.
        assert_eq!(
            Config::parse("", None).unwrap().asr.model,
            Model::Parakeet {
                decoding: Decoding::GreedySearch
            }
        );
        assert_eq!(
            Config::parse("[asr]\nvocabulary=[]", None)
                .unwrap()
                .asr
                .model,
            Model::Parakeet {
                decoding: Decoding::GreedySearch
            }
        );
        assert_eq!(
            Config::parse("[asr]\nhotwords_score=2.0", None)
                .unwrap()
                .asr
                .model,
            Model::Parakeet {
                decoding: Decoding::ModifiedBeamSearch {
                    vocabulary: vec![],
                    hotwords_score: 2.0
                }
            }
        );
        let c = Config::parse("[asr]\ndecoding='greedy_search'", None).unwrap();
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
    fn the_editor_mode_is_explicit_and_pane_by_default() {
        assert_eq!(Config::default().nvim.mode, Mode::Pane);
        let managed = Config::parse("[nvim]\nmode = 'managed'\nterminal = 'foot'", None).unwrap();
        assert_eq!(managed.nvim.mode, Mode::Managed);
        assert_eq!(managed.nvim.terminal, Terminal::Foot);
        let pane = Config::parse("[nvim]\nmode = 'pane'\nfont_size = 13.5", None).unwrap();
        assert_eq!(pane.nvim.mode, Mode::Pane);
        assert_eq!(pane.nvim.font_size.get(), 13.5);
        assert_eq!(pane.nvim.pane_dimensions, Dimensions::DEFAULT);
        assert_eq!(pane.nvim.pane_layout, PaneLayout::Floating);
        let tiled = Config::parse("[nvim]\npane_layout = 'tiled'", None).unwrap();
        assert_eq!(tiled.nvim.pane_layout, PaneLayout::Tiled);
        assert!(Config::parse("[nvim]\npane_layout = 'tabbed'", None).is_err());
        let sized = Config::parse(
            "[nvim]\nmode = 'pane'\npane_dimensions = { columns = 100, lines = 30 }",
            None,
        )
        .unwrap();
        assert_eq!(
            sized.nvim.pane_dimensions,
            Dimensions::new(100, 30).unwrap()
        );
        // A key another mode reads is harmless here, and the other way round,
        // so switching modes needs no other edit. That is deliberate: unlike
        // `[asr]`, where a hotword under a family that cannot use it would
        // silently do nothing the user expects, a leftover `terminal` or
        // `font_size` changes nothing at all.
        let attach = Config::parse(
            "[nvim]\nmode = 'attach'\nterminal = 'kitty'\nfont_size = 22.0",
            None,
        )
        .unwrap();
        assert_eq!(attach.nvim.mode, Mode::Attach);
        assert_eq!(attach.nvim.terminal, Terminal::Kitty);
    }

    #[test]
    fn the_panes_font_is_a_family_name_and_a_point_size() {
        // Alacritty's default, so an unconfigured pane matches an
        // unconfigured Alacritty.
        assert_eq!(Config::default().nvim.font_size.get(), 11.25);
        // A whole number is a size too, as it is in Alacritty's `font.size`.
        let whole = Config::parse("[nvim]\nfont_size = 12", None).unwrap();
        assert_eq!(whole.nvim.font_size.get(), 12.0);
        for bad in [
            "font_family = ''",
            "font_family = 'monospace:bold'",
            "font_family = 'mono,serif'",
            "font_size = 0.0",
            "font_size = 0.5",
            "font_size = 1e6",
            "font_size = 250",
            "font_size = nan",
        ] {
            assert!(
                Config::parse(&format!("[nvim]\nmode = 'pane'\n{bad}"), None).is_err(),
                "{bad} should be refused"
            );
        }
        // A hyphen is an ordinary character in a family name, and used to be
        // refused by the pane after the configuration had accepted it.
        for good in ["JetBrains Mono", "JetBrains-Mono", "Noto Sans Mono CJK JP"] {
            let config = Config::parse(&format!("[nvim]\nfont_family = '{good}'"), None)
                .unwrap_or_else(|error| panic!("{good:?} should be accepted: {error:#}"));
            assert_eq!(config.nvim.font_family.as_str(), good);
        }
    }
    #[test]
    fn unset_environment_rejected() {
        assert!(expand_path(Path::new("$SPOKENPAD_UNSET_TEST_VARIABLE/foo")).is_err());
    }
}
