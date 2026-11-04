# Remote Device Hub Agent

[English](README.md) | 简体中文

将 Linux、macOS 或 Windows 设备接入 Remote Device Hub，在设备用户的权限范围内执行远程文件、搜索和进程操作。Agent 是名为 `rdh` 的独立原生程序。

## 安装与运行

### 脚本直装

**Linux 和 macOS** 需要 `curl`，以及 `sha256sum` 或 `shasum`：

```sh
curl -fsSL https://raw.githubusercontent.com/easelian/remote-device-agent/main/packaging/install.sh | sh
```

**Windows** 使用 PowerShell：

```powershell
irm https://raw.githubusercontent.com/easelian/remote-device-agent/main/packaging/install.ps1 | iex
```

脚本选择 GitHub 上最新发布的 Release，包括预发布版，校验对应平台程序的 SHA-256 和版本后安装单个文件：Linux/macOS 为 `~/.local/bin/rdh`，Windows 为 `%LOCALAPPDATA%\RemoteDeviceAgent\rdh.exe`。Linux 默认使用 musl 构建，支持的 CPU 架构为 x86_64 和 ARM64。

安装器将命令目录保存到用户 PATH，并启动 `rdh`。POSIX 安装器没有可用终端时会完成安装，并提示在交互式终端运行 `rdh`。POSIX 的新终端会加载保存的 PATH；Windows 还会更新当前 PowerShell 会话。已有命令文件不会被覆盖。只安装不启动可设置 `RDH_NO_START=1`，不修改 PATH 可设置 `RDH_NO_MODIFY_PATH=1`。`RDH_INSTALL_DIR` 可指定其他绝对安装目录。离线产物、镜像和安装器选项见[分发说明](docs/distribution.md#bootstrap-installation)。

### 配对与连接

在交互式终端运行：

```sh
rdh
```

首次运行时选择共享目录，直接按 Enter 会采用提示中的当前目录。Agent 将选择保存到本地 `settings.json`，输出配对码和验证链接，并尝试打开浏览器。在页面中确认设备、共享目录和权限后批准配对。浏览器没有打开时，使用终端输出的链接。

已有绑定时，Agent 复用保存的设备身份和根目录，不打开浏览器。前台连接持续到按下 Ctrl+C 或进程终止。在其他目录运行 `rdh` 不会改变已保存的共享目录。

公开命令如下：

| 命令 | 行为 |
| --- | --- |
| `rdh` | 启动 Agent；未绑定时请求浏览器批准。 |
| `rdh unbind` | 停止 Agent 和受管工作，撤销绑定，并取消已记录的自动启动。 |
| `rdh update` | 在 Agent 停止后更新可执行文件。 |
| `rdh --help` | 查看命令帮助。 |
| `rdh --version` | 查看程序版本。 |

### 解绑或移除 Agent

删除程序前运行 `rdh unbind`。如果无法连接 Hub，Agent 会持久保存解绑请求，阻止旧绑定继续启动。恢复网络后再次运行 `rdh unbind`。手动运行 `rdh` 也会先完成尚未结束的撤销，然后退出；再次运行才开始配对。

解绑完成后，只删除已安装的可执行文件，保留共用命令目录和无关的 PATH 条目。单独删除程序文件不会撤销 Hub 上的设备身份。本地共享目录设置会保留，供以后配对使用。

### 更新

结束受管工作并停止 Agent 后运行：

```sh
rdh update
```

更新器选择 GitHub 上最新发布的 Release，包括预发布版，验证候选程序后替换可执行文件。绑定和启动设置保持不变。更新成功后运行 `rdh` 重新连接。失败处理见[维护说明](docs/maintenance.md#update-the-agent)。

## 权限、启动与网络

文件根目录和路径策略约束文件工具。Shell 命令以 Agent 所属操作系统用户的权限运行；共享一个目录并不意味着 shell 只能访问该目录。批准设备时，应单独确认 shell 权限。响应超时不能证明操作没有产生效果；重试结果不确定的写入或进程操作前，先检查实际状态。

自动启动请求通过启动管理 RPC 发起。在本机交互式终端再次运行 `rdh`，可以查看并批准或拒绝待处理请求。Windows 自动启动使用标准 `LOCALAPPDATA` 配置目录。解绑会取消为该绑定记录的启动项。公开 CLI 仅包含上表中的命令。

配置目录通常为 POSIX 上的 `~/.config/remote-device-hub`，以及 Windows 上的 `%LOCALAPPDATA%\remote-device-hub`。POSIX 遵循 `XDG_CONFIG_HOME`。目录保存私有绑定状态和 `settings.json`，使用前会检查权限。凭据和未完成的生命周期记录应保持私有。

新配对使用编译进 [deployment_defaults.rs](src/deployment_defaults.rs) 的 Hub 地址；已有绑定保留其保存的 Hub。连接默认直连，`RDH_USE_ENV_PROXY=1` 启用本机代理环境变量，`RDH_CA_BUNDLE` 添加 PEM 信任根。详见[网络配置](docs/network.md)和[安全加固](docs/hardening.md)。

## 开发与文档

使用 Rust 1.93.0 和仓库中的锁文件。构建、测试步骤见[维护说明](docs/maintenance.md#build-and-validate)；平台运行需要对应的操作系统、链接器和 SDK。源码检查与交叉编译不能证明原生运行行为或断电持久性。

- [代码导览](docs/code-structure-zhCN.md)（[English](docs/code-structure.md)）：源码职责和阅读顺序。
- [生命周期](docs/native-lifecycle-zhCN.md)（[English](docs/native-lifecycle.md)）：启动、解绑和程序替换。
- [架构](docs/architecture.md)：连接、请求和工作生命周期。
- [分发说明](docs/distribution.md)：产物、安装器和发布准备。

## 许可证

本项目采用 Apache License 2.0，完整条款见 [LICENSE](LICENSE)。随附依赖保留各自的许可证，记录在原生产物的清单和许可证目录中。
