#!/bin/sh
# Install Remote Device Hub Agent from a release directory.
#
# The bundle carries its own Node runtime, so the target machine needs no
# package manager, no Node and no root: detect platform, fetch the matching
# archive, verify it against SHA256SUMS, unpack into a versioned directory, pair
# the device, then run for this session unless automatic startup was selected.
# System services are an explicit advanced deployment requiring administrator access.
set -eu

DEFAULT_BASE="${RDH_BASE:-https://github.com/be-one-click/remote-device-agent/releases/latest/download}"
DEFAULT_PREFIX=$HOME/.local/lib/rdh
DEFAULT_BIN=$HOME/.local/bin

BASE=$DEFAULT_BASE
VERSION=
PREFIX=$DEFAULT_PREFIX
BIN_DIR=$DEFAULT_BIN
SERVICE=auto
STARTUP=choose
SERVICE_USER=
CONFIG=
DATA=
SERVER=
ROOTS=
PAIR=1
OFFLINE=
DRY_RUN=0
FORCE=0
UNINSTALL=0

usage() {
  cat <<'USAGE'
usage: install.sh [options]

  --base URL         release directory holding rdh-*.tar.gz and SHA256SUMS
                     (default: the GitHub release, or $RDH_BASE)
  --version VERSION  install this version instead of the newest one
  --prefix DIR       install root (default: ~/.local/lib/rdh)
  --bin-dir DIR      directory for the rdh command (default: ~/.local/bin)
  --startup MODE     none | user | service (default: prompt, Enter = none)
                     service is advanced deployment; requires root and --service-user
  --service-user USER existing non-root account for an advanced boot service
  --config FILE      existing paired configuration (copied for boot services)
  --service MODE     legacy: systemd | launchd | none | auto (selects user/none)
  --server URL       device hub address used for pairing
  --root NAME=PATH   directory to expose; repeatable (default: current dir)
  --no-pair          install only in none mode; background modes require --config
  --no-service       do not register or start a service
  --offline FILE     install from a local archive instead of downloading
  --force            reinstall even when the version is already installed
  --dry-run          show what would happen and exit
  --uninstall        stop the service and remove the installation
  -h, --help         show this help
USAGE
}

log() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case $1 in
    --base) BASE=${2:?--base needs a value}; shift 2 ;;
    --version) VERSION=${2:?--version needs a value}; VERSION=${VERSION#v}; shift 2 ;;
    --prefix) PREFIX=${2:?--prefix needs a value}; shift 2 ;;
    --bin-dir) BIN_DIR=${2:?--bin-dir needs a value}; shift 2 ;;
    --startup) STARTUP=${2:?--startup needs a value}; shift 2 ;;
    --service-user) SERVICE_USER=${2:?--service-user needs a value}; shift 2 ;;
    --config) CONFIG=${2:?--config needs a value}; shift 2 ;;
    --service) SERVICE=${2:?--service needs a value}; STARTUP=user; [ "$SERVICE" != none ] || STARTUP=none; shift 2 ;;
    --server) SERVER=${2:?--server needs a value}; shift 2 ;;
    --root) ROOTS="$ROOTS
${2:?--root needs a value}"; shift 2 ;;
    --no-pair) PAIR=0; shift ;;
    --no-service) SERVICE=none; STARTUP=none; shift ;;
    --offline) OFFLINE=${2:?--offline needs a value}; shift 2 ;;
    --force) FORCE=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

expand_path() {
  case $1 in
    "~/"*) printf '%s/%s' "$HOME" "${1#\~/}" ;;
    *) printf '%s' "$1" ;;
  esac
}

case $STARTUP in choose|none|user|service) ;; *) die "invalid --startup: $STARTUP" ;; esac
case $SERVICE in auto|systemd|launchd|none) ;; *) die "invalid --service: $SERVICE" ;; esac
if [ "$STARTUP" = choose ]; then
  STARTUP=none
  if [ "$PAIR" -eq 1 ] && [ "$UNINSTALL" -eq 0 ] && { [ -t 0 ] || [ -t 1 ]; }; then
    log '1. This time only (default; no automatic startup)'
    log '2. Run in the background now and after this user logs on'
    printf 'Startup mode [1/2, Enter = 1]: '
    if [ -t 0 ]; then IFS= read -r answer || answer=
    else IFS= read -r answer < /dev/tty || answer=; fi
    case $answer in ''|1) ;; 2) STARTUP=user ;; *) die 'select 1 or 2' ;; esac
  fi
fi
if [ "$STARTUP" != none ] && [ "$PAIR" -eq 0 ] && [ -z "$CONFIG" ] && [ "$UNINSTALL" -eq 0 ]; then
  die 'background startup requires pairing; with --no-pair, supply --config'
fi
if [ "$STARTUP" = service ]; then
  [ "$(id -u)" -eq 0 ] || die '--startup service requires administrator privileges'
  umask 022
  case $SERVICE_USER in ''|*[!a-zA-Z0-9_-]*) die '--service-user must name an existing non-root account' ;; esac
  service_uid=$(id -u "$SERVICE_USER") || die 'service account does not exist'
  [ "$service_uid" -ne 0 ] || die 'the Agent service must not run as root'
  service_gid=$(id -g "$SERVICE_USER")
  if [ "$(uname -s)" = Darwin ]; then
    service_prefix=/Library/RemoteDeviceHub
    DATA=/Library/Application\ Support/RemoteDeviceHub
  else
    service_prefix=/opt/remote-device-hub
    DATA=/var/lib/remote-device-hub
  fi
  [ "$PREFIX" = "$DEFAULT_PREFIX" ] || [ "$PREFIX" = "$service_prefix" ] || die "boot services use $service_prefix"
  PREFIX=$service_prefix
  [ "$BIN_DIR" != "$DEFAULT_BIN" ] || BIN_DIR=$PREFIX/bin
  have_sudo=$(command -v sudo || true)
  [ -n "$have_sudo" ] || die 'sudo is required to pair as the service account'
fi
PREFIX=$(expand_path "$PREFIX")
BIN_DIR=$(expand_path "$BIN_DIR")
UNIT_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
UNIT=$UNIT_DIR/rdh.service
LAUNCHD_LABEL=com.be-one-click.rdh
PLIST=$HOME/Library/LaunchAgents/$LAUNCHD_LABEL.plist
if [ "$STARTUP" = service ]; then
  UNIT_DIR=/etc/systemd/system
  UNIT=$UNIT_DIR/rdh.service
  PLIST=/Library/LaunchDaemons/$LAUNCHD_LABEL.plist
fi
# Absolute paths are persisted in service definitions; reject multiline values.
for value in "$PREFIX" "$BIN_DIR" "${CONFIG:-/}"; do
  case $value in /*) ;; *) die 'installation/config paths must be absolute' ;; esac
  case $value in *'
'*) die 'paths must not contain newlines' ;; esac
done
TMP=
trap 'if [ -n "${TMP:-}" ]; then rm -rf "$TMP"; fi' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

have() { command -v "$1" >/dev/null 2>&1; }

service_manager() {
  [ "$STARTUP" != none ] || { printf none; return; }
  case $SERVICE in
    systemd|launchd|none) printf '%s' "$SERVICE"; return ;;
  esac
  if [ "$(uname -s)" = Darwin ]; then
    if have launchctl; then printf 'launchd'; return; fi
  elif have systemctl && { [ "$STARTUP" = service ] || systemctl --user show-environment >/dev/null 2>&1; }; then
    printf 'systemd'; return
  fi
  printf 'none'
}

detect_platform() {
  os=$(uname -s)
  arch=$(uname -m)
  case $os in Linux) os=linux ;; Darwin) os=darwin ;; *) die "unsupported OS: $os" ;; esac
  case $arch in x86_64|amd64) arch=x64 ;; aarch64|arm64) arch=arm64 ;; *) die "unsupported architecture: $arch" ;; esac
  if [ "$os" = darwin ]; then printf 'darwin-%s' "$arch"; return; fi

  libc=gnu
  if [ -e /lib/ld-musl-x86_64.so.1 ] || [ -e /lib/ld-musl-aarch64.so.1 ]; then
    libc=musl
  elif have getconf; then
    glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}' || true)
    case $glibc in
      ''|*[!0-9.]*) : ;;
      *) awk -v v="$glibc" 'BEGIN{exit !(v+0 < 2.28)}' && libc=musl ;;
    esac
  fi
  printf '%s-%s-%s' "$os" "$arch" "$libc"
}

hash_file() {
  if have sha256sum; then sha256sum "$1" | cut -d' ' -f1
  elif have shasum; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "need sha256sum or shasum"; fi
}

systemctl_mode() {
  if [ "$STARTUP" = service ]; then systemctl "$@"; else systemctl --user "$@"; fi
}
launch_domain() {
  if [ "$STARTUP" = service ]; then printf system; else printf 'gui/%s' "$(id -u)"; fi
}
stop_service() {
  case $(service_manager) in
    systemd)
      if [ -e "$UNIT" ]; then
        systemctl_mode disable --now rdh || die 'cannot stop the existing service'
        rm -f "$UNIT"
        systemctl_mode daemon-reload
      fi
      ;;
    launchd)
      if launchctl print "$(launch_domain)/$LAUNCHD_LABEL" >/dev/null 2>&1; then
        launchctl bootout "$(launch_domain)/$LAUNCHD_LABEL" || die 'cannot stop the existing agent'
      fi
      rm -f "$PLIST"
      ;;
  esac
}

if [ "$UNINSTALL" -eq 1 ]; then
  # Legacy uninstall defaults to the current user's installation.
  [ "$STARTUP" != none ] || STARTUP=user
  step "removing Remote Device Hub Agent"
  stop_service
  rm -f "$BIN_DIR/rdh"
  rm -rf "$PREFIX"
  log "installation removed; device configuration and audit logs were kept"
  exit 0
fi

[ -n "$BASE" ] || die "--base is required when \$RDH_BASE is unset"
[ -n "$OFFLINE" ] || have curl || die "curl is required unless --offline is used"

PLATFORM=$(detect_platform)
manager=$(service_manager)
[ "$STARTUP" = none ] || [ "$manager" != none ] || die 'selected startup mode is unavailable; use --startup none'
# Do not claim a temporary session when an earlier startup registration remains.
if [ -e "$UNIT" ] || [ -e "$PLIST" ]; then
  die 'startup is already configured; remove the existing registration before reinstalling'
fi
if [ "$STARTUP" != service ] && { [ -e /etc/systemd/system/rdh.service ] || [ -e /Library/LaunchDaemons/$LAUNCHD_LABEL.plist ]; }; then
  die 'a boot service is already installed; stop and remove it before installing a user session'
fi
if [ "$STARTUP" = service ]; then
  # Never install privileged executables through user-controlled ancestors.
  for target in "$PREFIX" "$DATA" "$BIN_DIR"; do
    cursor=$target
    while [ "$cursor" != / ]; do
      [ ! -L "$cursor" ] || die "symlink in service path: $cursor"
      if [ -e "$cursor" ]; then
        if [ "$(uname -s)" = Darwin ]; then
          metadata=$(stat -f '%u %Lp' "$cursor")
        else
          metadata=$(stat -c '%u %a' "$cursor")
        fi
        owner=${metadata%% *}; mode=${metadata#* }
        [ "$owner" = 0 ] && [ $((0$mode & 0022)) -eq 0 ] || die "service path must be root-owned and not group/world writable: $cursor"
      fi
      cursor=$(dirname "$cursor")
    done
  done
  [ ! -e "$DATA" ] || die "service data already exists: $DATA (kept for manual recovery)"
fi
log "platform: $PLATFORM"
log "install:  $PREFIX"
log "service:  $manager"

# --- resolve version and archive ---------------------------------------------
if [ -n "$OFFLINE" ]; then
  [ -f "$OFFLINE" ] || die "offline archive not found: $OFFLINE"
  archive_file=$(basename "$OFFLINE")
  file_version=$(printf '%s' "$archive_file" | sed -n "s/^rdh-\(.*\)-$PLATFORM\.tar\.gz$/\1/p")
  [ -n "$file_version" ] || die "cannot read version from $archive_file"
  [ -z "$VERSION" ] || [ "$VERSION" = "$file_version" ] || die "archive is $file_version, not $VERSION"
  version=$file_version
  expected=$(sha256sum "$OFFLINE" 2>/dev/null | cut -d' ' -f1 || shasum -a 256 "$OFFLINE" | cut -d' ' -f1)
  actual=$expected
else
  TMP=$(mktemp -d "${TMPDIR:-/tmp}/rdh-install.XXXXXX")
  curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 15 -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS" ||
    die "cannot download $BASE/SHA256SUMS"
  if [ -n "$VERSION" ]; then
    version=$VERSION
    file=rdh-$VERSION-$PLATFORM.tar.gz
    expected=$(awk -v f="$file" '$2 == f {print $1}' "$TMP/SHA256SUMS")
    [ -n "$expected" ] || die "$file is not listed in SHA256SUMS"
  else
    file=$(awk '{print $2}' "$TMP/SHA256SUMS" | grep -E "^rdh-.*-$PLATFORM\.tar\.gz$" | head -1)
    [ -n "$file" ] || die "no build for $PLATFORM in SHA256SUMS"
    version=$(printf '%s' "$file" | sed -n "s/^rdh-\(.*\)-$PLATFORM\.tar\.gz$/\1/p")
    expected=$(awk -v f="$file" '$2 == f {print $1}' "$TMP/SHA256SUMS")
  fi
fi
[ -n "$version" ] || die "cannot determine version"

current=$(readlink "$PREFIX/current" 2>/dev/null || true)
need_install=1
if [ "$current" = "versions/$version" ] && [ "$FORCE" -eq 0 ]; then
  need_install=0
  log "version $version is already installed"
fi

if [ "$DRY_RUN" -eq 1 ]; then
  log "would install ${file:-$archive_file} into $PREFIX/versions/$version"
  exit 0
fi

if [ "$need_install" -eq 1 ]; then
  if [ -z "$OFFLINE" ]; then
    step "downloading $file"
    curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 15 -o "$TMP/$file" "$BASE/$file" ||
      die "cannot download $BASE/$file"
    archive=$TMP/$file
    actual=$(hash_file "$archive")
    [ "$actual" = "$expected" ] || die "checksum mismatch for $file"
  else
    archive=$OFFLINE
  fi

  step "unpacking"
  mkdir -p "$PREFIX/versions"
  dest=$PREFIX/versions/$version
  staging=$dest.new
  rm -rf "$staging"
  mkdir -p "$staging"
  tar -xzf "$archive" -C "$staging" --strip-components=1
  if [ "$STARTUP" = service ]; then
    # Root extraction may preserve the build machine's archive owners.
    chown -R 0:0 "$staging"
    chmod -R go-w "$staging"
  fi
  chmod +x "$staging/node" "$staging/rdh" "$staging/bin/remote-device-hub.mjs"

  installed=$("$staging/node" "$staging/bin/remote-device-hub.mjs" --version)
  [ "$installed" = "$version" ] || die "unpacked bundle reports $installed, expected $version"

  # Keep exactly one previous tree per version for rollback.
  if [ -d "$dest" ]; then
    rm -rf "$dest.previous"
    mv "$dest" "$dest.previous"
  fi
  mv "$staging" "$dest"
  ln -sfn "versions/$version" "$PREFIX/current"
  mkdir -p "$BIN_DIR"
  ln -sfn "$PREFIX/current/rdh" "$BIN_DIR/rdh"
  log "installed $BIN_DIR/rdh -> $PREFIX/current/rdh"
fi

# --- pairing and runtime arguments --------------------------------------------
if [ "$STARTUP" = service ]; then
  install -d -m 700 -o "$service_uid" -g "$service_gid" "$DATA"
  if [ -n "$CONFIG" ]; then
    install -m 600 -o "$service_uid" -g "$service_gid" "$CONFIG" "$DATA/agent.json"
    case $CONFIG in *.json) policy=${CONFIG%.json}.policy.json ;; *) policy=$CONFIG.policy.json ;; esac
    if [ -f "$policy" ]; then
      install -m 600 -o "$service_uid" -g "$service_gid" "$policy" "$DATA/agent.policy.json"
    fi
  fi
  CONFIG=$DATA/agent.json
  if [ -z "$ROOTS" ] && [ ! -f "$CONFIG" ]; then
    install -d -m 700 -o "$service_uid" -g "$service_gid" "$DATA/work"
    ROOTS="work=$DATA/work"
  fi
fi
set -- --no-browser
if [ -n "$CONFIG" ]; then set -- "$@" --config "$CONFIG"; fi
if [ -n "$SERVER" ]; then set -- "$@" --server "$SERVER"; fi
while IFS= read -r spec; do
  [ -n "$spec" ] || continue
  set -- "$@" --root "$spec"
done <<ROOT_SPECS
$ROOTS
ROOT_SPECS
run_agent() {
  if [ "$STARTUP" = service ]; then
    sudo -H -u "$SERVICE_USER" env XDG_STATE_HOME="$DATA" "$PREFIX/current/rdh" "$@"
  else
    "$PREFIX/current/rdh" "$@"
  fi
}
if [ "$PAIR" -eq 1 ] || [ "$STARTUP" != none ]; then
  step 'checking device pairing'
  run_agent "$@" --pair-only || die 'pairing failed; automatic startup was not registered'
fi

# --- automatic startup --------------------------------------------------------
# Quote values for the shell wrapper, systemd unit, and launchd plist separately.
shell_quote() { printf "'"; printf '%s' "$1" | sed "s/'/'\\\\''/g"; printf "'"; }
unit_quote() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g; s/\$/$$/g'; }
xml_quote() { printf '%s' "$1" | sed 's/\&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g; s/"/\&quot;/g'; }
if [ "$STARTUP" != none ]; then
  # The same paired arguments are used on this launch and subsequent starts.
  wrapper=$PREFIX/run-agent
  {
    printf '#!/bin/sh\n'
    if [ "$STARTUP" = user ]; then
      printf 'export XDG_CONFIG_HOME='; shell_quote "${XDG_CONFIG_HOME:-$HOME/.config}"; printf '\n'
      if [ -n "${XDG_STATE_HOME:-}" ]; then printf 'export XDG_STATE_HOME='; shell_quote "$XDG_STATE_HOME"; printf '\n'; fi
    fi
    if [ -n "$DATA" ]; then printf 'export XDG_STATE_HOME='; shell_quote "$DATA"; printf '\n'; fi
    printf 'cd '; shell_quote "${DATA:-$HOME}"; printf ' || exit 1\n'
    printf 'exec '; shell_quote "$PREFIX/current/rdh"
    for arg in "$@"; do printf ' '; shell_quote "$arg"; done
    printf ' --recover-lock\n'
  } > "$wrapper"
  chmod 755 "$wrapper"
  case $manager in
    systemd)
      mkdir -p "$UNIT_DIR"
      {
        printf '[Unit]\nDescription=Remote Device Hub Agent\nAfter=network-online.target\n\n[Service]\n'
        if [ "$STARTUP" = service ]; then printf 'User=%s\nGroup=%s\n' "$SERVICE_USER" "$service_gid"; fi
        printf 'ExecStart="%s"\n' "$(unit_quote "$wrapper")"
        printf 'Restart=on-failure\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=30\n\n[Install]\n'
        if [ "$STARTUP" = service ]; then printf 'WantedBy=multi-user.target\n'; else printf 'WantedBy=default.target\n'; fi
      } > "$UNIT"
      systemctl_mode daemon-reload
      if ! run_agent startup bind "$STARTUP" rdh.service "$@"; then
        rm -f "$UNIT"
        systemctl_mode daemon-reload
        die 'cannot bind startup management; startup registration was removed'
      fi
      if ! systemctl_mode enable --now rdh; then
        systemctl_mode disable --now rdh || true
        rm -f "$UNIT"
        systemctl_mode daemon-reload
        die 'service start failed; startup registration was removed'
      fi
      if [ "$STARTUP" = user ] && loginctl show-user "$(id -un)" -p Linger 2>/dev/null | grep -q 'Linger=yes'; then
        warn 'this account already has lingering enabled: its user services can run without login; the installer did not change that setting'
      fi
      log "Stop: systemctl $([ "$STARTUP" = service ] || printf '%s' '--user') stop rdh"
      log "Disable automatic startup: systemctl $([ "$STARTUP" = service ] || printf '%s' '--user') disable rdh"
      ;;
    launchd)
      mkdir -p "$(dirname "$PLIST")"
      {
        printf '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict>\n'
        printf '<key>Label</key><string>%s</string>\n' "$LAUNCHD_LABEL"
        printf '<key>ProgramArguments</key><array><string>%s</string></array>\n' "$(xml_quote "$wrapper")"
        printf '<key>WorkingDirectory</key><string>%s</string>\n' "$(xml_quote "${DATA:-$HOME}")"
        if [ "$STARTUP" = service ]; then printf '<key>UserName</key><string>%s</string>\n' "$SERVICE_USER"; fi
        printf '<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n</dict></plist>\n'
      } > "$PLIST"
      chmod 644 "$PLIST"
      if ! run_agent startup bind "$STARTUP" "$LAUNCHD_LABEL" "$@"; then
        rm -f "$PLIST"
        die 'cannot bind startup management; startup registration was removed'
      fi
      if ! launchctl bootstrap "$(launch_domain)" "$PLIST"; then
        rm -f "$PLIST"
        die 'launchd registration failed'
      fi
      log "Stop: launchctl kill SIGTERM $(launch_domain)/$LAUNCHD_LABEL"
      log "Start: launchctl kickstart $(launch_domain)/$LAUNCHD_LABEL"
      log "Stop and disable automatic startup: launchctl bootout $(launch_domain) '$PLIST'; remove '$PLIST'"
      ;;
  esac
fi

# --- verify -------------------------------------------------------------------
step "verifying"
"$BIN_DIR/rdh" --version >/dev/null || die "installed command is not runnable"
if [ -n "$SERVER" ] && have curl; then
  if curl -fsS --max-time 5 "$SERVER/healthz" >/dev/null 2>&1; then
    log "hub reachable: $SERVER/healthz"
  else
    warn "hub health check failed: $SERVER/healthz"
  fi
fi

log ""
log "Remote Device Hub Agent $version installed"
log "  binary:  $BIN_DIR/rdh"
log "  service: $manager"
case $manager in
  systemd) log "  logs:    journalctl --user -u rdh -f" ;;
  launchd) log "  logs:    log show --last 5m --predicate 'process == \"rdh\"'" ;;
esac

if [ "$STARTUP" = none ] && [ "$PAIR" -eq 1 ]; then
  log 'Running this time only. Press Ctrl+C to disconnect; automatic startup is disabled.'
  if [ -n "$TMP" ]; then rm -rf "$TMP"; TMP=; fi
  trap - EXIT INT TERM
  exec "$PREFIX/current/rdh" "$@"
fi
