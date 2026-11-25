#!/bin/sh
# Removes the Phase 2 drop-in, restoring the versioned __service-launcher unit.
# Does NOT reload or restart.
set -eu
dropin_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/t3code.service.d"
removed=0
for name in phase2-server.conf zz-connect-e2ee.conf; do
  dropin="$dropin_dir/$name"
  if [ -f "$dropin" ]; then
    rm -- "$dropin"
    echo "Removed $dropin"
    removed=1
  fi
done
if [ "$removed" -eq 0 ]; then
  echo "Nothing staged in $dropin_dir"
fi
echo "Apply rollback manually with:"
echo "  systemctl --user daemon-reload && systemctl --user restart t3code.service"
