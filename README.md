# Remote Device Hub Agent

使用 TypeScript 开发的设备 Agent，当前源码版本 `0.7.0`。目标平台为原生 Windows、Linux 和 macOS，三者共用 CLI、配对协议及文件/进程接口。设备需预装 Node.js 22+。

npm 包名为 `remote-device-hub`，命令别名为 `remote-device-agent` 和 `remote-device-hub`。最终包只包含编译压缩的 JavaScript、CLI 入口、README 和第三方许可证：无运行时 npm 依赖、原生 addon、安装脚本、首次启动编译或额外运行时下载。TypeScript、类型声明与构建工具仅用于开发，不进入发行包。

## 构建与验证

从仓库根目录运行：

```sh
npm ci --ignore-scripts
npm run typecheck
npm test
npm run pack:check
node scripts/release.mjs build
node scripts/release.mjs check releases/remote-device-hub-0.7.0.json
```

`src/*.ts` 是源码；`npm run build` 生成测试用的 `.build/` 模块和发行用的 `bundle/agent.mjs`。`npm pack` 会先做类型检查及编译。发行 TGZ 上限为 64 KiB，解包内容上限为 512 KiB；打包检查同时核对文件清单、空缓存离线安装和两个命令别名。未重新编译的源码或被修改的 bundle 会阻止生成发行记录。

构建只写本地文件，不执行 npm 发布、设备部署或账号绑定。版本记录及历史验证见 [维护与发布](docs/maintenance.md)。

## 部署与使用

将已构建的 `remote-device-hub-0.7.0.tgz` 分发到设备。在 TGZ 所在目录执行：

```sh
npm install --global --ignore-scripts --offline ./remote-device-hub-0.7.0.tgz
remote-device-agent --version
remote-device-agent --no-browser
```

版本应输出 `0.7.0`。首次运行打印绑定链接与验证码；在网页核对设备及权限后确认。默认文件根为当前目录，再次运行会复用绑定。已有 Node.js 的设备无需下载编译器或其他 npm 包。批量部署可缓存同一份固定版本 TGZ；固定下载地址和产物摘要能避免重复解析 registry 元数据，实际下载耗时仍取决于网络与分发节点。

用 `--root NAME=PATH` 指定目录，可重复；路径包含空格时给整个参数加引号。`--profile NAME` 创建独立绑定，`--server URL` 固定 Hub 地址。Windows 使用原生盘符或 UNC 根，文件工具内部的相对路径仍统一用 `/`。Windows 配对需要 Hub 使用 Protocol 1.0.1 或更新兼容版本；既有服务器的模型会拒绝 Windows 提案。

## 平台边界

| 能力 | Linux / macOS | Windows |
| --- | --- | --- |
| 配对、文件、搜索和进程接口 | 共用接口 | 共用接口，校验盘符、保留名称及备用数据流 |
| 默认 shell | `/bin/sh` | 系统 `cmd.exe`；命令内容使用对应系统语法 |
| 私有配置与审计 | 当前用户持有，文件 0600、目录 0700 | 当前用户持有且禁用继承的私有 ACL；使用系统自带 PowerShell 校验 |
| 进程终止 | POSIX 进程组 | `taskkill /T /F` 尽力终止存活命令树；父进程提前退出后的脱离子进程不保证清理 |
| bubblewrap 沙箱 | 仅 Linux；需系统另行提供 bwrap | 不支持；配置后拒绝启动 |
| shell 资源限制 | 按 shell 能力应用，失败退出 | 不支持 POSIX ulimit；配置后拒绝启动 |

默认配置目录为 Linux/macOS 的 `~/.config/remote-device-hub`，Windows 的 `%LOCALAPPDATA%\remote-device-hub`；可用 `XDG_CONFIG_HOME` 或 `--config` 显式指定。权限不合格的既有配置不会自动放宽或修复。操作策略与本地审计默认状态及配置方式见 [硬化配置](docs/hardening.md)。

文件 roots 和路径策略约束 Agent 文件工具，不是操作系统沙箱；本机其他进程并发替换路径仍有 TOCTOU 边界。shell 以 Agent 用户身份执行，不受文件路径策略约束。

网络短断线最多保留进程与有界输出 5 分钟；撤销授权、持续离线或主动退出会触发清理。超时或断线后结果未知时先核对状态再重试。Linux/macOS 的 `cleanup_pending` 表示监督进程资源尚未收尾；Windows 的此字段不证明所有脱离后代已结束。异常退出留下的配置锁需在确认旧进程结束后使用 `--recover-lock` 恢复。

当前本地验证环境为 Linux。CI 已配置三系统测试，其中 Windows 使用独立的原生文件、ACL、配对和进程用例；尚未实际运行的 Windows/macOS CI 与实机结果属于未验证范围，不能用 Linux 上的纯路径测试替代。历史发行状态以 `releases/` 原始记录为准。
