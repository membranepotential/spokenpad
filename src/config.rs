//! TOML is validated once, before starting threads or loading native code.
use crate::core::{font::Points, geometry::Dimensions};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

/// Both models are 16kHz: Silero's window is 512 samples at that rate and
/// Parakeet's features assume it. Nothing resamples in between.
pub const REQUIRED_SAMPLE_RATE: u32 = 16_000;
/// Upper bound for `audio.postroll_seconds`: every release blocks the event
/// loop this long at most, and a key pressed meanwhile ends it early.
pub const MAX_POSTROLL_SECONDS: f64 = 1.0;
/// Upper bound for `audio.preroll_seconds`. Together with
/// [`MAX_POSTROLL_SECONDS`] it bounds how much a recovery WAV holds beyond
/// the capture itself, which is what `shell::recorder`'s own limit has to
/// leave room for.
pub const MAX_PREROLL_SECONDS: f64 = 60.0;
/// The longest any other duration setting may be: an hour, far past any
/// value that makes sense, and short enough that a typo of a few zeros is
/// caught rather than taken.
pub const LONGEST_SECONDS: f64 = 3600.0;

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
    /// Audio kept from before the press, because the first word is often
    /// already sounding when the key goes down.
    pub preroll_seconds: f64,
    /// Audio still captured after a release, because speech is often still
    /// sounding when the key comes up.
    pub postroll_seconds: f64,
    pub device: Option<String>,
}
impl Default for Audio {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            preroll_seconds: 0.25,
            postroll_seconds: 0.25,
            device: None,
        }
    }
}
impl Audio {
    pub fn preroll_frames(&self) -> usize {
        self.frames_in(Duration::from_secs_f64(self.preroll_seconds))
    }
    pub fn postroll(&self) -> Duration {
        Duration::from_secs_f64(self.postroll_seconds)
    }
    /// Whole frames in `span` at the sample rate.
    pub fn frames_in(&self, span: Duration) -> usize {
        (u128::from(self.sample_rate) * span.as_micros() / 1_000_000) as usize
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
    pub silence_timeout_seconds: f64,
}
impl Default for Capture {
    fn default() -> Self {
        Self {
            silence_timeout_seconds: 300.,
        }
    }
}
impl Capture {
    /// The timeout as a duration, or `None` where it is off.
    pub fn silence_timeout(&self) -> Option<Duration> {
        (self.silence_timeout_seconds > 0.)
            .then(|| Duration::from_secs_f64(self.silence_timeout_seconds))
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

/// The speech model: a NeMo transducer such as Parakeet TDT, the default,
/// which sherpa-onnx loads offline.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "RawAsr")]
pub struct Asr {
    /// A configured relative path is resolved against the config file's own
    /// directory.
    pub model_dir: PathBuf,
    pub num_threads: u16,
    pub decoding: Decoding,
}
impl Default for Asr {
    fn default() -> Self {
        Self {
            model_dir: models_dir().join("parakeet-tdt-0.6b-v3-int8"),
            num_threads: 6,
            decoding: Decoding::GreedySearch,
        }
    }
}

/// `[asr]` as written: one flat table, whose hotword keys depend on
/// `decoding`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAsr {
    model_dir: Option<PathBuf>,
    num_threads: Option<u16>,
    decoding: Option<SearchMethod>,
    hotwords_score: Option<f32>,
    vocabulary: Option<Vec<String>>,
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
        // Greedy unless hotwords are asked for: sherpa-onnx's beam search on
        // Parakeet TDT returns "" or "Yeah." for clear speech about one time
        // in five (k2-fsa/sherpa-onnx#3267).
        let method = raw.decoding.unwrap_or(
            if raw.vocabulary.as_ref().is_some_and(|v| !v.is_empty())
                || raw.hotwords_score.is_some()
            {
                SearchMethod::ModifiedBeamSearch
            } else {
                SearchMethod::GreedySearch
            },
        );
        let decoding = match method {
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
        };
        let num_threads = raw.num_threads.unwrap_or(default.num_threads);
        ensure!(num_threads > 0, "asr.num_threads must be positive");
        Ok(Self {
            model_dir: raw.model_dir.unwrap_or(default.model_dir),
            num_threads,
            decoding,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Vad {
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
    /// Above the others, beside the pointer (`_NET_WM_WINDOW_TYPE_UTILITY`):
    /// tiling window managers float it.
    #[default]
    Floating,
    /// An ordinary window (`_NET_WM_WINDOW_TYPE_NORMAL`): tiling window
    /// managers tile it beside the window you are typing in. Stacking window
    /// managers have no tiles, and treat it as an ordinary window beside the
    /// pointer.
    Tiled,
}

impl std::fmt::Display for PaneLayout {
    /// As the configuration spells it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Floating => "floating",
            Self::Tiled => "tiled",
        })
    }
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
    pub editor: Vec<String>,
    pub init: Option<PathBuf>,
    pub colorscheme: Option<String>,
    pub transparent: bool,
    pub socket_path: PathBuf,
    pub dictation_dir: PathBuf,
    pub file_template: String,
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
    /// How long a dictation editor may take to start and answer.
    pub startup_timeout_seconds: f64,
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
            editor: vec!["nvim".into()],
            init: None,
            colorscheme: None,
            transparent: true,
            socket_path: env::var_os("XDG_RUNTIME_DIR")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(state_home)
                .join("spokenpad-nvim.sock"),
            dictation_dir: state_dir().join("dictation"),
            file_template: "dictation-%Y-%m-%d-%H%M%S.md".into(),
            pane_dimensions: Dimensions::DEFAULT,
            pane_layout: PaneLayout::Floating,
            font_family: FontFamily::default(),
            font_size: Points::DEFAULT,
            // Filled in by `Config::load`; `Default` is what a test builds,
            // and a test says which display it means.
            display: None,
            sway_socket: None,
            runtime_dir: None,
            startup_timeout_seconds: 20.,
            notify: true,
            copy_to_clipboard: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preview {
    /// The shortest time between two ticks.
    pub interval_seconds: f64,
    pub max_seconds: f64,
}
impl Default for Preview {
    fn default() -> Self {
        Self {
            interval_seconds: 1.1,
            max_seconds: 30.,
        }
    }
}
impl Preview {
    /// The shortest time between two ticks.
    pub fn interval(&self) -> Duration {
        Duration::from_secs_f64(self.interval_seconds)
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
        if let Ok(table) = input.parse::<toml::Table>()
            && let Some(removed) = removed(&table)
        {
            bail!("{removed}");
        }
        let mut c: Self = toml::from_str(input).map_err(|error| Unparsable::new(input, error))?;
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
        seconds(
            "audio.preroll_seconds",
            self.audio.preroll_seconds,
            0.0..=MAX_PREROLL_SECONDS,
        )?;
        seconds(
            "audio.postroll_seconds",
            self.audio.postroll_seconds,
            0.0..=MAX_POSTROLL_SECONDS,
        )?;
        let quiet = self.capture.silence_timeout_seconds;
        ensure!(
            quiet == 0. || (quiet.is_finite() && (1.0..=LONGEST_SECONDS).contains(&quiet)),
            "capture.silence_timeout_seconds must be 0 (off) or in [1,{LONGEST_SECONDS}]: it is how long a latched capture may hear no speech before it ends itself"
        );
        ensure!(
            self.vad.threshold.is_finite() && self.vad.threshold > 0. && self.vad.threshold < 1.,
            "vad.threshold must be in (0,1)"
        );
        for (name, value) in [
            ("vad.min_silence_seconds", self.vad.min_silence_seconds),
            ("vad.min_speech_seconds", self.vad.min_speech_seconds),
            ("vad.max_speech_seconds", self.vad.max_speech_seconds),
            ("vad.chunk_seconds", self.vad.chunk_seconds),
            ("preview.max_seconds", self.preview.max_seconds),
            (
                "nvim.startup_timeout_seconds",
                self.nvim.startup_timeout_seconds,
            ),
        ] {
            positive_seconds(name, value)?;
        }
        ensure!(
            self.vad.max_speech_seconds > self.vad.min_speech_seconds,
            "vad.max_speech_seconds must exceed min_speech_seconds"
        );
        for (name, value) in [
            ("vad.pad_seconds", self.vad.pad_seconds),
            ("vad.edge_pad_seconds", self.vad.edge_pad_seconds),
        ] {
            seconds(name, value, 0.0..=LONGEST_SECONDS)?;
        }
        ensure!(
            self.recording.max_total_bytes > 0,
            "recording.max_total_bytes must be positive"
        );
        // The editor's argv starts with the program; a first word starting
        // with `-` would be an option with no program to take it.
        ensure!(
            self.nvim
                .editor
                .first()
                .is_some_and(|program| !program.is_empty() && !program.starts_with('-')),
            "nvim.editor must name an executable"
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
        seconds(
            "preview.interval_seconds",
            self.preview.interval_seconds,
            0.2..=LONGEST_SECONDS,
        )?;
        // The silence timeout measures the time since a tick last reported
        // speech: the earliest a capture can report any is one tick after the
        // press. A timeout shorter than two ticks would end a capture the
        // user is talking into, having given it no chance to say so, so the
        // two keys are validated against each other rather than only against
        // their own ranges.
        if let Some(timeout) = self.capture.silence_timeout() {
            ensure!(
                timeout >= self.preview.interval() * 2,
                "capture.silence_timeout_seconds ({}) must be at least twice preview.interval_seconds ({}): the first speech a capture can report arrives one tick after the key press, so a shorter timeout would end a capture before anything had the chance to report speech",
                self.capture.silence_timeout_seconds,
                self.preview.interval_seconds
            );
        }
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

/// A configuration file that is not TOML, or not the settings spokenpad
/// has: what is wrong, and on which line.
#[derive(Debug)]
pub struct Unparsable {
    error: toml::de::Error,
    /// The line the error is on, counted from 1, and the key set on it.
    place: Option<(usize, Option<String>)>,
}

impl Unparsable {
    fn new(input: &str, error: toml::de::Error) -> Self {
        let place = error
            .span()
            .and_then(|span| input.get(..span.start))
            .map(|before| {
                let number = before.matches('\n').count() + 1;
                let key = input
                    .lines()
                    .nth(number - 1)
                    .and_then(|line| line.split_once('='))
                    .map(|(key, _)| key.trim().to_owned())
                    .filter(|key| !key.is_empty());
                (number, key)
            });
        Self { error, place }
    }

    /// TOML's message without the list of what it expected, and the line,
    /// with the key when the message does not name it.
    fn summary(&self) -> (String, Option<usize>) {
        let message = self.error.message();
        let clause = message
            .split(", expected")
            .next()
            .unwrap_or(message)
            .lines()
            .next()
            .unwrap_or_default()
            .trim();
        match &self.place {
            Some((line, Some(key))) if !clause.contains(key.as_str()) => {
                (format!("{key}: {clause}"), Some(*line))
            }
            Some((line, _)) => (clause.to_owned(), Some(*line)),
            None => (clause.to_owned(), None),
        }
    }
}

impl std::fmt::Display for Unparsable {
    /// The whole of TOML's own report, with the line quoted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid configuration: {}", self.error)
    }
}

impl std::error::Error for Unparsable {}

/// The most a [`summary`] says, in characters: short enough to stand beside
/// a notice's headline in a pane of the default width.
const SUMMARY_LENGTH: usize = 48;

/// Why a configuration did not load, in a few words for the dictation
/// window, which has no room for the whole report: `spokenpad check` gives
/// that. The first clause of the reason, and the line for a TOML error,
/// such as "unknown field `pane_dimension` (line 3)".
pub fn summary(error: &anyhow::Error) -> String {
    let (clause, line) = match error.downcast_ref::<Unparsable>() {
        Some(unparsable) => unparsable.summary(),
        // spokenpad's own messages: the rule, then a colon and why.
        None => (
            error
                .root_cause()
                .to_string()
                .split([':', '\n'])
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            None,
        ),
    };
    let clause = if clause.chars().count() <= SUMMARY_LENGTH {
        clause.to_owned()
    } else {
        let cut: String = clause.chars().take(SUMMARY_LENGTH).collect();
        let whole_words = cut
            .rsplit_once(' ')
            .map_or(cut.as_str(), |(words, _)| words);
        format!("{whole_words}…")
    };
    match line {
        Some(line) => format!("{clause} (line {line})"),
        None => clause,
    }
}

/// `key`'s `value` is a number of seconds within `range`.
fn seconds(key: &str, value: f64, range: std::ops::RangeInclusive<f64>) -> Result<()> {
    ensure!(
        value.is_finite() && range.contains(&value),
        "{key} must be a number of seconds in [{},{}]",
        range.start(),
        range.end()
    );
    Ok(())
}

/// `key`'s `value` is a number of seconds above zero and at most
/// [`LONGEST_SECONDS`].
fn positive_seconds(key: &str, value: f64) -> Result<()> {
    ensure!(
        value.is_finite() && value > 0. && value <= LONGEST_SECONDS,
        "{key} must be a number of seconds in (0,{LONGEST_SECONDS}]"
    );
    Ok(())
}

/// What became of a key spokenpad no longer reads.
enum Gone {
    /// Removed; the text follows "was removed" and says why and what to do
    /// instead.
    Removed(&'static str),
    /// Renamed to the key `to` in the same section, which is in seconds;
    /// the old one was in `unit`.
    Renamed { to: &'static str, unit: Unit },
}

/// The unit a renamed key was in.
#[derive(Clone, Copy)]
enum Unit {
    Milliseconds,
    Seconds,
}

impl Unit {
    /// `value`, as the new key in seconds takes it.
    fn in_seconds(self, value: f64) -> f64 {
        match self {
            Self::Milliseconds => value / 1000.,
            Self::Seconds => value,
        }
    }
}

/// Every key spokenpad no longer reads, by section. A configuration that
/// still sets one is refused with what became of it, rather than with
/// serde's bare "unknown field".
const GONE: &[(&str, &str, Gone)] = &[
    (
        "audio",
        "preroll_ms",
        Gone::Renamed {
            to: "preroll_seconds",
            unit: Unit::Milliseconds,
        },
    ),
    (
        "audio",
        "postroll_ms",
        Gone::Renamed {
            to: "postroll_seconds",
            unit: Unit::Milliseconds,
        },
    ),
    (
        "capture",
        "silence_timeout_s",
        Gone::Renamed {
            to: "silence_timeout_seconds",
            unit: Unit::Seconds,
        },
    ),
    (
        "preview",
        "interval_ms",
        Gone::Renamed {
            to: "interval_seconds",
            unit: Unit::Milliseconds,
        },
    ),
    (
        "nvim",
        "startup_timeout_s",
        Gone::Renamed {
            to: "startup_timeout_seconds",
            unit: Unit::Seconds,
        },
    ),
    (
        "asr",
        "family",
        Gone::Removed(
            ": spokenpad runs Parakeet TDT only. Delete the line, and asr.model_dir too if it points at another family's model",
        ),
    ),
    (
        "asr",
        "language",
        Gone::Removed(
            " with the other model families: Parakeet TDT recognises the language by itself. Delete the line",
        ),
    ),
    (
        "vad",
        "enabled",
        Gone::Removed(
            ": the voice activity detector is always on. Without it nothing settles, so nothing is committed before the release and nothing previews, and silence is decoded, which is where the recogniser invents words. Delete the line",
        ),
    ),
    (
        "preview",
        "enabled",
        Gone::Removed(
            ": the preview is always on; the same ticks commit settled text as you speak. Delete the line",
        ),
    ),
    ("nvim", "terminal", Gone::Removed(MANAGED_ONLY)),
    ("nvim", "window_instance", Gone::Removed(MANAGED_ONLY)),
    ("nvim", "window_fraction", Gone::Removed(MANAGED_ONLY)),
];

/// Why the keys only managed mode read are gone. It was removed on
/// 2026-09-22: the pane does what it did with no rule in the window
/// manager's configuration.
const MANAGED_ONLY: &str = " with managed mode, which the pane replaces: it sizes and places its own window and needs no rule in your window manager. Delete the line";

/// What a configuration still sets that spokenpad no longer has, and what
/// replaced it, so that the user reads that rather than serde's "unknown
/// field".
fn removed(table: &toml::Table) -> Option<String> {
    if table.contains_key("hotkey") {
        return Some(
            "[hotkey] was removed: spokenpad no longer reads the keyboard. Delete the [hotkey] table and bind keys in your window manager to `spokenpad start`, `spokenpad stop`, `spokenpad toggle` and `spokenpad cancel` (see \"Bind your keys\" in the README)".to_owned(),
        );
    }
    let section = |name: &str| table.get(name).and_then(toml::Value::as_table);
    if section("nvim")
        .and_then(|nvim| nvim.get("mode"))
        .and_then(toml::Value::as_str)
        == Some("managed")
    {
        return Some(
            "nvim.mode = \"managed\" was removed: the pane replaces it, a window spokenpad draws itself that needs no rule in your window manager. Delete the line to use the pane (the default), or set nvim.mode = \"attach\" and run `spokenpad editor` in a terminal of your own".to_owned(),
        );
    }
    GONE.iter().find_map(|(name, key, gone)| {
        let old = section(name)?.get(*key)?;
        Some(match gone {
            Gone::Removed(instead) => format!("{name}.{key} was removed{instead}"),
            Gone::Renamed { to, unit } => {
                let value = old
                    .as_float()
                    .or_else(|| old.as_integer().map(|whole| whole as f64));
                let instead = match value {
                    Some(value) => format!("write `{to} = {}`", unit.in_seconds(value)),
                    None => format!("write `{to}` in seconds"),
                };
                format!(
                    "{name}.{key} is now {name}.{to}: every duration is in seconds; {instead} under [{name}]"
                )
            }
        })
    })
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
            timeout("[capture]\nsilence_timeout_seconds=3600"),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(timeout("[capture]\nsilence_timeout_seconds=0"), None);
        // Exactly two ticks is allowed; the rejected side is in
        // `reject_invalid_boundaries`.
        assert_eq!(
            timeout("[capture]\nsilence_timeout_seconds=1\n[preview]\ninterval_seconds=0.5"),
            Some(Duration::from_secs(1))
        );
    }
    #[test]
    fn reject_invalid_boundaries() {
        for bad in [
            "[wat]",
            "[audio]\nprerol_seconds=0.1",
            "[audio]\nsample_rate=0",
            "[audio]\nsample_rate=44100",
            "[vad]\nchunk_seconds=0",
            "[preview]\nmax_seconds=3601",
            "[nvim]\ncolorscheme='ha ha; !'",
            "[nvim]\ncolorscheme=''",
            "[audio]\npreroll_seconds=-1",
            "[audio]\npostroll_seconds=-1",
            "[audio]\npostroll_seconds=1.001",
            "[preview]\ninterval_seconds=0.199",
            "[preview]\nmax_seconds=nan",
            "[vad]\nthreshold=nan",
            "[nvim]\npane_dimensions = { columns = 0, lines = 20 }",
            "[nvim]\npane_dimensions = { columns = 72 }",
            "[nvim]\npane_dimensions = { columns = 72, lines = 20, rows = 3 }",
            "[nvim]\nfile_template='../x'",
            "[nvim]\nfile_template='%Q'",
            "[nvim]\neditor=[]",
            "[nvim]\neditor=['--headless']",
            "[nvim]\nmode='spawn'",
            "[nvim]\nmode=true",
            "[recording]\nmax_total_bytes=0",
            "[capture]\nsilence_timeout_seconds=0.5",
            // Shorter than two preview ticks: it would end a capture the
            // user is talking into, before any tick could say so.
            "[capture]\nsilence_timeout_seconds=300\n[preview]\ninterval_seconds=600",
            "[capture]\nsilence_timeout_seconds=2\n[preview]\ninterval_seconds=5",
            "[capture]\nsilence_timeout_seconds=1\n[preview]\ninterval_seconds=0.501",
            "[capture]\nsilence_timeout_seconds=3601",
            "[capture]\nsilence_timeout_seconds=-1",
            "[capture]\nsilence_timeout_seconds=nan",
            "[capture]\nsilence_timeou_seconds=300",
            "[asr]\ndecoding='typo'",
            "[asr]\ndecoding='greedy_search'\nvocabulary=['rust']",
            "[asr]\ndecoding='greedy_search'\nhotwords_score=3.0",
            "[asr]\nvocabulary=['']",
            "[asr]\nhotwords_score=nan",
            "[asr]\nnum_threads=0",
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
            Config::parse("[audio]\npreroll_seconds=0", None)
                .unwrap()
                .audio
                .preroll_frames(),
            0
        );
    }
    #[test]
    fn beam_search_is_taken_only_for_hotwords() {
        let decoding = |toml: &str| Config::parse(toml, None).unwrap().asr.decoding;
        assert_eq!(decoding(""), Decoding::GreedySearch);
        assert_eq!(decoding("[asr]\nvocabulary=[]"), Decoding::GreedySearch);
        assert_eq!(
            decoding("[asr]\ndecoding='greedy_search'"),
            Decoding::GreedySearch
        );
        assert_eq!(
            decoding("[asr]\nhotwords_score=2.0"),
            Decoding::ModifiedBeamSearch {
                vocabulary: vec![],
                hotwords_score: 2.0
            }
        );
        assert_eq!(
            decoding("[asr]\nvocabulary=['mkdir']"),
            Decoding::ModifiedBeamSearch {
                vocabulary: vec!["mkdir".into()],
                hotwords_score: 1.5
            }
        );
    }

    /// The dictation window has room for a few words about a configuration
    /// that did not load; `spokenpad check` prints the rest.
    #[test]
    fn a_configuration_error_has_a_summary_short_enough_for_the_window() {
        let summarize = |toml: &str| summary(&Config::parse(toml, None).unwrap_err());
        assert_eq!(
            summarize("[nvim]\nmode = 'pane'\npane_dimension = 3\n"),
            "unknown field `pane_dimension` (line 3)"
        );
        assert_eq!(
            summarize("[audio]\nsample_rate = 44100\n"),
            "audio.sample_rate must be 16000"
        );
        assert_eq!(
            summarize("[audio]\npreroll_ms = 250\n"),
            "audio.preroll_ms is now audio.preroll_seconds"
        );
        let long = summarize("[capture]\nsilence_timeout_seconds = 0.5\n");
        assert!(long.chars().count() <= SUMMARY_LENGTH + 1, "{long}");
        assert!(
            long.starts_with("capture.silence_timeout_seconds"),
            "{long}"
        );
        assert_eq!(
            summarize("[nvim]\ncopy_to_clipboard = 3\n"),
            "copy_to_clipboard: invalid type: integer `3` (line 2)"
        );
        let full = format!(
            "{:#}",
            Config::parse("[nvim]\npane_dimension = 3\n", None).unwrap_err()
        );
        assert!(
            full.contains("pane_dimensions") && full.contains("line 2"),
            "the whole report still lists the keys and quotes the line: {full}"
        );
    }

    /// A key spokenpad no longer reads is refused with what became of it,
    /// never with serde's bare "unknown field".
    #[test]
    fn a_removed_key_says_what_became_of_it() {
        for (toml, says) in [
            ("[asr]\nfamily='whisper'", "asr.family was removed"),
            ("[asr]\nfamily='parakeet'", "Parakeet TDT only"),
            ("[asr]\nlanguage='de'", "asr.language was removed"),
            ("[vad]\nenabled=false", "vad.enabled was removed"),
            ("[preview]\nenabled=true", "preview.enabled was removed"),
            (
                "[audio]\npreroll_ms=250",
                "audio.preroll_ms is now audio.preroll_seconds: every duration is in seconds; write `preroll_seconds = 0.25` under [audio]",
            ),
            ("[audio]\npostroll_ms=1000", "`postroll_seconds = 1`"),
            (
                "[capture]\nsilence_timeout_s=120",
                "`silence_timeout_seconds = 120`",
            ),
            ("[preview]\ninterval_ms=1100", "`interval_seconds = 1.1`"),
            (
                "[nvim]\nstartup_timeout_s=20.5",
                "`startup_timeout_seconds = 20.5`",
            ),
            (
                "[nvim]\nstartup_timeout_s='x'",
                "write `startup_timeout_seconds` in seconds",
            ),
        ] {
            let error = format!("{:#}", Config::parse(toml, None).unwrap_err());
            assert!(error.contains(says), "{toml}: {error}");
        }
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
        // A key only the pane reads is harmless in attach mode, so switching
        // modes needs no other edit. That is deliberate: unlike `[asr]`,
        // where a hotword under greedy search would silently do nothing the
        // user expects, a leftover `font_size` changes nothing at all.
        let attach = Config::parse("[nvim]\nmode = 'attach'\nfont_size = 22.0", None).unwrap();
        assert_eq!(attach.nvim.mode, Mode::Attach);
    }

    /// Managed mode is gone, and so are the keys only it read. A config that
    /// still has one is refused with the key's name and what replaced it,
    /// never with serde's bare "unknown field".
    #[test]
    fn managed_mode_and_its_keys_say_the_pane_replaced_them() {
        let error = format!(
            "{:#}",
            Config::parse("[nvim]\nmode = 'managed'", None).unwrap_err()
        );
        assert!(
            error.contains("nvim.mode = \"managed\" was removed") && error.contains("pane"),
            "{error}"
        );
        for (key, line) in [
            ("terminal", "terminal = 'alacritty'"),
            ("window_instance", "window_instance = 'spokenpad'"),
            ("window_fraction", "window_fraction = 0.33"),
        ] {
            let error = format!(
                "{:#}",
                Config::parse(&format!("[nvim]\nmode = 'pane'\n{line}"), None).unwrap_err()
            );
            assert!(
                error.contains(&format!("nvim.{key} was removed with managed mode"))
                    && error.contains("pane"),
                "{error}"
            );
        }
        // The same word outside `[nvim]` is not one of managed mode's keys:
        // it stays an ordinary unknown field.
        let error = format!(
            "{:#}",
            Config::parse("terminal = 'alacritty'", None).unwrap_err()
        );
        assert!(!error.contains("managed"), "{error}");
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
