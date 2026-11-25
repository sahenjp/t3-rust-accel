#!/usr/bin/env bash
# Stage the Phase 1 memory-tuning drop-in for t3code.service.
# Safe: installs config only. It never runs daemon-reload or restarts the
# service; print the manual restart command at the end instead.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SRC="${SCRIPT_DIR}/../systemd/memory-tuning.conf"
DST_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/t3code.service.d"
DST="${DST_DIR}/memory-tuning.conf"
BRIDGE_BIN="${HOME}/.local/bin/t3-rust-bridge"

if [[ ! -f "$SRC" ]]; then
    echo "error: drop-in source missing: $SRC" >&2
    exit 1
fi

if [[ ! -f "$BRIDGE_BIN" ]]; then
    echo "warning: $BRIDGE_BIN is not installed yet; T3_RUST_BRIDGE_BIN will dangle" >&2
fi

if [[ ! -e /usr/lib/libjemalloc.so.2 ]]; then
    echo "warning: /usr/lib/libjemalloc.so.2 missing; install jemalloc (pacman -S jemalloc) or remove the LD_PRELOAD line" >&2
fi

mkdir -p "$DST_DIR"
if [[ -f "$DST" ]] && ! cmp -s "$SRC" "$DST"; then
    BACKUP="$(mktemp "${DST}.bak.XXXXXX")"
    cp -p "$DST" "$BACKUP"
    echo "backed up existing drop-in to $BACKUP"
fi
temp="$(mktemp "${DST}.tmp.XXXXXX")"
trap 'rm -f -- "$temp"' EXIT
install -m 0644 -- "$SRC" "$temp"
mv -fT -- "$temp" "$DST"
echo "installed $DST"

cat <<'EOF'

To activate, run manually:

    systemctl --user daemon-reload && systemctl --user restart t3code.service

(Do NOT run this from inside an active T3 Code agent thread; it restarts the
server hosting the agent.)
EOF
