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

The raw name is `rdh-VERSION-PLATFORM`, with `.exe` on Windows. Each raw executable
and archive has its own `.sha256` file. Use a target matching both the OS and CPU;
successful cross-compilation does not establish runtime support. Current native
runner coverage and outstanding acceptance work are recorded in the
[rewrite design](rust-rewrite-design.md).

## Bootstrap installation

Build or obtain a verified artifact before running these examples. A `latest` URL
must not be assumed to contain native artifacts until a release is published.
The scripts accept either a local directory
or an HTTPS mirror, reject HTTP downloads and verify the exact filename, SHA-256
and executable version. They delegate installation to Rust.

For a local Linux x64 musl artifact, replace the absolute paths:

```sh
sh packaging/install.sh --offline /absolute/artifacts/rdh-0.8.0-linux-x64-musl \
  -- --prefix /absolute/new-install
```

The sibling `.sha256` file is required. Linux defaults to musl; use `--libc gnu`
for a glibc executable. On macOS use the matching `darwin` filename. On Windows:

```powershell
.\packaging\install.ps1 -Offline 'C:\Artifacts\rdh-0.8.0-win32-x64.exe' `
  -NativeArguments @('--prefix', 'C:\Path\NewInstall')
```

For a mirror, use `--version VERSION --base HTTPS_URL` or
`-Version VERSION -Base HTTPS_URL`. A checksum fetched from the same source proves
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

Updates and rollback operate between Rust native versions. Installation creates
a new directory; migration of an earlier Node installation is outside this
unpublished replacement. Native indexes always use schema 2.

## Prepare and publish a release

The [release workflow](../.github/workflows/release.yml) starts for a `v*` tag.
The tag must match the workspace version in `Cargo.toml` and point to a commit
reachable from `main`. It runs all eight native CI targets, assembles only the
successful candidates with matching source, version and checksums, creates a
GitHub artifact attestation, and uploads the files to a **draft** release. Each
candidate manifest retains `release_ready: false`: the workflow validates and
attests the release set; that field alone does not represent publication.

The draft contains each raw binary and archive, their `.sha256` files, bootstrap
scripts, a schema-2 canary index, one npm tarball, and `SHA256SUMS`. The npm
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
