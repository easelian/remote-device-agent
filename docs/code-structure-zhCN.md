# Agent 代码导览

[English](code-structure.md) | 简体中文

仓库根目录包含 `rdh-agent` 包和工作区。`xtask` 使用同一份工作区版本与锁文件构建原生产物。阅读代码时，先找到需要修改的用户操作，再跟踪其状态和平台实现。

## 入口与生命周期

1. [main.rs](../src/main.rs) 创建运行时，分派由 [cli.rs](../src/cli.rs) 解析的命令。
2. [binding/mod.rs](../src/binding/mod.rs) 解析默认绑定和 `settings.json`，提示选择共享目录，并协调实例锁。
3. [app.rs](../src/app.rs) 组装配对、权限、审计、启动管理和连接运行时。
4. [binding/unbind.rs](../src/binding/unbind.rs) 持久保存撤销意图，完成本地与 Hub 清理。[binding/local.rs](../src/binding/local.rs) 负责与运行中的实例协调。
5. [self_update.rs](../src/self_update.rs) 获取并验证 Release，然后替换独立可执行文件。调用方必须确认 Agent 已停止，并持有实例锁。

CLI 接受启动、解绑、更新、帮助和版本查询。运行参数由本地状态解析。`system::open_browser` 处理各平台的浏览器启动；浏览器无法打开时，配对流程仍会输出验证链接。

## 运行时与功能职责

| 源码 | 职责 |
| --- | --- |
| `config.rs`、`deployment_defaults.rs` | 私有绑定格式、本地根目录和编译时 Hub 默认值。 |
| `pairing.rs` | 配对请求、浏览器批准轮询和设备身份保存。 |
| `network.rs`、`cluster.rs`、`transport.rs` | 网络策略、发现和认证传输。 |
| `session.rs`、`rpc.rs`、`receipts.rs` | 连接、请求准入、分派、工作生命周期和结果回执。 |
| `files/`、`searches.rs` | 根目录相对文件操作和有界搜索。 |
| `processes/` | 受管进程、输出、stdin 和进程树清理。 |
| `policy.rs`、`sandbox.rs`、`audit/` | 本地权限、操作系统隔离和脱敏审计。 |
| `storage/`、`profile_lock/`、`process_identity.rs` | 私有存储、权限检查，以及由 PID 和进程启动时间共同识别的实例所有权。 |
| `startup/` | 启动管理 RPC、本地授权和平台注册；解绑时禁用已记录的启动项。 |
| `update.rs` | 版本比较与发布元数据辅助函数。 |

功能目录包含共享接口和平台实现。`posix/` 放 Linux/macOS 共用代码，`linux/`、`macos/` 和 `windows/` 放各操作系统专用实现。较小功能保留为 `src/` 下的单文件。

`processes/native.rs` 管理共享进程状态和缓冲区。POSIX 使用进程组，Windows 使用 Job Object 和有界管道。所选后端导出 `Processes::TREE_TERMINATION`，该能力与 shell 权限分别报告。Agent 仍在运行且工作可能产生效果时，运行时必须保持实例所有权。

Agent 崩溃后，启动流程核对原 PID 和进程启动时间；确认原实例已退出后，可以回收实例锁并启动。旧操作不会自动重跑，内存中的任务和输出也不会恢复。崩溃前启动的外部命令可能继续运行，尤其在 POSIX 系统上；重新启动成功不代表这些命令已经停止。

回执补发在连接激活后等待现有发送队列腾出空间，等待期间仍处理收包和关闭。Agent 最多保留 1,024 条回执，满额时只回收已结束且经 Hub 确认的最旧记录。运行中及尚未确认的回执继续保留；Hub 自身的结果保留和请求准入限制仍然适用。

Windows 私有存储和启动授权操作复用 `storage/windows/` 中的 ACL 与句柄基础操作。启动注册通过模块接口和 RPC 分派访问，公开 CLI 不提供启动管理命令。本文不约定本地批准的交互方式。

## 打包与测试

`packaging/install.sh` 和 `packaging/install.ps1` 验证并复制单个可执行文件。`packaging/prepare-release.cjs` 组装原生发布产物和 npm 包。`xtask/` 负责产物构建、依赖许可证及归档验证。Cargo 输出位于 `target/`，生成产物和日志使用被忽略的 `.build/` 目录。

单元测试位于实现模块内。`tests/` 中的集成测试复用 `tests/support/` 的辅助代码。`native_bootstrap` 验证安装器行为，`self_update::tests` 覆盖候选检查和替换失败处理。Windows 运行中程序替换需要在 Windows 执行。命令和验证边界见[维护说明](maintenance.md#build-and-validate)。

调整源码布局时，检查 `lib.rs` 的模块声明、CI 路径过滤器、`xtask` 清单，以及使用 `CARGO_MANIFEST_DIR` 的测试。Hub 仓库的部署默认值生成器也需要与 Agent 源码路径保持一致。
