# Agent architecture

The Agent executes file, search, and process operations on one device for a Remote Device Hub. The Hub handles client-facing tool access and routes requests to the selected device. The Agent initiates an authenticated WebSocket connection to the Hub; it does not expose a local HTTP or MCP listener.

```mermaid
flowchart LR
    Client[Client / MCP host] -->|Tool calls| Hub[Remote Device Hub]
    Agent[Device Agent] -->|Opens authenticated WebSocket| Hub
    Hub -->|Operation requests| Agent
    Agent -->|Results| Hub
    Agent -->|File and search operations| Files[Configured file roots]
    Agent -->|Process operations| Shell[Local shell and child processes]
```

## Components

| Source | Responsibility |
| --- | --- |
| [cli.ts](../src/cli.ts), [bootstrap.ts](../src/bootstrap.ts) | Select upgrade/rollback or Agent startup, parse arguments, and handle termination signals. |
| [server-selection.ts](../src/server-selection.ts) | Choose the default Hub endpoint at startup and recognize the two configured aliases. |
| [connect.ts](../src/connect.ts), [config.ts](../src/config.ts), [profile-lock.ts](../src/profile-lock.ts) | Pair the device, validate and reuse private configuration, and prevent concurrent ownership of a profile. |
| [agent.ts](../src/agent.ts), [transport.ts](../src/transport.ts) | Manage WebSocket sessions, report capabilities, validate request envelopes, enforce request budgets, dispatch operations, and bound outgoing data. |
| [files.ts](../src/files.ts), [filesystem.ts](../src/filesystem.ts), [posix-fs.ts](../src/posix-fs.ts) | Validate root-relative paths and perform bounded reads, guarded writes, directory creation, and platform-specific file access. |
| [processes.ts](../src/processes.ts), [process-environment.ts](../src/process-environment.ts) | Start and supervise commands, retain bounded output, accept stdin, and construct the child environment. |
| [searches.ts](../src/searches.ts) | Run bounded background searches and retain paged results until expiration. |
| [hardening.ts](../src/hardening.ts), [policy.ts](../src/policy.ts), [sandbox.ts](../src/sandbox.ts), [audit.ts](../src/audit.ts) | Load optional restrictions, apply policy and sandbox settings, and record redacted local audit events. |
| [upgrade.ts](../src/upgrade.ts), [update.ts](../src/update.ts), [install.sh](../packaging/install.sh) | Install versioned runtime archives, select rollout candidates, switch the active installation, and roll back. |

## Startup and connection

Without `--server`, startup probes `https://xdistribute.site:8090/healthz` with a 500 ms deadline that includes setup, TLS, and response-body reading. A failed probe selects `https://mcp.xdistribute.site`. An explicit `--server` fixes the endpoint. This selection happens at startup; runtime disconnections use WebSocket reconnection.

The two default origins are aliases for one Hub and can reuse an existing pairing. Custom Hubs retain same-origin pairing validation and credential separation. Profile locks cover the configuration paths used for reuse. Pairing requests browser approval before the Agent saves and uses a new device credential.

After configuration and hardening validation, the Agent connects to `/agent/<device_id>` with a bearer token, sends `hello` with device information, and waits for `ready` before accepting requests. Device information includes operations, limits, and effective restrictions. Session heartbeats detect lost connections. See [Hardening configuration](hardening.md) for policy reporting and configuration privacy.

## Request execution

The Agent validates each request envelope and request ID before dispatch. At most eight operations may be in flight. Dispatch applies operation and file-write policies, records any required pre-execution audit event, and calls the file, process, or search component. The reply contains the same request ID and enters a bounded send queue.

The default RPC response deadline is 20 seconds. Expiring a response or losing its socket does not cancel underlying filesystem I/O; that operation keeps its concurrency slot until it settles. An expired mutation can still have effects. Inspect the target state before retrying an operation with an unknown outcome.

Starting a process returns a job that can continue after the RPC finishes. Its execution timeout is separate from the response deadline. A later `process_exit` audit event records final status and exit code; a successful `start_process` response alone does not establish command success.

Background search uses `start_search`, `get_search_results`, `stop_search`, and `list_searches`. Defaults allow two concurrent tasks, a 30-second active lifetime, traversal depth 30, and 10,000 visited entries. Results stop at 1,000 matches or 256 KiB, with at most 200 results per page. At most 16 task records are retained, and finished tasks expire after up to five minutes. Responses report `status`, `truncated`, `truncated_reason`, and `skipped` so callers can distinguish a complete search from a bounded result.

## State and resource lifetime

| State | Storage and lifetime |
| --- | --- |
| Device pairing and policy | Private configuration and sidecar files on disk; reused at startup. |
| Profile ownership | A directory lock held until shutdown has completed sufficiently to release it. |
| Process jobs and captured output | Memory in the running Agent. Output is bounded; Agent restart does not recover jobs or captured output. |
| Search tasks | Memory with concurrency, traversal, result-size, and expiration limits. Cancellation retains the concurrency slot until worker cleanup completes. |
| Local audit | Redacted JSONL files on disk, retaining at most 1,000 records per device across restarts, subject to byte and rotation limits. |
| Runtime installation | Version directories and active/previous pointers used by upgrade and rollback. |

An ordinary network disconnect retains managed processes and bounded output for up to five minutes. Successful reconnection cancels the offline cleanup timer. Authorization rejection, prolonged disconnection, or explicit shutdown stops managed work. Shutdown stops accepting requests and closes processes, searches, audit handles, and profile ownership; an unresolved cleanup can leave an operation's outcome unknown.

## Permission boundaries

File-tool roots and write policies restrict file operations. They do not confine a shell command, which runs with the Agent OS user's authority. Linux bubblewrap can provide additional command isolation when explicitly configured. Path identity checks and guarded replacement still have races with concurrent external path or file changes.

Platform behavior differs for private file permissions, shell syntax, and process-tree termination. See the [platform table](../README.md#platform-behavior-and-limits) and [Hardening configuration](hardening.md). Build outputs, distribution formats, and release operations are described in [Maintenance and releases](maintenance.md) and [Distribution](distribution.md).
