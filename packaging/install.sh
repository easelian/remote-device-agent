#!/bin/sh
# Install one verified standalone executable for the current user.
set -eu
umask 077

rdh_base=${RDH_BASE:-}
rdh_version=${RDH_VERSION:-}
rdh_offline=
rdh_libc=${RDH_LIBC:-musl}
rdh_temporary=
rdh_no_start=${RDH_NO_START:-0}
rdh_no_modify_path=${RDH_NO_MODIFY_PATH:-0}

usage() {
    cat <<'USAGE'
usage: install.sh [--version VERSION] [--base HTTPS_URL|DIRECTORY]
                  [--offline BINARY] [--libc musl|gnu] [--no-start] [--no-modify-path]

Install the newest published GitHub release (including prereleases) as
$HOME/.local/bin/rdh, verify its SHA256SUMS entry and version, and launch rdh.
RDH_INSTALL_DIR overrides the absolute installation directory.
RDH_VERSION, RDH_BASE and RDH_LIBC override release defaults (Linux: musl).
RDH_NO_START=1 disables launch; RDH_NO_MODIFY_PATH=1 leaves shell files unchanged.
RDH_SHELL_RC selects a custom shell startup file. Existing rdh files are preserved;
use rdh update to update an installed executable.
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
        --no-start) rdh_no_start=1; shift ;;
        --no-modify-path) rdh_no_modify_path=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown installer option: $1" ;;
    esac
done
case $rdh_no_start:$rdh_no_modify_path in 0:0|0:1|1:0|1:1) ;; *) die 'RDH_NO_START and RDH_NO_MODIFY_PATH must be 0 or 1' ;; esac
rdh_bin_dir=${RDH_INSTALL_DIR:-${HOME:?HOME is unset; set RDH_INSTALL_DIR}/.local/bin}
case $rdh_bin_dir in /*) ;; *) die 'RDH_INSTALL_DIR must be absolute' ;; esac
if [ "$rdh_no_modify_path" = 0 ]; then
    case $rdh_bin_dir in *:*|*'
'*) die 'the command directory cannot contain a colon or newline when updating PATH' ;; esac
fi
[ ! -e "$rdh_bin_dir/rdh" ] && [ ! -L "$rdh_bin_dir/rdh" ] || die 'command path already exists; use rdh update'
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
if [ -z "$rdh_version" ]; then
    have curl || die 'curl is required to find the latest release'
    rdh_release=$(curl --fail --show-error --silent --location --proto '=https' --proto-redir '=https' \
        --connect-timeout 15 --max-time 30 --max-filesize 1048576 \
        -H 'Accept: application/vnd.github+json' \
        'https://api.github.com/repos/easelian/remote-device-agent/releases?per_page=1') \
        || die 'could not find the latest release; retry or specify --version'
    rdh_tag=$(printf '%s\n' "$rdh_release" | tr '\n' ' ' | \
        sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    case $rdh_tag in
        v*) rdh_version=${rdh_tag#v} ;;
        *) die 'the latest release has no supported version tag' ;;
    esac
fi
printf '%s\n' "$rdh_version" | LC_ALL=C awk '
    /^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$/ { valid=1 }
    END { exit !valid }' || die 'invalid version'
case $rdh_version in *'
'*) die 'invalid version' ;; esac
rdh_base=${rdh_base:-https://github.com/easelian/remote-device-agent/releases/download/v$rdh_version}
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
            if [ "$rdh_fetch_name" = "$rdh_name" ]; then
                printf 'Downloading Remote Device Hub %s (%s)...\n' "$rdh_version" "$rdh_platform" >&2
                rdh_progress=--progress-bar
            else
                rdh_progress=--silent
            fi
            curl --fail --show-error "$rdh_progress" --location --proto '=https' --proto-redir '=https' \
                --connect-timeout 15 --max-time 300 --max-filesize 134217728 \
                --output "$rdh_fetch_destination" "${rdh_base%/}/$rdh_fetch_name"
            ;;
        *://*) die '--base must use HTTPS or a local directory' ;;
        *) cp "$rdh_base/$rdh_fetch_name" "$rdh_fetch_destination" ;;
    esac
}
if [ -n "$rdh_offline" ]; then
    rdh_checksum_dir=${rdh_offline%/*}
    if [ -f "$rdh_checksum_dir/SHA256SUMS" ]; then
        cp "$rdh_checksum_dir/SHA256SUMS" "$rdh_temporary/checksum"
    else
        cp "$rdh_offline.sha256" "$rdh_temporary/checksum"
    fi
    cp "$rdh_offline" "$rdh_temporary/$rdh_name"
else
    if ! fetch SHA256SUMS "$rdh_temporary/checksum"; then
        fetch "$rdh_name.sha256" "$rdh_temporary/checksum"
    fi
    fetch "$rdh_name" "$rdh_temporary/$rdh_name"
fi
[ "$(wc -c < "$rdh_temporary/checksum")" -le 65536 ] || die 'checksum record is too large'
[ "$(wc -c < "$rdh_temporary/$rdh_name")" -le 134217728 ] || die 'native executable is too large'
rdh_expected=$(LC_ALL=C awk -v name="$rdh_name" '
    NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/ { invalid=1 }
    $2 == name { count++; hash=$1 }
    END { if (invalid || count != 1) exit 1; print hash }' "$rdh_temporary/checksum") || die 'invalid checksum record'
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

# Stage beside the destination so publication cannot expose a partial copy.
mkdir -p "$rdh_bin_dir"
rdh_stage=$(mktemp -d "$rdh_bin_dir/.rdh-install.XXXXXXXX") || die 'cannot stage executable'
trap '[ -z "$rdh_temporary" ] || rm -rf "$rdh_temporary"; [ -z "$rdh_stage" ] || rm -rf "$rdh_stage"' 0
cp "$rdh_temporary/$rdh_name" "$rdh_stage/rdh"
chmod 700 "$rdh_stage/rdh"
# A hard link publishes atomically and refuses to overwrite an existing entry.
ln "$rdh_stage/rdh" "$rdh_bin_dir/rdh" || die 'command path already exists or cannot be installed'
rm -rf "$rdh_stage" "$rdh_temporary"
rdh_stage=
rdh_temporary=
printf 'Installed %s/rdh\n' "$rdh_bin_dir"

append_path() {
    rdh_rc=$1
    if [ -e "$rdh_rc" ] || [ -L "$rdh_rc" ]; then
        [ -f "$rdh_rc" ] || die "shell startup path is not a regular file: $rdh_rc"
    else
        mkdir -p "$(dirname "$rdh_rc")"
    fi
    if [ ! -f "$rdh_rc" ] || ! grep -Fqx "$rdh_path_line" "$rdh_rc"; then
        printf '\n%s\n' "$rdh_path_line" >> "$rdh_rc"
    fi
    printf 'PATH configured in %s\n' "$rdh_rc"
}

if [ "$rdh_no_modify_path" = 0 ]; then
    rdh_escaped=$(printf '%s' "$rdh_bin_dir" | sed "s/'/'\\\\''/g")
    rdh_path_line="case \":\${PATH-}:\" in *:'$rdh_escaped':*) ;; *) export PATH='$rdh_escaped':\"\${PATH-}\" ;; esac # Remote Device Hub"
    rdh_shell=${SHELL:-sh}
    rdh_shell=${rdh_shell##*/}
    if [ "$rdh_shell" = fish ]; then
        rdh_escaped=$(printf '%s' "$rdh_bin_dir" | sed "s/\\\\/\\\\\\\\/g; s/'/\\\\'/g")
        rdh_path_line="if not contains -- '$rdh_escaped' \$PATH; set -gx PATH '$rdh_escaped' \$PATH; end # Remote Device Hub"
    fi
    if [ -n "${RDH_SHELL_RC:-}" ]; then
        append_path "$RDH_SHELL_RC"
    else
        [ -n "${HOME:-}" ] || die 'HOME is unset; set RDH_SHELL_RC or use --no-modify-path'
        case $rdh_shell in
            zsh)
                append_path "${ZDOTDIR:-$HOME}/.zprofile"
                append_path "${ZDOTDIR:-$HOME}/.zshrc"
                ;;
            fish)
                append_path "${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
                ;;
            *)
                if [ "$rdh_shell" = bash ]; then append_path "$HOME/.bashrc"; fi
                if [ -f "$HOME/.bash_profile" ] && [ "$rdh_shell" = bash ]; then
                    append_path "$HOME/.bash_profile"
                elif [ -f "$HOME/.bash_login" ] && [ "$rdh_shell" = bash ]; then
                    append_path "$HOME/.bash_login"
                else
                    append_path "$HOME/.profile"
                fi
                ;;
        esac
    fi
    case ":${PATH-}:" in *:"$rdh_bin_dir":*) ;; *) PATH=$rdh_bin_dir:${PATH-}; export PATH ;; esac
    printf 'PATH saved for new terminals.\n'
fi
if [ "$rdh_no_start" = 0 ]; then
    if [ -t 0 ]; then
        printf 'Starting Remote Device Hub. Press Ctrl+C to stop.\n'
        exec "$rdh_bin_dir/rdh"
    elif ( : < /dev/tty ) >/dev/null 2>&1; then
        # A piped installer must give the directory prompt the user's terminal.
        printf 'Starting Remote Device Hub. Press Ctrl+C to stop.\n'
        exec "$rdh_bin_dir/rdh" < /dev/tty
    else
        printf 'Installation complete. Run rdh in an interactive terminal to choose the shared directory and connect.\n'
    fi
fi
