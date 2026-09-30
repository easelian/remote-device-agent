# Agent 维护与发布

本仓库只维护 Node Agent。源码、测试、构建器和 CI 都不依赖 Server、Integrations 或 Python。

使用 Node.js 22+ 从仓库根目录运行：

```bash
node --test test/*.test.mjs
node scripts/release.mjs build
node scripts/release.mjs check releases/remote-device-hub-0.6.1.json
```

版本来自 `package.json`，TGZ 和 record 名称随版本生成。包名保持 `remote-device-hub`，两个 bin 别名保持 `remote-device-hub` 和 `remote-device-agent`。同版本不能覆盖不同源码或产物。

构建器使用标准 Node 库生成确定性 tar/gzip，并严格打包已有的 vendored `ws` client 文件。TGZ 预算 64 KiB，解包内容预算 512 KiB；不包含测试、构建脚本、Server、Python、原生 addon、安装 hook 或 runtime npm dependencies。修改 vendored 库时同步审核来源、license、SHA 和 client allowlist。

发布记录包含实际 Node 版本、Git 状态、源码摘要、builder 摘要和产物摘要。`record` 命令只保存已执行动作的 host、检查结果和证据文件，不执行 npm 发布或设备部署；阶段不能倒退。`verified` 要求 package-integrity、install-cli、binding-reuse、protocol-compatibility 均通过。

跨仓库兼容性通过 Server 仓库的检查器显式传入这个 TGZ 验证，不让任一构建器读取兄弟工作树。公网 TLS、生产 OAuth 和 ChatGPT 工具调用需要单独记录真实环境证据。本次分仓的记录保留 `prepared` 状态。

Node profile 使用目录锁。迁移前停止旧 Agent，随后复用相同配置；异常退出留下的锁使用显式 `--recover-lock` 恢复，只有已退出 PID 的私有合法锁才可回收。恢复 guard 遗留时须先人工核对。旧 Python Agent 与 Node Agent 不共享锁协议。文件适配器的父目录检查仍有本机其他进程并发替换时的 TOCTOU 限制，维护更新不能把文件 roots 描述为系统沙箱。

## Node 0.6.1 当前源码能力（发布待验）

普通网络断线时保留进程及有界输出最多 5 分钟并等待重连；授权撤销以及 HTTP 401/403 属于终止条件，必须停止并清理进程。后台搜索 API 使用 `start_search`、`get_search_results`、`stop_search` 和 `list_searches`：结果分页上限 200，active TTL 30 秒，完成任务最多保留 5 分钟，最多 2 个并发任务，遍历最多 30 层/10000 个条目，结果最多 1000 条或 256 KiB，并通过 `truncated`、`truncated_reason`、`skipped` 和 `status` 表示边界。

这些是当前 `0.6.1` 接口和资源边界，搜索元数据最多保留 16 个任务；已取消的 worker 在文件描述符清理完成前仍占用并发额度。发布状态以本仓库的 release record 与 evidence 为准。Python Agent `0.4.0` 保留 legacy core operation 合同，并明确拒绝这些扩展 operation。

## 2026-09-30 本地安装包验证

`0.6.0` 已生成 [prepared record](../releases/remote-device-hub-0.6.0.json)：TGZ 48,899
bytes，解包文件 196,632 bytes。macOS arm64 和 Linux x86_64 在 Node `22.23.3` 下分别
通过完整 59 项测试；最后仅修正 gzip OS header，两端各通过最终打包器的 5 项 release
测试。Linux 重建的 TGZ 与 macOS 正式产物逐字节一致。

安装后的 Hub `0.5.0` wheel 与最终 TGZ 完成配对、基础与扩展工具操作、搜索、进程操作、
相同配置重连；WebSocket 断线后同一 job 的 PID 保持，重连后可继续输入和读取输出。
这项跨包验证在 macOS arm64 执行。测试与 Linux 重建、wheel 联调证据分别见
[tests](../releases/evidence/node-0.6.0-tests-20260930.json)、
[Linux](../releases/evidence/linux-0.6.0-20260930.json) 和
[Server compatibility](../releases/evidence/server-compat-0.6.0-20260930.json)。

这些是本地与 Linux 真机证明；没有执行 npm 发布、生产部署或真实 ChatGPT 验收，
产物状态保持 `prepared`。CI 工作流已包含 Linux/macOS Node 22，但本批没有远端运行
GitHub Actions。

## 0.6.1 质量门槛

更新异步文件 I/O、分段输出缓冲、有界 WebSocket 发送队列、跨重连操作预算和显式锁恢复。
`check_stability.mjs` 在真实 loopback WebSocket 上重复运行文件、搜索和进程操作，并检查
输出上限、网络重连和实际资源清理；`check_lifecycle.mjs --full` 使用真实时间验证
1/10/60/299/301 秒离线样本及永久授权拒绝。测试脚本不进入 TGZ。

任何 24 小时 soak、生产 WSS/OAuth 或真实 ChatGPT 验收未完成时，候选不标为
production ready。Server 1000 在线／100 并发结果必须与 Agent 本机执行性能分别记录，
并绑定最终源码和 artifact SHA。既有 0.6.0 prepared artifact 与历史证据保留。
