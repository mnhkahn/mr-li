#!/bin/bash
# Unattended startup without administrator access: cron loads Background launchd jobs.
set -euo pipefail
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
export HOME="$(/usr/bin/dscl . -read "/Users/$(id -un)" NFSHomeDirectory | /usr/bin/sed 's/^NFSHomeDirectory: //')"
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
DOMAIN="user/$(id -u)"
STATE="$ROOT/.data/boot-launchd"
label() { printf 'com.cyeam.mr-li.%s' "$1"; }
ensure_jobs() {
  for name in opencode bridge watchdog; do
    if ! /bin/launchctl print "$DOMAIN/$(label "$name")" >/dev/null 2>&1; then
      /bin/launchctl bootstrap "$DOMAIN" "$STATE/$(label "$name").plist"
      printf '%s Loaded %s in %s\n' "$(date '+%F %T %z')" "$name" "$DOMAIN"
    fi
  done
  date +%s > "$STATE/last-check"
}
case "${1:-ensure}" in
  install)
    [[ "$(uname -s)" == Darwin && "$EUID" != 0 ]] || { echo 'Run as the deployment user on macOS, without sudo.' >&2; exit 1; }
    "$ROOT/scripts/run.sh" check
    mkdir -p "$STATE" "$ROOT/logs"
    # Preserve unrelated crontab entries and user LaunchAgents for rollback.
    stamp="$(date '+%Y%m%d-%H%M%S')"
    backup="$STATE/backup-$stamp"
    mkdir -p "$backup"
    /usr/bin/crontab -l > "$backup/crontab" 2> "$backup/crontab.error" || {
      /usr/bin/grep -q 'no crontab for' "$backup/crontab.error" || { cat "$backup/crontab.error" >&2; exit 1; }
    }
    rendered="$(/bin/bash "$ROOT/scripts/service.sh" render user | /usr/bin/sed -n 's/^Rendered only (not installed): //p')"
    [[ -d "$rendered" ]] || { echo 'Cannot render launchd jobs.' >&2; exit 1; }
    for name in opencode bridge watchdog; do
      p="$STATE/$(label "$name").plist"
      cp "$rendered/$(label "$name").plist" "$p"
      /usr/bin/plutil -insert LimitLoadToSessionType -string Background "$p"
      if [[ "$name" == watchdog ]]; then
        /usr/bin/plutil -replace ProgramArguments -xml '<array/>' "$p"
        /usr/bin/plutil -insert ProgramArguments.0 -string /bin/bash "$p"
        /usr/bin/plutil -insert ProgramArguments.1 -string "$ROOT/scripts/boot-start.sh" "$p"
        /usr/bin/plutil -insert ProgramArguments.2 -string watchdog "$p"
      fi
      /usr/bin/plutil -lint "$p"
    done
    # Remove the login-only copies to avoid duplicate bridges after desktop login.
    for name in watchdog bridge opencode; do
      agent="$HOME/Library/LaunchAgents/$(label "$name").plist"
      if [[ -f "$agent" ]]; then mv "$agent" "$backup/"; fi
      /bin/launchctl bootout "gui/$(id -u)/$(label "$name")" 2>/dev/null || true
      /bin/launchctl bootout "$DOMAIN/$(label "$name")" 2>/dev/null || true
    done
    # Validate Background bootstrapping before installing the startup schedule.
    ensure_jobs
    /usr/bin/awk '/^# BEGIN MR-LI BOOT$/{skip=1;next} /^# END MR-LI BOOT$/{skip=0;next} !skip{print}' "$backup/crontab" > "$STATE/crontab.next"
    {
      printf '\n# BEGIN MR-LI BOOT\n'
      printf '@reboot /bin/bash "%s/scripts/boot-start.sh" ensure >> "%s/logs/boot-start.log" 2>&1\n' "$ROOT" "$ROOT"
      printf '* * * * * /bin/bash "%s/scripts/boot-start.sh" ensure >> "%s/logs/boot-start.log" 2>&1\n' "$ROOT" "$ROOT"
      printf '# END MR-LI BOOT\n'
    } >> "$STATE/crontab.next"
    /usr/bin/crontab "$STATE/crontab.next"
    printf 'Installed unattended startup in %s; backups: %s\n' "$DOMAIN" "$backup"
    ;;
  ensure) ensure_jobs ;;
  watchdog)
    failures="$STATE/health-failures"
    if "$ROOT/scripts/run.sh" health >/dev/null 2>&1; then
      echo 0 > "$failures"
    else
      n=0
      if [[ -f "$failures" ]]; then read -r n < "$failures" || true; fi
      [[ "$n" =~ ^[0-9]+$ ]] || n=0
      n=$((n+1)); echo "$n" > "$failures"
      if ((n>=3)); then
        /bin/launchctl kickstart -k "$DOMAIN/$(label opencode)"
        echo 0 > "$failures"
      fi
    fi
    last=0
    if [[ -f "$ROOT/.data/heartbeat" ]]; then last="$(/usr/bin/stat -f %m "$ROOT/.data/heartbeat")"; fi
    if (( $(date +%s) - last > 180 )); then
      /bin/launchctl kickstart -k "$DOMAIN/$(label bridge)"
    fi
    ;;
  *) echo 'Usage: scripts/boot-start.sh {install|ensure|watchdog}' >&2; exit 2 ;;
esac
