# semantic_dom_v1 实现与验证

`semantic_dom_v1` 是 `experimental-browser-bidi` 开关下的内部页面 profile，契约见[共同语义](browser-semantic-dom-v1-zhCN.md)。CDP、BiDi 和 Fake 使用同一个 `semantic::Page<P>`。公开 browser v1 的入口、输出、默认浏览器和页面算法仍由原有模块提供；公开 v2 的协商、RPC/MCP、人工截图、文件和资料格式属于 D4。

## 模块职责

| 模块 | 职责 |
| --- | --- |
| `src/browser/semantic/page.rs` | 观察与续页、引用、材料复核、输入前后检查、输入任务所有权及人工交接 |
| `src/browser/semantic/program.js` | 两个真实浏览器共用的固定 DOM 采集、命名、材料、几何与表单程序 |
| `src/browser/semantic/program.rs`、`types.rs` | 封闭操作集、严格解码、预算、文档与 realm 代次、规范化材料指纹 |
| `src/browser/semantic/policy.rs` | 复用站点与 DNS 规则，管理一次性请求决定及能力准入 |
| `src/browser/engine/cdp/semantic.rs` | CDP 原生身份、隔离 world、节点解析、坐标路由与原生输入 |
| `src/browser/engine/bidi/page.rs` | BiDi 原生身份、sandbox、节点与 frame owner 解析、原生 actions 及请求拦截 |

公共端口传递类型化的页面事实、几何信息和输入意图，不接受协议方法、原生对象 ID 或调用者脚本。测试夹具的任意表达式入口仅在 `cfg(test)` 下编译。

同一会话的所有页面包装共享引用表、控制代次、观察锁和输入锁。人工接管先同步关闭旧代次入口，再等待已提交输入及清理结算；不等待普通观察锁。人工许可绑定会话、控制代次和 300 秒期限，交回要求披露确认并重新观察。断线后即使 pending 归零，也不发放新的输入许可。

## 身份与资源

CDP 使用原生 frame/loader、隔离 world 及固定程序的恢复标记；BiDi 使用原生 context/document sharedId、realm 和导航提交标识。Firefox 可以在 BFCache 恢复时保留文档而重建 sandbox，因此不能仅依赖 sandbox 内的 `pageshow` 计数。BiDi 读任务在发布后续响应前记录导航提交，实时文档查询还检查查询期间是否发生提交。事件缺口使通道失效；事件记录不能替代原生文档查询。

普通 realm 更换只改变 realm 代次。同 URL 重载、BFCache 恢复或文档连续性无法证明时，旧文档引用失效。动作按已保留节点解析原生节点，原生对象仅在本次检查或输入中持有；不根据名称或选择器重建旧引用。

每会话最多 32 个 frame，每 frame 一个扫描，强节点表最多 1,000 项。续页清掉上一页强节点，新观察、过期和 release 释放旧扫描。固定脚本有一个 300 秒回收定时器；Rust 层也在下一次读写入口检查过期。节点遍历、关联名称索引、记录材料、解码后结构与完整响应均有独立上限。

关闭会话先撤销控制许可并清空协调器引用，再执行原生关闭。输入子命令仍由会话任务持有到结算；不能通过删除 pending、目录或所有权记录伪造清理完成。

## 导航与输入边界

CDP 在新目标恢复前安装 Fetch 拦截。目标初始化与请求决策分别使用有界任务集合，事件循环持续处理请求，避免 popup 初始化等待自身首请求。BiDi 在开放导航前安装全局拦截。决定 token 绑定连接、上下文、请求和 hop，最多 32 个并发决定、每会话 4,096 个去重条目；期限为 2 秒，超限或决定无法完成时关闭通道。

导航保护的验收对象是文档 HTTP 请求。TCP 预连接与文档请求分别计数，不能将首请求被拒绝写成没有发生连接。两个新 adapter 都尚不提供实际连接地址证据；需要严格私网连接检查的策略在创建浏览器进程和资料目录前返回 `browser_policy_unsupported`。原有 v1 的策略路径保持独立。

普通文本输入经过原生编辑或按键。select、日期、颜色、范围使用独立的固定表单操作，响应标明合成事件；Check 和文本输入检查操作后的实际状态。每个副作用子命令重新经过提交门，释放按键和指针采用受限清理权限。清理失败或已提交输入结果不明时保留 unknown 并暂停，不重放。

跨 frame 输入以原生 frame owner 和节点身份为依据，逐层检查可见性、裁剪和命中。不能证明的旋转、透视等几何组合明确拒绝。最后一次检查与网页收到输入之间仍有非原子窗口，输入确认不等于业务结果确认。共同测试在原生命中检查完成后、鼠标按下派发前替换同位置节点，记录真实输入命中的替代节点；这类输入确认不带业务成功声明，旧引用再次执行时必须拒绝。

## 测试入口

在仓库根目录运行常规测试；这些测试不启动浏览器：

```sh
cargo +1.93.0 test --profile ci --locked --workspace --all-targets --all-features
cargo +1.93.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
```

原生测试需要显式指定 `RDH_BROWSER_EXECUTABLE`、`RDH_FIREFOX_EXECUTABLE`、同一源码构建的 `RDH_BIDI_HELPER`，以及新的私有 `RDH_D3_EVIDENCE` 目录。Firefox 运行环境还须支持 D2b 的原生 namespace 与守护边界，不以关闭宿主隔离策略作为产品前提。

```sh
cargo +1.93.0 build --profile ci --locked --all-features --bin rdh
cargo +1.93.0 test --profile ci --locked --all-features --lib browser::semantic::tests:: \
  -- --ignored --skip resource_cycles --skip performance_budget --nocapture --test-threads=1
cargo +1.93.0 test --profile ci --locked --all-features --lib browser::semantic::tests::resources:: \
  -- --ignored --nocapture --test-threads=1
```

原生短测试调用与 Fake 相同的断言，覆盖命名、记录与分页、引用、跨源 frame、表单、原生输入、交接、受控 keyDown 故障、历史恢复与首请求策略。资源测试分别执行 100 个会话和 10,000 次观察替换。性能测试单独使用 release 构建，验证实际 CPU 配额、CPU 亲和性、4 GiB 内存上限和禁用交换，不能与普通测试的耗时混算。

CI 使用独立的 Ubuntu 22.04 x86_64 任务运行 `.github/scripts/test-semantic-browser.sh`，先验证宿主允许所需的非特权 namespace，再校验固定 Firefox 安装包摘要并运行两个浏览器的短测试。Ubuntu 24.04 托管镜像的默认策略会拒绝自定义 helper 所需的 namespace；测试不修改 sysctl 或 AppArmor 来绕过该限制。原有八目标构建矩阵和 24 个 Chrome/v1 场景继续执行，产物组装同时依赖原生语义任务。资源压力和受限性能测量显式运行。测试结果、失败记录、浏览器版本和原始样本保存在提交对应的外部证据中；构建通过不等于所有 OS/CPU 的浏览器运行验收通过。
