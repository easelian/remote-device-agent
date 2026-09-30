# Remote Device Hub Agent

独立发布包为 `0.5.0`，是一个轻量 pure Node Agent launcher。它要求 Node.js 22+，把 `ws` 固定版本直接 vendored 在包内，没有 npm runtime dependencies、install hooks、原生 addon、Python/uv 下载、wheel 下载或首次启动编译。

当前 0.5.0 尚未发布到 npm。在目标 Linux 或 macOS 的工作目录使用本地构建的 TGZ：

```sh
npx --yes /path/to/remote-device-hub-0.5.0.tgz
```

文件适配器只使用 Node 标准库：通过 `lstat`/`realpath` 检查、`fstat` 和 `O_NOFOLLOW` 静态拒绝符号链接，更新时校验 SHA-256 并以临时文件原子替换。父目录被其他本地进程并发替换时仍存在无法完全防住的 TOCTOU 窗口；file roots 不构成本地其他进程攻击的 sandbox。

启动时只负责读取本地配置、完成绑定请求、通过 WebSocket 连接 Hub 并管理本机 Agent 进程。首次启动不创建 Python 虚拟环境、不下载 uv/Python/wheel，也不访问 npm registry 安装运行时依赖，因此包大小和首次启动时间应以本地构建/基准记录为准；构建器硬性限制 TGZ 不超过 64 KiB、解包后不超过 512 KiB；实际大小和计时条件见仓库的发布验证记录。

默认文件目录是当前目录。可以使用 `--root projects=/home/me/projects` 指定目录，用 `--no-browser` 在 SSH 服务器上仅显示链接，用 `--profile NAME` 为同一账户建立另一份独立绑定。

设备配置保存在 `~/.config/remote-device-hub/`（尊重 `XDG_CONFIG_HOME`），权限为 0600。再次运行同一命令会复用绑定。保持终端运行；Ctrl+C 停止 Agent。暂不自动安装开机服务。

同一 Node profile 使用 atomic `mkdir` 加私有 owner PID/nonce 文件互斥。异常退出遗留的锁会 fail closed，确认旧进程停止后需要手动清理；不会使用 `flock` 或自动 reclaim。旧 Python Agent 与 Node Agent 不共享锁协议，迁移时先停止旧 Python Agent，再复用相同 config/token/identity；Hub 会拒绝 duplicate connections。

独立 Python Agent `remote-device-agent` 仍然作为可维护、可单独发布的兼容实现；它不再被 npm Agent 打包。Python Agent 的 wheel/source archive 通过根项目的发布脚本单独构建，不是 Node Agent 的首次启动依赖。

`0.4.1` npm 包只是历史 prepared 版本，不能覆盖或冒充 `0.5.0`。`0.5.0` 只有在纯 Node 文件适配器、包大小、启动速度、绑定复用和重连完成本地验证并生成发布记录后，才可进入发布评估；当前尚未发布到 npm。
