# 浏览器实现与维护

远程浏览器运行在设备上。AI 通过 MCP 读取页面、提交输入、核对网站结果。用户在同一个浏览器会话中输入敏感信息或完成本人验证。安装与本地策略见[使用手册](browser-zhCN.md)。新增能力的顺序和验收条件见[架构与路线](browser-design-zhCN.md)。

## 选择运行路径

| 路径 | 谁可以使用 | 页面能力 |
| --- | --- | --- |
| v1 / Chromium CDP | 在支持的平台安装 Chromium，启用本地 `browser.enabled`，打开时省略 `selection` | 既有观察、引用操作、截图、弹窗、人工交接、可选命名资料及文件传输 |
| v2 / Chromium CDP | Linux x86_64，构建启用 `experimental-browser-bidi`，本地启用 `browser.enabled` 和 `browser.capability_v2`，显式选择 `cdp` | `semantic_dom_v1` 共同页面模型及会话声明的扩展 |
| v2 / Firefox BiDi | 同一构建和策略，配置 `browser.firefox_executable` 的绝对路径，具备所需 Linux namespace 和 pidfd 能力，显式选择 `bidi` | 同一页面模型，后端差异由能力声明和覆盖范围表示 |

浏览器功能默认关闭。Agent 使用设备上已安装的浏览器，Chromium 的本地路径由 `browser.executable` 指定或按平台规则查找。`device_info.capabilities.browser.version` 保持 `1`，`v2.backends[]` 中的 `stage: "candidate"` 表示设备允许尝试。`browser_open` 完成启动、文档身份和截图检查后才返回 `session_ready`。客户端按本次打开结果的 `capabilities` 选择功能，也可从 `browser_sessions` 取回 v2 会话能力。两个 v2 后端的 `observed_connection_address` 和 `network_isolation` 均为 `false`，要求严格连接地址检查的策略会在创建浏览器前拒绝。[选择与设备声明](../src/browser/selection.rs) · [会话能力](../src/browser/runtime/v2.rs)

下面是显式选择 Chromium 的 `browser_open` 参数。将 `device_id` 替换为账户中已绑定的设备 ID。`selection` 只接受所示三个字段，`backend` 也可取 `bidi`。后端在会话内固定，打开失败会报告原因。

```json
{
  "device_id": "example-device",
  "selection": {
    "version": 2,
    "observation_profile": "semantic_dom_v1",
    "backend": "cdp"
  }
}
```

## 从公开工具定位到源码

| 位置 | 维护职责 |
| --- | --- |
| Server `contracts/mcp/v1/tools-list.json`、`apps/core/src/mcp.rs`、`mcp_tools.rs`、`runtime.rs` | Gate/Core 使用公开工具目录。Core 绑定已认证主体、设备连接与 Agent 实例，处理准入、计量、操作回执及返回前的授权检查 |
| Server `apps/core/src/browser/{guidance,evidence,presentation}.rs` | 分别生成能力指引、核对诊断结果、选择完整或增量的 MCP 响应。Core 不持有原生页面节点 |
| [Agent RPC](../src/rpc.rs)、[会话运行时](../src/browser/runtime.rs)、[v2 路由](../src/browser/runtime/v2.rs)、[控制权](../src/browser/control.rs) | Agent 持有会话、活动 tab、控制代次和原生资源，分别在提交输入和交付结果时检查控制权 |
| [v1 页面](../src/browser/page.rs)、[v2 共同算法](../src/browser/semantic/page.rs)、[隔离环境中的固定页面程序](../src/browser/semantic/program.js) | 采集 DOM、计算名称与记录、签发引用、复核操作目标。v2 的记录和输入语义在此统一 |
| [CDP](../src/browser/engine/cdp)、[BiDi](../src/browser/engine/bidi)、[进程监督](../src/browser/process) | 处理协议事件、文档和脚本执行环境（realm）的身份、原生输入、待决命令、资源释放与所属进程退出 |
| [人工控制](../src/browser/runtime/human.rs)、[文件批准](../src/browser/runtime/file_approval.rs)、[私有文件](../src/browser/files)、[资料](../src/browser/profiles.rs) | Business 的 `Browser.vue`、`BrowserFileApproval.vue` 和文件页面通过经认证的人工接口访问这些资源 |

## 观察、操作与结果验证

1. 用 `browser_sessions`、`browser_tabs` 确认会话、tab 和当前 `control_epoch`。用 `browser_observe` 读取目标记录。`filter`、`find`、`within` 可缩小范围。`coverage` 说明哪些内容已读、页面是否在读取时变化，以及哪些内容被省略。
2. 从最新观察取元素 `ref`，通过 `browser_act` 导航、操作控件、滚动、处理标签页或弹窗。v2 的 `fill` 走原生文本编辑，日期、时间、颜色和范围控件走 `select` 的固定设值路径。`scroll` 当前滚动主文档。
3. 用 `browser_wait` 等待明确的 URL 或文本，再观察目标记录的业务状态。`input_acknowledged` 只确认原生输入，`business_result_verified` 保持 `false`。一次提交即使收到了成功回执，仍需从网站读到与目标记录对应的结果。

v2 逐 frame 遍历 DOM 与 open shadow tree。名称带有来源和质量，记录材料有独立预算。closed shadow、canvas 内部像素和浏览器自身界面超出语义观察范围。相同 URL 重新加载、文档或 realm 替换、节点或所在记录改变，都可能使旧引用失效。续页替换上一页的动作绑定。出现 `browser_stale_reference` 时重新观察当前记录，按原 ref 派发的动作始终重新检查身份、敏感性、几何和命中条件。[页面算法](../src/browser/semantic/page.rs) · [共同语义语料](../tests/fixtures/semantic-dom-v1/oracle.json)

| 标识 | 可用于 | 失效与拒绝 |
| --- | --- | --- |
| `ref`、`next_cursor` | 操作当前元素或续读原扫描 | 绑定会话、tab、文档、控制代次和读取序列。旧标识不能按同名或同位置重新匹配 |
| `node_id` | 读取 DOM 或局部布局诊断 | 与动作 ref 独立。会话注册表保留 128 项、32 KiB，创建后 60 秒过期。换代、节点替换或被回收后需重新查找 |
| `revision` | 受控接收端应用观察增量 | 不赋予输入资格。重新连接、换代或基线失效后从完整观察重建，每次替换全部 ref 和 frame 绑定 |
| `file_grant_id` | 给一个当前的文件输入控件选择经用户批准的文件 | 绑定原主体、会话、tab、站点与一次选择。结果不明也不会恢复次数 |

### 可用的诊断与增量展示

`browser_guidance` 提供 `open`、`observe`、`recover`、`handoff`、`files` 五个主题。Core 从已认证连接的会话元数据选择静态模板。模板的 `content_trust` 为 `server_template`，网页观察和诊断中的网页数据为 `untrusted_page`。指引里的示例仍需实际调用时的授权、参数和状态。

- **操作前置条件。** v2 的 `actionability` 扩展使用 `browser_inspect(ref, action_kind)` 检查一个动作。结果为 `ready_now`、`needs_scroll`、`blocked` 或 `indeterminate`。检查不触发滚动、聚焦、输入或文件授权消费，也不延长 ref。执行前仍要复核。[动作判定](../src/browser/semantic/actionability.rs)
- **DOM 与布局。** v2 两个后端的 `dom_evidence` 扩展使用 `browser_evidence`。`find_nodes` 以 tag、role 或文本查找诊断节点，`read_node` 读取受限的节点事实，`read_layout` 读取 frame 视口内的布局、计算样式子集和局部命中情况。父 frame 遮挡、字段缺失和预算截断通过 `coverage` 表达。用 `node_id` 查读不会替换当前观察或增量基线。[证据采集](../src/browser/semantic/evidence.rs) · [注册表](../src/browser/runtime/v2/evidence.rs)
- **观察增量。** `observation_delta` 是展示扩展。受控客户端在每次调用的外层 `params._meta` 中声明 `com.easelian/browser-observation-delta` 的版本 1 和 reducer `rdh-observation-v1`，并显式传 `presentation`。Agent 执行完整观察，Core 按真实 MCP 编码在 `full`、`delta` 和 `unchanged` 中选择。只有候选至多为同次 full 的 70% 才发送较小格式。Server `clients/browser-observation-delta.mjs` 重建并校验当前绑定。普通客户端继续使用完整观察。[Agent 展示缓存](../src/browser/runtime/v2/presentation.rs)

匹配 CSS 规则、控制台异常、网络请求摘要与性能采样尚无公开工具。`read_layout` 的计算样式和局部命中可帮助解释可见性。精确到样式表规则、脚本异常或失败请求的证据需要相应扩展。

#### 单节点原生无障碍证据

原生 AX 使用独立的 `extensions.native_ax: {version: 1, available, reason}`。当前只在显式 v2/CDP 会话提供；BiDi 明确返回 `available: false`、`reason: browser_feature_unsupported`，不以 DOM 推导替代 Firefox 原生树。构建、平台和本地策略拒绝仍优先保留其原因。实际会话原有的布尔字段 `capabilities.native_ax` 同步表示本次 CDP 支持状态，BiDi 仍为 false；工具准入使用独立版本化扩展，不把布尔值当作完整契约。候选与实际会话分别声明；旧 Agent、v1、缺失或未知扩展及不支持的后端在原生 AX 读取前拒绝。

`browser_evidence` 增加封闭查询 `query: {kind: read_accessibility, node_id}`，其余必填字段、主体/tab/epoch 绑定、2 秒绝对期限和最终披露检查沿用 DOM 证据。node_id 必须来自同一会话的独立 den_ 注册表；不接受 AX ID、DOM 原生 ID、对象 ID、选择器、脚本或任意协议方法。读取只针对这个节点，不自动选择父节点或改用相似名称。节点、realm、文档或祖先身份失效时仍返回 stale。

CDP 使用固定的隔离 realm 函数，从 WeakRef 表取得当前节点，再调用 `Accessibility.getPartialAXTree`，固定 `fetchRelatives: false`。不调用全树、祖先、子树查询，不为这次读取启用 AX 事件订阅。结果最多包含一个原生 AX 节点，AX 遍历深度固定为 0；零节点表示本节点没有原生 AX 记录。多于一个节点或不符合协议结构时拒绝，不把截断后的树声称为完整树。深层或超量 DOM 不改变这一读取范围。原生消息继续受现有 2 MiB 硬上限约束，不能把输出截断当作无界原生接收的替代。

结果封装沿用证据接口，固定 `kind: read_accessibility`、`source: native_ax`，载荷字段为 `accessibility`，不使用 nodes/node/layout。载荷是以下封闭字段：

| 字段 | 类型与含义 |
| --- | --- |
| node_id、frame_index、main_frame | 本次实际读取的诊断身份和 frame；不签发动作 ref |
| status | present 或 absent；仅说明这个节点是否有原生 AX 记录 |
| role | 原生角色字符串，最多 64 个 Unicode 标量；不可用时为 null |
| name、name_status | name 为最多 256 个 Unicode 标量或 null；name_status 为 available、omitted 或 absent |
| ignored | present 时为原生布尔值，absent 时为 null；不能由 DOM 可见性推断 |
| states | 最多 18 个封闭状态字段，未返回字段不等于 false |
| ignored_reasons | 最多 16 个已知原生原因名；不附带关联节点或原始属性值 |

absent 时 role/name/ignored 均为 null、name_status 为 absent，states 和 ignored_reasons 为空。名称不得带入新的表单值或编辑文本：先从同一诊断节点取得有界 DOM 名称；只有节点不敏感、原生名称在 256 标量内的前缀与该 DOM 名称完全一致时才公开该原生前缀。原生名称缺失时为 absent，不能满足披露条件时为 omitted 并记录 native_ax_name；不以 DOM 名称冒充原生名称。原生 value、description、命名来源展开、关系、AX 父子 ID 和 BackendNodeId 始终不公开。

states 只允许 atomic、busy、disabled、focusable、focused、hidden、modal、multiline、multiselectable、readonly、required、selected、expanded 的布尔值；checked、pressed 为布尔值或 mixed；invalid 为 false/true/grammar/spelling 字符串；editable 为 plaintext/richtext；level 为 0～1024 的整数。每次最多检查 32 个原生 property 和 16 个 ignoredReason，超量记录 ax_property_limit/ax_reason_limit；未知字段或类型不进入结果。ignored_reasons 只允许 activeFullscreenElement、activeModalDialog、activeAriaModalDialog、ariaHiddenElement、ariaHiddenSubtree、emptyAlt、emptyText、inertElement、inertSubtree、labelContainer、labelFor、notRendered、notVisible、presentationalRole、probablyPresentational、inactiveCarouselTabContent、uninteresting。角色或名称前缀截断分别记录 ax_role_limit/ax_name_limit。

coverage 保留同次 DOM 名称读取的计数，并增加 native_ax_nodes（0～1）、native_ax_properties（0～32）和 native_ax_ignored_reasons（0～16）；这些字段只用于 read_accessibility。omitted 明确包含 native_ax_tree、native_ax_value、native_ax_description、native_ax_relations 和 native_ax_other_properties；未知状态或原因分别增加 unrecognized_ax_property/unrecognized_ax_reason。Agent 正文仍最多 16 KiB，完整 MCP JSON-RPC 仍最多 64 KiB；两种输出来自同一份结果。不保存 AX 正文、AX ID 或对象缓存，不更改 canonical 观察、原动作引用和增量基线。

取得节点包装对象前由既有 CDP 租约账本预留，返回后以作用域持有。正常返回、解码失败、取消或超时均必须进入同一有界释放路径；已接收对象不能因上层 future 被丢弃而脱离账本。释放成功或原生 session 退出才结算对象；迟到创建仍按现有机制回收。清理通路容量、3 秒收尾期限和失败后的连接停止规则保持不变，人工控制 fence 不等待本次只读工作。

实现入口为 [原生 AX 类型](../src/browser/semantic/accessibility.rs)、[CDP 投影与读取](../src/browser/engine/cdp/accessibility.rs)和 [CDP 作用域回复](../src/browser/engine/cdp/wire.rs)。Core 依据 query 选择 native_ax 准入并验证 source、后端、节点身份、状态/名称与计数的一致性；DOM 查询拒绝混入原生 AX 载荷或计数。字段依据为 CDP [Accessibility.getPartialAXTree](https://chromedevtools.github.io/devtools-protocol/tot/Accessibility/#method-getPartialAXTree)，实际浏览器支持须由固定产物的原生验收证明。

## 输入不明、人工接管与文件

需要在响应丢失后找回同一次写入时，客户端可在动作前用 `prepare_operation` 为目标工具预留并保存操作 ID。若省略，支持回执的实例由 Core 自动准备。

`Submission::enqueue` 在同一把控制锁内校验代次、将命令同步入队并标记可能的副作用。`not_sent` 表示该操作尚未提交副作用，可以在修正原因并重新观察后决定下一步。`unknown` 表示可能已经生效。先查询原 `operation_id` 的 `get_operation_status`，再取会话和标签状态。尚有相关输入或清理待决时等待结算，允许恢复后用当前代次 `resume` 再观察。仍无法确认的写入向调用者报告未验证，避免第二次提交。[控制门](../src/browser/control.rs)

遇到密码、一次性验证码或需要本人完成的验证时，AI 暂停任务并提供 `human_url`。用户登录并明确接管后，AI 的页面读取、截图和输入受阻。用户核对可分享的页面和文件元数据后明确交回并同意披露。原对话继续时，AI 重新查询会话、tab 和新代次，从新观察取得 ref。普通网络或页面故障走诊断与回执恢复。人工输入受 view、租约、代次、序号和截图帧约束。系统级生物识别、硬件安全密钥界面不在远程网页视口内。[人工输入与交还](../src/browser/runtime/human.rs)

`browser.persistent_profiles`、`browser.uploads`、`browser.downloads` 分别启用，默认均关闭。独立文件批准还需设备的 `capabilities.browser.extensions.file_authorization` 版本 1 可用。用户从 `files_url` 或会话列表进入“批准文件使用”，确认 tab、目标 origin 和本地文件。AI 保留控制，批准后从 `browser_files` 取得授权，用最新主文档文件输入 ref 执行 `attach_file`。Agent 在原生派发前持久化消费。旧版租约上传仍可用于已接管的会话。文件字节只走授权的人工接口。[文件批准](../src/browser/runtime/file_approval.rs) · [上传存储与消费](../src/browser/files/upload.rs)

下载接收原始 HTTP 200 附件响应，要求唯一 `Content-Length` 且无压缩编码。Firefox 在私有 namespace 内处理原下载，取消进行中的下载影响整个会话。Chromium 的取消范围为单文件。`cancellation_scope` 告知用户影响范围，收到取消请求后继续核对终态。命名资料绑定主体与后端，v1、v2 Chromium、v2 Firefox 各自独占。[文件与下载](../src/browser/files) · [资料命名空间](../src/browser/profiles.rs)

## 资源边界与回退

| 资源 | 当前上限 | 源码 |
| --- | --- | --- |
| 会话、tab、frame | 每设备 v1/v2 合计 2 个会话。每会话 8 个 tab。v2 32 个 frame | [runtime](../src/browser/runtime.rs)、[v2](../src/browser/runtime/v2.rs)、[types](../src/browser/semantic/types.rs) |
| v1/CDP 拓扑与连接证据 | 页面最多 32 个 frame。会话最多 40 个附着子 target。连接地址证据最多 1,024 项并淘汰最旧记录 | [CDP page](../src/browser/engine/cdp/page.rs)、[session](../src/browser/engine/cdp/session.rs)、[runtime](../src/browser/runtime.rs) |
| 观察 | 默认 200 节点/20,000 字符，最大 1,000/64,000。每段遍历 5,000 节点，待遍历队列 50,000 项 | [page](../src/browser/semantic/page.rs)、[program](../src/browser/semantic/program.js) |
| 扫描与引用 | 每 frame 一个扫描、每扫描最多 1,000 强节点。空闲期 300 秒 | [page](../src/browser/semantic/page.rs)、[program](../src/browser/semantic/program.js) |
| 命名与记录 | 名称 512 Unicode 标量、32 关联、16 层、256 命名节点。记录 256 节点/4,096 标量 | [program](../src/browser/semantic/program.js)、[types](../src/browser/semantic/types.rs) |
| 可操作性诊断 | 一个有效 ref、最多 8 项原因、Agent 正文最多 4 KiB、总期限最多 1 秒。稳定性采样间隔 50 ms | [actionability](../src/browser/semantic/actionability.rs)、[page](../src/browser/semantic/page.rs)、[v2](../src/browser/runtime/v2.rs) |
| 能力指引 | 每次一个封闭主题，最多 3 个 variant、每个最多 3 个示例。含 `support_id`/`human_url` 的 Core 正文最多 16 KiB | Server `apps/core/src/browser/guidance.rs`、`mcp_tools.rs` |
| 独立文件批准 | 创建后 600 秒、不续期、一次选择。64 KiB 数据块。接口期限 20 秒，状态正文 4 KiB。与原文件共用配额 | [file approval](../src/browser/runtime/file_approval.rs)、[upload](../src/browser/files/upload.rs) |
| 增量观察基线 | 每份编码投影 256 KiB。每会话 2 份，Agent 合计 1 MiB。节点索引 4,000 项、frame 索引 128 项。创建后 60 秒、不续期 | [presentation](../src/browser/runtime/v2/presentation.rs) |
| 增量观察构造与选择 | 单会话新增暂存 512 KiB。O(N) 前后缀比较、最多一段 splice。含新绑定与 MCP 双输出的候选不超过同次 full 的 70% 才采用 | [presentation](../src/browser/runtime/v2/presentation.rs)、Server `apps/core/src/browser/presentation.rs` |
| DOM 与布局证据 | 访问 5,000 元素/10,000 文本/5,000 其他节点、深度 64。最多 64 结果。正文 16 KiB、完整 MCP 64 KiB。总期限 2 秒 | [semantic evidence](../src/browser/semantic/evidence.rs)、Server `apps/core/src/browser/evidence.rs` |
| 单节点原生 AX | 每次 0～1 节点、深度 0。最多检查 32 属性/16 原因，状态最多 18 项。原生消息 2 MiB、正文 16 KiB/MCP 64 KiB。一个作用域节点包装对象，释放确认前保留占用 | [AX adapter](../src/browser/engine/cdp/accessibility.rs)、[作用域原生回复](../src/browser/engine/cdp/wire.rs) |
| 诊断节点身份 | 每会话 128 项/32 KiB、两会话 64 KiB。创建后 60 秒不续期。每 realm 独立 128 个 WeakRef | [evidence registry](../src/browser/runtime/v2/evidence.rs)、[DOM program](../src/browser/semantic/evidence.js) |
| 人工控制 | 公开视图租约 120 秒。内部共同语义输入许可 300 秒，并受外层视图、epoch 约束 | [v2 human](../src/browser/runtime/v2/human.rs)、[page](../src/browser/semantic/page.rs) |
| 原生完整消息 | 入站 2 MiB。出站完整 JSON 256 KiB。BiDi 单帧与完整消息分别受限 | [CDP wire](../src/browser/engine/cdp/wire.rs)、[BiDi wire](../src/browser/engine/bidi/wire.rs) |
| 待决命令与发送队列 | CDP 普通 32 槽，另有 4 个仅供对象/对象组/流释放的槽，发送队列 36。BiDi 普通 32 | [CDP wire](../src/browser/engine/cdp/wire.rs)、[BiDi wire](../src/browser/engine/bidi/wire.rs) |
| CDP 原生 session | 每连接 128 个 attachment。单 ID 256 字节。ID 与父 ID 合计 32 KiB。退出后未结算工作最多保留 3 秒局部宽限，再关闭连接准入并恢复 | [CDP sessions](../src/browser/engine/cdp/wire/sessions.rs) |
| 协议租约 | CDP 对象 256、流 16，分类不可借用。包括 pending 预留。BiDi root handle 0 | [CDP leases](../src/browser/engine/cdp/wire/leases.rs)、[BiDi wire](../src/browser/engine/bidi/wire.rs) |
| 事件 | CDP 256 条/合计 8 MiB 已编码字节/最多两个关键消费者，单消息受 2 MiB 限制。BiDi 256 条/合计 1 MiB/单事件 64 KiB | [CDP events](../src/browser/engine/cdp/wire/events.rs)、[BiDi wire](../src/browser/engine/bidi/wire.rs) |
| 导航决定 | 32 个并发、4,096 个去重项、2 秒期限 | [policy](../src/browser/semantic/policy.rs) |
| BiDi 启动上下文与写入 | 128 活动项、128 销毁标记、深度 32。写入 3 秒。启动日志 64 KiB | [BiDi](../src/browser/engine/bidi)、[Firefox process](../src/browser/process/firefox.rs) |
| 截图 | viewport JPEG，宽高各不超过 1920 CSS px，base64 最多 512 KiB。完整响应另外受限 | [image](../src/browser/image.rs)、[v2 native](../src/browser/runtime/v2/native.rs) |
| 命名资料与文件 | 每命名空间 16 个资料。文件/暂存合计 16 项/128 MiB，单文件 32 MiB。读取块 64 KiB。完成文件保留 24 小时 | [profiles](../src/browser/profiles.rs)、[files](../src/browser/files.rs) |

回退 Agent 前，用支持当前格式的版本关闭会话并确认所属浏览器进程、下载与清理都已结算。按完整 `selection` 清点 v2 资料与下载，保留仍需使用的数据。对明确退役的数据使用相应删除接口，再停止 Agent，只用 `rmdir` 从最深层移除已确认空的 v2 目录。未使用的版本 2 上传批准由当前版本撤销或完成重启清理。旧版遇到不认识的目录或元数据会保留数据并返回 `browser_cleanup_pending`。用目标版本支持的本地配置启动并验证原有 v1 资料。[资料](../src/browser/profiles.rs) · [文件格式](../src/browser/files/upload.rs)

回退 Server 时先排空受影响 Core，再用目标版本的 `rdh-migrate activate-runtime-transactions ENV CORE_ROLE` 切换配套 Runtime 函数契约。`ENV` 是保存维护数据库连接的环境变量名。目标 Core 的 `check-storage CONFIG.json` 成功后再恢复服务。Gate、Core 工具目录与数据库函数按各自契约核对。
