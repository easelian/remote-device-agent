# Agent distribution

The Agent is a standalone Rust executable. The development builder produces a
raw executable for installation and updates, plus an archive containing that
same executable, provenance metadata and dependency license texts. Building,
testing and running the native Agent require no Node or npm. The npm distribution
uses Node to select and start a packaged native executable.

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

The raw name is `rdh-VERSION-PLATFORM`, with `.exe` on Windows. Release assets
share a `SHA256SUMS` file. Use a target matching both the OS and CPU;
successful cross-compilation does not establish runtime support. See
[Native lifecycle](native-lifecycle.md#platforms-and-validation) for native
runner coverage and validation limits.

## Bootstrap installation

The [README](../README.md#install-with-a-script) provides one-line installation
commands. Without arguments, the scripts install their default release into
`$HOME/.local/lib/rdh` on POSIX or `%LOCALAPPDATA%\RemoteDeviceAgent` on Windows.
Add the printed installation directory to your user PATH. The release assembler
sets each attached script's default version to that release's version.
The scripts on `main` select `0.8.0`.

Use `RDH_INSTALL_DIR` for a different destination, `RDH_VERSION` for another
release, and `RDH_BASE` for a mirror. `RDH_LIBC=gnu` selects the GNU build on Linux;
the `0.8.0` GNU binaries require glibc 2.39 or later. Explicit script options
override the corresponding environment variables. Service installation requires
explicit native arguments.

Build or obtain a verified artifact before running these examples. Check that
the selected release contains the required platform artifact.
The scripts accept either a local directory
or an HTTPS mirror, reject HTTP downloads and verify the exact filename, SHA-256
and executable version. They delegate installation to Rust.

For a local Linux x64 musl artifact, replace the absolute paths:

```sh
sh packaging/install.sh --offline /absolute/artifacts/rdh-0.8.0-linux-x64-musl \
  -- --prefix /absolute/new-install
```

Put the release `SHA256SUMS` file in the same directory as the offline binary.
A per-binary sibling `.sha256` file is also supported when `SHA256SUMS` is absent.
Linux defaults to musl; use `--libc gnu`
for a glibc executable. On macOS use the matching `darwin` filename. On Windows:

```powershell
.\packaging\install.ps1 -Offline 'C:\Artifacts\rdh-0.8.0-win32-x64.exe' `
  -NativeArguments @('--prefix', 'C:\Path\NewInstall')
```

For a mirror, use `--version VERSION --base HTTPS_URL` or
`-Version VERSION -Base HTTPS_URL`. The scripts fetch `SHA256SUMS`, falling back
to the per-binary `.sha256` file when it is unavailable. A checksum fetched
from the same source proves
content consistency, not independent publisher authentication. Protect the mirror
and retain its immutable artifact records.

Installation creates a new program directory and the `rdh`, `remote-device-agent`
and `remote-device-hub` commands (`.exe` on Windows). It does not replace an
existing directory, pair a device or start a background service. Run the installed
command to pair; use `startup` or `install-service` for background registration.
See the [README](../README.md#install-and-run) for that sequence. `--service` or
`-Service` forwards arguments to `install-service`.

## Native version updates

Native update indexes use schema `2` and artifact format `native-binary`.
`xtask native-development` emits a local canary index for its target. The index
records version, byte length and SHA-256; it can also constrain `min_from` and
`rollout.percent`. An installation ID determines a stable rollout bucket.

Use the explicit index when testing a development build:

```sh
/absolute/install/rdh upgrade --index /absolute/artifacts/rdh-0.8.0-linux-x64-musl.channel.json --channel canary --check
```

Use the generated index's actual filename. A check does not switch versions.
After ending managed work and stopping the Agent, use `--no-restart` for an
offline update. Restart locally and verify a new Hub connection before removing
rollback material. [Maintenance](maintenance.md) describes rollback and recovery.

Updates and rollback operate between native executable versions. Installation
creates a new directory. Native indexes use schema 2.

## Prepare and publish a release

The [release workflow](../.github/workflows/release.yml) starts for a `v*` tag.
The tag must match the workspace version in `Cargo.toml` and point to a commit
reachable from `main`. It runs all eight native CI targets, assembles only the
successful candidates with matching source, version and checksums, creates a
GitHub artifact attestation, and uploads the files to a **draft** release. Each
candidate manifest retains `release_ready: false`: the workflow validates and
attests the release set; that field alone does not represent publication.

The draft contains each raw binary and archive, bootstrap scripts, a schema-2
canary index, one npm tarball, and `SHA256SUMS`. The npm
tarball `@easelian/remote-device-agent` contains all eight binaries and their
license texts. Its `rdh`, `remote-device-agent` and `remote-device-hub` launchers
select the native binary; running those npm commands requires Node 22.14 or later.
The native binaries and bootstrap installation do not require Node.

Review the eight CI jobs, checksums, attestations, installation and upgrade
evidence, release notes and exact version before publishing the GitHub draft.
Prerelease versions are marked as prereleases. The generated canary index is a
release asset for inspection; neither `channels/canary.json` nor the stable
channel is updated automatically. Publish and validate those indexes separately
after verifying that their artifact URLs are reachable to devices.

The [npm workflow](../.github/workflows/npm-publish.yml) is started manually
with a **published** GitHub Release tag: select that tag as the workflow ref and
enter the same tag in its input. It downloads the exact release tarball,
checks `SHA256SUMS` and verifies its attestation against the release workflow,
tag and commit before publishing using npm Trusted Publishing. Prereleases use
the npm `next` tag; versions without a prerelease suffix use `latest`. The npm
organization must authorize publishing `@easelian/remote-device-agent` and
configure `easelian/remote-device-agent` with workflow filename
`npm-publish.yml`, including permission for `npm publish`. The npm CLI must be
at least 11.5.1. If the package does not exist yet, arrange its initial creation
with an authorized npm maintainer before configuring the package's trusted
publisher. Do not create a placeholder or publish a first version automatically.

After an uncertain publish result, inspect the registry's exact version and
integrity before retrying: npm versions are immutable. GitHub attestations
require the appropriate repository plan and permissions. Neither checksums nor
attestations substitute for platform signing, notarization or runtime testing.
