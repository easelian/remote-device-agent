# Agent code navigation

The Agent package lives at the repository root. `Cargo.toml` defines the `rdh-agent` package and workspace; `xtask` is a build helper that shares the version and lockfile.

```text
remote-device-agent/
├── Cargo.toml              # Agent package, workspace, version and dependencies
├── Cargo.lock              # Locked dependency versions
├── src/                    # Agent source, grouped by feature where useful
│   ├── app.rs              # Assembly and startup
│   ├── config.rs           # Single-file feature
│   ├── network.rs
│   ├── audit/
│   ├── files/
│   ├── install_lock/
│   ├── native_install/
│   ├── processes/
│   ├── profile_lock/
│   ├── service_host/
│   ├── service_install/
│   ├── startup/
│   └── storage/
├── tests/                  # Integration tests
│   └── support/            # Shared fixtures
├── xtask/                  # Build helper
│   ├── Cargo.toml
│   └── src/
├── packaging/              # Installation bootstrap scripts
├── docs/                   # Architecture, configuration and maintenance
└── .github/workflows/      # Native platform CI
```

Single-file features such as `network.rs`, `config.rs`, `rpc.rs` and `searches.rs` remain at the `src/` root; the tree omits some of them. Features with multiple files are grouped in directories containing their public interface and shared implementation. Where needed, platform code and tests live directly in `posix/`, `linux/`, `macos/` or `windows/` subdirectories. `posix/` contains code shared by Linux and macOS. Avoid classification-only intermediate directories and splitting existing single-file implementations solely to reorganize the tree.

Cargo output goes in `target/`; candidate releases and validation logs go in the ignored `.build/` directory.

## Feature directories

| Directory | Entry points and contents |
| --- | --- |
| `files/` | `mod.rs` exposes the public interface, `native.rs` contains shared operations, and platform directories handle descriptors or handles. |
| `processes/` | `mod.rs` exposes the interface, `native.rs` manages processes, and platform directories handle process groups, Job Objects, pipes and reaping. |
| `audit/` | `mod.rs` handles audit records and rotation; `posix/` contains POSIX file operations. |
| `storage/` | Private storage and permission checks; `posix/` contains POSIX file operations, `windows/` contains ACL and handle code, and `macos/acl.rs` checks macOS ACLs. |
| `profile_lock/`, `install_lock/` | `mod.rs` selects platforms and exports the existing lock interfaces; `posix/` and `windows/` contain their implementations. |
| `native_install/` | Native installation and version selection, implemented in `posix/` and `windows/`. |
| `service_host/` | System service entry point and platform-specific startup and shutdown. |
| `service_install/` | System service installation, including POSIX account, profile and storage operations and Windows installation and updates. |
| `startup/` | Startup settings, CLI, grants and platform selection. Platform directories contain native registration; `posix/` contains shared helpers and grants, and `windows/tasks.rs` handles Task Scheduler. |

Low-level Windows audit and startup grant operations remain in `storage/windows/audit.rs` and `storage/windows/startup_grants.rs`, reusing private storage ACL and handle operations. `audit/` and `startup/` call them through existing interfaces.

`app.rs` uses `service_install::runtime_guard` to validate system service startup and hold the installation lock. `service_install/mod.rs` exports implementations according to platform and build features: Linux and macOS with installation enabled use POSIX; Windows with installation enabled uses Windows. The caller holds the returned guard through startup and shutdown cleanup. Builds without native service installation return `None` and do not acquire this lock.

Upgrade commands acquire a service installation lease through `service_install::Lease`; the POSIX service entry point checks installation state through `service_install::check_service_entry`. The Windows stable launcher obtains and holds the selected program through `install_lock::command_target` or `service_install::command_target`. Existing implementations retain ownership and release ordering for locks and handles.

The canonical startup management entry point is `startup::cli`; `grant`, `native` and platform implementations are declared inside `startup`. Other features use the exported command execution, administrator checks and task update interfaces. `lib.rs` retains the public `startup_cli` alias, so `startup_cli::run` still points to the same function.

`profile_lock/posix/mod.rs` and `storage/posix/mod.rs` map to each feature's internal `platform` via `#[path]`; `startup/posix/grant.rs` is loaded by `startup::grant::posix`. Platform feature conditions remain unchanged. Startup tests and the CI native LaunchAgent selector use paths under `startup::`.

## Reading order

1. Start with [main.rs](../src/main.rs) and [cli.rs](../src/cli.rs) for the process entry point, argument parsing and runtime creation.
2. Read [app.rs](../src/app.rs) for configuration, pairing, locks, policy, audit and runtime assembly.
3. Read [session.rs](../src/session.rs) and [rpc.rs](../src/rpc.rs) for connections, reconnection, admission, dispatch and shutdown cleanup.
4. Follow the feature relevant to the task: `files/mod.rs`, `processes/mod.rs` or `searches.rs` are starting points for files, processes and search.

`system.rs` supplies system information and local browser launching. Pairing uses `system::open_browser` to open a validated verification URL; the helper selects platform commands, checks SSH and Linux display environments and reaps background children. Pairing decides whether to honor `--no-browser`; a browser launch failure does not stop polling.

`processes/mod.rs` contains the public interface, capability declaration and platform gates. `processes/native.rs` manages shared process state, output buffers and lifecycle; `posix/mod.rs` implements process groups and reaping, `macos/mod.rs` observes macOS process groups, and `windows/` contains Job Object, pipe and test code. The Rust module remains `processes::native::platform` and callers and test selectors retain that path.

RPC reads the backend's termination mechanism from `Processes::TREE_TERMINATION`: `process-group` on POSIX, `job-object` on Windows and `unavailable` without a compiled backend. This is reported separately from the local `allow_shell` permission.

See [architecture.md](architecture.md) for module responsibilities and resource lifetimes. Local management commands are organized in `startup/cli.rs` and `upgrade_cli.rs`; installation and version selection start at `native_install/mod.rs`, `install_lock/mod.rs` and `update.rs`, while system service execution and installation start at `service_host/mod.rs` and `service_install/mod.rs`. Online update coordination remains in `service_controller.rs` and `service_update.rs`.

## Build and test entry points

Run the commands in [maintenance.md](maintenance.md#build-and-validate) from the repository root. The package remains `rdh-agent` and the executable remains `rdh`; existing `cargo ... -p rdh-agent` and `cargo ... -p xtask` commands still apply.

Unit tests live in their source modules. `files/mod.rs` lists public types and primary methods first, with tests at the end. Files in `tests/` are Cargo integration test entry points; for example, `cargo test --test native_bootstrap` verifies installation bootstrap scripts. Upgrade and readiness tests require a separate candidate built with `xtask native-test-candidate` and `RDH_NATIVE_TEST_CANDIDATE_DIR` set to its directory.

When changing the layout, check CI path filters, the `xtask` source-copy inventory, test paths based on `CARGO_MANIFEST_DIR`, and the Hub repository's deployment-default generation script so that outputs and test paths remain correct.
