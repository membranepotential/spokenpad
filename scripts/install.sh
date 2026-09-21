#!/bin/sh
# Build spokenpad and install it for the current user:
#
#   target/release/spokenpad     -> ~/.local/bin/spokenpad
#   packaging/spokenpad.service  -> $XDG_CONFIG_HOME/systemd/user/spokenpad.service
#
# Usage: scripts/install.sh [--uninstall]
#
# Both files are copies, so the checkout can be moved or deleted afterwards.
# Re-running updates them. A file at either path that spokenpad did not
# write is never replaced or removed.
#
# Window-manager rules (the dictation window must float and never take
# focus) are not installed here; see packaging/ for your window manager.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
bin=$HOME/.local/bin/spokenpad
unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/spokenpad.service

# ours PATH: whether PATH is absent or something this script installed.
# The binary is an ELF file that names spokenpad, the unit carries our first
# line, or is the symlink into a checkout that older versions installed.
ours() {
    if [ -L "$1" ]; then
        case $(readlink "$1") in */packaging/spokenpad.service) return 0 ;; *) return 1 ;; esac
    fi
    [ -e "$1" ] || return 0
    case $1 in
    "$bin") [ "$(od -An -tx1 -N4 "$1" | tr -d ' \n')" = 7f454c46 ] && grep -qF spokenpad "$1" ;;
    "$unit") head -n 1 "$1" | grep -qF '# spokenpad systemd user unit' ;;
    *) return 1 ;;
    esac
}

refuse() {
    echo "install: $1 exists and was not written by spokenpad; move it aside and re-run" >&2
    exit 1
}

# place SOURCE DEST MODE: copy next to DEST, then rename over it. A rename
# works even while the old binary is running.
place() {
    mkdir -p "$(dirname "$2")"
    cp "$1" "$2.new"
    chmod "$3" "$2.new"
    mv -f "$2.new" "$2"
    echo "  installed $2"
}

if [ "${1:-}" = --uninstall ]; then
    for f in "$bin" "$unit"; do ours "$f" || refuse "$f"; done
    if [ -e "$unit" ] || [ -L "$unit" ]; then
        systemctl --user disable --now spokenpad.service || true
    fi
    for f in "$unit" "$bin"; do
        if [ -e "$f" ] || [ -L "$f" ]; then
            rm -f "$f"
            echo "  removed $f"
        fi
    done
    systemctl --user daemon-reload || true
    echo "Models in ${XDG_DATA_HOME:-$HOME/.local/share}/spokenpad and settings in"
    echo "${XDG_CONFIG_HOME:-$HOME/.config}/spokenpad are left in place."
    exit 0
elif [ $# -gt 0 ]; then
    echo "usage: $0 [--uninstall]" >&2
    exit 2
fi

for f in "$bin" "$unit"; do ours "$f" || refuse "$f"; done
cargo build --locked --release --manifest-path "$repo/Cargo.toml"
place "${CARGO_TARGET_DIR:-$repo/target}/release/spokenpad" "$bin" 755
place "$repo/packaging/spokenpad.service" "$unit" 644
systemctl --user daemon-reload || echo "install: run 'systemctl --user daemon-reload' in your session" >&2
cat <<EOF

Next:
  scripts/fetch-models.sh                          # once, ~640 MB
  systemctl --user enable --now spokenpad          # first install
  systemctl --user restart spokenpad               # after an update
EOF
