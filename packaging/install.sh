#!/bin/sh
# Install Remote Device Hub Agent from a release directory.
#
# The bundle carries its own Node runtime, so the target machine needs no
# package manager, no Node and no root: detect platform, fetch the matching
# archive, verify it against SHA256SUMS, unpack into a versioned directory, pair
# the device in the browser, then register a user service. Nothing is written
# outside ~/.local and ~/.config.
set -eu

DEFAULT_BASE="${RDH_BASE:-https://github.com/be-one-click/remote-device-agent/releases/latest/download}"
DEFAULT_PREFIX=$HOME/.local/lib/rdh
DEFAULT_BIN=$HOME/.local/bin

BASE=$DEFAULT_BASE
VERSION=
PREFIX=$DEFAULT_PREFIX
BIN_DIR=$DEFAULT_BIN
SERVICE=auto
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
  --service MODE     systemd | launchd | none | auto (default: auto)
  --server URL       device hub address used for pairing
  --root NAME=PATH   directory to expose; repeatable (default: current dir)
  --no-pair          install only; pair later with "rdh --pair-only"
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
    --service) SERVICE=${2:?--service needs a value}; shift 2 ;;
    --server) SERVER=${2:?--server needs a value}; shift 2 ;;
    --root) ROOTS="$ROOTS
${2:?--root needs a value}"; shift 2 ;;
    --no-pair) PAIR=0; shift ;;
    --no-service) SERVICE=none; shift ;;
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

PREFIX=$(expand_path "$PREFIX")
BIN_DIR=$(expand_path "$BIN_DIR")
UNIT_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
UNIT=$UNIT_DIR/rdh.service
LAUNCHD_LABEL=com.be-one-click.rdh
PLIST=$HOME/Library/LaunchAgents/$LAUNCHD_LABEL.plist
TMP=
trap 'if [ -n "${TMP:-}" ]; then rm -rf "$TMP"; fi' EXIT INT TERM

have() { command -v "$1" >/dev/null 2>&1; }

service_manager() {
  case $SERVICE in
    systemd|launchd|none) printf '%s' "$SERVICE"; return ;;
  esac
  if [ "$(uname -s)" = Darwin ]; then
    if have launchctl; then printf 'launchd'; return; fi
  elif have systemctl && systemctl --user show-environment >/dev/null 2>&1; then
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

stop_service() {
  case $(service_manager) in
    systemd)
      systemctl --user disable --now rdh >/dev/null 2>&1 || true
      rm -f "$UNIT"
      systemctl --user daemon-reload >/dev/null 2>&1 || true
      ;;
    launchd)
      launchctl bootout "gui/$(id -u)/$LAUNCHD_LABEL" >/dev/null 2>&1 || true
      rm -f "$PLIST"
      ;;
  esac
}

if [ "$UNINSTALL" -eq 1 ]; then
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

# --- pair ---------------------------------------------------------------------
if [ "$PAIR" -eq 1 ]; then
  step "pairing this device"
  set -- --pair-only --no-browser
  if [ -n "$SERVER" ]; then set -- "$@" --server "$SERVER"; fi
  while IFS= read -r spec; do
    [ -n "$spec" ] || continue
    set -- "$@" --root "$spec"
  done <<ROOT_SPECS
$ROOTS
ROOT_SPECS
  if ! "$PREFIX/current/rdh" "$@"; then
    warn "pairing did not complete; the installation is usable once paired:"
    warn "  $PREFIX/current/rdh --pair-only --no-browser"
    exit 1
  fi
fi

# --- service ------------------------------------------------------------------
start_service() {
  case $manager in
    systemd)
      step "registering the systemd user service"
      mkdir -p "$UNIT_DIR"
      cat > "$UNIT" <<UNIT_FILE
[Unit]
Description=Remote Device Hub Agent
After=network-online.target

[Service]
ExecStart="$PREFIX/current/rdh"
Restart=on-failure
RestartSec=5
KillMode=mixed
TimeoutStopSec=30

[Install]
WantedBy=default.target
UNIT_FILE
      systemctl --user daemon-reload
      systemctl --user enable --now rdh
      if ! loginctl show-user "$(id -un)" -p Linger 2>/dev/null | grep -q 'Linger=yes'; then
        warn "enable lingering so the agent starts without a login session:"
        warn "  sudo loginctl enable-linger $(id -un)"
      fi
      ;;
    launchd)
      step "registering the launchd agent"
      mkdir -p "$(dirname "$PLIST")"
      cat > "$PLIST" <<PLIST_FILE
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LAUNCHD_LABEL</string>
  <key>ProgramArguments</key><array><string>$PREFIX/current/rdh</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
</dict>
</plist>
PLIST_FILE
      launchctl bootout "gui/$(id -u)/$LAUNCHD_LABEL" >/dev/null 2>&1 || true
      launchctl bootstrap "gui/$(id -u)" "$PLIST"
      ;;
    none)
      step "no service manager detected"
      log "start the agent manually: $PREFIX/current/rdh"
      ;;
  esac
}

if [ "$SERVICE" != none ]; then start_service; fi

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
