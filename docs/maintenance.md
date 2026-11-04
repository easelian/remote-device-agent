# Agent maintenance

Use this guide to build the Agent, update its executable, and complete a binding
removal. The user commands are `rdh`, `rdh unbind`, `rdh update`, `rdh --help` and
`rdh --version`. Local state is resolved by the Agent.

## Build and validate

Use Rust 1.93.0, the checked-in `Cargo.lock`, and the target platform's linker and
SDK. Run from the repository root:

```sh
cargo +1.93.0 fmt --all -- --check
cargo +1.93.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +1.93.0 test --release --locked --workspace --all-targets --all-features --no-fail-fast
```

Session tests need local sockets. Platform isolation and startup tests may need
OS facilities or a disposable host. Ignored tests are not covered by an ordinary
run; inspect [Agent CI](../.github/workflows/ci.yml) for platform-specific commands.
Native Rust builds and tests do not require npm; npm packaging checks require
Node and npm. A passing cross-build does not establish runtime behavior.

Focused lifecycle and bootstrap checks are:

```sh
cargo +1.93.0 test --locked --lib cli::tests
cargo +1.93.0 test --locked --test pairing_session
cargo +1.93.0 test --locked --test lifecycle_recovery
cargo +1.93.0 test --locked --lib self_update::tests
cargo +1.93.0 test --locked --test native_bootstrap
```

The Windows updater tests include a copied test executable that replaces its own
running image. Run that test on Windows; Linux tests cannot validate the Windows
replacement operation. Tests using real startup registrations or user PATH
changes belong on disposable hosts.

To build a development artifact, replace the target with the host target and
choose an unused output directory:

```sh
cargo +1.93.0 run --locked -p xtask -- native-development \
  --target x86_64-unknown-linux-gnu --out .build/native-development
```

The builder records source and toolchain metadata, includes dependency licenses,
and checks the extracted archive against its inventory. A dirty source tree is
recorded as dirty. See [Distribution](distribution.md) for installing a local
artifact. These commands are validation procedures, not a record of completed
checks.

## Update the Agent

Finish managed work and stop the Agent before updating:

```sh
rdh update
```

An active Agent or pending unbind prevents the update. The updater uses the
newest published GitHub release, including prereleases, and proceeds only when
its version is newer. It selects the current OS, CPU and Linux libc, bounds the
download, checks SHA256SUMS, and executes the candidate's version check before
replacement. The binding, shared-directory settings and startup configuration
are retained. After success, run `rdh --version`, then `rdh`, and confirm the
expected device reconnects.

Download, checksum and candidate-check failures leave the installed executable
in place. On Unix, replacement uses an atomic rename and retains the original
until the directory change is synchronized; a synchronization failure attempts
to restore it. A restoration failure names the retained original in its error.
Windows requires atomic replacement support and refuses an unsupported or denied
operation without moving the installed executable aside. No successful update
message is printed when replacement fails.

Inspect the actual executable and any file named in an error before retrying an
uncertain replacement. Retain diagnostic material if restoration fails. These
filesystem operations do not establish a tested power-loss guarantee.

## Complete unbinding

Run `rdh unbind` to stop the active Agent and managed work, revoke its device
access at the Hub, and cancel automatic startup recorded for that binding. Local
revocation intent is persisted so an interrupted or offline command cannot make
the old identity available for a normal restart.

If the command cannot finish, retain the configuration and pending records.
Restore connectivity, then run `rdh unbind` again. A manual `rdh` also completes
pending revocation and exits with instructions to run again. It does not pair a
new device in that cleanup invocation. After successful unbinding, the next
ordinary `rdh` begins pairing using the saved shared-directory settings.

An ordinary restart checks both the previous Agent's PID and its process start
identity. It can reclaim ownership after that process exits, including after a
forced termination, while a live matching instance still prevents a second
runtime. An unreadable or invalid identity prevents automatic recovery.

Recovery does not rerun old operations or restore their in-memory results.
Commands started before a crash can still be running, especially on POSIX.
Inspect those commands and their effects before retrying an uncertain operation;
successful restart or unbinding does not establish that such commands stopped.
Keep pending unbind records so revocation can finish.

Once unbinding succeeds, removing the installed executable removes the program.
It does not require deleting shared directories, unrelated PATH entries, audit
records or the saved shared-directory settings.

## Release boundary

The workspace version is defined in `Cargo.toml`. Generated artifacts and logs
belong under ignored `.build/` directories or external artifact storage. Building
an artifact does not publish or deploy it. Release preparation is described in
[Distribution](distribution.md#prepare-a-release); signing and notarization are
separate from checksums and build provenance.
