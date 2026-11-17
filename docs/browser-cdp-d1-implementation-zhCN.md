# CDP 公共边界迁移与 D1 验收方法

D1 保持浏览器 v1 的接口、页面程序和业务断言。本文说明代码边界和可重复执行的验收方法；实际通过状态以对应提交的 PR 和原始验收记录为准。BiDi 及 `semantic_dom_v1` 不在本次实现范围内。

## 模块职责

| 模块 | 负责内容 |
| --- | --- |
| `engine.rs` | 页面读取、动作、会话和文件流的语义端口；不透明身份、扫描租约及规范化事件 |
| `engine/cdp/` | 原生命令编码、错误解码、隔离世界、节点解析、renderer/frame 拓扑、事件订阅、私有通道及启动参数 |
| `page.rs` | 分页、字符及节点预算、查询范围、引用资格、记录签名、动作预检、目标几何校验及操作计划 |
| `runtime.rs`、`runtime/human/` | 会话及人工控制、站点策略、操作回执和结果披露 |
| `files.rs`、`files/upload.rs` | 文件授权、配额、消费、暂存及交付策略 |
| `process.rs`、`process/` | 原生进程所有权、平台监督和资料目录寿命 |
| `engine/programs.rs`、`observe.js`、`marks.js` | 原有固定 DOM 程序；不开放任意 JavaScript 入口 |

公共代码不能构造原生命令、协议 JSON 或原生节点句柄，也没有通用 `call`、`evaluate` 或 `cleanup` 逃生接口。释放扫描、关闭流、拒绝请求、关闭超额 tab 等维护操作各自携带具体资源。`FrameId::v1` 和 `TabId::v1` 只在原有 v1 字段的展示和匹配处使用；不可用展示值构造内部句柄。

节点、文档和扫描句柄绑定连接所有权。节点不放入额外的全局映射表；扫描的显式关闭会立即撤销本地租约，即使原生释放失败也不能重新调用。待决表仍独占原生命令及并发槽位；公共提交门只维护授权和单调副作用进展。迟到结果不会自动重放动作。

页面 Fake 通过真正的公共观察和动作入口验证分页、引用、记录失效及预检；会话 Fake 将规范化事件送入真正的策略处理入口。它们不需要 CDP 命令或原生 JSON。原有依赖原生端点的测试移入适配器目录，保留原断言。`tests/browser_session.rs` 保持不变。

## CDP/v1 预算

| 资源 | 上限及判定 |
| --- | --- |
| 会话 / tab | 2 / 8；额外请求在提交前拒绝 |
| 页面 frame / 会话 target | 32 / 40；超过可读 frame 数拒绝观察，超额 target 关闭 |
| 每次观察 | `max_nodes` ≤ 1,000、`max_chars` ≤ 64,000；分页不保留上一页可用引用 |
| 传输 | 32 个普通待决槽、4 个对象／对象组／流释放槽、36 个发送队列项、256 个事件环项；关键事件丢失关闭引擎 |
| 原生消息 | 出站完整 JSON ≤ 256 KiB、入站帧 ≤ 2 MiB；坏帧或超长帧关闭连接并结算 pending |
| 截图 | 视口每边 ≤ 1,920 CSS 像素，JPEG base64 ≤ 512 KiB |
| 连接地址证据 | 最多 1,024 项；容量到达后淘汰最旧记录 |

CDP 每连接最多持有 256 个原生对象租约和 16 个流租约；待决创建命令预留的容量计入上限。BiDi 的 root handle 上限及其节点身份标识另见[原生句柄租约预算](browser-backend-acceptance-zhCN.md)，不与页面内强节点引用混计。

## 验收入口

常规检查使用仓库锁定依赖以及明确记录的 Rust 工具链：

```sh
cargo +stable fmt --all -- --check
cargo +stable clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +stable test --profile ci --locked --workspace --all-targets --all-features --no-fail-fast
RDH_BROWSER_EXECUTABLE=/path/to/chrome cargo +stable test --profile ci --locked --all-features --test browser_session -- --ignored --test-threads=1
```

Linux 的长时间验收位于 `browser::runtime::acceptance_tests`，默认忽略，必须显式运行。设置 `RDH_BROWSER_EXECUTABLE`、`RDH_D1_EVIDENCE` 和不重复的 `RDH_D1_LABEL`，使用 release profile：

- `release_performance_samples`：观察、点击、截图分别预热 20 次、测量 100 次；所有失败保留，点击准备时间单列，记录实际命令数。
- `normal_sessions_and_observation_replacement_release_owned_resources`：10 个预热会话、100 个正式会话和 10,000 次观察／引用替换；每次检查 pending、原生对象和流，关闭后检查进程、目录及锁。
- `frozen_session_tab_reference_and_image_budgets`：会话、tab、观察预算的拒绝，以及低／高熵画面的截图边界。

使用 `systemd-run --user --scope -p CPUQuota=200% -p MemoryMax=4G` 与固定的 `taskset` CPU 集。系统没有向用户作用域委派 cpuset 控制器时，以 `/proc/self/status` 中的实际 affinity 作为证据，不将缺失文件当作无限制运行成功。性能测试不得与其他编译或压力测试并发。

基线为 `06e43b18763292a89fe79f2df1992e4053225ff2`。仅向其测试构建注入同一套计数器和验收 fixture；不修改基线生产行为。按 baseline/candidate 顺序交替运行至少三组，保存浏览器、字体、工具链、构建摘要、配额和全部 JSONL。`scripts/browser-d1-report.py EVIDENCE_DIR` 计算每组 p50/p95、命令数、基线自身波动和体积门槛，不挑选较快样本。

故障验收另外执行 10,000 个有界、可重放的内存调度序列，1,000 次队列及事件压力下的控制 fence，以及真实浏览器关键场景连续 20 次、崩溃恢复 100 次。fence 指公共准入关闭的耗时，不是所有已提交浏览器动作完成的耗时。

## Linux 回收缺口

长时间会话循环发现旧监督逻辑可能在第一轮 `waitpid` 后又出现已退出的被收养子进程，并在它们仍为僵尸状态时返回关闭成功。独立回归先等待这个明确状态，再检查关闭结果；旧实现会留下 leader 以外的记录。修复在确认整个进程组停止后再收割一次，只允许保留尚未释放的 leader。无法完成收割时继续等待既有关闭预算，不提前删除资料或释放身份。

该修复独立于协议迁移，适用于 Linux。macOS、Windows 的运行时验收不能从 Linux 结果推导。八目标原生 CI 证明构建和自动化测试，不能代替其他平台的真实浏览器运行。原有 macOS 主修饰键缺口和最后一次命中校验后的网页非原子变化窗口仍单独保留，不在 D1 中声称消除。
