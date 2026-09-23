//! The default models spokenpad can fetch for itself, Parakeet TDT 0.6B v3
//! int8 and the Silero VAD, each one typed value: where it goes under the
//! models directory, and the URL, size and sha256 of each of its files. The
//! configuration's defaults and every download list are derived from these
//! two values and named nowhere else. Downloading and reading files is
//! [`shell::models`](crate::shell::models); this module only describes what
//! is expected and compares it against what a download (or a directory
//! already on disk) produced -- no I/O, so it is usable from either side.
use crate::config::{Asr, Vad, models_dir};
use std::path::PathBuf;

/// One pinned file of a default model: its name there, and what it must be.
#[derive(Debug, Clone, Copy)]
pub struct PinnedFile {
    /// The file's name in its model's place under the models directory.
    pub name: &'static str,
    /// What follows its model's base URL to name it.
    url_suffix: &'static str,
    pub size: u64,
    /// Lowercase hex sha256.
    pub sha256: &'static str,
}

/// The default speech model: a directory of pinned files under the models
/// directory, which is what `asr.model_dir` names.
#[derive(Debug)]
pub struct DefaultAsr {
    /// The directory's name under the models directory.
    pub dir_name: &'static str,
    /// The URL's fixed base, one per release.
    base_url: &'static str,
    /// What loading the model reads.
    pub files: &'static [PinnedFile],
    /// A recording published with the model, which only the real-model
    /// end-to-end test reads. `spokenpad fetch-models` downloads it; loading
    /// the model never needs it.
    pub sample: PinnedFile,
}

impl DefaultAsr {
    /// Where the model goes: `asr.model_dir` when the configuration leaves
    /// it out.
    pub fn dir(&self) -> PathBuf {
        models_dir().join(self.dir_name)
    }

    fn download(&self, file: &PinnedFile) -> ModelFile {
        ModelFile::pinned(PathBuf::from(self.dir_name), self.base_url, file)
    }
}

/// The default voice activity detector: one pinned file in the models
/// directory, which is what `vad.model` names.
#[derive(Debug)]
pub struct DefaultVad {
    /// The URL's fixed base.
    base_url: &'static str,
    pub file: PinnedFile,
}

impl DefaultVad {
    /// Where the model goes: `vad.model` when the configuration leaves it
    /// out.
    pub fn path(&self) -> PathBuf {
        models_dir().join(self.file.name)
    }

    fn download(&self) -> ModelFile {
        ModelFile::pinned(PathBuf::new(), self.base_url, &self.file)
    }
}

// The pinned Hugging Face revision and sherpa-onnx release these files were
// downloaded from; see `scripts/fetch-models.sh`'s git history (before it
// was deleted) for how they were chosen.

/// Parakeet TDT 0.6B v3, int8, as sherpa-onnx publishes it.
pub static DEFAULT_ASR: DefaultAsr = DefaultAsr {
    dir_name: "parakeet-tdt-0.6b-v3-int8",
    base_url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78",
    files: &[
        PinnedFile {
            name: "encoder.int8.onnx",
            url_suffix: "/encoder.int8.onnx",
            size: 652_184_281,
            sha256: "acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247",
        },
        PinnedFile {
            name: "decoder.int8.onnx",
            url_suffix: "/decoder.int8.onnx",
            size: 11_845_275,
            sha256: "179e50c43d1a9de79c8a24149a2f9bac6eb5981823f2a2ed88d655b24248db4e",
        },
        PinnedFile {
            name: "joiner.int8.onnx",
            url_suffix: "/joiner.int8.onnx",
            size: 6_355_277,
            sha256: "3164c13fc2821009440d20fcb5fdc78bff28b4db2f8d0f0b329101719c0948b3",
        },
        PinnedFile {
            name: "tokens.txt",
            url_suffix: "/tokens.txt",
            size: 93_939,
            sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
        },
    ],
    sample: PinnedFile {
        name: "test_en.wav",
        url_suffix: "/test_wavs/en.wav",
        size: 184_608,
        sha256: "148b936b43ce7c546a866e64da059f0458aee2d65e617f16e9d94f06e8d99ed6",
    },
};

/// The Silero VAD, from sherpa-onnx's model release.
pub static DEFAULT_VAD: DefaultVad = DefaultVad {
    base_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models",
    file: PinnedFile {
        name: "silero_vad.onnx",
        url_suffix: "/silero_vad.onnx",
        size: 643_854,
        sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
    },
};

/// One file to download: where it goes under the models directory, where it
/// comes from, and what it must be.
#[derive(Debug, Clone)]
pub struct ModelFile {
    /// Path under the models directory (`models_dir()`).
    pub relative_path: PathBuf,
    pub url: String,
    pub size: u64,
    /// Lowercase hex sha256.
    pub sha256: &'static str,
}

impl ModelFile {
    fn pinned(dir: PathBuf, base_url: &str, file: &PinnedFile) -> Self {
        Self {
            relative_path: dir.join(file.name),
            url: format!("{base_url}{}", file.url_suffix),
            size: file.size,
            sha256: file.sha256,
        }
    }
}

/// Every file of both default models, the speech model's sample recording
/// included: what `spokenpad fetch-models` downloads.
pub fn every_default_file() -> Vec<ModelFile> {
    DEFAULT_ASR
        .files
        .iter()
        .chain([&DEFAULT_ASR.sample])
        .map(|file| DEFAULT_ASR.download(file))
        .chain([DEFAULT_VAD.download()])
        .collect()
}

/// True if `asr` is configured to use the default speech model spokenpad
/// can download for itself: the default directory.
pub fn asr_uses_default_model(asr: &Asr) -> bool {
    asr.model_dir == DEFAULT_ASR.dir()
}

/// True if `vad` is configured to use the default detector.
pub fn vad_uses_default_model(vad: &Vad) -> bool {
    vad.model == DEFAULT_VAD.path()
}

/// The default files this configuration loads. A user-configured
/// `asr.model_dir` or `vad.model` never triggers a download: only the
/// default speech model (if `asr.model_dir` is the default) and the
/// default detector (if `vad.model` is), without the sample recording.
pub fn files_to_ensure(asr: &Asr, vad: &Vad) -> Vec<ModelFile> {
    let asr_files: &[PinnedFile] = if asr_uses_default_model(asr) {
        DEFAULT_ASR.files
    } else {
        &[]
    };
    let asr_files = asr_files.iter().map(|file| DEFAULT_ASR.download(file));
    let vad_file = vad_uses_default_model(vad).then(|| DEFAULT_VAD.download());
    asr_files.chain(vad_file).collect()
}

/// Pure comparison: does `size`/`sha256` (lowercase hex, as
/// [`shell::models`](crate::shell::models) computes it) match what `file`
/// pins?
pub fn matches(file: &ModelFile, size: u64, sha256_hex: &str) -> bool {
    size == file.size && sha256_hex.eq_ignore_ascii_case(file.sha256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn every_pinned_sha256_is_a_lowercase_hex_digest() {
        let files = every_default_file();
        assert_eq!(files.len(), DEFAULT_ASR.files.len() + 2);
        for f in files {
            let path = f.relative_path.display();
            assert_eq!(f.sha256.len(), 64, "{path}");
            assert!(
                f.sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "{path}"
            );
            assert!(f.url.starts_with("https://"), "{path}");
        }
    }

    /// The download list puts each file where the configuration's default
    /// looks for it.
    #[test]
    fn the_downloads_land_where_the_defaults_point() {
        let files = files_to_ensure(&Asr::default(), &Vad::default());
        let root = models_dir();
        for name in ["encoder.int8.onnx", "tokens.txt"] {
            assert!(
                files
                    .iter()
                    .any(|f| root.join(&f.relative_path) == Asr::default().model_dir.join(name)),
                "{name}"
            );
        }
        assert!(
            files
                .iter()
                .any(|f| root.join(&f.relative_path) == Vad::default().model)
        );
        assert_eq!(
            DEFAULT_VAD.download().relative_path,
            Path::new("silero_vad.onnx")
        );
    }

    #[test]
    fn files_to_ensure_is_empty_for_a_custom_model() {
        let in_asr = |f: &ModelFile| f.relative_path.starts_with(DEFAULT_ASR.dir_name);
        let asr = Asr {
            model_dir: "/somewhere/else".into(),
            ..Asr::default()
        };
        let files = files_to_ensure(&asr, &Vad::default());
        assert!(!files.iter().any(in_asr));
        assert_eq!(files.len(), 1, "the detector's file");

        let vad = Vad {
            model: "/somewhere/else.onnx".into(),
            ..Vad::default()
        };
        let files = files_to_ensure(&Asr::default(), &vad);
        assert!(files.iter().all(in_asr));
        assert_eq!(files.len(), DEFAULT_ASR.files.len());
    }

    #[test]
    fn files_to_ensure_covers_both_defaults_without_the_sample() {
        let files = files_to_ensure(&Asr::default(), &Vad::default());
        assert_eq!(files.len(), DEFAULT_ASR.files.len() + 1);
        assert!(
            !files
                .iter()
                .any(|f| f.relative_path.ends_with(DEFAULT_ASR.sample.name))
        );
    }
}
