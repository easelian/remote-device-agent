# Node Agent 0.5.0 本地验证

`remote-device-hub` npm Agent 0.5.0 已改为直接运行 Node.js 22+。包不包含 Python、uv、wheel、原生 `.node` addon、平台预编译文件或安装 hook；WebSocket 只携带固定版本 `ws` 的客户端所需源码和 MIT 许可。Hub 与独立 Python Agent 仍按各自组件维护，不参与这个 npm 包的启动。

源码基点为 `0f54e9b84cda470ccd3f7af98b704571a6441d2c`。本地发布物是 [remote-device-hub-0.5.0.tgz](../dist/remote-device-hub-0.5.0.tgz)，摘要与输入记录见 [agent-manual-0.5.0.json](../releases/agent-manual-0.5.0.json)。状态保持 `prepared`，没有发布到 npm、推送 Git、部署生产服务或更新 ChatGPT 插件。

## 实测结果

最终发布物的 SHA-256 是 `5f5a5be581cc4449ea2a5fe3ec95e8d7ef5a1a0c4157ad3c11082b15cac5c3e3`。

| 项目 | 实测 |
| --- | --- |
| TGZ 文件 | 43,045 bytes，约 42.0 KiB |
| 包内文件合计 | 169,950 bytes，约 166.0 KiB |
| 空 npm 缓存、首次启动到配对码 | 688 ms |
| 缓存后启动到配对码，5 次样本 | 329 / 332 / 325 / 335 / 328 ms |
| 复用已有绑定到 WebSocket 在线 | 415 ms |
| 首次启动到在线，测试自动批准 | 3,785 ms |

测试使用 macOS arm64、Node 22.23.3、本地 TGZ 和本地真实 Hub fixture。npm 以 `--offline` 执行，registry 指向不可连接的 loopback 端口，PATH 只有 Node 和系统 `/bin/sh`。首次计时包含 npm 对本地 TGZ 的安装和 Agent 启动，不包含互联网下载 TGZ 或人手确认绑定的时间。配对码时间从子进程启动计至输出行到达；在线时间包含协议要求的至少 3 秒配对轮询等待。

原始非敏感结果见 [node-agent-0.5.0-runtime.json](../releases/evidence/2026-09-30/node-agent-0.5.0-runtime.json)。测试只观察到 `/api/pair/start`、`/api/pair/poll` 和 `/agent/:device_id`，没有 Python/uv/runtime 缓存或外部 npm 运行依赖下载。配置、token、验证码和 Agent 原始日志不保存到这份结果中。

## 功能与维护验证

本地真实 Hub fixture 已验证设备信息、目录列表、创建目录、文件读写、SHA-256 冲突、文本替换、文件搜索、进程输出、stdin 和进程终止。Ctrl+C 后再次启动复用相同配置字节、device id 和 Hub 中唯一的设备记录。npm 重复转发中断信号的退出路径也已修复，正常退出完成任务与 profile 锁清理。

完整组件测试为 103 项 Python 测试、31 项 Node 测试通过。Node 22 下的核心测试在 Linux x64/arm64、macOS arm64 和通过 Rosetta 运行的 macOS x64 样本中均为 31/31；平台测试覆盖开发候选源码，最终 TGZ 的全链路 fixture 检查在 macOS arm64 上执行。GitHub Actions 配置已更新为 Node 22，Agent 单元测试不安装 Python，手动 Agent 构建增加最终包的本地 Hub 检查；本次没有运行远程 Actions。

运行时源码在 `npm/agent/lib/`；固定 WebSocket 源码与来源记录在 `npm/agent/vendor/ws/`。构建器只打包客户端所需文件，不把测试用的 WebSocket 服务端代码放入 TGZ。`scripts/release.py build agent` 单独构建 Agent，不依赖 Hub、Protocol 或 Python Agent 的源码指纹。仓库构建与验证工具仍使用 Python；目标设备上的 npm Agent 运行时不需要 Python。

重新执行发布构建和本地验证：

```bash
.venv/bin/python scripts/release.py build agent
.venv/bin/python scripts/check_node_agent.py \
  --artifact dist/remote-device-hub-0.5.0.tgz \
  --node /absolute/path/to/node \
  --output state/release-evidence/node-agent-runtime.json
```

## 实现边界

纯 Node 文件操作保留路径越界检查、静态符号链接拒绝、大小限制、SHA-256 冲突检查和原子替换，并在操作前后校验目录身份。它不等价于 `openat` 的目录 fd 锚定，无法完全防住其他本地进程在检查后替换父目录的竞态；文件根目录不作为防御本地进程攻击的安全沙箱。shell 仍使用 Agent 系统账户权限。

Node profile 使用原子 `mkdir` 锁目录和私有 PID/nonce owner 文件。正常退出释放自己持有的锁；SIGKILL、断电等异常退出可能留下锁，需要确认对应进程停止后清理。Node 锁不与旧 Python `flock` 互操作，迁移时先停止旧 Agent，再复用相同配置；Hub 拒绝同一 device id 的重复连接。

这些结果没有验证生产 HTTPS/WSS、公开 OAuth、真实 ChatGPT 客户端或生产设备重连，不将本地通过写成已上线。
