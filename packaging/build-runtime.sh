#!/bin/sh
# Build self-contained Agent bundles: the compiled Agent plus a Node runtime, so
# target machines need no Node installation. Artifacts are plain .tar.gz files
# that a one-line installer can unpack into a versioned directory.
#
# This is repackaging, not compilation: the Agent bundle comes from
# `npm run build`, and the runtime comes from the official Node release files
# (or, for musl targets, from the unofficial-builds line). Only `bin/node` and
# `LICENSE` are taken from that archive, so the npm tree never enters our
# artifacts.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
ALL_PLATFORMS="linux-x64-gnu linux-arm64-gnu linux-x64-musl linux-arm64-musl darwin-x64 darwin-arm64 win32-x64 win32-arm64"

OUT=$ROOT/dist
VERSION=
NODE_VERSION=22.21.1
PLATFORMS=$ALL_PLATFORMS
NODE_BINARY=
KEEP_WORK=0

usage() {
  cat <<'USAGE'
usage: packaging/build-runtime.sh [options]

  --version VERSION    release version, with or without a leading "v"
                       (default: package.json version; must match it)
  --node VERSION       Node runtime version to bundle (default: 22.21.1)
  --platform LIST      comma-separated platforms, or "all" (default)
  --out DIR            artifact directory (default: ./dist)
  --node-binary PATH   reuse an existing node binary instead of downloading
                       (single-platform builds only; for local testing)
  --keep-work          keep the staging directory for inspection
  -h, --help           show this help

platforms: linux-x64-gnu linux-arm64-gnu linux-x64-musl
           linux-arm64-musl darwin-x64 darwin-arm64 win32-x64 win32-arm64
USAGE
}

log() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case $1 in
    --version) VERSION=${2:?--version needs a value}; shift 2 ;;
    --node) NODE_VERSION=${2:?--node needs a value}; shift 2 ;;
    --platform) PLATFORMS=${2:?--platform needs a value}; shift 2 ;;
    --out) OUT=${2:?--out needs a value}; shift 2 ;;
    --node-binary) NODE_BINARY=${2:?--node-binary needs a value}; shift 2 ;;
    --keep-work) KEEP_WORK=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

package_version=$(sed -n 's/^ *"version": *"\([^"]*\)".*/\1/p' "$ROOT/package.json" | head -1)
[ -n "$package_version" ] || die "cannot read version from package.json"

if [ -n "$VERSION" ]; then VERSION=${VERSION#v}; else VERSION=$package_version; fi
[ "$VERSION" = "$package_version" ] ||
  die "version $VERSION does not match package.json ($package_version); bump the package first"

[ -f "$ROOT/bundle/agent.mjs" ] || die "bundle/agent.mjs is missing; run: npm run build"
[ -f "$ROOT/bin/remote-device-hub.mjs" ] || die "bin/remote-device-hub.mjs is missing"

if [ "$PLATFORMS" = all ]; then PLATFORMS=$ALL_PLATFORMS; fi
PLATFORMS=$(printf '%s' "$PLATFORMS" | tr ',' ' ')

if [ -n "$NODE_BINARY" ]; then
  [ -x "$NODE_BINARY" ] || die "--node-binary is not executable: $NODE_BINARY"
  count=$(printf '%s\n' $PLATFORMS | wc -l)
  [ "$count" -eq 1 ] || die "--node-binary supports exactly one --platform"
fi

mkdir -p "$OUT"
OUT=$(CDPATH= cd -- "$OUT" && pwd -P)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/rdh-runtime.XXXXXX")
cleanup() { if [ "$KEEP_WORK" -eq 1 ]; then log "staging kept at $WORK"; else rm -rf "$WORK"; fi; }
trap cleanup EXIT INT TERM

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "need sha256sum or shasum"; fi
}

bytes_of() { wc -c < "$1" | tr -d ' '; }

platform_parts() {
  case $1 in
    linux-x64-gnu)    echo "linux x64 glibc" ;;
    linux-arm64-gnu)  echo "linux arm64 glibc" ;;
    linux-x64-musl)   echo "linux x64 musl" ;;
    linux-arm64-musl) echo "linux arm64 musl" ;;
    darwin-x64)       echo "darwin x64 none" ;;
    darwin-arm64)     echo "darwin arm64 none" ;;
    win32-x64)       echo "win x64 none" ;;
    win32-arm64)     echo "win arm64 none" ;;
    *) die "unknown platform: $1" ;;
  esac
}

node_base_url() {
  if [ "$2" = musl ]; then
    printf 'https://unofficial-builds.nodejs.org/download/release/v%s' "$NODE_VERSION"
  else
    printf 'https://nodejs.org/dist/v%s' "$NODE_VERSION"
  fi
}

node_asset_candidates() {
  if [ "$1" = win ]; then
    printf 'node-v%s-win-%s.zip\n' "$NODE_VERSION" "$2"
  elif [ "$3" = musl ]; then
    printf 'node-v%s-linux-%s-musl.tar.xz\n' "$NODE_VERSION" "$2"
    printf 'node-v%s-linux-%s-musl.tar.gz\n' "$NODE_VERSION" "$2"
  else
    printf 'node-v%s-%s-%s.tar.xz\n' "$NODE_VERSION" "$1" "$2"
    printf 'node-v%s-%s-%s.tar.gz\n' "$NODE_VERSION" "$1" "$2"
  fi
}

fetch() {
  # These are repeatable artifact GETs written to a file. HTTP/2 stream errors
  # are not included in curl's default transient-error retry set. The archive
  # is still checked against the downloaded SHA-256 list before extraction.
  curl -fsSL --retry 3 --retry-all-errors --retry-delay 2 --connect-timeout 15 --max-time 1800 -o "$2" "$1" ||
    die "download failed: $1"
}

obtain_runtime() {
  os=$1; arch=$2; libc=$3
  base=$(node_base_url "$os" "$libc")
  sums=$WORK/SHASUMS256.txt
  fetch "$base/SHASUMS256.txt" "$sums"

  ASSET=
  for candidate in $(node_asset_candidates "$os" "$arch" "$libc"); do
    if grep -Eq " [ *]$candidate$" "$sums"; then ASSET=$candidate; break; fi
  done
  [ -n "$ASSET" ] || die "no Node $NODE_VERSION archive for $os-$arch ($libc)"

  PKG=$WORK/$ASSET
  fetch "$base/$ASSET" "$PKG"
  ( cd "$WORK" && grep -E " [ *]$ASSET$" "$sums" | sha256sum -c - ) ||
    die "checksum mismatch for $ASSET"
}

build_platform() {
  platform=$1
  set -- $(platform_parts "$platform")
  os=$1; arch=$2; libc=$3

  stage=$WORK/stage
  rm -rf "$stage"; mkdir -p "$stage"
  name=rdh-$VERSION-$platform
  tree=$stage/$name
  mkdir -p "$tree/bin" "$tree/bundle"

  runtime=node
  [ "$os" != win ] || runtime=node.exe
  if [ -n "$NODE_BINARY" ]; then
    cp "$NODE_BINARY" "$tree/$runtime"
  else
    obtain_runtime "$os" "$arch" "$libc"
    if [ "$os" = win ]; then
      prefix=node-v$NODE_VERSION-win-$arch
      unzip -p "$PKG" "$prefix/node.exe" > "$tree/node.exe"
      unzip -p "$PKG" "$prefix/LICENSE" > "$tree/LICENSE.node"
    else
      prefix=$(tar -tf "$PKG" | sed -n '1p' | cut -d/ -f1)
      tar -xf "$PKG" -C "$WORK" --strip-components=1 "$prefix/bin/node" "$prefix/LICENSE"
      mv "$WORK/bin/node" "$tree/node"
      mv "$WORK/LICENSE" "$tree/LICENSE.node"
      rmdir "$WORK/bin"
    fi
    rm -f "$PKG" "$WORK/SHASUMS256.txt"
  fi

  cp "$ROOT/package.json" "$tree/package.json"
  cp "$ROOT/bundle/agent.mjs" "$tree/bundle/agent.mjs"
  cp "$ROOT/bin/remote-device-hub.mjs" "$tree/bin/remote-device-hub.mjs"
  cp "$ROOT/THIRD_PARTY_LICENSES" "$tree/THIRD_PARTY_LICENSES"
  if [ -f "$ROOT/LICENSE" ]; then cp "$ROOT/LICENSE" "$tree/LICENSE"; fi
  if [ -f "$ROOT/README.md" ]; then cp "$ROOT/README.md" "$tree/README.md"; fi

  if [ "$os" = win ]; then
    host=${RDH_SERVICE_HOSTS:?RDH_SERVICE_HOSTS must contain native Windows service hosts}/service-host-$arch
    [ -f "$host/shawl.exe" ] || die "missing native service host: $host/shawl.exe"
    cp "$host/shawl.exe" "$tree/shawl.exe"
    cp "$ROOT/packaging/windows-logon.ps1" "$tree/windows-logon.ps1"
    cp "$host/shawl-v1.9.0-legal.txt" "$tree/SHAWL_LICENSES.txt"
    printf '@echo off\r\n"%%~dp0node.exe" "%%~dp0bin\\remote-device-hub.mjs" %%*\r\nexit /b %%errorlevel%%\r\n' > "$tree/rdh.cmd"
  else
  cat > "$tree/rdh" <<'WRAPPER'
#!/bin/sh
# Run the bundled Agent with the bundled runtime, independent of PATH.
set -eu
target=$0
while [ -L "$target" ]; do
  link=$(readlink "$target")
  case $link in
    /*) target=$link ;;
    *) target=${target%/*}/$link ;;
  esac
done
dir=$(CDPATH= cd -- "${target%/*}" && pwd -P)
exec "$dir/node" "$dir/bin/remote-device-hub.mjs" "$@"
WRAPPER
  fi
  # Normalise modes so the artifact does not inherit the builder's umask: a
  # 077 umask would otherwise ship files that other local users cannot read.
  find "$tree" -type d -exec chmod 755 {} +
  find "$tree" -type f -exec chmod 644 {} +
  chmod 755 "$tree/$runtime" "$tree/bin/remote-device-hub.mjs"
  if [ "$os" = win ]; then
    artifact=$OUT/$name.zip
    ( cd "$stage" && zip -qr "$WORK/$name.zip" "$name" )
    mv "$WORK/$name.zip" "$artifact"
  else
    chmod 755 "$tree/rdh"
    artifact=$OUT/$name.tar.gz
    ( cd "$stage" && tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -cf - "$name" ) |
      gzip -9 > "$artifact"
  fi

  size=$(bytes_of "$artifact")
  digest=$(hash_file "$artifact")
  printf '%s  %s\n' "$digest" "$(basename "$artifact")" >> "$WORK/SHA256SUMS"
  printf '%s\t%s\t%s\t%s\n' "$platform" "$(basename "$artifact")" "$size" "$digest" >> "$WORK/artifacts.tsv"
  log "$(printf '%-18s %7.1f MB  %s...' "$platform" "$(awk -v b="$size" 'BEGIN{print b/1048576}')" "$(echo "$digest" | cut -c1-16)")"
}

log "building Agent $VERSION with Node $NODE_VERSION"
for platform in $PLATFORMS; do build_platform "$platform"; done

mv "$WORK/SHA256SUMS" "$OUT/SHA256SUMS"

manifest=$OUT/rdh-manifest.json
commit=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)
dirty=false
[ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null)" ] && dirty=true
{
  printf '{\n'
  printf '  "version": "%s",\n' "$VERSION"
  printf '  "node": "%s",\n' "$NODE_VERSION"
  printf '  "built_at": "%s",\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '  "commit": "%s",\n' "$commit"
  printf '  "dirty": %s,\n' "$dirty"
  printf '  "artifacts": [\n'
  first=1
  while IFS="$(printf '\t')" read -r platform file size digest; do
    [ "$first" -eq 1 ] || printf ',\n'
    first=0
    printf '    { "platform": "%s", "file": "%s", "bytes": %s, "sha256": "%s" }' \
      "$platform" "$file" "$size" "$digest"
  done < "$WORK/artifacts.tsv"
  printf '\n  ]\n}\n'
} > "$manifest"

log ""
log "artifacts in $OUT:"
ls -1 "$OUT" | sed 's/^/  /'
log ""
log "wrote $manifest and $OUT/SHA256SUMS"
