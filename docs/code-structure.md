# Agent code navigation

English | [简体中文](code-structure-zhCN.md)

The repository root contains the `rdh-agent` package and workspace. `xtask` builds
native artifacts using the same workspace version and lockfile. Start with the
user action being changed, then follow its state and platform implementation.

## Entry points and lifecycle

1. [main.rs](../src/main.rs) creates the runtime and routes commands parsed by [cli.rs](../src/cli.rs).
2. [binding/mod.rs](../src/binding/mod.rs) resolves the default binding and `settings.json`, prompts for the shared directory, and coordinates instance locks.
3. [app.rs](../src/app.rs) assembles pairing, permissions, audit, startup and the connected runtime.
4. [binding/unbind.rs](../src/binding/unbind.rs) persists revocation intent and finishes local and Hub cleanup. [binding/local.rs](../src/binding/local.rs) coordinates with the running instance.
5. [self_update.rs](../src/self_update.rs) obtains and verifies a release, then replaces the standalone executable. Its caller must ensure the Agent is stopped and hold the instance locks.

The CLI accepts start, unbind, update, help and version. Runtime options are
resolved locally. `system::open_browser` handles platform browser invocation;
pairing prints the verification link even when a browser cannot be opened.

## Runtime and feature ownership

| Source | Responsibility |
| --- | --- |
| `config.rs`, `deployment_defaults.rs` | Private binding format, local roots and compiled Hub defaults. |
| `pairing.rs` | Pairing requests, browser approval polling and saved device identity. |
| `network.rs`, `cluster.rs`, `transport.rs` | Network policy, discovery and authenticated transport. |
| `session.rs`, `rpc.rs`, `receipts.rs` | Connections, request admission, dispatch, work lifetime and result receipts. |
| `files/`, `searches.rs` | Root-relative file operations and bounded searches. |
| `processes/` | Managed processes, output, stdin and process-tree cleanup. |
| `policy.rs`, `sandbox.rs`, `audit/` | Local permissions, OS isolation and redacted audit records. |
| `storage/`, `profile_lock/`, `process_identity.rs` | Private storage, permission checks and instance ownership identified by PID and process start time. |
| `startup/` | Startup RPCs, local authorization and platform registration; unbind disables recorded startup. |
| `update.rs` | Version comparison and release metadata helpers. |

Feature directories contain shared interfaces and platform implementations.
`posix/` contains Linux/macOS shared code; `linux/`, `macos/` and `windows/` contain
OS-specific behavior. Small features remain single files under `src/`.

`processes/native.rs` owns shared process state and buffers. POSIX uses process
groups; Windows uses Job Objects and bounded pipes. The selected backend exports
`Processes::TREE_TERMINATION`, which is reported separately from shell permission.
While the Agent is alive, work that can still have effects retains instance ownership.

After a crash, startup checks the recorded PID and process start identity before
reclaiming the lock. Old operations are not replayed, and in-memory tasks and
output are not recovered. External commands may survive, especially on POSIX;
successful restart does not establish that those commands stopped.

Receipt replay waits for space in the existing writer after connection activation
while incoming frames and shutdown continue to be handled. At its 1,024-record
limit, the Agent evicts only the oldest settled receipt acknowledged by the Hub.
Running and unacknowledged work stays protected; the Hub's own result-retention
and admission limits still apply.

Windows private storage and startup grant operations reuse ACL and handle
primitives in `storage/windows/`. Startup registration is reached through its
module interfaces and RPC dispatch; there is no public startup-management CLI.
The local approval interaction is not specified by this navigation document.

## Packaging and tests

`packaging/install.sh` and `packaging/install.ps1` verify and copy one executable.
`packaging/prepare-release.cjs` assembles native release assets and the npm package.
`xtask/` owns artifact construction, dependency licenses and archive validation.
Cargo outputs go in `target/`; generated artifacts and logs use ignored `.build/`
directories.

Unit tests live beside their implementation. Integration tests in `tests/` use
shared fixtures from `tests/support/`. `native_bootstrap` tests installer behavior;
`self_update::tests` covers candidate checks and replacement failure handling.
Windows running-image replacement needs execution on Windows. See
[Maintenance](maintenance.md#build-and-validate) for commands and validation limits.

When changing source layout, inspect imports in `lib.rs`, CI path filters,
`xtask` inventories and tests using `CARGO_MANIFEST_DIR`. Keep the Hub repository's
deployment-default generator aligned with the Agent source path.
