# 设备硬化配置（0.6.5）

Agent 的文件工具和 shell 工具默认以 Agent 的操作系统用户身份运行。0.6.5 增加三项**可选**的硬化能力：
操作策略、shell 沙箱与资源限制、本地审计日志。本地审计默认关闭，没有旁路配置时不创建审计目录。
Server 负责中央请求审计；设备本地日志用于按需排障。0.6.5 当前为源码草稿，尚未发布。

## 为什么是旁路配置文件

设备配置文件的字段是跨实现的契约：共享协议组件会拒绝未知字段，旧版本 Agent 读取到新字段也会启动失败。
把硬化选项放进主配置会破坏回滚。因此硬化选项放在主配置旁边的独立文件里，只有 Node Agent 读取：

```
<主配置路径去掉 .json>.policy.json
例如 ~/.config/remote-device-hub/<hash>-default.policy.json
```

文件要求与主配置相同：由当前用户持有、权限 `0600`、不能是符号链接，否则 Agent 拒绝启动。
字段严格校验，未知字段会报错。

## 完整示例

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

## 操作策略

策略约束允许调用的操作；路径与只读根规则只约束**文件工具**，在请求到达文件层之前检查：

| 字段 | 含义 |
|---|---|
| `allowed_operations` | 白名单；省略表示不限制。`device_info` 始终允许。 |
| `readonly_roots` | 这些根禁止 `write_file`、`edit_file`、`make_directory`、`move_file`。根名必须是主配置里已有的根。 |
| `protected_paths` | 相对路径片段。任意写类操作的路径中只要连续包含该片段（任意深度）就被拒绝；`move_file` 同时检查来源和目标；两种参数格式不得混用。 |

`protected_paths` 使用字面路径匹配。这是安全的，因为文件层逐级打开路径且不跟随符号链接
（测试 `symlinks cannot be used to bypass protected paths` 覆盖了这一点）。

**文件路径策略不约束 shell。** `start_process` 以 Agent 用户身份运行，可以绕过 `readonly_roots` 和
`protected_paths`。要约束 shell，请使用沙箱，或在 `allowed_operations` 中去掉 `start_process`。

## shell 沙箱

`sandbox.mode`：`"none"`（默认）或 `"bwrap"`。选择 `bwrap` 需要系统有 bubblewrap 且能创建用户命名空间；
启动时会探测，不可用则 Agent **拒绝启动**，不会静默降级为无隔离。

`bwrap` 模式下，每个命令运行在：

- 只读的根文件系统上，`/tmp` 是私有 tmpfs；
- 独立的 PID/IPC/UTS 命名空间，Agent 进程退出时命令随之终止；
- `network: false` 时没有网络（只有回环）。`network` 必须显式填写，没有默认值；
- `hide_home`（默认 `true`）时 `$HOME` 被空 tmpfs 覆盖，`expose` 里的绝对路径以只读方式回挂
  （例如工具链目录；不存在的路径会被跳过）；
- Agent 的配置目录和审计目录在工作根挂载完成后隐藏；自定义主配置及旁路配置文件也会被遮蔽。首次创建的审计目录同样受保护；
- 文件根按 `allow_write` 和 `readonly_roots` 决定可写或只读；目录重叠或多个根指向同一路径时，只读约束优先，只读目录中的可写子根也保持只读。`expose` 目录同样保持只读。

`limits` 通过 shell 的 `ulimit` 实现，`"mode": "none"` 时也可单独使用：
`cpu_seconds`、`memory_kb`（虚拟内存）、`open_files`、`file_size_blocks`（单位由 shell 的 `ulimit -f` 决定，
dash 和 POSIX sh 为 512 字节块）。限制无法应用时命令以状态 126 退出，不会无限制地运行。

## 审计日志

只有显式设置 `audit.enabled: true` 才开启，省略该字段或配置文件均保持关闭。开启后的位置：`$XDG_STATE_HOME/remote-device-hub/audit/<device_id>.jsonl`
（默认 `~/.local/state/...`），目录 `0700`、文件 `0600`，按大小轮转。

- 每次操作记录一行 JSON：时间、请求 ID、操作、结果（`ok`/`error`/`denied`）、耗时、参数摘要、结果摘要。
- **默认拒绝式脱敏**：`content`、`old_text`、`new_text`、`text`（stdin）与 `query` 只记字节数和 SHA-256；命令只记字节数、字符数和 SHA-256，不保存命令正文；未知字符串字段只记长度；文件内容和进程输出从不写入。
- 单条记录按 UTF-8 字节数限制大小，必要时省略参数或结果摘要，保证不超过 `max_bytes`。
- 临时轮转失败后会重试打开日志；短写会继续写完，失败时先清除残缺记录，再接受后续记录。
- 每条记录包含上一条记录的哈希前缀（`prev`），用于发现中间行被删改。
- 审计写入是尽力而为：失败不会中断业务操作，但会被计数。设置 `audit.required: true` 则审计日志无法打开时 Agent
  拒绝启动。

**限制**：未开启沙箱时，shell 命令以 Agent 用户身份运行，理论上可以修改自己的审计日志。哈希链只能发现中间改动，
不能防止末尾截断。需要更强保证时请开启沙箱，并把日志转发到 Agent 无法写入的地方。

## 生效限制上报

`device_info.hardening` 与 hello 中的同名字段上报当前生效配置，`version` 为 1。
`effective_operations` 在实现能力列表上应用操作白名单、`allow_write` 和 `allow_shell`；
`readonly_roots` 和 `protected_paths` 仍会按具体请求限制写入，文件策略不约束 shell。
`sandbox` 包含模式、网络、HOME 隐藏和资源限制，`local_audit.enabled` 表示本次启动是否打开本地审计。
保护片段列表有 2048 字节预算；`protected_paths_count` 和 `protected_paths_truncated` 标明完整数量与是否截断。
`policy_revision` 用于比较限制配置是否变化，不返回审计位置、私有配置路径或凭证。
修改旁路配置后须重启 Agent；旧 Agent 不上报这些字段时，应视为状态未知。

策略拒绝在兼容的 `error` 字符串之外返回固定 `error_code`：操作不允许为 `policy_operation_denied`，
只读根为 `policy_root_readonly`，受保护路径为 `policy_path_protected`，混用移动参数格式为 `policy_parameters_invalid`。
Server 用固定文案呈现分类，并将中央审计的 `request_id` 与本地日志 `id`、客户端 `support_id` 关联。
请求成功表示该次 RPC 完成，启动进程成功不代表最终退出成功。

## 尚未包含

- 读取侧的敏感路径过滤（`search_files` 的内容搜索目前不会参考策略）。
- 写前备份与回滚、分块写入、截图。
- Server 端把读取到的内容标记为不可信数据（提示词注入缓解）和写操作审批。
