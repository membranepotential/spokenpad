#!/usr/bin/env bash
# Downloads the sherpa-onnx static libraries a build links into spokenpad
# (sherpa-onnx and onnxruntime, prebuilt by the sherpa-onnx project) into DIR,
# and checks them against the sha256 pinned in packaging/aur/PKGBUILD, the
# one place the version and the sum are written down. Point the build at DIR
# and it copies the archive from there instead of downloading it unchecked:
#
#   packaging/sherpa-archive.sh ~/.cache/spokenpad-sherpa
#   SHERPA_ONNX_ARCHIVE_DIR=~/.cache/spokenpad-sherpa cargo build --locked --release
#
# A target directory that already holds target/sherpa-onnx-prebuilt reuses
# what is there without looking at the archive: remove that directory once.
set -euo pipefail

dest=${1:?usage: packaging/sherpa-archive.sh DIR}
if [[ $(uname -m) != x86_64 ]]; then
    echo "the pinned archive is the x86-64 one; this machine is $(uname -m)" >&2
    exit 1
fi
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# Sourced in a subshell: a PKGBUILD only assigns variables and defines
# functions, none of which run here.
read -r version sum < <(
    cd "$here/aur"
    # shellcheck disable=SC1091
    source ./PKGBUILD
    # shellcheck disable=SC2154  # both come from the PKGBUILD
    printf '%s %s\n' "$_sherpa" "${sha256sums[1]}"
)
name="sherpa-onnx-v$version-linux-x64-static-lib.tar.bz2"
mkdir -p "$dest"
if ! echo "$sum  $dest/$name" | sha256sum --check --status 2>/dev/null; then
    curl --fail --silent --show-error --location \
        --proto '=https' --proto-redir '=https' \
        --output "$dest/$name.part" \
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/v$version/$name"
    if ! echo "$sum  $dest/$name.part" | sha256sum --check --status; then
        rm -f "$dest/$name.part"
        echo "$name does not match the sha256 pinned in packaging/aur/PKGBUILD" >&2
        exit 1
    fi
    mv "$dest/$name.part" "$dest/$name"
fi
echo "$dest/$name: sha256 $sum, as pinned"
