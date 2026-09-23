# Third-party software in spokenpad

spokenpad's own source is under the MIT licence ([LICENSE](LICENSE)). The
`spokenpad` binary also contains code of other projects, linked into it
statically, and it downloads two models on first use. Each keeps its own
licence. This file names them; the licence texts are those of each project,
and on Arch Linux the common ones are in `/usr/share/licenses/common/`.

## Linked into the binary

The build links sherpa-onnx's prebuilt static libraries, version 1.13.8
(`sherpa-onnx-v1.13.8-linux-x64-static-lib.tar.bz2`, pinned by sha256 in
`packaging/aur/PKGBUILD`). Which of the libraries in that archive end up in
the binary was read from the symbols of a built `spokenpad` (`nm`):

| component | licence | upstream |
|---|---|---|
| sherpa-onnx | Apache-2.0 | https://github.com/k2-fsa/sherpa-onnx |
| ONNX Runtime | MIT, Copyright (c) Microsoft Corporation | https://github.com/microsoft/onnxruntime |
| kaldi-native-fbank | Apache-2.0 | https://github.com/csukuangfj/kaldi-native-fbank |
| kaldi-decoder | Apache-2.0 | https://github.com/k2-fsa/kaldi-decoder |
| kaldifst | Apache-2.0 | https://github.com/k2-fsa/kaldifst |
| OpenFst | Apache-2.0 | https://www.openfst.org |
| SentencePiece | Apache-2.0 | https://github.com/google/sentencepiece |
| KISS FFT | BSD-3-Clause | https://github.com/mborgerding/kissfft |
| piper-phonemize | MIT | https://github.com/rhasspy/piper-phonemize |
| eSpeak NG, with ucd-tools | GPL-3.0-or-later | https://github.com/espeak-ng/espeak-ng |

eSpeak NG and piper-phonemize belong to sherpa-onnx's text-to-speech, which
spokenpad never calls; its prebuilt library links them in all the same. A
binary that contains GPL-3.0-or-later code is distributed under the terms of
the GPL-3.0-or-later as a whole; spokenpad's own MIT-licensed source is
compatible with that.

The Rust crates spokenpad depends on (`Cargo.lock`) are under MIT,
Apache-2.0, ISC, BSD-3-Clause, Zlib, Unlicense, Unicode-3.0 and
CDLA-Permissive-2.0 (`webpki-roots`, the TLS root certificates), most of
them offering a choice; `cargo tree -e normal -f '{p} {l}'` lists each.

At run time spokenpad uses, without linking them in: the system's PortAudio,
glibc, libstdc++ and libgcc, and, for the pane, libxcb, libxkbcommon and
fontconfig; it runs Neovim, `fc-match` and a clipboard tool.

## Downloaded on first use

`spokenpad fetch-models`, or the first press, downloads these from fixed URLs
into `~/.local/share/spokenpad/models`. They are not part of the package.

| model | licence | source |
|---|---|---|
| Parakeet TDT 0.6B v3 (NVIDIA), int8 export for sherpa-onnx | CC-BY-4.0 | https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3, exported at https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8 |
| Silero VAD | MIT | https://github.com/snakers4/silero-vad |
