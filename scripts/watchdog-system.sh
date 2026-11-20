#!/bin/bash
# Installed root-owned; user-writable project code always executes as the service user.
set -euo pipefail
ROOT="$1"
OWNER="$2"
STATE='/Library/Application Support/com.cyeam.mr-li/health-failures'
# A failed OpenCode backend must not interrupt Gemini/Kilo/TRAE/Cline work.
for service in opencode bridge; do
  mode=health
  [[ "$service" != bridge ]] || mode=probe
  state="$STATE-$service"
  if /usr/bin/sudo -H -u "$OWNER" -- /bin/bash "$ROOT/scripts/run.sh" "$mode"; then
    echo 0 > "$state"
  else
    n=0
    if [[ -f "$state" ]]; then read -r n < "$state" || true; fi
    [[ "$n" =~ ^[0-9]+$ ]] || n=0
    n=$((n+1))
    echo "$n" > "$state"
    if ((n>=3)); then
      /bin/launchctl kickstart -k "system/com.cyeam.mr-li.$service"
      echo 0 > "$state"
    fi
  fi
done
