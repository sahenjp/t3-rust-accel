#!/bin/sh
# Stages the Phase 2 drop-in for t3code.service. Does NOT reload or restart.
set -eu
dropin_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/t3code.service.d"
conf="$(dirname "$0")/../systemd/phase2-server.conf"
mkdir -p "$dropin_dir"
dest="$dropin_dir/phase2-server.conf"
temp="$(mktemp "${dest}.tmp.XXXXXX")"
trap 'rm -f -- "$temp"' EXIT
install -m 0644 -- "$conf" "$temp"
if [ -f "$dest" ] && ! cmp -s "$temp" "$dest"; then
  backup="$(mktemp "${dest}.bak.XXXXXX")"
  cp -p -- "$dest" "$backup"
  echo "Backed up $backup"
fi
mv -fT -- "$temp" "$dest"
echo "Staged $dropin_dir/phase2-server.conf"
echo "Activate manually with:"
echo "  systemctl --user daemon-reload && systemctl --user restart t3code.service"
