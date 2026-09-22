//! The pinned set of default model files spokenpad can fetch for itself:
//! Parakeet TDT 0.6B v3 int8 and the Silero VAD, at the URLs, sizes and
//! sha256 hashes `scripts/fetch-models.sh` used to pin. Downloading and
//! reading files is [`shell::models`](crate::shell::models); this module
//! only describes what is expected and compares it against what a download
//! (or a directory already on disk) produced -- no I/O, so it is usable from
//! either side.
use crate::config::{Asr, Vad, models_dir};
use std::path::PathBuf;

/// Which loader a file belongs to, so a configuration that does not use the
/// default of one of them is never downloaded for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Asr,
    Vad,
}

/// One file of the default model set.
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub kind: ModelKind,
    /// Whether this file is read while loading the model, as opposed to
    /// being part of the set only for `spokenpad fetch-models` /
    /// `scripts/fetch-models.sh` parity (the bundled test sample).
    pub required_for_load: bool,
    /// Path under the models directory (`models_dir()`).
    pub relative_path: &'static str,
    /// The URL's fixed base, one per release (shared by several files).
    /// `pub(crate)`, not private: `shell::models`' tests build their own
    /// `ModelFile`s pointing at a local test server.
    pub(crate) base_url: &'static str,
    /// What follows `base_url` to name this file.
    pub(crate) url_suffix: &'static str,
    pub size: u64,
    /// Lowercase hex sha256.
    pub sha256: &'static str,
}
impl ModelFile {
    pub fn url(&self) -> String {
        format!("{}{}", self.base_url, self.url_suffix)
    }
}

// The pinned Hugging Face revision and sherpa-onnx release these files were
// downloaded from; see `scripts/fetch-models.sh`'s git history (before it
// was deleted) for how they were chosen.
const PARAKEET: &str = "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78";
const SILERO: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models";

/// The default Parakeet TDT 0.6B v3 (int8) + Silero VAD set, exactly what
/// `scripts/fetch-models.sh` downloaded before this moved into the binary.
pub static DEFAULT_MODEL_FILES: &[ModelFile] = &[
    ModelFile {
        kind: ModelKind::Asr,
        required_for_load: true,
        relative_path: "parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx",
        base_url: PARAKEET,
        url_suffix: "/encoder.int8.onnx",
        size: 652_184_281,
        sha256: "acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247",
    },
    ModelFile {
        kind: ModelKind::Asr,
        required_for_load: true,
        relative_path: "parakeet-tdt-0.6b-v3-int8/decoder.int8.onnx",
        base_url: PARAKEET,
        url_suffix: "/decoder.int8.onnx",
        size: 11_845_275,
        sha256: "179e50c43d1a9de79c8a24149a2f9bac6eb5981823f2a2ed88d655b24248db4e",
    },
    ModelFile {
        kind: ModelKind::Asr,
        required_for_load: true,
        relative_path: "parakeet-tdt-0.6b-v3-int8/joiner.int8.onnx",
        base_url: PARAKEET,
        url_suffix: "/joiner.int8.onnx",
        size: 6_355_277,
        sha256: "3164c13fc2821009440d20fcb5fdc78bff28b4db2f8d0f0b329101719c0948b3",
    },
    ModelFile {
        kind: ModelKind::Asr,
        required_for_load: true,
        relative_path: "parakeet-tdt-0.6b-v3-int8/tokens.txt",
        base_url: PARAKEET,
        url_suffix: "/tokens.txt",
        size: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    },
    ModelFile {
        kind: ModelKind::Asr,
        // Only the real-model e2e test reads this; loading the model never does.
        required_for_load: false,
        relative_path: "parakeet-tdt-0.6b-v3-int8/test_en.wav",
        base_url: PARAKEET,
        url_suffix: "/test_wavs/en.wav",
        size: 184_608,
        sha256: "148b936b43ce7c546a866e64da059f0458aee2d65e617f16e9d94f06e8d99ed6",
    },
    ModelFile {
        kind: ModelKind::Vad,
        required_for_load: true,
        relative_path: "silero_vad.onnx",
        base_url: SILERO,
        url_suffix: "/silero_vad.onnx",
        size: 643_854,
        sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
    },
];

pub fn default_asr_dir() -> PathBuf {
    models_dir().join("parakeet-tdt-0.6b-v3-int8")
}
pub fn default_vad_path() -> PathBuf {
    models_dir().join("silero_vad.onnx")
}

/// True if `asr` is configured to use the default Parakeet model spokenpad
/// can download for itself -- the default family, at the default directory.
pub fn asr_uses_default_model(asr: &Asr) -> bool {
    asr.model_dir == default_asr_dir()
}

/// True if `vad` is configured to use the default Silero model.
pub fn vad_uses_default_model(vad: &Vad) -> bool {
    vad.model == default_vad_path()
}

/// The default files this configuration would load but a user-configured
/// `model_dir`/`vad.model` never triggers a download for:
/// only the default Parakeet weights (if `asr` is unmodified from default)
/// and the default Silero VAD (if `vad.model` is unmodified).
pub fn files_to_ensure(asr: &Asr, vad: &Vad) -> Vec<&'static ModelFile> {
    let asr_default = asr_uses_default_model(asr);
    let vad_default = vad_uses_default_model(vad);
    DEFAULT_MODEL_FILES
        .iter()
        .filter(|f| f.required_for_load)
        .filter(|f| match f.kind {
            ModelKind::Asr => asr_default,
            ModelKind::Vad => vad_default,
        })
        .collect()
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

    #[test]
    fn every_pinned_sha256_is_a_lowercase_hex_digest() {
        for f in DEFAULT_MODEL_FILES {
            assert_eq!(f.sha256.len(), 64, "{}", f.relative_path);
            assert!(
                f.sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "{}",
                f.relative_path
            );
            assert!(f.url().starts_with("https://"), "{}", f.relative_path);
        }
    }

    #[test]
    fn files_to_ensure_is_empty_for_a_custom_dir_or_family() {
        let asr = Asr {
            model_dir: "/somewhere/else".into(),
            ..Asr::default()
        };
        assert!(
            files_to_ensure(&asr, &Vad::default())
                .iter()
                .all(|f| f.kind != ModelKind::Asr)
        );

        let vad = Vad {
            model: "/somewhere/else.onnx".into(),
            ..Vad::default()
        };
        assert!(
            files_to_ensure(&Asr::default(), &vad)
                .iter()
                .all(|f| f.kind != ModelKind::Vad)
        );
    }

    #[test]
    fn files_to_ensure_covers_both_defaults() {
        let files = files_to_ensure(&Asr::default(), &Vad::default());
        assert!(files.iter().any(|f| f.kind == ModelKind::Asr));
        assert!(files.iter().any(|f| f.kind == ModelKind::Vad));
        assert!(files.iter().all(|f| f.required_for_load));
    }
}
