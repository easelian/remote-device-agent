# Remote Device Hub Agent

这是独立发布的 pure Node Agent，当前源码版本 `0.6.0`，npm 包名保持 `remote-device-hub`，两个
bin 别名保持 `remote-device-hub` 与 `remote-device-agent`。要求 Node.js 22+，运行时无 npm
dependencies、安装 hook、原生 addon、Python/uv/wheel 下载或首次启动编译。

## 本地验证和构建

```bash
node --test test/*.test.mjs
node scripts/release.mjs build
node scripts/release.mjs check releases/remote-device-hub-0.6.0.json
```

构建只生成本仓库的 `dist/` 和 `releases/` 记录，不调用 Python builder、npm registry、
Server 或 ChatGPT。构建后的 TGZ 由 Server 通过显式绝对路径 artifact 检查；它不会被 Server
自动发现或打包。

## 使用

```bash
npx --yes /absolute/path/remote-device-hub-0.6.0.tgz \
  --root projects=/home/alice/projects --name '开发机'
```

默认文件根是当前目录；`--no-browser` 用于 SSH，`--profile NAME` 建立另一份绑定。配置
位于 `XDG_CONFIG_HOME` 下并使用 0600 权限。文件根限制属于 Agent 工具边界，不是对本机
其他进程的 sandbox。

该仓库只维护 Node Agent；Python 兼容 Agent 和 Protocol 位于 `remote-device-server`。
本地 prepared artifact 和运行验证不表示 npm 发布、设备部署或 ChatGPT 客户端已更新。实际发行状态与证据记录在本仓库的 `releases/`。

版本和记录规则见 [维护与发布](docs/maintenance.md)。`MIGRATION.json` 记录初始导入来源，`docs/history/` 保留分仓前的验证证据。

## Node 0.6.0 当前源码能力

普通网络断线时保留进程和有界输出最多 5 分钟并等待重连；授权撤销或 HTTP 401/403 会立即停止并清理，并报告失败。超过离线期限或主动退出也会清理进程。

当前源码还包括后台有界搜索、`file_info`、`read_multiple_files`、`move_file` 和 `read_file_chunk`。这些是 Node `0.6.0` 扩展能力，不改变 Python Agent `0.4.0` 的 legacy core operation 合同。

搜索结果按 offset 分页，任务支持取消，最多保留 16 个任务的有界结果。文件移动仅限同一 root 内的普通文件，目标必须不存在。批量读取最多 16 个文件，每个文件不超过 1 MiB，返回有效 UTF-8 预览和局部错误。大文件 chunk 接口支持最大 64 MiB 的文件和最多 64 KiB 的 byte range；`from_end` 从尾部定位，`content_base64` 保留精确字节，`utf8_valid` 标明文本预览是否完整有效。批量预览与 chunk 的 fingerprint 不能替代写入所需的整文件 SHA-256。
