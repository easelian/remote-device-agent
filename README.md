# Remote Device Hub Agent

这是独立发布的 pure Node Agent，版本 `0.5.1`，npm 包名保持 `remote-device-hub`，两个
bin 别名保持 `remote-device-hub` 与 `remote-device-agent`。要求 Node.js 22+，运行时无 npm
dependencies、安装 hook、原生 addon、Python/uv/wheel 下载或首次启动编译。

## 本地验证和构建

```bash
node --test test/*.test.mjs
node scripts/release.mjs build
node scripts/release.mjs check releases/remote-device-hub-0.5.1.json
```

构建只生成本仓库的 `dist/` 和 `releases/` 记录，不调用 Python builder、npm registry、
Server 或 ChatGPT。构建后的 TGZ 由 Server 通过显式绝对路径 artifact 检查；它不会被 Server
自动发现或打包。

## 使用

```bash
npx --yes /absolute/path/remote-device-hub-0.5.1.tgz \
  --root projects=/home/alice/projects --name '开发机'
```

默认文件根是当前目录；`--no-browser` 用于 SSH，`--profile NAME` 建立另一份绑定。配置
位于 `XDG_CONFIG_HOME` 下并使用 0600 权限。文件根限制属于 Agent 工具边界，不是对本机
其他进程的 sandbox。

该仓库只维护 Node Agent；Python 兼容 Agent 和 Protocol 位于 `remote-device-server`。
本地 prepared TGZ 不表示 npm 发布、设备部署或 ChatGPT 客户端已更新。

版本和记录规则见 [维护与发布](docs/maintenance.md)。`MIGRATION.json` 记录初始导入来源，`docs/history/` 保留分仓前的验证证据。
