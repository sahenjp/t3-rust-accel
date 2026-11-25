#!/usr/bin/env bash
# Remove the Phase 1 memory-tuning drop-in. Safe: deletes the staged drop-in
# only; it never runs daemon-reload or restarts the service.
set -euo pipefail

DST_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/t3code.service.d"
DST="${DST_DIR}/memory-tuning.conf"

if [[ ! -f "$DST" ]]; then
    echo "nothing to remove: $DST does not exist"
    exit 0
fi

rm -f "$DST"
echo "removed $DST"

cat <<'EOF'

To activate the rollback, run manually:

    systemctl --user daemon-reload && systemctl --user restart t3code.service

(Do NOT run this from inside an active T3 Code agent thread; it restarts the
server hosting the agent.)
EOF
