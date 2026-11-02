# Remote Device Hub Agent

[English](README.md) | 简体中文

将 Linux、macOS 或 Windows 设备接入 Remote Device Hub，在设备用户的权限范围内执行远程文件、搜索和进程操作。

可以选择[脚本直装](#脚本直装)或 [npm 安装](#npm-安装)。两种方式都提供可直接运行的原生 Agent，无需安装 Rust 工具链或下载源码。

## 安装与运行

### 脚本直装

脚本根据操作系统和 CPU 选择二进制文件，校验摘要与版本后安装、保存用户 PATH 并启动 Agent。请在希望开放访问的目录中运行安装命令。支持 x86_64 和 ARM64，不需要 Node.js 或 npm。

**Linux 和 macOS**：需要 `curl`，以及 `sha256sum` 或 `shasum`。

```sh
curl -fsSL https://raw.githubusercontent.com/easelian/remote-device-agent/main/packaging/install.sh | sh
```

**Windows**：在 PowerShell 中运行。

```powershell
irm https://raw.githubusercontent.com/easelian/remote-device-agent/main/packaging/install.ps1 | iex
```

脚本默认获取 GitHub 上最新发布的 Release（包括预发布版）：Linux/macOS 安装到 `~/.local/lib/rdh`，Windows 安装到 `%LOCALAPPDATA%\RemoteDeviceAgent`。下载时显示进度。Linux/macOS 自动写入 shell 启动配置，新终端会加载 PATH；Windows 同时更新用户 PATH 和当前 PowerShell 会话。Linux 默认选择 musl，macOS 和 Windows 选择对应的原生二进制文件，无需填写参数。

安装完成后，Agent 直接在前台运行，打开或输出配对页面。在页面中批准设备和权限，按 Ctrl+C 可停止 Agent。只安装不启动时使用 `--no-start` / `-NoStart`；不修改 PATH 时使用 `--no-modify-path` / `-NoModifyPath`。

脚本不会覆盖已有安装。自定义目录、版本、GNU libc 构建及离线安装见[分发说明](docs/distribution.md#bootstrap-installation)。

### npm 安装

需要 Node.js 22.14 或更高版本及 npm。在使用 glibc 的 Linux 上，`0.8.0` 包要求 glibc 2.39 或更高版本；glibc 版本较低的系统可使用脚本默认安装的 musl 版本。

安装 [GitHub Release](https://github.com/easelian/remote-device-agent/releases/tag/v0.8.0) 中的 npm 包：

```sh
npm install --global https://github.com/easelian/remote-device-agent/releases/download/v0.8.0/easelian-remote-device-agent-0.8.0.tgz
```

包名为 `@easelian/remote-device-agent`，内含各平台的原生二进制文件，启动时选择匹配的平台。同一条命令适用于 Linux、macOS 和 Windows；npm 的全局可执行文件目录需要位于 PATH 中。

两种安装方式都提供 `rdh`、`remote-device-agent` 和 `remote-device-hub` 命令。npm 命令只安装包，安装后运行 `rdh`。后台服务需要单独配置。

### 配对与连接

脚本安装完成后会自动启动 Agent。使用 npm 安装或以后再次运行时，进入希望开放访问的目录并启动：

```sh
rdh --no-browser
```

首次启动时，Agent 输出配对网址和配对码。在浏览器中确认设备、根目录和权限后批准配对。`--no-browser` 只输出网址，不自动打开浏览器。未指定根目录时使用当前目录。

`--root NAME=PATH` 可以重复指定；参数包含空格时，将整个参数用引号括起。`--profile NAME` 选择独立配置，`--config PATH` 指定私有配置文件。`--pair-only` 保存或验证配对后退出。

配对后的前台 Agent 会保持连接，直到按下 Ctrl+C 或进程终止。再次使用相同命令会复用设备身份。同一份配置不能同时运行两个实例。要将已有且已停止使用的配置登记到原生安装中，可向 `install-native` 传入 `--config /absolute/paired.json`；程序校验后保留原路径，不复制凭据。

### 后台启动

使用已安装的命令完成配对后，查看并启用用户后台启动：

```sh
rdh startup status --config /absolute/paired.json
rdh startup set user --config /absolute/paired.json
```

这些命令创建或启用后续启动配置，不负责启动或停止当前连接。结束前台实例后，通过本机服务管理器启动已登记的单元、LaunchAgent 或任务。Linux 使用 systemd 用户单元，macOS 使用 LaunchAgent，Windows 使用任务计划程序。`startup set none` 禁用后续启动。

远程启动管理请求需要本地批准或管理员授权。`startup --help` 说明了 `approve`、`deny`、`grant`、`revoke` 和显式 `bind` 的用法。

系统服务安装说明见 `install-service --help`。POSIX 安装要求 root 权限、已有的非 root 服务账户，以及该账户已配对且停止使用的配置。Windows 要求管理员权限，并创建 LocalService 服务注册。两者均将可写设备状态与受保护的程序文件分开，创建的服务处于停止状态，需要在本机启动；程序不会自动提权。

### 更新与卸载

npm 安装的 Agent 应先停止，再通过 npm 安装所需版本的发布包。卸载前禁用后台启动注册，然后执行 `npm uninstall --global @easelian/remote-device-agent`。

脚本安装使用原生更新机制，校验 schema `2` 的索引和候选可执行文件。执行 `upgrade --no-restart` 或 `rollback --no-restart` 前，先结束受管工作并停止 Agent；之后在本机重启，确认重新连接到 Hub。Linux 上经过验证的 systemd 用户服务路径还支持自动更新，以及就绪检查失败时回滚。macOS 和 Windows 采用停机更新、手动重启。详见[维护说明](docs/maintenance.md#update-and-recover-an-installation)。

卸载时，先禁用后台启动，停止使用该安装的所有 Agent，并通过 systemd、launchd、任务计划程序或 SCM 删除相应注册。从 shell 配置或 Windows 用户环境变量中移除对应的 PATH 条目，再只删除程序目录，保留配置、策略、审计和数据目录，包括服务账户独立使用的状态目录。删除前核对路径。删除程序文件不会撤销 Hub 上的设备身份。

## 平台行为与限制

| 行为 | Linux / macOS | Windows |
| --- | --- | --- |
| Shell | `/bin/sh` | 系统 `cmd.exe` |
| 私有配置 | 仅所有者可访问的文件和目录 | 原生 SID 与受保护 ACL 检查 |
| 文件访问 | 根目录相对路径，持有目录描述符 | 持有句柄，检查重解析点和保留名称 |
| 进程监督 | POSIX 进程组 | Job Object 与有界管道处理 |
| 后台启动 | systemd / launchd | 任务计划程序 / 原生 SCM 服务入口 |
| bubblewrap | 仅 Linux，需单独安装 | 不支持；配置后拒绝启动 |
| POSIX 资源限制 | 在支持的平台应用；设置失败时阻止启动 | 不支持；配置后拒绝启动 |

POSIX 的默认配置目录为 `~/.config/remote-device-hub`，Windows 为 `%LOCALAPPDATA%\remote-device-hub`。可通过 `XDG_CONFIG_HOME` 或 `--config` 指定其他位置。权限过宽的已有配置会被拒绝，不会自动修复。

审计默认开启，每台设备最多保留 1,000 条脱敏记录，同时受字节数和轮转文件数限制。日志通常位于 `~/.local/state/remote-device-hub` 或 `%LOCALAPPDATA%\remote-device-hub` 下；系统服务使用显式配置的状态目录。

文件根目录和路径策略约束文件工具。Shell 命令以 Agent 所属操作系统用户的权限运行，需要更强隔离时应显式配置沙箱。响应超时不能证明操作没有产生效果；对结果不确定的写入或进程操作，重试前先检查实际状态。

短时断线会保留受管任务，最长五分钟；认证被拒绝、持续离线或关闭 Agent 会启动清理。仍有可能产生副作用的工作时，配置锁保持占用。

交叉编译通过不能证明原生运行行为。UNC 路径语法测试不代表实际 SMB 验证，进程和文件测试也不代表断电持久性验证。策略与审计规则见[安全加固](docs/hardening.md)，原生 CI 的覆盖范围见[平台与验证](docs/native-lifecycle-zhCN.md#平台与验证)。

## Hub 与网络设置

Hub 地址依次取自 `--server`、`RDH_SERVER` 和 [deployment_defaults.rs](src/deployment_defaults.rs) 中的默认值。文件中列出的公共入口与 MCP 入口共享配置别名和锁；自定义入口保持隔离。显式指定的配置若属于另一个 Hub，会被拒绝，凭据不会转发。修改默认值不会改写已安装服务的命令或已有绑定。

配对返回 `discovery_url` 时，Agent 使用保存的设备凭据向对应 Gate 认证。Gate 返回短时、单次使用的票据，绑定分配的 Core 和当前 Agent 实例；发送到 Core 的只有该票据。Gate 的认证协议要求支持 v2 的 Agent，discovery v1 客户端无法通过这一 Gate/Core 边界认证。详见[集群发现](docs/network.md#cluster-discovery)。

连接默认直连。设置 `RDH_USE_ENV_PROXY=1` 可显式启用本机代理环境变量，`RDH_CA_BUNDLE` 可添加 PEM 信任根。详见[网络配置](docs/network.md)。

## 开发

### 构建与验证

使用 Rust 1.93.0、目标平台的原生链接器和 SDK，以及仓库中的锁文件：

```sh
cargo +1.93.0 fmt --all -- --check
cargo +1.93.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
mkdir -p .build
cargo +1.93.0 run --locked -p xtask -- native-test-candidate \
  --target x86_64-unknown-linux-gnu --out "$PWD/.build/native-test-candidate"
RDH_NATIVE_TEST_CANDIDATE_DIR="$PWD/.build/native-test-candidate" \
  cargo +1.93.0 test --release --locked --workspace --all-targets --all-features --no-fail-fast
cargo +1.93.0 run --locked -p xtask -- native-development \
  --target x86_64-unknown-linux-gnu --out .build/native-development
```

将目标三元组改为与本机匹配的值，每次构建使用新的输出目录。构建工具生成原生可执行文件、校验和、归档、来源记录，并验证解包结果。

需要独立更新候选或真实服务管理器的集成测试还有额外前提，见[维护说明](docs/maintenance.md)和 [Agent CI](.github/workflows/ci.yml)。普通本地测试不会覆盖全部特权测试和平台专用测试。

将本地构建的程序安装到新目录：

```sh
/absolute/artifact/rdh install-native --prefix /absolute/new-install
```

Windows 使用 `rdh.exe` 和 Windows 绝对路径。工作区版本定义在 [Cargo.toml](Cargo.toml) 中。

## 文档

- [架构](docs/architecture.md)：组件、协议和工作生命周期。
- [代码导览](docs/code-structure-zhCN.md)（[English](docs/code-structure.md)）：阅读顺序、目录职责和扩展入口。
- [安全加固](docs/hardening.md)：策略、沙箱、环境变量和审计。
- [分发说明](docs/distribution.md)：产物、引导安装和更新渠道格式。
- [维护说明](docs/maintenance.md)：检查、升级和恢复。
- [原生生命周期](docs/native-lifecycle-zhCN.md)（[English](docs/native-lifecycle.md)）：安装、更新和平台验证。

## 许可证

本项目采用 Apache License 2.0，完整条款见 [LICENSE](LICENSE)。随附或链接的依赖保留各自的许可证，记录在原生构建产物的清单和许可证目录中。
