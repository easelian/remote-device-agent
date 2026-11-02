# Remote Device Hub Agent

English | [zhCN](README-zhCN.md)

Connect a Linux, macOS or Windows device to Remote Device Hub for remote file,
search and process operations under the device user's authority.

Install with a [download script](#install-with-a-script) or
[npm](#install-with-npm). Both methods provide a ready-to-run native Agent;
no Rust toolchain or source checkout is required.

## Install and run

### Install with a script

The script selects the binary for your operating system and CPU, verifies its
checksum and version, and installs it into a new directory. Supported CPUs are
x86_64 and ARM64. This method does not require Node.js or npm.

**Linux and macOS** — requires `curl` and either `sha256sum` or `shasum`:

```sh
curl -fsSL https://raw.githubusercontent.com/easelian/remote-device-agent/main/packaging/install.sh | sh
```

**Windows** — run in PowerShell:

```powershell
irm https://raw.githubusercontent.com/easelian/remote-device-agent/main/packaging/install.ps1 | iex
```

The scripts fetch the newest published GitHub release, including prereleases,
and install into `~/.local/lib/rdh` on Linux/macOS or
`%LOCALAPPDATA%\RemoteDeviceAgent` on Windows. Add that directory to your user
PATH to run `rdh`. Linux selects musl; macOS and Windows select their native
binaries. No version or destination arguments are required.

An existing installation is not overwritten. For custom directories, versions,
GNU libc builds and offline installation, see
[Distribution](docs/distribution.md#bootstrap-installation).

### Install with npm

Requires Node.js 22.14 or later and npm. On glibc-based Linux, the `0.8.0` package
requires glibc 2.39 or later; use the script's default musl binary on systems
with an earlier glibc version.

Install the npm package attached to the
[GitHub release](https://github.com/easelian/remote-device-agent/releases/tag/v0.8.0):

```sh
npm install --global https://github.com/easelian/remote-device-agent/releases/download/v0.8.0/easelian-remote-device-agent-0.8.0.tgz
```

The package is named `@easelian/remote-device-agent`. It bundles the native
binaries and selects the matching platform at launch. The same command works
on Linux, macOS and Windows; npm's global executable directory must be on PATH.

Both installation methods provide `rdh`, `remote-device-agent` and
`remote-device-hub`. Installation does not pair the device or start a service.

### Pair and connect

From the directory you want to make available, start the Agent:

```sh
rdh --no-browser
```

On first launch, the Agent prints a pairing URL and code. Review the device,
roots and permissions in the browser before approving. `--no-browser` prints the
URL without opening it. With no explicit root, the current directory is used.
`--root NAME=PATH` is repeatable; quote the whole argument when it contains spaces.
`--profile NAME` chooses a separate profile, and `--config PATH` selects a private
configuration file. `--pair-only` saves or validates pairing and exits.

A paired foreground Agent stays connected until Ctrl+C or termination. Reusing
the same command reuses its device identity. Never run two copies of the same
profile. To register an existing stopped profile with an installation, pass
`--config /absolute/paired.json` to `install-native`; it is validated and retained
at that path, without copying credentials.

### Background startup

After pairing with an installed command, inspect and enable user startup:

```sh
rdh startup status --config /absolute/paired.json
rdh startup set user --config /absolute/paired.json
```

This creates or enables future user startup; it does not start or stop the current
connection. Start the registered unit, LaunchAgent or task locally after ending
the foreground instance. Linux uses systemd user units, macOS uses LaunchAgents,
and Windows uses Task Scheduler. Use `startup set none` to disable future startup.
Remote requests require local approval or an administrator grant. See
`startup --help` for `approve`, `deny`, `grant`, `revoke` and explicit `bind`.

For a system service, run `install-service --help`. POSIX installation requires
root, an existing non-root service account and its paired, stopped configuration.
Windows requires administrator authority and creates a LocalService registration.
Both keep writable device state separate from protected program files, create a
stopped registration and require a local start. They never elevate automatically.

### Updates and removal

For npm installations, stop the Agent and install the desired release package
with npm. Use `npm uninstall --global @easelian/remote-device-agent` to remove
the package after disabling any background startup registration.

For script installations, native updates validate a schema-2 index and candidate
executable. End managed work and stop the Agent before `upgrade --no-restart` or `rollback --no-restart`;
restart locally and verify a new Hub connection. Linux's verified systemd user
service path also supports automatic update and readiness rollback. macOS and
Windows use stopped updates and manual restart. See
[Maintenance](docs/maintenance.md#update-and-recover-an-installation).

To remove an installation, disable startup, stop every Agent using it and remove
its registration with the local service manager: systemd, launchd, Task Scheduler
or SCM. Then remove only the program directory. Keep the configuration, policies
and audit/data directory, including the separate service-account state directory.
Inspect the paths before deleting files. Deleting program files does not revoke
the device identity on the Hub.

## Platform behavior and limits

| Behavior | Linux / macOS | Windows |
| --- | --- | --- |
| Shell | `/bin/sh` | System `cmd.exe` |
| Private configuration | Owner-only files and directories | Native SID and protected ACL checks |
| File access | Root-relative paths, held directory descriptors | Held handles, reparse-point and reserved-name checks |
| Process supervision | POSIX process groups | Job Objects and bounded pipe handling |
| Startup | systemd / launchd | Task Scheduler / native SCM service entry |
| bubblewrap | Linux only, separately installed | Unsupported; configured use fails |
| POSIX resource limits | Applied where supported; failure blocks launch | Unsupported; configured use fails |

The default configuration directory is `~/.config/remote-device-hub` on POSIX and
`%LOCALAPPDATA%\remote-device-hub` on Windows. `XDG_CONFIG_HOME` or `--config` can
select another location. Existing permissive configuration is rejected rather
than silently repaired. Audit is enabled by default and retains up to 1,000
redacted records per device, subject to byte and rotation limits. Audit normally
lives below `~/.local/state/remote-device-hub` or `%LOCALAPPDATA%\remote-device-hub`;
system services use their explicitly configured state directory.

File roots and path policies constrain file tools. Shell commands run as the
Agent OS user and need an explicit sandbox for stronger isolation. A response
timeout does not prove an operation had no effect. Inspect uncertain write or
process outcomes before retrying. Brief disconnects retain managed jobs for up to
five minutes; authorization rejection, prolonged disconnection or shutdown
initiates cleanup. Profile ownership is not released while effectful work remains.

Cross-compilation is not native runtime evidence. UNC syntax coverage does not
establish live SMB compatibility, and process/file tests do not establish
power-loss durability. See [Hardening](docs/hardening.md) for policy and audit
contracts and [platform validation](docs/native-lifecycle.md#platforms-and-validation)
for the scope of native CI.

## Hub and network settings

Hub selection is `--server`, then `RDH_SERVER`, then the committed default in
[deployment_defaults.rs](src/deployment_defaults.rs). The exact
public and MCP origins listed there share profile aliases and locks. Custom
origins remain isolated; an explicit configuration for a different Hub is rejected
without forwarding its token. Changing defaults does not rewrite installed
service commands or existing bindings.

When pairing returns `discovery_url`, the Agent authenticates to that Gate with
its saved device credential. The Gate returns a short-lived, single-use ticket
bound to the assigned Core and this Agent instance; only that ticket is sent to
the Core. The current Gate authority protocol requires this v2-capable Rust
Agent. Discovery v1 clients cannot authenticate to this Gate/Core boundary. See [Network configuration](docs/network.md#cluster-discovery).

Connections are direct by default. Opt into local proxy variables with
`RDH_USE_ENV_PROXY=1`; add PEM trust roots with `RDH_CA_BUNDLE`.
See [Network configuration](docs/network.md).

## Development

### Build and validate

Use Rust 1.93.0, the target's native linker/SDK and the checked-in lockfile:

```sh
cargo +1.93.0 fmt --all -- --check
cargo +1.93.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
mkdir -p .build
cargo +1.93.0 run --locked -p xtask -- native-test-candidate \
  --target x86_64-unknown-linux-gnu --out "$PWD/.build/native-test-candidate"
RDH_NATIVE_TEST_CANDIDATE_DIR="$PWD/.build/native-test-candidate" \
  cargo +1.93.0 test --release --locked --workspace --all-targets --all-features --no-fail-fast
cargo +1.93.0 run --locked -p xtask -- native-development \
  --target x86_64-unknown-linux-gnu --out .build/native-development
```

Change the target to match the host and use new output directories for each build.
The builder creates the raw executable,
checksum, archive, provenance record and a verified extraction. Integration tests
which need an independent update candidate or a real service manager have
additional prerequisites: follow [Maintenance](docs/maintenance.md) and
[Agent CI](.github/workflows/ci.yml). An ordinary local test run does not include
all privileged and platform-specific cases.

To install a locally built artifact into a new directory:

```sh
/absolute/artifact/rdh install-native --prefix /absolute/new-install
```

On Windows, use `rdh.exe` and a Windows absolute path. The workspace version is
specified in [Cargo.toml](Cargo.toml).

## Documentation

- [Architecture](docs/architecture.md): components, protocol and work lifetimes.
- [Code navigation](docs/code-structure.md) ([zhCN](docs/code-structure-zhCN.md)): reading order, directory ownership and extension points.
- [Hardening](docs/hardening.md): policies, sandbox, environment and audit.
- [Distribution](docs/distribution.md): artifacts, bootstrap and channel format.
- [Maintenance](docs/maintenance.md): checks, upgrades and recovery.
- [Native lifecycle](docs/native-lifecycle.md) ([zhCN](docs/native-lifecycle-zhCN.md)): installation, updates and platform validation.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for the
full text. Bundled and linked dependencies keep their own licenses, as recorded
in each native artifact's manifest and license directory.
