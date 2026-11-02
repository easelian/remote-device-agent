# Agent 代码导航

Agent 主包直接位于仓库根目录。根目录的 `Cargo.toml` 同时定义 `rdh-agent` 包和 workspace；`xtask` 是构建辅助包，与主包共享版本和锁文件。

```text
remote-device-agent/
├── Cargo.toml              # Agent 包、workspace、版本和依赖
├── Cargo.lock             # 固定依赖版本
├── src/                   # Agent 源码，按功能适度归组
│   ├── app.rs             # 组装与启动
│   ├── config.rs          # 单文件功能直接保留
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
├── tests/                 # 集成测试
│   └── support/           # 共享测试夹具
├── xtask/                 # 构建辅助包
│   ├── Cargo.toml
│   └── src/
├── packaging/             # 安装引导脚本
├── docs/                  # 架构、配置和维护说明
└── .github/workflows/     # 原生平台 CI
```

只有一个文件的功能保留在 `src/` 根目录，例如 `network.rs`、`config.rs`、`rpc.rs`、`searches.rs`。上图省略了部分这样的文件。已有多个文件的功能才归组，功能目录直接放公共接口和共享实现；需要区分平台时，按需设置 `posix/`、`linux/`、`macos/`、`windows/` 子目录，平台代码及其测试文件直接放在里面。`posix/` 承载 Linux 与 macOS 可复用的实现，各平台特有机制按需放入对应目录。避免再增加只用于分类的中间目录，也不为整理目录拆开已有的单文件实现。

Cargo 构建产物放在 `target/`，候选版本和验证日志放在已忽略的 `.build/`。

## 功能目录

| 目录 | 入口与内容 |
| --- | --- |
| `files/` | `mod.rs` 为公共接口，`native.rs` 为共享文件操作；各平台目录处理文件描述符或句柄。 |
| `processes/` | `mod.rs` 为公共接口，`native.rs` 为共享进程管理；平台目录负责进程组、Job Object、管道与回收。 |
| `audit/` | `mod.rs` 为审计记录与轮转逻辑，`posix/` 为 POSIX 文件操作。 |
| `storage/` | 私有存储与权限校验；`posix/` 放 POSIX 文件操作，`windows/` 放 ACL 和句柄实现，`macos/acl.rs` 放 macOS ACL 校验。 |
| `profile_lock/`、`install_lock/` | `mod.rs` 保留平台选择和原有锁接口导出，`posix/`、`windows/` 放对应平台实现。 |
| `native_install/` | 原生安装与版本切换，平台实现位于 `posix/`、`windows/`。 |
| `service_host/` | 系统服务运行入口，以及 `posix/`、`windows/` 的启动和退出处理。 |
| `service_install/` | 系统服务安装；POSIX 的账号、profile 和存储操作，以及 Windows 的安装与版本更新，分别位于平台目录中。 |
| `startup/` | 启动设置、命令行、授权与平台选择；`linux/`、`macos/`、`windows/` 放原生注册实现，`posix/` 放复用辅助与授权操作，Windows 任务计划逻辑在 `windows/tasks.rs`。 |

Windows 审计文件和启动授权的底层操作保留在 `storage/windows/audit.rs`、`storage/windows/startup_grants.rs`，它们直接复用私有存储模块的 ACL 与句柄操作。`audit/` 和 `startup/` 通过现有接口调用这些操作。

`app.rs` 通过 `service_install::runtime_guard` 校验系统服务启动并持有安装锁。`service_install/mod.rs` 根据平台和构建特性导出原有实现；Linux 与启用安装功能的 macOS 使用 POSIX 实现，启用安装功能的 Windows 使用 Windows 实现。调用方将返回的 guard 保留至启动流程及退出清理结束。未启用原生服务安装的构建返回 `None`，保持原先不获取该安装锁的行为。

升级命令通过 `service_install::Lease` 获取服务安装租约，POSIX 服务运行入口通过 `service_install::check_service_entry` 校验安装状态。Windows 稳定启动器通过 `install_lock::command_target` 或 `service_install::command_target` 获取并持有选定程序。各功能直接导出现有平台类型与函数，锁、句柄的所有权和释放顺序由原实现保持。

启动管理命令的规范入口是 `startup::cli`；`grant`、`native` 和各平台实现均由 `startup` 内部声明。其他功能通过 `startup` 导出的命令执行、管理员检查和任务更新接口调用现有实现。`lib.rs` 保留公开的 `startup_cli` 别名，使原有 `startup_cli::run` 调用仍指向同一个函数。

`profile_lock/posix/mod.rs` 和 `storage/posix/mod.rs` 继续通过 `#[path]` 映射到各功能内部的 `platform`；`startup/posix/grant.rs` 由 `startup::grant::posix` 载入。平台功能开关保持原有条件。启动模块的测试使用 `startup::` 下的新路径，CI 中的原生 LaunchAgent 测试选择器也使用该路径。

## 阅读顺序

1. 从 [main.rs](../src/main.rs) 和 [cli.rs](../src/cli.rs) 查看进程入口、命令解析和运行时创建。
2. 阅读 [app.rs](../src/app.rs)，跟踪配置、配对、锁、策略、审计与运行时的组装。
3. 阅读 [session.rs](../src/session.rs) 和 [rpc.rs](../src/rpc.rs)，跟踪连接、重连、请求准入、操作分派与退出清理。
4. 按维护任务进入对应模块。文件、进程和搜索分别从 `files/mod.rs`、`processes/mod.rs`、`searches.rs` 开始。

`system.rs` 提供系统信息和本地浏览器启动辅助，继续保持单文件。配对流程通过 `system::open_browser` 打开已经校验的验证链接；平台命令选择、SSH 与 Linux 显示环境检查、后台子进程回收由该辅助函数负责。`--no-browser` 的决定仍留在配对流程中，浏览器启动失败也不阻止配对轮询。

`processes/mod.rs` 保留公共接口、能力声明和平台开关，`processes/native.rs` 处理共享的进程状态、输出缓冲和生命周期。`posix/mod.rs` 实现进程组与子进程回收，`macos/mod.rs` 提供 macOS 进程组观察，`windows/` 放 Job Object 实现、管道和对应测试。源码路径改变后，Rust 模块仍为 `processes::native::platform`，调用方和测试选择器继续使用原路径。

RPC 通过 `Processes::TREE_TERMINATION` 读取所选 backend 的进程终止机制：POSIX 为 `process-group`，Windows 为 `job-object`，未编译进程 backend 时为 `unavailable`。该能力与本地 `allow_shell` 权限分开报告，RPC 无须自行判断 OS。

完整模块职责与资源生命周期见 [architecture.md](architecture.md)。本地管理命令由 `startup/cli.rs` 和 `upgrade_cli.rs` 组织；安装与版本切换从 `native_install/mod.rs`、`install_lock/mod.rs`、`update.rs` 阅读，系统服务运行和安装分别从 `service_host/mod.rs`、`service_install/mod.rs` 阅读。单文件的在线更新协调仍在 `service_controller.rs` 和 `service_update.rs`。

## 构建与测试入口

在仓库根目录执行 [maintenance.md](maintenance.md#build-and-validate) 中的命令。包名仍为 `rdh-agent`，可执行文件仍为 `rdh`，原有 `cargo ... -p rdh-agent` 和 `cargo ... -p xtask` 命令继续适用。

单元测试保留在所属源码模块中。`files/mod.rs` 先列出公共类型和主要方法，现有测试模块集中在文件末尾。`tests/` 下的文件是 Cargo 集成测试入口，例如 `cargo test --test native_bootstrap` 会验证安装引导脚本。升级与就绪测试需要先通过 `xtask native-test-candidate` 构建独立候选版本，再设置 `RDH_NATIVE_TEST_CANDIDATE_DIR`。

调整仓库布局时，同步检查 CI 的路径过滤器、`xtask` 的源码复制清单、测试中基于 `CARGO_MANIFEST_DIR` 的路径，以及 Hub 仓库的部署默认值生成脚本。这样根目录变化不会遗漏构建产物或使测试引用旧路径。
