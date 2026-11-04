# Agent distribution

The Agent is a standalone Rust executable. Native archives contain the executable,
provenance metadata and dependency licenses. Bootstrap scripts install one binary;
`rdh update` replaces the executable in place while retaining local binding and
startup settings.

| Platform suffix | Rust target | Archive |
| --- | --- | --- |
| `linux-x64-gnu` | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| `linux-arm64-gnu` | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| `linux-x64-musl` | `x86_64-unknown-linux-musl` | `.tar.gz` |
| `linux-arm64-musl` | `aarch64-unknown-linux-musl` | `.tar.gz` |
| `darwin-x64` | `x86_64-apple-darwin` | `.tar.gz` |
| `darwin-arm64` | `aarch64-apple-darwin` | `.tar.gz` |
| `win32-x64` | `x86_64-pc-windows-msvc` | `.zip` |
| `win32-arm64` | `aarch64-pc-windows-msvc` | `.zip` |

Raw assets are named `rdh-VERSION-PLATFORM`, followed by `.exe` on Windows. The
release includes `SHA256SUMS`. A matching OS, CPU and libc are required; the
existence of a cross-compiled artifact does not establish native runtime support.

## Bootstrap installation

The [README](../README.md#install-with-a-script) contains the download commands.
The scripts select the newest published GitHub release, including prereleases,
verify the checksum and executable version, and install:

- Linux/macOS: `$HOME/.local/bin/rdh`.
- Windows: `%LOCALAPPDATA%\RemoteDeviceAgent\rdh.exe`.

Set `RDH_INSTALL_DIR` to an absolute directory to change the destination. Existing
command files are preserved; update an installed executable with `rdh update`.
The installer can use an existing destination directory containing unrelated
files. It does not create version directories or command aliases.

Use `RDH_VERSION` to select a release, `RDH_BASE` for an HTTPS mirror or local
artifact directory, and `RDH_LIBC=gnu` for the GNU build on Linux. Linux otherwise
uses musl. Explicit installer options override their corresponding environment
variables. Without an explicit version or offline binary, version selection still
uses GitHub even when `RDH_BASE` is set.

On POSIX, the command directory is saved in the user's shell startup files;
`RDH_SHELL_RC` selects a custom file. New terminals load that PATH. The installer
cannot change its parent shell's environment. Windows updates the user PATH and
current PowerShell process. Existing PATH entries are retained.

`--no-start` / `-NoStart`, or `RDH_NO_START=1`, disables automatic launch.
`--no-modify-path` / `-NoModifyPath`, or `RDH_NO_MODIFY_PATH=1`, disables PATH changes.
Use both for unattended installation. When the POSIX script is piped to `sh`, it
reopens the controlling terminal for the first-run directory prompt. Without a
terminal it completes installation and asks the user to run `rdh` interactively.
The Agent chooses its shared directory and pairs when run; the installer does not
select roots or approve permissions.

### Offline artifacts and mirrors

Obtain the binary for the intended platform together with its `SHA256SUMS` file.
A per-binary sibling `.sha256` is accepted when `SHA256SUMS` is absent. Replace the
version and absolute paths below with the actual artifact and destination.

For Linux x64 musl:

```sh
RDH_INSTALL_DIR=/absolute/new-bin sh packaging/install.sh \
  --offline /absolute/artifacts/rdh-VERSION-linux-x64-musl --no-start
```

For a GNU Linux binary, add `--libc gnu`. On macOS use the matching `darwin`
filename. For Windows x64:

```powershell
$env:RDH_INSTALL_DIR = 'C:\Path\NewInstall'
.\packaging\install.ps1 -Offline 'C:\Artifacts\rdh-VERSION-win32-x64.exe' -NoStart
```

For an HTTPS mirror, use `--version VERSION --base HTTPS_URL` or
`-Version VERSION -Base HTTPS_URL`. The scripts require HTTPS for network downloads
and redirects. They verify the selected filename, size, SHA-256 and reported
version before publishing the command. Checksums from the same source establish
content consistency; access to the artifact source remains part of the trust
boundary.

## Executable updates

`rdh update` uses the GitHub Releases API and raw binary assets from
`easelian/remote-device-agent`. It downloads `SHA256SUMS` and the matching platform
binary from the selected release. Its release location is compiled into
[self_update.rs](../src/self_update.rs). Installer mirror variables do not change
that update source.

The Agent must be stopped, and a pending unbind must finish first. Replacement
retains the executable path so recorded startup entries continue to refer to it.
After success, start `rdh` manually. See [Maintenance](maintenance.md#update-the-agent)
for failure handling and platform limits.

## Prepare a release

[prepare-release.cjs](../packaging/prepare-release.cjs) assembles the eight native
platform artifacts after checking their versions, source commits, inventories,
checksums and licenses. It includes raw binaries, archives, bootstrap scripts,
an npm tarball and `SHA256SUMS`. Additional generated metadata is not the source
used by `rdh update`.

The npm package contains native binaries and a Node launcher that selects the
matching platform and forwards the public `rdh` commands. Running that package
requires Node 22.14 or later. Native binaries and bootstrap installations do not
require Node or npm. Installing an npm package does not perform device pairing;
run `rdh` afterward in an interactive terminal.

The [release workflow](../.github/workflows/release.yml) checks the tag against the
workspace version, invokes native CI, assembles and attests artifacts, then creates
a draft release. Review the actual native test results, artifacts and version
before publication. The manifest's `release_ready: false` field does not indicate
publication status.

The separate [npm workflow](../.github/workflows/npm-publish.yml) consumes a
published release tag and verifies the selected tarball before registry
publication. Artifact generation, attestation and draft creation do not establish
that a release was published, deployed, platform-signed or notarized.
