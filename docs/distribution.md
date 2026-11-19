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
command files are preserved by default; use `rdh update` for normal updates or
Windows installer `-Update` for recovery.
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
nine npm tarballs and `SHA256SUMS`. Additional generated metadata is not the source
used by `rdh update`.

Install the stable npm release with `npm install -g @easelian/remote-device-agent`.
Use `npm install -g @easelian/remote-device-agent@next` for prereleases. The main
package contains a Node launcher. npm installs the optional native build for
the current operating system and CPU, with separate glibc and musl builds on
Linux. The Linux GNU builds for x64 and ARM64 target glibc 2.35 or later.
The npm package requires Node 22.14 or later. The native binaries and
bootstrap installers run independently of Node and npm. Run `rdh` in an
interactive terminal to pair the device after installation.

The [release workflow](../.github/workflows/release.yml) checks the tag against the
workspace version, invokes native CI, assembles and attests artifacts, then creates
a draft release. Review the actual native test results, artifacts and version
before publication. The manifest's `release_ready: false` field does not indicate
publication status.

The [npm workflow](../.github/workflows/npm-publish.yml) runs when a GitHub
Release is published. To retry a split-package release, select the release tag
in the manual workflow and pass the same tag as input. The workflow verifies
all nine tarballs against the release checksums and GitHub attestations. It
publishes the eight platform versions first and verifies their registry
integrities before publishing the launcher to `latest` or `next`. A retry
reuses published versions with matching integrity.

All nine versions share the npm package name `@easelian/remote-device-agent`.
Platform versions add a suffix such as `-linux-x64-gnu`; the launcher pins
these versions by npm alias. The workflow publishes platform versions under
the `native` dist-tag. The launcher uses the ordinary release version and is
the installation entry point.

For a new npm package, a maintainer with access to the npm organization can
create it by publishing one verified split platform tarball to `native`.
Configure Trusted Publishing on the package for
`easelian/remote-device-agent` and `npm-publish.yml`, with permission for
direct publish, then retry the npm workflow for that release.

### Windows installer repair option

The default installer refuses to overwrite an existing command. `-Update`, its
`-Force` alias, or `RDH_UPDATE=1` explicitly enables executable replacement,
including a same-version reinstall. These options do not skip checksum or version
validation. Stop the Agent before updating. The previous executable is retained
under a unique `.rdh-previous-*.exe` name in the installation directory; binding
and configuration files remain in place. See [maintenance](maintenance.md#recover-a-windows-installation-when-the-old-updater-fails)
for the online recovery command. Offline recovery uses the same checks:

```powershell
.\packaging\install.ps1 -Offline 'C:\Artifacts\rdh-VERSION-win32-x64.exe' -Update -NoStart
```
