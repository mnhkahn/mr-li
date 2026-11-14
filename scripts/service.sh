#!/bin/bash
# launchd owns both processes directly; no background shell or nested supervisor.
set -euo pipefail
umask 077
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
ACTION="${1:-status}"
MODE="${2:-user}"
if [[ "$(uname -s)" != Darwin ]]; then echo 'This service installer requires macOS.' >&2; exit 1; fi
if [[ "$MODE" == system ]]; then
  [[ "$ACTION" == render || ( "$EUID" == 0 && -n "${SUDO_USER:-}" && "$SUDO_USER" != root ) ]] || { echo 'Use: sudo scripts/service.sh install system (from the deployment user)' >&2; exit 1; }
  OWNER="${SUDO_USER:-$(id -un)}"
  OWNER_HOME="$(dscl . -read "/Users/$OWNER" NFSHomeDirectory | sed 's/^NFSHomeDirectory: //')"
  DOMAIN=system
  PLIST_DIR=/Library/LaunchDaemons
else
  [[ "$MODE" == user && "$EUID" != 0 ]] || { echo 'For user mode, run without sudo.' >&2; exit 1; }
  OWNER="$(id -un)"; OWNER_HOME="$HOME"
  DOMAIN="gui/$(id -u)"
  PLIST_DIR="$HOME/Library/LaunchAgents"
fi
if [[ "$ACTION" == render ]]; then PLIST_DIR="$(mktemp -d /tmp/mr-li-plists.XXXXXX)"; fi
label() { echo "com.cyeam.mr-li.$1"; }
plist() { echo "$PLIST_DIR/$(label "$1").plist"; }
make_plist() {
  local name="$1" p
  p="$(mktemp "$PLIST_DIR/.mr-li.XXXXXX")"
  /usr/bin/plutil -create xml1 "$p"
  /usr/bin/plutil -insert Label -string "$(label "$name")" "$p"
  /usr/bin/plutil -insert ProgramArguments -xml '<array/>' "$p"
  /usr/bin/plutil -insert ProgramArguments.0 -string /bin/bash "$p"
  /usr/bin/plutil -insert ProgramArguments.1 -string "$ROOT/scripts/run.sh" "$p"
  /usr/bin/plutil -insert ProgramArguments.2 -string "$name" "$p"
  if [[ "$name" == watchdog ]]; then
    if [[ "$MODE" == system ]]; then
      /usr/bin/plutil -replace ProgramArguments.1 -string '/Library/Application Support/com.cyeam.mr-li/watchdog.sh' "$p"
      /usr/bin/plutil -replace ProgramArguments.2 -string "$ROOT" "$p"
      /usr/bin/plutil -insert ProgramArguments.3 -string "$OWNER" "$p"
    else
      /usr/bin/plutil -insert ProgramArguments.3 -string "$DOMAIN" "$p"
    fi
    /usr/bin/plutil -insert StartInterval -integer 60 "$p"
  else
    /usr/bin/plutil -insert KeepAlive -bool true "$p"
    /usr/bin/plutil -insert ThrottleInterval -integer 15 "$p"
    /usr/bin/plutil -insert ExitTimeOut -integer 30 "$p"
  fi
  /usr/bin/plutil -insert RunAtLoad -bool true "$p"
  /usr/bin/plutil -insert WorkingDirectory -string "$ROOT" "$p"
  /usr/bin/plutil -insert EnvironmentVariables -xml '<dict/>' "$p"
  /usr/bin/plutil -insert EnvironmentVariables.HOME -string "$OWNER_HOME" "$p"
  /usr/bin/plutil -insert StandardOutPath -string "$ROOT/logs/$name.log" "$p"
  /usr/bin/plutil -insert StandardErrorPath -string "$ROOT/logs/$name.error.log" "$p"
  /usr/bin/plutil -insert ProcessType -string Background "$p"
  if [[ "$MODE" == system ]]; then
    # Only the root-owned watchdog wrapper runs as root; the agent runs as OWNER.
    if [[ "$name" != watchdog ]]; then /usr/bin/plutil -insert UserName -string "$OWNER" "$p"; fi
    if [[ "$ACTION" != render ]]; then chown root:wheel "$p"; fi
    chmod 644 "$p"
  else chmod 600 "$p"; fi
  /usr/bin/plutil -lint "$p"
  /bin/mv "$p" "$(plist "$name")"
}
case "$ACTION" in
  render)
    for name in opencode bridge watchdog; do make_plist "$name"; done
    echo "Rendered only (not installed): $PLIST_DIR"
    ;;
  install)
    # Validate without making network requests or printing secrets. Do not install on development Mac.
    if [[ "$MODE" == system ]]; then sudo -u "$OWNER" "$ROOT/scripts/run.sh" check; else "$ROOT/scripts/run.sh" check; fi
    mkdir -p "$PLIST_DIR" "$ROOT/logs" "$ROOT/.data"
    chmod 700 "$ROOT/logs" "$ROOT/.data"
    if [[ "$MODE" == system ]]; then chown "$OWNER" "$ROOT/logs" "$ROOT/.data"; fi
    chmod 600 "$ROOT/.env"
    if [[ "$MODE" == system ]]; then
      /usr/bin/install -d -o root -g wheel -m 755 '/Library/Application Support/com.cyeam.mr-li'
      /usr/bin/install -o root -g wheel -m 755 "$ROOT/scripts/watchdog-system.sh" '/Library/Application Support/com.cyeam.mr-li/watchdog.sh'
    fi
    for name in opencode bridge watchdog; do
      /bin/launchctl bootout "$DOMAIN/$(label "$name")" 2>/dev/null || true
      make_plist "$name"
      /bin/launchctl bootstrap "$DOMAIN" "$(plist "$name")"
    done
    echo "Installed in $DOMAIN. Inspect with: scripts/service.sh status $MODE"
    ;;
  status)
    for name in opencode bridge watchdog; do /bin/launchctl print "$DOMAIN/$(label "$name")" || true; done
    ;;
  restart)
    for name in opencode bridge; do /bin/launchctl kickstart -k "$DOMAIN/$(label "$name")"; done
    ;;
  uninstall)
    for name in watchdog bridge opencode; do
      /bin/launchctl bootout "$DOMAIN/$(label "$name")" 2>/dev/null || true
      # Leave the plist and data for inspection; bootout disables this installation until bootstrap.
      if [[ -f "$(plist "$name")" ]]; then /bin/mv "$(plist "$name")" "$(plist "$name").disabled"; fi
    done
    echo 'Services stopped. Configuration, logs and conversation data are retained.'
    ;;
  *) echo 'Usage: scripts/service.sh {render|install|status|restart|uninstall} [user|system]' >&2; exit 2;;
esac
