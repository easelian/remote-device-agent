# Agent maintenance

The repository builds the Rust Agent with Cargo and a Rust `xtask` artifact
builder. Use Rust `1.93.0` and the checked-in `Cargo.lock`. No Server checkout,
Node, npm or Python is required by the Agent build and test chain. OS SDKs,
linkers and platform service tools are still required for their respective targets.
See [Code navigation](code-structure.md) for module ownership and extension points.

## Build and validate

Run from the repository root:

```sh
cargo +1.93.0 fmt --all -- --check
cargo +1.93.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
mkdir -p .build
cargo +1.93.0 run --locked -p xtask -- native-test-candidate \
  --target x86_64-unknown-linux-gnu --out "$PWD/.build/native-test-candidate"
RDH_NATIVE_TEST_CANDIDATE_DIR="$PWD/.build/native-test-candidate" \
  cargo +1.93.0 test --release --locked --workspace --all-targets --all-features --no-fail-fast
```

Replace the target with the host target and choose a new candidate output
directory for each source snapshot. Upgrade and readiness tests require the
independently built candidate selected by `RDH_NATIVE_TEST_CANDIDATE_DIR`;
they fail if it is missing. Session tests open local TCP and Unix sockets, and
the Linux isolation test creates namespaces.

Tests requiring real service registrations are explicitly ignored in an ordinary
local run. Ignored tests are not passing evidence. [Agent CI](../.github/workflows/ci.yml) records
the exact platform commands, creates the candidates and runs those fixtures on
eight native OS/CPU/libc targets, one job at a time. Run privileged service tests
only on disposable hosts: they create real temporary service registrations and
accounts. Do not run them against an installed production profile.

The retained `experimental-*` Cargo feature names are compatibility switches;
the normal default enables all applicable platform backends. The CI safety-gate
checks use `--no-default-features` to verify deliberate reduced builds.

Build and round-trip a development artifact for the host target, replacing the
target and unused output directory as needed:

```sh
cargo +1.93.0 run --locked -p xtask -- native-development \
  --target x86_64-unknown-linux-gnu --out .build/native-development
```

The builder verifies executable headers, copies active dependencies' license
texts, archives and re-extracts every file, and compares contents and metadata.
When target and host match it runs the extracted executable's version check with
an empty environment. The record binds source hashes, Git state, toolchain,
target and artifact digests. A dirty source tree is recorded as dirty. Existing
outputs are not overwritten; choose a new directory for a new build.

`native-test-candidate` builds an independent native version used by upgrade,
rollback and interrupted-switch tests. Candidate and main executable are built
from the same source snapshot with separate version identities.

## Update and recover an installation

For an offline update, finish commands and searches, stop every Agent using the
installation, then run its stable command:

```sh
/absolute/install/rdh upgrade --index /absolute/native-index.json --channel canary --no-restart
```

The installer validates the candidate's size, digest, architecture, version and
registered profiles before switching. Restart the Agent using the stable command
or its installed service registration and confirm a new Hub `ready` message.
macOS and Windows require this manual restart. Root-owned POSIX system services
and Windows service-account installations also require a stopped service and
root/administrator authority.

To roll back, stop the Agent again and use:

```sh
/absolute/install/rdh rollback --no-restart
```

An interrupted switch leaves a pending record and prevents normal startup.
`rdh upgrade --recover` reconciles that recorded switch. Preserve the record and
program directories if recovery detects outside changes. Recovery only completes
or reverses the recorded native switch; it does not infer missing state.

Linux also supports the existing automatic update path for a verified user
systemd unit: `upgrade --service-unit NAME --service-config ABSOLUTE_PATH`.
It checks active work, stops the matching instance, switches, restarts and checks
that instance's new Hub readiness. Failure restores the old version. This path
rejects service definitions it cannot prove belong to the installation; it is
not a generic controller for arbitrary services. See `upgrade --help` for local
inspection and recovery commands.

Use `--recover-lock` only after confirming the previous Agent and its work have
stopped. Recovery rejects a live or uninspectable owner. On Windows an explicitly
requested stopped-profile recovery can be needed after Task Scheduler forcibly
terminates a task. System-service installers use kernel locks and do not accept
that profile-recovery override.

## Release boundary

The workspace version is in `Cargo.toml`. Generated artifacts, records and logs
belong in ignored `.build/` directories or external artifact storage. The old
Node release and promotion workflows are removed. The current builder never
publishes, installs on a device or changes a channel in GitHub.

Public release work still needs a project license decision, supported OS baseline
validation, signing/provenance, immutable hosting and channel promotion. The
builder explicitly records whether a project license is present and leaves
`release_ready` false. Do not substitute dependency licenses for a project license.
npm packaging and publication are a separate later phase.
