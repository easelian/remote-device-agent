# Remote Device Hub Agent

A standalone Rust Agent for Linux, macOS and Windows. It opens an authenticated
WebSocket connection to Remote Device Hub and executes bounded file, search and
process operations under the device user's authority. Agent runtime, business
logic, build and tests do not require Node or npm.

This branch is the unpublished `0.7.2-rust.1` replacement under native acceptance.
The latest validation scope and remaining work are recorded in the
[rewrite design](docs/rust-rewrite-design.md). Public release and npm packaging
are later phases; the development artifacts are not a published stable channel.

## Build and validate

Use Rust 1.93.0, the target's native linker/SDK and the checked-in lockfile:

```sh
cargo +1.93.0 fmt --all -- --check
cargo +1.93.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +1.93.0 test --release --locked --workspace --all-targets --all-features --no-fail-fast
cargo +1.93.0 run --locked -p xtask -- native-development \
  --target x86_64-unknown-linux-gnu --out .build/native-development
```

Change the target to match the host. The builder creates the raw executable,
checksum, archive, provenance record and a verified extraction. Integration tests
which need an independent update candidate or a real service manager have
additional prerequisites: follow [Maintenance](docs/maintenance.md) and
[Agent CI](.github/workflows/ci.yml). An ordinary local test run does not include
all privileged and platform-specific cases.

## Install and run

Obtain or build the correct native artifact first. The examples below use an
executable extracted from a development archive; replace absolute paths with
local paths. For user installation on Linux or macOS:

```sh
/absolute/artifact/rdh install-native --prefix /absolute/new-install
/absolute/new-install/rdh --root work=/absolute/work --no-browser
```

On Windows, the equivalent commands are:

```powershell
& 'C:\Artifact\rdh.exe' install-native --prefix 'C:\Path\NewInstall'
& 'C:\Path\NewInstall\rdh.exe' --root 'work=C:\Work' --no-browser
```

The destination must be new. Installation creates stable `rdh`,
`remote-device-agent` and `remote-device-hub` commands (`.exe` on Windows).
They use the selected native version after an update. Add that program directory
to your local PATH if desired. The installer does not change shell startup files.
[Bootstrap scripts](docs/distribution.md#bootstrap-installation) can download and
verify the raw artifact before invoking the same Rust command.

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
/absolute/new-install/rdh startup status --config /absolute/paired.json
/absolute/new-install/rdh startup set user --config /absolute/paired.json
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

### Native updates and removal

Native updates validate a schema-2 index and candidate executable. End managed
work and stop the Agent before `upgrade --no-restart` or `rollback --no-restart`;
restart locally and verify a new Hub connection. Linux's verified systemd user
service path also supports automatic update and readiness rollback. macOS and
Windows use stopped updates and manual restart in this phase. See
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
contracts and the design's acceptance section for platform evidence.

## Hub and network settings

Hub selection is `--server`, then `RDH_SERVER`, then the committed default in
[deployment_defaults.rs](crates/rdh-agent/src/deployment_defaults.rs). The exact
public and MCP origins listed there share profile aliases and locks. Custom
origins remain isolated; an explicit configuration for a different Hub is rejected
without forwarding its token. Changing defaults does not rewrite installed
service commands or existing bindings.

Connections are direct by default. Opt into local proxy variables with
`RDH_USE_ENV_PROXY=1`; add PEM trust roots with `RDH_CA_BUNDLE`.
See [Network configuration](docs/network.md).

## Documentation

- [Architecture](docs/architecture.md): components, protocol and work lifetimes.
- [Hardening](docs/hardening.md): policies, sandbox, environment and audit.
- [Distribution](docs/distribution.md): artifacts, bootstrap and channel format.
- [Maintenance](docs/maintenance.md): checks, upgrades and recovery.
- [Rust replacement design](docs/rust-rewrite-design.md): decisions and acceptance.
