# Device hardening configuration

By default, file and shell tools run as the Agent's operating-system user. Operation policies, shell sandboxing, and resource limits are optional. Local auditing is enabled by default and retains at most the latest 1,000 operation records per device, including records from earlier Agent runs. The Server also provides central request auditing. The current 0.7.0 source includes basic native Windows support. This does not imply publication or completed acceptance testing on every platform.

## Why use a sidecar file?

Device configuration fields form a contract shared across implementations. The shared protocol component rejects unknown fields, and older Agents fail to start when they encounter new fields. Adding hardening options to the main configuration would therefore break rollback. Instead, the Node Agent reads a separate file next to the main configuration:

```text
<main configuration path without .json>.policy.json
For example: ~/.config/remote-device-hub/<hash>-default.policy.json
```

The file has the same privacy requirements as the main configuration: current-user ownership and mode `0600` on Linux/macOS, or a private ACL owned by the current user with inheritance disabled on Windows. Symbolic links and Windows reparse paths are prohibited; the Agent refuses to start if it encounters them. Fields are strictly validated, and unknown fields cause an error.

## Complete example

```json
{
  "version": 1,
  "allowed_operations": ["list_directory", "read_file", "read_file_chunk", "read_multiple_files",
                         "file_info", "search_files", "write_file", "edit_file", "start_process",
                         "read_process", "list_processes", "terminate_process"],
  "readonly_roots": ["docs"],
  "protected_paths": [".git/hooks", ".env", ".ssh"],
  "sandbox": {
    "mode": "bwrap",
    "network": false,
    "hide_home": true,
    "expose": ["/home/kylin/.nvm"],
    "limits": { "cpu_seconds": 600, "memory_kb": 4194304, "open_files": 1024, "file_size_blocks": 2097152 }
  },
  "audit": { "enabled": true, "max_bytes": 5242880, "max_files": 3, "required": false }
}
```

## Operation policy

The policy controls which operations may be called. Path and read-only root rules apply only to **file tools** and are checked before a request reaches the file layer:

| Field | Meaning |
| --- | --- |
| `allowed_operations` | An allowlist; omitted means unrestricted. `device_info` is always allowed. |
| `readonly_roots` | Denies `write_file`, `edit_file`, `make_directory`, and `move_file` in these roots. Each name must identify a root in the main configuration. |
| `protected_paths` | Relative path segments. A write is denied if its path contains one of these contiguous segments at any depth. `move_file` checks both source and destination; its two parameter formats must not be mixed. |

`protected_paths` uses literal path matching, ignoring case on Windows/macOS. The file layer rejects observed symbolic links and Windows path aliases. Concurrent path replacement by another local process remains a TOCTOU limitation; these checks do not provide operating-system sandbox isolation.

**File path policies do not constrain the shell.** `start_process` runs as the Agent's user and can bypass `readonly_roots` and `protected_paths`. To constrain shell access, use sandboxing or remove `start_process` from `allowed_operations`.

## Shell sandbox

`sandbox.mode` is either `"none"` (the default) or `"bwrap"`. The `bwrap` mode is Linux-only and requires bubblewrap plus working user namespaces. The Agent probes availability at startup and **refuses to start** if isolation is unavailable; it never silently falls back to unisolated execution.

In `bwrap` mode, each command runs with:

- A read-only root filesystem and a private tmpfs at `/tmp`.
- Separate PID, IPC, and UTS namespaces; commands terminate when the Agent process exits.
- No network access except loopback when `network: false`. `network` must be set explicitly and has no default.
- An empty tmpfs over `$HOME` when `hide_home` is enabled (the default). Absolute paths in `expose` are mounted back read-only, for example to expose toolchains. Missing paths are skipped.
- Hidden Agent configuration and audit directories after work-root mounts are applied. Custom main and sidecar configuration files are also hidden, including an audit directory created for the first time.
- File roots mounted according to `allow_write` and `readonly_roots`. Read-only restrictions take precedence when directories overlap or multiple roots resolve to the same path. A writable child root within a read-only directory remains read-only. Directories in `expose` also remain read-only.

Windows does not support these bubblewrap or POSIX resource-limit settings and refuses to start if they are configured. macOS does not support bubblewrap.

On POSIX systems, `limits` uses the shell's `ulimit` and can also be used independently with `"mode": "none"`. Supported fields are `cpu_seconds`, `memory_kb` (virtual memory), `open_files`, and `file_size_blocks`. The unit for `file_size_blocks` follows the shell's `ulimit -f`; dash and POSIX sh use 512-byte blocks. If a limit cannot be applied, the command exits with status 126 rather than running without the limit.

## Audit logs

Local auditing is enabled even when the sidecar file or `audit.enabled` field is omitted. Set `"audit": { "enabled": false }` in the version 1 sidecar to disable it. Logs are written to `$XDG_STATE_HOME/remote-device-hub/audit/<device_id>.jsonl` (by default `~/.local/state/remote-device-hub/audit/<device_id>.jsonl` on Linux/macOS). Windows uses `%LOCALAPPDATA%\remote-device-hub\audit\<device_id>.jsonl`. Linux/macOS directories use mode `0700` and files `0600`; Windows uses private owner-only ACLs.

The active JSONL file and its numbered rotation files retain at most 1,000 records **in total per device**. Startup and subsequent writes remove the oldest records as needed, so restarting the Agent does not reset the retention limit. Each line is one JSON object. The existing `max_bytes` and `max_files` limits also apply and may retain fewer records. Retention rewrites use a private temporary file and rename; an interrupted final append is removed on startup before new records are added.

- Each operation produces one JSON line with its timestamp, request ID, operation, outcome (`ok`, `error`, or `denied`), duration, argument summary, and result summary.
- **Allowlist-based redaction:** `content`, `old_text`, `new_text`, `text` (stdin), and `query` retain only byte counts and SHA-256 digests. Commands retain only byte counts, character counts, and SHA-256 digests, never command text. Unknown string fields retain only their lengths. File contents and process output are never logged.
- Each record is limited by its UTF-8 byte length. Argument or result summaries are omitted when necessary to stay within `max_bytes`.
- After a temporary rotation failure, the logger retries opening the file. It completes short writes and removes a partial record after failure before accepting subsequent records.
- Each record contains a hash prefix of the preceding record (`prev`) within the same Agent run (`boot`). Retention preserves these links between retained records; the first retained record may refer to a record that has expired. A new Agent run starts a new chain.
- Audit writes are best effort: failures are counted but do not interrupt operations. With `audit.required: true`, the Agent refuses to start if it cannot open the audit log.

**Limitations:** without a sandbox, shell commands run as the Agent's user and can potentially modify its audit logs. The hash chain detects intermediate changes but cannot prevent truncation at the end. For stronger guarantees, enable sandboxing and forward logs to a destination the Agent cannot modify.

## Reporting effective restrictions

`device_info.hardening` and the corresponding hello field report the active configuration with `version: 1`. `effective_operations` applies the operation allowlist, `allow_write`, and `allow_shell` to the implementation's capabilities. `readonly_roots` and `protected_paths` still restrict individual write requests; file policies do not constrain shell commands.

`sandbox` reports the mode, network access, HOME hiding, and resource limits. `local_audit.enabled` indicates whether this Agent run opened local auditing. The protected-segment list has a 2,048-byte budget; `protected_paths_count` and `protected_paths_truncated` report the full count and whether the list was truncated. `policy_revision` allows clients to detect restriction changes without exposing audit locations, private configuration paths, or credentials. Restart the Agent after changing the sidecar file. Treat missing reports from older Agents as unknown.

Policy denials include a stable `error_code` alongside the compatible `error` string: `policy_operation_denied` for a disallowed operation, `policy_root_readonly` for a read-only root, `policy_path_protected` for a protected path, and `policy_parameters_invalid` for mixed move parameter formats. The Server presents a fixed message for each category and correlates its central audit `request_id` with the local log `id` and client `support_id`. A successful response means that RPC completed; successfully starting a process does not establish its eventual exit status.

## Not yet included

- Sensitive-path filtering for reads (`search_files` content searches do not currently consult the policy).
- Pre-write backups and rollback, chunked writes, or screenshots.
- Server-side untrusted-content marking for prompt-injection mitigation, or write approvals.
