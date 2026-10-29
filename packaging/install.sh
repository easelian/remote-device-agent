#!/bin/sh
# Download and verify one native executable; Rust owns installation.
set -eu
umask 077

rdh_base=${RDH_BASE:-https://github.com/easelian/remote-device-agent/releases/latest/download}
rdh_version=
rdh_offline=
rdh_libc=musl
rdh_command=install-native
rdh_temporary=

usage() {
    cat <<'USAGE'
usage: install.sh --version VERSION [--base HTTPS_URL|DIRECTORY] [--libc musl|gnu] -- NATIVE_ARGUMENTS...
       install.sh --offline NATIVE_BINARY -- NATIVE_ARGUMENTS...

The native binary must have a matching .sha256 file. Linux defaults to the
static musl artifact; --libc gnu selects the glibc artifact. No Node/npm is used.

By default arguments after -- are passed to rdh install-native. Use
--service to call rdh install-service, using its explicit service account options.
These commands retain the native CLI's ownership, stopped-profile and recovery
checks. They do not pair a device, start a service or overwrite an installation.

Examples (replace the version and absolute paths):
  sh install.sh --version VERSION -- --prefix /absolute/new-install
  sh install.sh --offline /absolute/rdh-VERSION-linux-x64-musl -- --prefix /absolute/new-install --config /absolute/paired.json
  sudo sh install.sh --version VERSION --service -- --prefix /absolute/new-program --config /absolute/paired.json --state-directory /absolute/state --service-user USER --name rdh.service
USAGE
}
die() { printf '%s\n' "Remote Device Hub installer: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
value() { [ "$#" -ge 2 ] && [ -n "$2" ] || die "$1 requires a value"; }
while [ "$#" -gt 0 ]; do
    case $1 in
        --base) value "$@"; rdh_base=$2; shift 2 ;;
        --version) value "$@"; rdh_version=${2#v}; shift 2 ;;
        --offline) value "$@"; rdh_offline=$2; shift 2 ;;
        --libc) value "$@"; rdh_libc=$2; shift 2 ;;
        --service) rdh_command=install-service; shift ;;
        --) shift; break ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown bootstrap option: $1; native options follow --" ;;
    esac
done
[ "$#" -gt 0 ] || die 'supply native installation arguments after --'
case $rdh_libc in musl|gnu) ;; *) die '--libc must be musl or gnu' ;; esac
rdh_system=$(uname -s)
rdh_arch=$(uname -m)
case $rdh_arch in x86_64|amd64) rdh_arch=x64 ;; arm64|aarch64) rdh_arch=arm64 ;; *) die 'unsupported CPU architecture' ;; esac
case $rdh_system in
    Linux) rdh_platform=linux-$rdh_arch-$rdh_libc ;;
    Darwin) rdh_platform=darwin-$rdh_arch ;;
    *) die 'use install.ps1 on Windows; other operating systems are unsupported' ;;
esac
if [ -n "$rdh_offline" ]; then
    case $rdh_offline in /*) ;; *) die '--offline requires an absolute path' ;; esac
    rdh_name=${rdh_offline##*/}
    rdh_detected=${rdh_name#rdh-}
    rdh_detected=${rdh_detected%-$rdh_platform}
    [ "$rdh_name" = "rdh-$rdh_detected-$rdh_platform" ] || die 'offline binary name does not match this platform'
    [ -z "$rdh_version" ] || [ "$rdh_version" = "$rdh_detected" ] || die 'offline binary version differs from --version'
    rdh_version=$rdh_detected
fi
[ -n "$rdh_version" ] || die '--version is required when downloading'
printf '%s\n' "$rdh_version" | LC_ALL=C awk '
    /^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$/ { valid=1 }
    END { exit !valid }' || die 'invalid version'
case $rdh_version in *'
'*) die 'invalid version' ;; esac
rdh_name=rdh-$rdh_version-$rdh_platform
rdh_temporary=$(mktemp -d "${TMPDIR:-/tmp}/rdh-install.XXXXXXXX") || die 'cannot create temporary directory'
trap 'if [ -n "$rdh_temporary" ]; then rm -rf "$rdh_temporary"; fi' 0
trap 'exit 130' INT
trap 'exit 143' TERM
fetch() {
    rdh_fetch_name=$1
    rdh_fetch_destination=$2
    case $rdh_base in
        https://*)
            have curl || die 'curl is required for HTTPS downloads'
            curl --fail --show-error --silent --location --proto '=https' --proto-redir '=https' \
                --connect-timeout 15 --max-time 300 --max-filesize 268435456 \
                --output "$rdh_fetch_destination" "${rdh_base%/}/$rdh_fetch_name"
            ;;
        *://*) die '--base must use HTTPS or a local directory' ;;
        *) cp "$rdh_base/$rdh_fetch_name" "$rdh_fetch_destination" ;;
    esac
}
if [ -n "$rdh_offline" ]; then
    cp "$rdh_offline.sha256" "$rdh_temporary/checksum"
    cp "$rdh_offline" "$rdh_temporary/$rdh_name"
else
    fetch "$rdh_name.sha256" "$rdh_temporary/checksum"
    fetch "$rdh_name" "$rdh_temporary/$rdh_name"
fi
[ "$(wc -c < "$rdh_temporary/checksum")" -le 1024 ] || die 'checksum record is too large'
[ "$(wc -c < "$rdh_temporary/$rdh_name")" -le 268435456 ] || die 'native executable is too large'
rdh_expected=$(LC_ALL=C awk -v name="$rdh_name" '
    NR == 1 && NF == 2 && length($1) == 64 && $1 !~ /[^0-9a-f]/ && $2 == name { hash=$1 }
    END { if (NR != 1 || hash == "") exit 1; print hash }' "$rdh_temporary/checksum") || die 'invalid checksum record'
if have sha256sum; then
    rdh_actual=$(sha256sum "$rdh_temporary/$rdh_name")
elif have shasum; then
    rdh_actual=$(shasum -a 256 "$rdh_temporary/$rdh_name")
else
    die 'sha256sum or shasum is required'
fi
rdh_actual=${rdh_actual%% *}
[ "$rdh_actual" = "$rdh_expected" ] || die 'native executable checksum mismatch'
chmod 700 "$rdh_temporary/$rdh_name"
rdh_reported=$("$rdh_temporary/$rdh_name" --version) || die 'native executable self-check failed'
[ "$rdh_reported" = "$rdh_version" ] || die 'native executable version mismatch'
"$rdh_temporary/$rdh_name" "$rdh_command" "$@"
