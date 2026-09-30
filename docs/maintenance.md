# Agent 维护与发布

本仓库只维护 Node Agent。源码、测试、构建器和 CI 都不依赖 Server、Integrations 或 Python。

使用 Node.js 22+ 从仓库根目录运行：

```bash
node --test test/*.test.mjs
node scripts/release.mjs build
node scripts/release.mjs check releases/remote-device-hub-0.6.0.json
```

版本来自 `package.json`，TGZ 和 record 名称随版本生成。包名保持 `remote-device-hub`，两个 bin 别名保持 `remote-device-hub` 和 `remote-device-agent`。同版本不能覆盖不同源码或产物。

构建器使用标准 Node 库生成确定性 tar/gzip，并严格打包已有的 vendored `ws` client 文件。TGZ 预算 64 KiB，解包内容预算 512 KiB；不包含测试、构建脚本、Server、Python、原生 addon、安装 hook 或 runtime npm dependencies。修改 vendored 库时同步审核来源、license、SHA 和 client allowlist。

发布记录包含实际 Node 版本、Git 状态、源码摘要、builder 摘要和产物摘要。`record` 命令只保存已执行动作的 host、检查结果和证据文件，不执行 npm 发布或设备部署；阶段不能倒退。`verified` 要求 package-integrity、install-cli、binding-reuse、protocol-compatibility 均通过。

跨仓库兼容性通过 Server 仓库的检查器显式传入这个 TGZ 验证，不让任一构建器读取兄弟工作树。公网 TLS、生产 OAuth 和 ChatGPT 工具调用需要单独记录真实环境证据。本次分仓的记录保留 `prepared` 状态。

Node profile 使用目录锁。迁移前停止旧 Agent，随后复用相同配置；异常退出留下的锁应在确认旧进程停止后清理。旧 Python Agent 与 Node Agent 不共享锁协议。文件适配器的父目录检查仍有本机其他进程并发替换时的 TOCTOU 限制，维护更新不能把文件 roots 描述为系统沙箱。

## Node 0.6.0 当前源码能力（发布待验）

普通网络断线时保留进程及有界输出最多 5 分钟并等待重连；授权撤销以及 HTTP 401/403 属于终止条件，必须停止并清理进程。后台搜索 API 使用 `start_search`、`get_search_results`、`stop_search` 和 `list_searches`：结果分页上限 200，active TTL 30 秒，完成任务最多保留 5 分钟，最多 2 个并发任务，遍历最多 30 层/10000 个条目，结果最多 1000 条或 256 KiB，并通过 `truncated`、`truncated_reason`、`skipped` 和 `status` 表示边界。

这些是当前 `0.6.0` 接口和资源边界，搜索元数据最多保留 16 个任务；已取消的 worker 在文件描述符清理完成前仍占用并发额度。发布状态以本仓库的 release record 与 evidence 为准。Python Agent `0.4.0` 保留 legacy core operation 合同，并明确拒绝这些扩展 operation。
