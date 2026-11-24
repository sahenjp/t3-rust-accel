#!/bin/sh
# Removes the Phase 2 drop-in, restoring the versioned __service-launcher unit.
# Does NOT reload or restart.
set -eu
dropin="$HOME/.config/systemd/user/t3code.service.d/phase2-server.conf"
if [ -f "$dropin" ]; then
  rm "$dropin"
  echo "Removed $dropin"
else
  echo "Nothing staged at $dropin"
fi
echo "Apply rollback manually with:"
echo "  systemctl --user daemon-reload && systemctl --user restart t3code.service"
