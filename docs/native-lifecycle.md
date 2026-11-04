# Agent lifecycle

English | [简体中文](native-lifecycle-zhCN.md)

The Agent has one local binding and three actions: start with `rdh`, remove the
binding with `rdh unbind`, and replace the executable with `rdh update`. The
executable and private device state have separate lifetimes.

## Start and bind

`binding::prepare_options` resolves local settings and the selected configuration.
On first interactive launch, the user chooses a shared directory; the selection
is stored in `settings.json`. New pairings use the compiled Hub default. A saved
binding supplies its existing Hub, roots and device identity.

`app::launch` checks pending lifecycle state and acquires the binding's instance
locks before admitting work. An unbound Agent prints a verification link and code,
attempts to open the browser and waits for approval. A bound Agent connects using
its saved identity without opening the browser. It retains instance ownership
through shutdown cleanup.

File roots constrain file tools. Shell commands use the Agent OS account's
permissions. Response deadlines and work lifetimes are separate: a timeout does
not prove that a write or managed process stopped. These distinctions matter when
stopping or unbinding a device.

## Unbind

`rdh unbind` persists its intent, stops the Agent and managed work, revokes device
access at the Hub, and cancels recorded automatic startup. Configuration is not
discarded before the information required to finish revocation is available.
Shared-directory settings are retained for a later pairing.

| State | Next action |
| --- | --- |
| Bound and running | `rdh unbind` requests shutdown and completes cleanup and revocation. |
| Revocation interrupted or Hub unavailable | Retain the intent and binding data; block ordinary reuse of the old identity. |
| Pending unbind, invoked with `rdh unbind` | Resume revocation and cleanup. |
| Pending unbind, invoked with `rdh` | Finish revocation, then exit and ask the user to run again. |
| Unbound with no pending cleanup | The next `rdh` starts browser pairing. |

The running instance receives local lifecycle requests through the binding's
private local-control mechanism. Failure to establish that work has stopped
prevents unsafe cleanup. Manually deleting state or lock files is not a substitute
for completing the lifecycle operation.

Startup RPCs retain their local authorization checks. This document does not
specify an approval interaction for them. Unbinding uses the recorded registration
to cancel automatic startup.

## Update the executable

`binding::update` rejects an active Agent or pending unbind and holds instance
locks while `self_update::run` operates. The updater selects a newer published
GitHub release, including prereleases, and verifies the current platform's binary
size, SHA-256 and reported version before replacement. The binding, shared roots
and startup settings are not rewritten.

On Unix, an atomic rename replaces the executable; the original remains reachable
until the directory change is synchronized. Windows uses `ReplaceFileW` with a
backup in a newly reserved `.rdh-previous-*` directory. If publication fails after
moving the original, the updater attempts to restore it without overwriting a
new file at the target name. Failure diagnostics identify any retained original.
A backup still mapped by the running command can be removed after it exits.

The Windows replacement is not a power-loss transaction. If an interrupted update
leaves the command path absent, restore the executable from its `.rdh-previous-*`
directory or reinstall the binary before starting the Agent again.

A successful update does not start the Agent. Run `rdh` to reconnect. See
[Maintenance](maintenance.md#update-the-agent) for operator steps and
[Distribution](distribution.md) for asset names and installation paths.

## Source and validation

Read [binding/mod.rs](../src/binding/mod.rs),
[binding/unbind.rs](../src/binding/unbind.rs), [app.rs](../src/app.rs), and
[self_update.rs](../src/self_update.rs) for the lifecycle implementation.
[Code navigation](code-structure.md) describes related modules.

Lifecycle validation must exercise saved settings, pending revocation and identity
reuse. The updater tests exercise candidate verification and failure preservation.
Platform-specific startup, process and running-image replacement need native
execution on their corresponding OS. Test presence,
cross-compilation and source inspection do not establish a completed runtime or
power-loss validation.
