# Agent distribution

The Agent is a standalone Rust executable. The development builder produces a
raw executable for installation and updates, plus an archive containing that
same executable, provenance metadata and dependency license texts. Building,
testing and running it require no Node or npm. npm distribution is a later task.

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

Build or obtain a verified development artifact before running these examples.
The current branch has no public release workflow, and a `latest` URL must not be
assumed to contain native artifacts. The scripts accept either a local directory
or an HTTPS mirror, reject HTTP downloads and verify the exact filename, SHA-256
and executable version. They delegate installation to Rust.

For a local Linux x64 musl artifact, replace the absolute paths:

```sh
sh packaging/install.sh --offline /absolute/artifacts/rdh-0.7.2-rust.1-linux-x64-musl \
  -- --prefix /absolute/new-install
```

The sibling `.sha256` file is required. Linux defaults to musl; use `--libc gnu`
for a glibc executable. On macOS use the matching `darwin` filename. On Windows:

```powershell
.\packaging\install.ps1 -Offline 'C:\Artifacts\rdh-0.7.2-rust.1-win32-x64.exe' `
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
/absolute/install/rdh upgrade --index /absolute/artifacts/rdh-0.7.2-rust.1-linux-x64-musl.channel.json --channel canary --check
```

Use the generated index's actual filename. A check does not switch versions.
After ending managed work and stopping the Agent, use `--no-restart` for an
offline update. Restart locally and verify a new Hub connection before removing
rollback material. [Maintenance](maintenance.md) describes rollback and recovery.

Updates and rollback operate between Rust native versions. Installation creates
a new directory; migration of an earlier Node installation is outside this
unpublished replacement. Native indexes always use schema 2.

Publication, signing, stable-channel promotion and npm platform packages are
outside this development stage. `release_ready: false` in an artifact record is
intentional until those release prerequisites are resolved.
