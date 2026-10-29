# Agent Rust 实现与版本更新设计

本阶段把 Remote Device Hub Agent 的运行、业务、平台适配、构建和测试全部替换为 Rust。交付物是独立的 `rdh` 原生程序，保持现有 Hub 协议与设备操作。当前版本为尚未发布的 `0.7.2-rust.1`。

2026-10-29 确认的范围是：原生运行和安装，以及今后 Rust 版本之间的升级、失败恢复与回滚。旧 Node 安装迁移、还原旧服务及相应测试已删除。npm 平台包、签名、正式发布和生产部署属于后续发布阶段。

## 实现结构

共享模块负责协议、策略和工作生命周期；平台模块负责文件句柄、进程、私有存储与服务注册。管理命令直接执行；配对、连接和更新下载按需建立 Tokio 运行时，调度线程为一至四个。该数量不包含阻塞任务、搜索及进程监督线程。

| 模块 | 职责 |
| --- | --- |
| `cli`、`app`、`config`、`pairing` | 参数、私有配置、浏览器配对、已有设备身份及启动顺序。 |
| `transport`、`session`、`rpc`、`receipts` | 认证 WebSocket、ready 状态、心跳、请求准入、期限和结果回执。 |
| `files`、`searches` | 根目录相对操作、编码、摘要、分页、搜索及取消。 |
| `processes` | 有界输出、stdin、进程状态和进程树清理。 |
| `policy`、`sandbox`、`audit` | 本地权限、Linux 隔离与资源限制、脱敏审计。 |
| `storage`、`profile_lock` | 私有文件权限、对象身份与单配置实例互斥。 |
| `startup`、`startup_native`、`startup_grant` | 用户启动项、服务启动类型、审批及管理员授权。 |
| `native_install`、`install_lock`、`native_launcher` | 原生版本目录、安装互斥、稳定命令入口与切换恢复。 |
| `service_install`、`service_host` | 服务账户安装、原生 SCM 入口及服务关闭。 |
| `upgrade_cli`、`update`、`service_controller`、`service_update` | 更新索引、候选验证、停机切换和 Linux systemd 自动更新。 |
| `xtask` | 原生产物、来源清单、依赖许可证及归档往返校验。 |

代码入口见 [lib.rs](../crates/rdh-agent/src/lib.rs)。模块、协议及资源生命周期详见 [architecture.md](architecture.md)，配置约束见 [hardening.md](hardening.md)。

## 协议与生命周期

Agent 主动连接 Hub，不提供本机 HTTP 或 MCP 监听服务。连接携带设备凭据，发送 `hello`，收到 Hub `ready` 后才接收操作。用户通过浏览器批准新设备；已有私有配置保存设备身份。Hub 选择顺序为 `--server`、`RDH_SERVER`、源码默认地址，其他 Hub 的凭据不会随意复用。

普通请求最多同时执行八个。响应期限和实际工作生命周期分开：RPC 超时或断线不代表文件操作已经取消，也不代表没有发生写入；未完成工作继续占用名额。运行时回执、搜索、进程输出及发送队列均有容量和期限约束。旧连接的迟到回复不能进入新连接。

短时断线保留受管进程，持续离线、认证被拒绝或正常关闭会进入清理。配置锁在受管工作尚未完成时保持。POSIX 使用进程组，Windows 使用 Job Object；主动脱离 POSIX 进程组的程序不在同一清理保证内。文件根目录约束文件工具，不能代替 shell 的操作系统权限边界。

连接默认直连。`RDH_USE_ENV_PROXY=1` 显式启用代理变量，`RDH_CA_BUNDLE` 增加 PEM 信任根。配对、更新与 WebSocket 共用网络策略；TLS 证书和主机名校验保持开启。详见 [network.md](network.md)。

## 原生安装

`install-native` 在新的私有目录安装程序。Linux/macOS 使用版本目录及 `current` 链接，Windows 使用受校验的版本记录和原生命令转发。三个稳定命令名为 `rdh`、`remote-device-agent`、`remote-device-hub`，Windows 带 `.exe` 后缀。版本选择发生变化后，从这些入口运行会选择当前版本。

安装可登记一个已停机、已配对的私有配置。配置保留在原路径。正常 Agent 持有共享安装锁，更新管理器取得排他锁，并锁定已登记配置；有 Agent 或受管工作仍在运行时拒绝切换。

用户后台启动使用 systemd、LaunchAgent 或 Task Scheduler。`startup set` 管理未来启动状态，不负责停止当前连接。系统服务通过 `install-service` 创建：POSIX 要求 root、已有非 root 服务账户及其私有配置和状态目录；Windows 使用管理员所有的程序目录及 LocalService 数据目录。新服务以禁用或手动启动状态注册，操作者在本机启动后核对 Hub 连接。

引导脚本只负责取得正确平台的原生文件、校验摘要与版本，再调用 Rust 安装入口。构建、安装、运行及测试链不调用 Node/npm。具体命令见 [README](../README.md#install-and-run) 和 [distribution.md](distribution.md)。

## Rust 版本之间的更新与回滚

### 版本与数据兼容规则

原生更新索引采用 schema `2`、格式 `native-binary`，记录版本、大小、SHA-256、平台和灰度比例，可用 `min_from` 限定最低来源版本。客户端检查目标架构、内容摘要、候选报告的版本及已登记配置。索引中的摘要验证文件一致性；发布渠道的来源信任、签名和不可变托管在发布阶段落实。

本阶段更新不改写设备凭据和配置模式。候选程序先验证现有配置与策略，通过后才允许选中；回滚前同样验证旧程序仍能读取当前数据。这样可以在当前兼容格式内切换 Rust 版本，不能据此承诺未来任意数据格式都能回退。

后续版本如果需要改变持久化格式，应先声明可读取的格式和最低可升级版本。可兼容字段应保留回退能力；不兼容转换必须另行实现备份、转换成功确认和恢复测试，在具备这些机制前拒绝跨越不兼容版本。不得只切回旧二进制却让它读取不可识别的新格式。

### 停机更新

所有平台均支持停止 Agent 后更新。macOS、Windows 和系统服务使用停机切换与手动重启；Linux 用户 systemd 另有下述自动更新路径。

1. 结束进程、搜索等受管工作，停止使用该安装的 Agent 和服务。
2. 执行 `upgrade --no-restart`，校验候选及注册配置，保存切换意图，更新版本选择并保留前一版本。
3. 从稳定入口或本机服务管理器启动，核对新版本、原设备身份和 Hub 连接。
4. 需要返回前一版本时，再次停机，执行 `rollback --no-restart`，然后启动并验证。

以下绝对路径均为占位值。命令使用同一安装的稳定入口，索引必须来自所需平台的可信来源：

```sh
/absolute/install/rdh upgrade --index /absolute/native-index.json --channel canary --check
/absolute/install/rdh upgrade --index /absolute/native-index.json --channel canary --no-restart
/absolute/install/rdh rollback --no-restart
```

Windows 使用对应的 `rdh.exe`。系统服务更新需保持原服务账户、配置和服务定义一致；管理员先停止服务，再执行更新。更新不会取得用户凭据的所有权，也不会以管理员身份运行服务的候选配置验证。

### 中断恢复

更新先持久化 pending 记录，再修改当前版本选择和安装记录。存在未完成切换时禁止正常启动。`upgrade --recover` 只接受记录中的旧、新版本及预期文件状态，继续或恢复该次切换；记录缺失、文件摘要变化或额外外部修改会导致拒绝，并保留现场。

```sh
/absolute/install/rdh upgrade --recover
```

恢复完成后重新检查所选版本，再启动 Agent。配置锁恢复是另一项操作：只有确认原进程及其工作已结束，才使用显式 `--recover-lock`；活进程或无法核实的所有者会使恢复拒绝。不能通过删除锁绕过正在执行的工作。

### Linux systemd 自动更新

受支持的用户 systemd 路径核对单元定义、安装、配置与当前实例，确认工作已空闲后停机、切换并重启。试运行实例只有在持久化提交和明确激活后才执行远程请求。就绪判断核对该实例本次连接收到的 Hub ready；失败会切回保留的版本，控制器中断可从持久化记录恢复。

该入口仅控制可核实的目标单元，不是任意服务的通用控制器。macOS 和 Windows 的自动停机、自动重启与 Hub 就绪回退不在本阶段；它们仍可停机更新和回滚。详细参数见 `upgrade --help` 及 [maintenance.md](maintenance.md#update-and-recover-an-installation)。

## 平台与验证

正常构建默认开启已经实现的平台后端；保留的 `experimental-*` 名称用于显式裁剪构建。CI 用 `--no-default-features` 检查主动关闭能力时的错误行为。

[Agent CI](../.github/workflows/ci.yml) 针对以下八个目标逐个执行原生验证：Linux x86_64/ARM64 的 glibc/musl、macOS x86_64/ARM64、Windows x86_64/ARM64。每个目标构建独立版本候选，执行 Clippy、优化构建测试、产物归档与解包校验，以及适用的真实用户启动项和系统服务测试。服务测试使用临时对象，只应在可丢弃的 CI 主机执行。

最终验收覆盖删除迁移代码、启用默认后端和加入稳定命令入口后的完整版本。历史提交的通过记录不能替代这些变化后的验收；结果以 [草稿 PR #1](https://github.com/easelian/remote-device-agent/pull/1) 当前提交对应的 CI 为准。仅记录范围变更的 CI 作业不算测试通过。

本机 Linux 验证与 Windows 交叉检查、Darwin 模块检查提供不同层次的证据。交叉编译不证明 Windows ACL、Task Scheduler、SCM 或 launchd 的实际行为；模块检查也不代替完整链接。CI 服务验证不证明重登录、生产 Hub、特定企业代理、SMB 或断电持久性。当前阶段没有部署到用户设备。

## 构建与发布边界

仓库使用 Rust `1.93.0` 和固定 `Cargo.lock`。`xtask native-development` 生成二进制、校验文件、归档、来源清单和许可证集合；清单记录源码摘要、提交、脏工作树状态、工具链和目标。归档被重新解包，逐文件比较，并在本机目标运行解包后的版本检查。

当前源码没有项目自身的 `LICENSE`，产物明确保留 `release_ready: false`。这不妨碍重构验收，但正式发布前必须落实项目许可证、支持的最低系统版本、签名和托管。npm 包以后仅承担平台选择与原生程序分发；Agent 业务执行、服务启动和运行时更新继续由原生程序完成。
