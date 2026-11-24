#!/bin/sh
# Stages the Phase 2 drop-in for t3code.service. Does NOT reload or restart.
set -eu
dropin_dir="$HOME/.config/systemd/user/t3code.service.d"
conf="$(dirname "$0")/../systemd/phase2-server.conf"
mkdir -p "$dropin_dir"
sed "s|__HOME__|${HOME}|g" "$conf" > "$dropin_dir/phase2-server.conf"
echo "Staged $dropin_dir/phase2-server.conf"
echo "Activate manually with:"
echo "  systemctl --user daemon-reload && systemctl --user restart t3code.service"
