#!/bin/bash
# Installed root-owned; user-writable project code always executes as the service user.
set -euo pipefail
ROOT="$1"
OWNER="$2"
STATE='/Library/Application Support/com.cyeam.mr-li/health-failures'
if /usr/bin/sudo -H -u "$OWNER" -- /bin/bash "$ROOT/scripts/run.sh" probe; then
  echo 0 > "$STATE"
else
  n=0
  if [[ -f "$STATE" ]]; then read -r n < "$STATE" || true; fi
  [[ "$n" =~ ^[0-9]+$ ]] || n=0
  n=$((n+1))
  echo "$n" > "$STATE"
  if ((n>=3)); then
    /bin/launchctl kickstart -k system/com.cyeam.mr-li.opencode
    /bin/launchctl kickstart -k system/com.cyeam.mr-li.bridge
    echo 0 > "$STATE"
  fi
fi
