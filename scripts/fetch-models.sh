#!/bin/sh
# Download spokenpad's default models: Parakeet TDT 0.6B v3 (int8, ~670 MB)
# and the Silero VAD (~0.6 MB).
#
# Usage: scripts/fetch-models.sh [DIR]
#
# DIR defaults to $XDG_DATA_HOME/spokenpad/models (~/.local/share/spokenpad/models),
# which is where spokenpad looks when asr.model_dir and vad.model are unset.
# Every file is checked against a pinned size and sha256. A file that already
# matches is skipped, so re-running repairs a partial download.
set -eu

dir=${1:-${XDG_DATA_HOME:-$HOME/.local/share}/spokenpad/models}
parakeet=https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78
# sherpa-onnx's copy of Silero: the build sherpa-onnx is tested against.
silero=https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models

command -v curl >/dev/null || { echo "fetch-models: curl is required" >&2; exit 1; }
command -v sha256sum >/dev/null || { echo "fetch-models: sha256sum is required" >&2; exit 1; }

# fetch URL DEST SIZE SHA256
fetch() {
    if [ -f "$2" ] && [ "$(wc -c <"$2")" -eq "$3" ] &&
        echo "$4  $2" | sha256sum -c --status; then
        echo "  $2: present"
        return
    fi
    echo "  $2: downloading $1"
    mkdir -p "$(dirname "$2")"
    curl -fL --retry 3 --progress-bar -o "$2.part" "$1"
    if [ "$(wc -c <"$2.part")" -ne "$3" ] || ! echo "$4  $2.part" | sha256sum -c --status; then
        rm -f "$2.part"
        echo "fetch-models: $1 does not match the pinned size and sha256" >&2
        exit 1
    fi
    mv -f "$2.part" "$2"
}

echo "model directory: $dir"
fetch "$parakeet/encoder.int8.onnx" "$dir/parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx" \
    652184281 acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247
fetch "$parakeet/decoder.int8.onnx" "$dir/parakeet-tdt-0.6b-v3-int8/decoder.int8.onnx" \
    11845275 179e50c43d1a9de79c8a24149a2f9bac6eb5981823f2a2ed88d655b24248db4e
fetch "$parakeet/joiner.int8.onnx" "$dir/parakeet-tdt-0.6b-v3-int8/joiner.int8.onnx" \
    6355277 3164c13fc2821009440d20fcb5fdc78bff28b4db2f8d0f0b329101719c0948b3
fetch "$parakeet/tokens.txt" "$dir/parakeet-tdt-0.6b-v3-int8/tokens.txt" \
    93939 d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d
# A 12-second English sample; the real-model e2e test decodes it.
fetch "$parakeet/test_wavs/en.wav" "$dir/parakeet-tdt-0.6b-v3-int8/test_en.wav" \
    184608 148b936b43ce7c546a866e64da059f0458aee2d65e617f16e9d94f06e8d99ed6
fetch "$silero/silero_vad.onnx" "$dir/silero_vad.onnx" \
    643854 9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6
echo "done"
