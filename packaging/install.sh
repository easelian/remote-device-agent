#!/bin/sh
# Download and verify one native executable; Rust owns installation.
set -eu
umask 077

rdh_base=${RDH_BASE:-}
rdh_version=${RDH_VERSION:-}
rdh_offline=
rdh_libc=${RDH_LIBC:-musl}
rdh_command=install-native
rdh_temporary=
rdh_no_start=${RDH_NO_START:-0}
rdh_no_modify_path=${RDH_NO_MODIFY_PATH:-0}

usage() {
    cat <<'USAGE'
usage: install.sh [--version VERSION] [--base HTTPS_URL|DIRECTORY] [--libc musl|gnu]
                  [--no-start] [--no-modify-path] [-- NATIVE_ARGUMENTS...]
       install.sh --offline NATIVE_BINARY -- NATIVE_ARGUMENTS...

The native binary must have a matching entry in SHA256SUMS, or a per-binary sibling
.sha256 file. Linux defaults to musl; --libc gnu selects glibc. No Node/npm is used.

With no arguments, install the newest published GitHub release (including
prereleases) into $HOME/.local/lib/rdh.
RDH_VERSION, RDH_BASE, RDH_LIBC and RDH_INSTALL_DIR override these defaults.
Installation links rdh, remote-device-agent and remote-device-hub in
$HOME/.local/bin. RDH_BIN_DIR selects a different absolute command directory.
Existing command paths must be links to this installation's current/rdh.
The command directory is added to the user's shell configuration, then the Agent
starts in the foreground. New terminals load the saved PATH.
Use --no-start or RDH_NO_START=1 to install without launching the Agent.
Use --no-modify-path or RDH_NO_MODIFY_PATH=1 to leave PATH and shell files alone;
command links are still created.
RDH_SHELL_RC selects a custom shell startup file instead of the default files.

By default arguments after -- are passed to rdh install-native. Use
--service to call rdh install-service, using its explicit service account options.
These commands retain the native CLI's ownership, stopped-profile and recovery
checks. Service installation does not change PATH or start the service. Existing
installations are not overwritten. Device pairing requires browser approval.

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
        --no-start) rdh_no_start=1; shift ;;
        --no-modify-path) rdh_no_modify_path=1; shift ;;
        --) shift; break ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown bootstrap option: $1; native options follow --" ;;
    esac
done
case $rdh_no_start:$rdh_no_modify_path in 0:0|0:1|1:0|1:1) ;; *) die 'RDH_NO_START and RDH_NO_MODIFY_PATH must be 0 or 1' ;; esac
rdh_default_prefix=
if [ "$#" -eq 0 ]; then
    [ "$rdh_command" = install-native ] || die 'supply service installation arguments after --'
    if [ -n "${RDH_INSTALL_DIR:-}" ]; then
        rdh_default_prefix=$RDH_INSTALL_DIR
    else
        [ -n "${HOME:-}" ] || die 'HOME is unset; set RDH_INSTALL_DIR or pass -- --prefix PATH'
        rdh_default_prefix=$HOME/.local/lib/rdh
    fi
    set -- --prefix "$rdh_default_prefix"
fi
rdh_install_prefix=
rdh_config=
rdh_previous=
for rdh_argument do
    case $rdh_previous in
        --prefix) rdh_install_prefix=$rdh_argument ;;
        --config) rdh_config=$rdh_argument ;;
    esac
    rdh_previous=$rdh_argument
done
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
                --connect-timeout 15 --max-time 300 --max-filesize 268435456 \
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
[ "$(wc -c < "$rdh_temporary/$rdh_name")" -le 268435456 ] || die 'native executable is too large'
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

check_command_links() {
    for rdh_entry_name in rdh remote-device-agent remote-device-hub; do
        rdh_entry=$rdh_bin_dir/$rdh_entry_name
        if [ -L "$rdh_entry" ]; then
            [ "$(readlink "$rdh_entry")" = "$rdh_command_target" ] \
                || die "command path points to another target: $rdh_entry"
        elif [ -e "$rdh_entry" ]; then
            die "command path already exists: $rdh_entry"
        fi
    done
}

if [ "$rdh_command" = install-native ] && [ -n "$rdh_install_prefix" ]; then
    case $rdh_install_prefix in /*) ;; *) die 'native installation prefix must be absolute' ;; esac
    if [ -n "${RDH_BIN_DIR:-}" ]; then
        rdh_bin_dir=$RDH_BIN_DIR
    else
        [ -n "${HOME:-}" ] || die 'HOME is unset; set RDH_BIN_DIR'
        rdh_bin_dir=$HOME/.local/bin
    fi
    case $rdh_bin_dir in /*) ;; *) die 'RDH_BIN_DIR must be absolute' ;; esac
    rdh_install_parent=$(dirname "$rdh_install_prefix")
    mkdir -p "$rdh_install_parent"
    rdh_install_prefix=$(cd "$rdh_install_parent" && pwd -P)/$(basename "$rdh_install_prefix")
    rdh_command_target=$rdh_install_prefix/current/rdh
    mkdir -p "$rdh_bin_dir"
    rdh_bin_dir=$(cd "$rdh_bin_dir" && pwd -P)
    if [ "$rdh_no_modify_path" = 0 ]; then
        case $rdh_bin_dir in *:*|*'
'*) die 'the command directory cannot contain a colon or newline when updating PATH' ;; esac
    fi
    check_command_links
fi

"$rdh_temporary/$rdh_name" "$rdh_command" "$@"
rm -rf "$rdh_temporary"
rdh_temporary=

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

if [ "$rdh_command" = install-native ] && [ -n "$rdh_install_prefix" ]; then
    check_command_links
    for rdh_entry_name in rdh remote-device-agent remote-device-hub; do
        rdh_entry=$rdh_bin_dir/$rdh_entry_name
        if [ ! -L "$rdh_entry" ]; then
            ln -s "$rdh_command_target" "$rdh_entry"
        fi
    done
    printf 'Commands linked in %s\n' "$rdh_bin_dir"
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
        printf 'Starting Remote Device Hub. Press Ctrl+C to stop.\n'
        if [ -n "$rdh_config" ]; then
            exec "$rdh_bin_dir/rdh" --config "$rdh_config"
        fi
        exec "$rdh_bin_dir/rdh"
    fi
fi
