# Native Agent lifecycle

The Remote Device Hub Agent is a standalone Rust executable, `rdh`. It connects to the Hub and executes device operations. This document describes its module boundaries, installation, updates and validation.

Native installation supports upgrades, interrupted-switch recovery and rollback between native executable versions. The release process distributes binaries through GitHub Releases and npm. Platform signing and production deployment are separate operations.

## Implementation structure

Shared modules handle protocol, policy and work lifetimes; platform modules handle file handles, processes, private storage and service registration. Management commands execute directly. Pairing, connections and update downloads create a Tokio runtime as needed, with one to four scheduler threads, excluding blocking work, search and process supervision threads.

| Modules | Responsibilities |
| --- | --- |
| `cli`, `app`, `config`, `pairing` | Arguments, private configuration, browser pairing, existing device identity and startup order. |
| `transport`, `session`, `rpc`, `receipts` | Authenticated WebSocket, ready state, heartbeats, request admission, deadlines and result receipts. |
| `files`, `searches` | Root-relative operations, encoding, hashes, pagination, search and cancellation. |
| `processes` | Bounded output, stdin, process state and process tree cleanup. |
| `policy`, `sandbox`, `audit` | Local permissions, Linux isolation and resource limits, redacted audit records. |
| `storage`, `profile_lock` | Private file permissions, object identity and single-instance exclusion per profile. |
| `startup` | User startup, service startup type, approval and administrator grants; the CLI entry is `startup::cli`. |
| `native_install`, `install_lock`, `native_launcher` | Native version directories, installation exclusion, stable entry points and switch recovery. |
| `service_install`, `service_host` | Service account installation, native SCM entry and service shutdown. |
| `upgrade_cli`, `update`, `service_controller`, `service_update` | Update indexes, candidate validation, offline switching and Linux systemd automatic updates. |
| `xtask` | Native artifacts, provenance manifests, dependency licenses and archive round-trip validation. |

See [lib.rs](../src/lib.rs), [architecture.md](architecture.md) for modules, protocols and resource lifetimes, and [hardening.md](hardening.md) for configuration constraints.

## Protocol and lifetime

The Agent connects to the Hub and does not expose a local HTTP or MCP listener. With `discovery_url` configured, long-term device credentials authenticate only to the Gate; the Agent establishes WSS using a one-time v2 ticket issued by the Gate and bound to the process and target Core. The connection is confirmed after `hello`, `ready` and `ready_ack`. Without discovery configuration, the Agent sends its credential directly to servers that accept bearer authentication. Gate/Core discovery requires v2 tickets and rejects v1 discovery clients. Users approve new devices in a browser and existing private configuration retains device identity. Hub selection follows `--server`, `RDH_SERVER`, then the source default; credentials for other Hubs are not reused arbitrarily.

At most eight ordinary requests execute concurrently. Response deadlines and actual work lifetimes are separate: RPC timeout or disconnection does not mean a file operation was canceled or that no write occurred. Unfinished work continues to occupy a slot. Runtime receipts, searches, process output and send queues have capacity and deadline limits; late replies from an old connection cannot enter a new one.

Managed processes survive brief disconnects; prolonged offline state, authentication rejection or normal shutdown starts cleanup. The profile lock remains held while managed work is unfinished. POSIX uses process groups and Windows uses Job Objects; programs that deliberately leave a POSIX process group are outside the same cleanup guarantee. File roots constrain file tools and do not replace operating system permissions for shell commands.

Connections are direct by default. `RDH_USE_ENV_PROXY=1` explicitly enables proxy environment variables; `RDH_CA_BUNDLE` adds PEM trust roots. Pairing, updates and WebSocket share network policy; TLS certificate and hostname verification remain enabled. See [network.md](network.md).

## Native installation

`install-native` installs into a new private directory. Linux and macOS use version directories and a `current` link; Windows uses validated version records and native command forwarding. The stable commands are `rdh`, `remote-device-agent` and `remote-device-hub`, with `.exe` suffixes on Windows. They select the current version when invoked after a version change.

An installation can register a stopped, paired private profile, which remains at its original path. A normal Agent holds a shared installation lock; the update manager acquires an exclusive lock and locks registered profiles. Switching is refused while an Agent or managed work is still running.

User background startup uses systemd, LaunchAgent or Task Scheduler. `startup set` changes future startup state and does not stop the current connection. `install-service` creates system services: POSIX requires root, an existing non-root service account and its private profile and state directories; Windows uses an administrator-owned program directory and LocalService data directory. New services are registered disabled or for manual startup; the operator starts them locally and checks the Hub connection.

Bootstrap scripts obtain the correct platform binary, check its digest and version, then call the Rust installation entry point. Native builds, installation, execution and Rust tests do not invoke Node/npm; npm distribution and its tests require Node/npm. See [README](../README.md#install-and-run) and [distribution.md](distribution.md).

## Updates and rollback between Rust versions

### Version and data compatibility

The native update index uses schema `2` and format `native-binary`, recording version, size, SHA-256, platform and rollout percentage; `min_from` can set a minimum source version. The client checks target architecture, digest, the candidate's reported version and registered profiles. The index digest checks file consistency; source trust, signatures and immutable hosting are release-channel responsibilities.

Upgrade and rollback require the selected binary to read the registered configuration and policy. The updater validates these files before switching; it does not perform persistent-format conversion.

### Offline update

All platforms support updating after the Agent stops. macOS, Windows and system services use offline switching and manual restart; Linux user systemd also has the automatic path below.

1. Finish managed processes, searches and other work, then stop Agents and services using the installation.
2. Run `upgrade --no-restart` to validate the candidate and registered profiles, save switch intent, update version selection and retain the previous version.
3. Start from a stable entry point or local service manager and verify the new version, original device identity and Hub connection.
4. To return to the previous version, stop again, run `rollback --no-restart`, then start and verify.

The absolute paths below are placeholders. Use the stable entry point for the same installation and an index from a trusted source for the required platform:

```sh
/absolute/install/rdh upgrade --index /absolute/native-index.json --channel canary --check
/absolute/install/rdh upgrade --index /absolute/native-index.json --channel canary --no-restart
/absolute/install/rdh rollback --no-restart
```

On Windows use the corresponding `rdh.exe`. Service updates must preserve the service account, profile and service definition; an administrator stops the service before updating. Updates do not take ownership of user credentials or run service candidate profile validation as administrator.

### Interrupted switch recovery

Updates persist a pending record before changing the current version selection and installation record. Normal startup is blocked while a switch is unfinished. `upgrade --recover` accepts only the recorded old and new versions and expected file state, then continues or restores that switch. Missing records, changed digests or extra external modifications cause refusal and leave the evidence in place.

```sh
/absolute/install/rdh upgrade --recover
```

After recovery, check the selected version before starting the Agent. Profile lock recovery is separate: use explicit `--recover-lock` only after confirming the original process and its work have ended. Recovery is refused for a live or unverifiable owner. Do not delete the lock to bypass ongoing work.

### Linux systemd automatic update

The supported user systemd path checks the unit definition, installation, profile and current instance, then stops, switches and restarts after work is idle. A trial instance executes remote requests only after durable commit and explicit activation. Readiness checks the Hub ready received by that instance on its current connection. Failure switches back to the retained version; an interrupted controller can recover from its persisted record.

This entry controls only a verifiable target unit and is not a general controller for arbitrary services. macOS and Windows support offline update and rollback with manual service control; automatic stopping, restarting and Hub readiness rollback are unsupported. See `upgrade --help` and [maintenance.md](maintenance.md#update-and-recover-an-installation).

## Platforms and validation

Normal builds enable implemented platform backends by default; `experimental-*` Cargo features select platform backends for reduced builds. CI uses `--no-default-features` to check errors when capabilities are deliberately disabled.

[Agent CI](../.github/workflows/ci.yml) validates eight native targets in parallel on independent runners: Linux x86_64 and ARM64 with glibc and musl, macOS x86_64 and ARM64, and Windows x86_64 and ARM64. Each target builds a separate candidate and runs Clippy, optimized tests, archive and extraction checks, and applicable real user startup and system service tests. Service tests use temporary objects and should run only on disposable CI hosts.

Cross-compilation does not verify Windows ACL, Task Scheduler, SCM or launchd behavior; module checks do not replace full linking. Native CI exercises these mechanisms on its runners. Relogin, production Hub connections, enterprise proxies, SMB and power-loss behavior require validation in the target environment.

## Build and release boundary

The repository uses Rust `1.93.0` and a locked `Cargo.lock`. `xtask native-development` generates a binary, checksums, archive, provenance manifest and license collection. The manifest records the source digest, commit, dirty state, toolchain and target. Archives are extracted again and compared file by file; the extracted version is run on the native target.

The repository is licensed under Apache-2.0. `xtask native-release-candidate` requires a clean source tree, a `v` plus workspace-version tag at the current commit, all features enabled, and native self-checks on a matching OS and CPU. Candidates remain marked `release_ready: false`. The [release workflow](distribution.md#prepare-and-publish-a-release) assembles artifacts after all eight native targets pass, creates attestations and creates a draft GitHub release; the npm package handles platform selection and native binary distribution. Consult the corresponding CI run for the current commit's multi-platform results.
