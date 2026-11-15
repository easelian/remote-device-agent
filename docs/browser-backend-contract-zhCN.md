# 原生浏览器后端规约与架构方案

状态：设计稿，待评审；本稿不代表 Firefox/BiDi 已实现或验收通过。

版本：0.1，2026-11-15。

代码基线：Agent `06e43b18763292a89fe79f2df1992e4053225ff2`；配套 Server `be90380f8acde29c07d7e93c83c292ccc5bd1949`。

配套文档：[验收指标与实施门槛](browser-backend-acceptance-zhCN.md)。

## 1. 决策与目标

采用“公共浏览器语义层 + 类型化引擎接口 + 原生协议适配”的分层。公共层定义观察、引用、动作、控制权和恢复的含义；CDP、BiDi 后端负责把有明确前置条件和结果语义的浏览器操作映射到各自协议。后端不能自行改变引用有效期、授权范围、重试规则或观察完整性的含义。

部署约束保持不变：Agent 使用 Rust，适配代码编入同一二进制，复用用户已安装的受支持浏览器。运行时不要求 Node、Puppeteer、ChromeDriver、GeckoDriver、扩展或独立代理，不自动下载浏览器。开发和 CI 可以使用测试工具；这些工具不得进入用户运行时依赖。

首批实现 Chromium/CDP 和 Firefox/BiDi。浏览器品牌、内核、协议和操作系统分别记录，不把“支持 CDP”视为对所有 Chromium 版本的兼容承诺。已安装浏览器不满足能力要求时，返回具体不可用原因。

本设计首先冻结规约和验收，再迁移现有 CDP 实现，最后接入 BiDi。性能 trace、堆分析、原始控制台和任意脚本执行不属于基础后端规约；后续通过有独立类型和预算的扩展提供。

## 2. 当前代码给出的约束

以下是基线的已核实状态，不是计划中的模块名称。

| 当前实现 | 对抽象的影响 |
| --- | --- |
| [control.rs](https://github.com/easelian/remote-device-agent/blob/06e43b18763292a89fe79f2df1992e4053225ff2/src/browser/control.rs) 已独立保存控制代次及暂停状态 | 完成了一个共享状态的提取，尚未形成后端规约 |
| [page.rs](https://github.com/easelian/remote-device-agent/blob/06e43b18763292a89fe79f2df1992e4053225ff2/src/browser/page.rs) 的 Frame 保存 CDP session、frame、loader，Element 保存 backend node ID | 不能直接将现有类型作为公共页面身份；需要隔离原生 ID 和生命期 |
| page.rs 结合固定 DOM 脚本与 Accessibility.getPartialAXTree 产生名称、角色和引用 | 公共观察语义不能无条件依赖 CDP 可访问性树；需要区分公共语义与原生增强 |
| [runtime.rs](https://github.com/easelian/remote-device-agent/blob/06e43b18763292a89fe79f2df1992e4053225ff2/src/browser/runtime.rs) 直接处理 target、Fetch、Network、弹窗等事件 | 事件归一化、导航策略和任务调度必须一起划定边界 |
| [cdp.rs](https://github.com/easelian/remote-device-agent/blob/06e43b18763292a89fe79f2df1992e4053225ff2/src/browser/cdp.rs) 以命令名称识别部分输入副作用，入队后保留迟到响应及并发槽位 | 公共层应使用类型化副作用分类；不得把超时当成取消或释放工作所有权 |
| [process.rs](https://github.com/easelian/remote-device-agent/blob/06e43b18763292a89fe79f2df1992e4053225ff2/src/browser/process.rs) 直接持有 Cdp，并协调私有目录和原生进程回收 | 启动、通道与进程所有权要解耦；各后端仍必须经过同一回收流程 |
| [files.rs](https://github.com/easelian/remote-device-agent/blob/06e43b18763292a89fe79f2df1992e4053225ff2/src/browser/files.rs)、human 模块直接调用 CDP | 文件策略和人工接管属于公共层；协议侧只提供受限输入、截图、文件流原语 |
| [browser_session.rs](https://github.com/easelian/remote-device-agent/blob/06e43b18763292a89fe79f2df1992e4053225ff2/tests/browser_session.rs) 有 24 个显式运行的真实浏览器场景 | 它们是 CDP 迁移的回归基线，不能因为抽象调整而删除断言或改成忽略 |

不采用三个容易形成长期耦合的方案：

- 仅将 `call(method, Value)` 包成 trait：协议名称、ID、错误和时序仍会进入公共层。
- 每个后端完整实现一套 MCP `observe/act/control`：授权、引用、覆盖率和恢复规则会出现两份实现。
- 用 BiDi 命令模拟整个 CDP API：会把 CDP 的对象和事件模型固定成所有后端的共同模型。

## 3. 分层及代码所有权

下图表示调用和实现关系；公共层只依赖语义接口，不选择协议命令。

```mermaid
flowchart TD
    A["MCP / Agent RPC 边界"] --> B["Browser service：主体、回执、能力选择"]
    B --> C["Session coordinator：控制权、任务、披露、关闭"]
    C --> D["Page semantics：观察、引用、动作计划"]
    D --> E["Engine：类型化浏览器原语"]
    E --> F["CDP adapter"]
    E --> G["BiDi adapter"]
    F --> H["私有管道 / Chromium"]
    G --> I["受管回环 WebSocket / Firefox"]
    C --> J["进程监管、资料目录、文件策略"]
    F --> K["公共提交门与事件入口"]
    G --> K
    K --> C
```

建议逐步形成以下源码职责，目录本身不是验收目标。

| 模块 | 独占职责 | 禁止承担的职责 |
| --- | --- | --- |
| `browser/contract/` | ID、请求/结果、事件、错误、能力及预算类型 | CDP/BiDi JSON、MCP wire schema、平台进程操作 |
| `browser/session/` | 控制权、在途工作、回执关联、结果披露、关闭顺序 | 原生节点定位、协议错误字符串匹配 |
| `browser/page/` | 观察分页、记录指纹、引用注册、动作前置检查、等待条件 | 依据 backend kind 分支选择行为 |
| `browser/page/programs/` | 随二进制交付的固定 DOM 采集、校验和表单算法 | 调用者提供的 JavaScript、任意方法名、动态下载代码 |
| `browser/backend/cdp/`、`bidi/` | 协议编码、原生 ID 映射、realm、原生输入、事件归一化 | 创建 AI 引用、修改权限、静默重试未知动作 |
| `browser/backend/transport/` | 有界读写、请求关联、关闭和协议解码错误 | 通过命令名称猜测业务副作用 |
| `browser/process/` | 进程树所有权、启动记录、停止和资料回收 | 将通道断开当成进程已停止 |
| `browser/files/` | 授权、一次性上传许可、私有暂存、配额、交付 | 向后端交付任意调用者路径 |

固定 DOM 算法由两种后端复用。后端仅负责在正确隔离环境执行这些固定程序、编码参数及解析原生对象；不得复制两份观察、命名和记录校验脚本。程序以封闭的 ProgramId 和类型化参数选择，构建时嵌入；不能把任意脚本文本作为 Engine 接口参数。

初期保留单一 crate，使用 Rust 可见性和模块依赖约束边界。暂不引入动态插件 ABI、全协议代码生成框架、通用工作流引擎或任意命令扩展口。

## 4. Engine 接口：统一操作语义

Engine 是内部接口，不是新网络协议，也不直接对应 MCP 工具。公共接口只接受约定类型；JSON 仅存在于 MCP 边界、协议编解码和经过校验的固定程序返回值中。

建议使用私有 sealed trait，通过对象安全的 `DriverFuture<'a, T>` 返回借用期明确的异步结果。该别名可由标准库的 `Pin<Box<dyn Future<Output = DriverResult<T>> + Send + 'a>>` 表达，不需要新增 async-trait 运行时依赖。先以边界正确性为准，性能验收发现实际成本后再考虑静态分派。

以下是接口形状说明，不是可直接编译的实现；省略了类型完整定义和扩展接口。

```rust
trait Engine: sealed::Sealed + Send + Sync {
    fn contexts(&self, cx: ReadContext) -> DriverFuture<'_, ContextTree>;
    fn open_scan(&self, cx: ReadContext, q: ScanRequest)
        -> DriverFuture<'_, ScanLease>;
    fn next_scan(&self, cx: ReadContext, scan: &ScanLease, b: ReadBudget)
        -> DriverFuture<'_, DomChunk>;
    fn inspect(&self, cx: ReadContext, node: &NodeLease)
        -> DriverFuture<'_, ElementFacts>;
    fn input(&self, step: AuthorizedInput)
        -> DriverFuture<'_, InputAck>;
    fn navigate(&self, step: AuthorizedNavigation)
        -> DriverFuture<'_, NavigationAck>;
    fn tabs(&self, step: AuthorizedTabChange)
        -> DriverFuture<'_, TabAck>;
    fn capture(&self, cx: ReadContext, q: CaptureRequest)
        -> DriverFuture<'_, ImageArtifact>;
    fn handle_prompt(&self, step: AuthorizedPrompt)
        -> DriverFuture<'_, PromptAck>;
    fn release(&self, leases: LeaseBatch)
        -> DriverFuture<'_, ReleaseReport>;
    fn close(&self, cx: CloseContext)
        -> DriverFuture<'_, ChannelCloseReport>;
}
```

操作必须遵守以下约定：

| 操作组 | 前置条件与成功含义 | 由公共层承担的部分 |
| --- | --- | --- |
| 上下文枚举、创建/选择/关闭 tab | 返回规范化上下文树；创建的新页面在策略钩子就绪前不执行用户导航 | 配额、会话归属、最后一个 tab 的行为；tab 变更使用独立的类型化控制请求 |
| open/next scan | 只在指定文档与隔离环境采集有界 DOM 数据；返回原生句柄的租约，不生成 AI ref | 筛选、find/within 范围、覆盖率、分页令牌和最终引用 |
| inspect | 返回连接状态、角色/命名依据、值、记录指纹材料、可编辑性、命中与几何信息 | 对照已观察记录、选择可执行动作、敏感输入检查 |
| input | 执行明确的输入意图；成功仅表示协议输入序列已确认 | click/check/select/fill 的行为规则、动作前后校验、回执和未知结果处置 |
| navigate | 返回导航 ID、当前阶段及目标状态；不能用命令响应代替文档已加载 | 站点决策、等待条件和恢复策略 |
| capture | 指定 viewport、缩放及裁剪语义，返回有界图像和文档/视口戳 | 人工通路授权、图像交付、控制代次及披露复核 |
| prompt | 明确匹配 prompt ID 和代次；不得自动确认未知弹窗 | 操作者授权、是否可在 pending 状态恢复 |
| release/close | 报告资源释放结果；channel close 不证明浏览器进程停止 | 工作所有权、目录隔离、进程回收和最终关闭状态 |

`InputIntent` 至少区分 Focus、Pointer、Key、ReplaceText、ScrollIntoView、SetFormValue；不得退化为“协议方法名 + 任意参数”。例如 ReplaceText 必须走该浏览器的编辑路径，支持的最终值和事件语义由公共规约定义，后端可以用不同数量的原生命令实现。普通文本填写不能悄悄退化为 DOM `value=`、剪贴板或 OS 桌面输入。

`AuthorizedInput` 只能由公共协调器构造，并携带当前主体许可、会话代次、文档预期和 EffectTicket。后端每个可能产生副作用的子命令仍须经过公共提交门；得到一次授权不意味着整个序列以后可以跳过控制权检查。清理权限单独限制为释放已按下按键、取消已持有拦截和释放本次租约，不能恢复普通输入权限。

## 5. 身份与资源生命周期

原生 ID 只在所属 adapter 内解释。公共类型不能包含名为 `cdp_session`、`loader_id`、`backend_node_id` 或 `bidi_shared_id` 的字段。

| 公共身份 | 范围与失效规则 |
| --- | --- |
| SessionId / SessionGeneration | 一次受管浏览器实例；崩溃、重建或不可恢复的通道丢失后换代 |
| ContextId | 规范化的 tab/frame 身份；frame 销毁后不得把同一值用于新 frame |
| DocumentEpoch | 某 context 的文档代次；新文档、BFCache 恢复及无法确定文档连续性时递增 |
| RealmEpoch | 固定程序执行环境的代次；realm 重建使其脚本句柄失效，不必一律视为新文档 |
| NodeLease / ScanLease | 绑定 session、context、document、realm 的有界租约；不能跨 backend 使用 |
| ObservationId / ElementRef | 公共层生成，绑定观察、控制代次、文档和记录指纹；协议层看不到 AI ref |
| NavigationId / PromptId | 关联一次导航或弹窗生命周期；不得复用先前对象的决定 |
| ViewportStamp | 文档、视口尺寸、缩放和截图版本；人工坐标输入必须匹配 |

每个 session 只有一个 IdentityRegistry，由公共基础模块实现；adapter 通过封闭更新接口提交已验证的 context/document/realm 变化，并在自己的私有表中维护原生 ID 映射。语义层只读取规范化身份。身份事件用于通知，不再重复换代，避免 adapter 和 coordinator 各维护一套相互滞后的计数。句柄同时绑定 Registry 命名空间，伪造整数或另一个 session 的句柄不能通过检查。

同文档 hash/history 变化通常不创建新文档，但应更新地址与导航状态；是否继续一段观察另由观察规则决定。不能简单地以 URL 是否相同判定文档是否相同。顶层文档更新使整棵旧 frame 树失效；子 frame 更新不必废弃不相关 frame 的有效引用。

事件只能帮助维护身份，不能代替动作前复核。临近命令的导航和 realm 变化必须通过原生查询或执行环境内的文档标记再次确认。ID 重用、迟到事件和未知事件顺序一律不能恢复旧引用。

Rust Drop 只能发起有界释放，不能伪装成完成异步回收。Session 持有资源登记表与回收任务；即使调用 future 被取消，原生对象、在途命令及进程所有权仍被追踪。租约达到预算时停止接纳新资源，返回明确的资源不足结果。

## 6. 控制权、提交点与超时

使用三个正交状态，避免把“暂停”“输入已结束”“可以披露”塞进同一个布尔量：

- 生命周期：Starting、Ready、Closing、Closed、Lost、CleanupPending。
- 控制方：AI、Human、Paused，并保存递增的 ControlEpoch。
- 在途状态：EffectTicket 集合、按键/指针清理租约及阻塞导航；集合是否清空决定能否交接。

### 6.1 提交点

EffectTicket 在公共提交门内登记，与控制权换代使用同一串行化机制。**提交点定义为经授权的副作用子命令进入可发送队列，不定义为浏览器已经执行。** 未进入提交点的工作可以明确取消；进入后，即使尚未取得原生响应，也按可能生效处理。

输入、导航、tab 变更、focus、滚动、表单设置及弹窗决定均属于副作用。纯读取不产生输入副作用，但它持有的资源与披露授权仍有独立生命周期。每个原语的分类是静态契约，不能依赖协议方法前缀猜测。

暂停首先换代并阻止旧代次的新提交；已提交的旧工作进入结算阶段。人工输入许可和 AI 恢复许可只在相关输入及必要清理已结算后发放。暂停成功不等于已提交输入不再发生。

```mermaid
sequenceDiagram
    participant A as AI 动作
    participant G as 公共提交门
    participant P as 协议后端
    participant H as 人工接管
    A->>G: 旧代次子命令
    G->>P: 登记 EffectTicket 后入队
    H->>G: 换代并关闭旧代次准入
    A->>G: 下一子命令
    G-->>A: ControlChanged，未提交
    P-->>G: 已提交命令响应及输入清理结果
    G-->>H: 仅在输入结算后授予输入许可
```

高优先级控制路径不能排在普通观察或网络等待之后；不得持有 Session 操作锁等待控制权交接。读取循环、导航策略决定和响应处理各自可推进，避免“等待加载，而加载等待被同一任务阻塞的拦截决定”。

### 6.2 期限与结果

每个上层操作携带绝对 Deadline，所有普通子命令共享剩余预算，不逐层重置超时。另有明确且有上限的清理预算，不能用它继续用户动作。

| 内部结果依据 | 对外处置 |
| --- | --- |
| 尚无副作用子命令提交，因参数、权限、能力或资源限制拒绝 | 可报告 not_sent；不冒充已接受的操作 |
| 一部分序列已提交，后续校验失败、超时、断连或清理不确定 | 报告 unknown，暂停会话并保留回执及 pending 工作；禁止自动重放 |
| 协议确认全部输入序列 | input_acknowledged；仍不代表网站业务成功 |
| wait 没有观察到目标条件 | 条件超时；不重复先前动作 |
| 纯读取失败且没有输入 | 报告读取失败及其实际状态；不虚构已经发生页面输入，也不释放未结束的工作 |

新连接不能接管旧连接中的未知动作并重新执行。回执去重由公共层负责，后端只报告原始执行进展。RPC 准入拒绝的有限重试与页面动作重试是两件事，测试夹具的重试规则不得自动进入产品执行逻辑。

## 7. 观察和动作的一致性

### 7.1 公共观察 profile

定义 `semantic_dom_v1`：两种后端执行同一组固定 DOM 算法，采用同一条命名规则、记录边界、截断预算和完整性定义。原生可访问性信息作为独立增强字段，不覆盖公共字段。

首批命名语料覆盖原生控件文本、关联 label、aria-label、aria-labelledby、alt、显式 role 和可观察状态；每条规则均需给出固定页面及期望结果。不能把这一有界实现称为完整浏览器可访问性树或完整 AccName 实现。无法准确命名时返回来源与限制；不能伪造 native 质量。

canonical 控件名先依据冻结的 HTML/ARIA 支持表判断命名是否适用，再采用以下优先级：有效的 aria-labelledby 引用文本、非空 aria-label、适用 HTML 控件的关联 label、适用元素的 alt、允许由内容命名的控件文本、最后才使用 title。禁止命名的 role 不因存在 aria-label 就取得可访问名称。仅在当前文档和适用树范围内解析关联；不跨 frame 猜测标签。多引用保持声明顺序；循环引用、缺失关联、重复 ID 歧义和预算耗尽均记录质量限制。名称规范化仅折叠规定的空白，不改变大小写和 Unicode 内容。每次命名最多处理 32 个关联引用、递归深度 16、访问 256 个节点；截断必须进入 coverage/name_quality，不能把部分名称冒充完整结果。具体 fixture 包括多 label、隐藏的引用文本、空标签、循环、shadow 边界和长名称。这是有界产品语义，标准算法参考 [AccName 1.2 工作草案](https://www.w3.org/TR/2026/WD-accname-1.2-20261104/)，不据此宣称完整标准兼容。

当前 CDP 观察利用原生 AX 数据。迁移期通过边界兼容投影保留原有 v1 输出，公共模型同时保留 canonical 字段及可选 native 信息。兼容投影不是另一套页面采集算法，不能复制引用、指纹或动作执行逻辑。是否把默认外部观察切换到新 profile，必须经过第 11 节的版本迁移门槛。

coverage 的“完整”仅指指定 profile、frame 范围及可访问 DOM 内遍历完成。closed shadow、canvas 像素文字、浏览器 UI 等不在该语义域内；既不能声称已阅读，也不能虚构其数量。open shadow、跨源 frame、懒加载和动态列表必须有明确的支持与遗漏结果。

分页固定初始查询范围；续页替换上一页 ref。DOM 持续变化可以返回部分结果及 changed_during_read，记录上下文在读取中改变的控件不得取得可执行 ref。不能为了让测试通过而无限重启观察，或把未扫描部分标为完整。

记录指纹也必须有明确的语义范围：包含控件身份材料、声明的记录容器、命名/值依据及该记录的受支持语义内容；由公共层规范化后散列，不能直接信任后端返回的任意 signature 字符串。semantic_dom_v1 在记录采集不完整时不签发可执行 ref，并报告原因；不能只散列前一段文字却声称整条记录未变化。D1 的旧指纹行为只保留在明确的 v1 兼容 profile 下，不能作为新 profile 的验收证据。散列用于变化检测，不作为网站业务对象的可信身份认证。

### 7.2 动作模型

公共动作计划依次完成：引用归属检查、文档和记录复核、参数/敏感性检查、可操作性检查、必要滚动、再次检查、输入提交、按键释放及回执结算。

check/select 等幂等意图先读状态，已满足时返回 changed=false；普通 click 不具有可重放保证。inferred 引用只接受规定的 click/press，不能因换后端而获得填值权限。跨 frame 坐标、滚动、设备缩放及 macOS 的主修饰键转换必须由原生输入适配验证。

“输入已确认”与“业务结果已验证”始终分开。公共层可以等待明确条件；不能仅依据协议 success 将支付、保存或删除报告为完成。

**一致性不承诺任意网页上的原子点击。** 最后一次命中检查与浏览器派发坐标事件之间仍可能发生页面变化。对于提交前已观察到的身份或记录变化，必须拒绝；对不可观察的最后间隙，不得承诺绝对不会点错，也不得用不可信的 DOM click 偷换原生输入。验收必须分别记录可控变更场景与这一固有限制。

## 8. 事件、导航策略和扩展能力

### 8.1 事件入口

后端输出类型化事件：ContextCreated/Destroyed、DocumentChanged、RealmInvalidated、NavigationProgress、PromptOpened/Closed、CapabilitiesLost、TransportLost 及 PolicyDecisionRequired。协议 JSON 不得进入 Session coordinator。

事件带 SessionGeneration、可用的 ContextId/DocumentEpoch、适配器接收序号和关联命令 ID。接收序号只表示本地顺序，不宣称浏览器内部全局因果关系。事件和响应可能交错，必须依据身份及状态转移消除迟到信息。

控制、身份、拦截和连接丢失是关键事件；可选诊断流与其分开限额。关键事件溢出或解析丢失时立即停止普通输入和披露，使会话进入 Lost/Paused 并清理；不能丢事件后继续使用旧引用。首版不实现复杂的透明恢复。

### 8.2 导航策略

站点规则只有一份，由公共层计算；后端负责安装拦截、报告必要事实并执行一次性 Allow/Deny 决定。DecisionToken 绑定导航、跳转 hop、context 和 session 代次，超时或无法归属时拒绝。

策略处理覆盖顶层、子 frame、弹窗、脚本导航和重定向；在创建的上下文可执行页面导航前安装所需保护。响应阶段拒绝或拒绝重定向目标时，可能已有前序请求发出，不能报告“整个操作未发送”。导航限制继续不等同于所有子资源的网络隔离。

能力分别报告：

- `navigation_intercept`：能在规定范围暂停文档请求并作出决定；
- `resolved_destination_check`：使用受管解析结果检查目标；
- `observed_connection_address`：能够获得实际连接地址/地址空间的可用证据。

第三项不能用 DNS 结果冒充。当前 BiDi 标准 ResponseData 不提供与 CDP 连接地址观测直接等价的保证，因此 **Firefox 的严格私网策略兼容性是实施阻断项，不能默认宣告支持**。只有浏览器提供经过验证的证据，或在不增加部署依赖的前提下实现等价约束，才能启用要求该能力的策略组合；否则在打开前报告不可用，不能静默降低设备策略。[BiDi 网络类型](https://w3c.github.io/webdriver-bidi/#type-network-responsedata)

这也是对 CDP 的验收要求：不能因为现有代码读取过地址空间就无条件宣告满足新保证。缓存命中、证据缺失和重定向必须分别验证；D1 保持既有 v1 行为，v2 的新能力断言以单独证据为准。导航状态按关联 NavigationId 收敛；不能假设事件必定晚于命令响应，也不把通用 network-idle 作为页面或业务完成的定义。

### 8.3 能力模型

能力值包含 supported、限制、证据状态及不可用原因。区分“候选浏览器存在”“握手后协议可用”“本次 session 满足策略并已启用”。成功打开一个会话后，其核心保证固定；核心能力丢失应暂停或关闭，不能把运行中的会话悄悄降级。

基础合格后端必须支持：受管生命周期、文档身份、公共观察、规定输入、截图、弹窗、人工交接、导航策略及保守恢复。named profile、上传下载、原生 AX、网络诊断和性能 trace 单独声明；能力缺失在产生相关副作用前拒绝。核心保证不允许靠 optional 标记规避。

上传下载先保留现有 CDP 路径，通过类型化 ArtifactSource/UploadLease 接入公共文件策略。BiDi 在满足同样的私有暂存、一次性授权、来源绑定和取消语义前保持不支持；不能为了功能对齐而交付任意本地路径。

## 9. 原生启动与私有资料

Launcher 先确定浏览器家族、版本、平台及所需策略，再由 Supervisor 创建隔离资料和进程所有权记录，建立通道并握手；最后才对外发布 Ready。后端提供有限的启动参数描述，不能绕过公共 Supervisor 自行创建不受管进程。

Firefox 可以直接使用原生 BiDi WebSocket，避免依赖 GeckoDriver；端口 0 可由系统分配端口。[Mozilla 连接说明](https://developer.mozilla.org/en-US/docs/Web/WebDriver/How_to/Create_BiDi_connection) 启动器只接受本次受管子进程产生且经过校验的回环端点，拒绝外部 URL、继承的任意 endpoint 和重定向；正常退出关闭监听。

Firefox Remote Agent 不提供认证或加密。回环地址限制不是私有管道式的访问控制，也不能阻止同机其他可连接进程。不得把 Host/Origin 检查描述为认证，不得启用任意 origin/host 或 Gecko system access。后端必须维持部署既定的主体与资料隔离边界；若无法提供所需 OS 隔离，Firefox 在该平台/策略组合不可用，不以增加外部代理或悄悄弱化主体隔离规避约束。[Mozilla Remote Agent 安全模型](https://firefox-source-docs.mozilla.org/remote/Security.html)

启动判定还必须考虑现有 shell_coexistence。文件系统 sandbox 不自动限制对回环调试端口的访问；不能仅因 shell 被 sandboxed 就宣告通道隔离成立。单用户、一次性的受控环境可以用于 BiDi 原型验证，但不能据此通过产品隔离验收。本机同权限任意代码和 root 原有的信任边界应明确记录；不得额外扩大到原本不可信的其他主体。这一可行性须在投入完整页面适配前完成评估。

新资料目录必须包含浏览器家族和格式版本。逻辑 profile 名相同也不能让 Chromium 与 Firefox 共享文件。旧目录只在内存中识别为既有 Chromium 格式，不原位覆写其元数据；新格式使用独立目录与版本化元数据。不得原地转换、自动复制凭据或用新浏览器打开旧家族资料。临时目录、命名目录和回收记录继续遵守现有私有存储与锁机制，回退行为须经独立验证。

启动失败、半握手、端口冲突和 Agent 崩溃均进入同一回收流程。只有原生进程树停止得到确认后，才释放独占锁并删除运行目录。无法确认时保留 CleanupPending，不伪报关闭成功。

## 10. 错误、预算和诊断

DriverError 分开保存 Kind、Phase、EffectProgress、Scope 和 SessionHealth。Phase 至少包括 Prepare、Committed、AwaitingReply、Cleanup；EffectProgress 至少区分 None、Possible、Acknowledged。其中的 Possible 一经出现，不能因稍后的局部错误被降回 None。

公共层依据整个动作的进展映射现有 browser_* 错误与执行状态。协议的 unknown command、no such node、invalid argument 等不能统一转换成可重试错误。保留协议错误类别用于受限诊断，但不输出原始 payload、页面输入、私有路径或调试端点。原生错误码只在 adapter 中匹配。

规约预算采用现有限制作为起点：2 个 session、每 session 8 个 tab、32 个 frame；观察最多 1,000 节点、64,000 字符，默认 200/20,000；cursor 空闲期 300 秒。新接口不得取消这些限制。

拟定的新增内部预算：每连接普通在途命令 32 个，独立控制/清理名额 8 个；普通事件 256 条且至多 8 MiB，关键事件 64 条且至多 2 MiB；单条入站 2 MiB、出站 256 KiB；每 session 原生租约 2,048 个。计数和字节预算同时生效，不能靠增加大包或事件缓存解决背压。

上述数值是本稿拟定的实施验收上限，不代表所有值已存在于当前代码。截图和文件需要更大数据时使用有独立预算的分块交付方案，不无界扩大协议消息限制。预算调整必须说明对应资源与测试依据。

诊断记录 backend、浏览器版本、规约/profile 版本、阶段、命令数量、耗时、事件缺口和资源计数。不得默认保存完整浏览历史、页面正文、输入值或凭据。测试保存合成页面的标识与计数，避免依赖生产用户内容。

## 11. 外部兼容与迁移

内部 Engine 规约版本、观察 profile 版本、浏览器协议版本和外部 browser capability 版本是不同概念，不能共用一个数字。

CDP 迁移阶段保持既有 MCP v1、默认浏览器选择、错误映射和观察行为；所有兼容转换集中在边界。旧客户端不带选择参数时，不能因为安装了 Firefox 就改变它使用的浏览器，也不能因一次失败自动换内核并重放操作。

公开跨浏览器语义采用明确的 browser capability v2 方案：Server 同时识别 v1 和 v2；v2 的打开请求显式选择浏览器/语义 profile，会话响应返回实际选择和能力。未启用 v2 的设备继续按 v1 工作，不能将其字段静默解释成另一种语义。旧客户端需要 v1 行为时，由 Server 的兼容入口保持既有选择；若客户端硬编码只接受 version=1，则不向它承诺 v2 兼容。

v2 的具体 JSON schema、错误目录和协商字段应在公开 Firefox 前通过单独的 Agent/Server 契约变更冻结并加入兼容测试。本稿已经确定升级条件与行为，不提前修改现有 tools-list.json。该门槛未满足时，BiDi 只能进行内部开发验收，不能对外广告为可用能力。

## 12. 分阶段落地与退出条件

| 阶段 | 交付物 | 进入下一阶段的条件 |
| --- | --- | --- |
| D0：规约 | 本方案、验收表、命名语料范围、未决能力证据清单 | 状态、提交点、身份、能力和禁止降级条件均可形成可判定测试；当前只完成设计稿 |
| D1：CDP 保真迁移 | 类型边界、公共协调器、CDP adapter、兼容投影 | 现有 24 项场景无回归；公共层无协议分支；性能和资源门槛通过 |
| D2：BiDi 受管通道 | Firefox 启动、握手、事件、迟到响应与退出 | 部署、生命周期、背压、控制权和故障注入通过；不暴露页面自动化能力 |
| D3：公共页面语义 | 两个后端通过同一 semantic_dom_v1 场景 | 引用、命名、分页、输入、人工交接、导航策略通过；限制被准确报告 |
| D4：公开与扩展 | v2 Agent/Server 契约、实际平台矩阵、按能力开放文件等 | 客户端兼容、真实 MCP、各声明支持的平台组合全部通过后发布 |

保持每阶段可回退。D1 不迁移用户资料格式；D2/D3 使用独立开发开关和测试资料；D4 的新元数据必须允许旧程序拒绝不认识的格式，不能误删或误打开。不得通过放宽输入门、删掉断言、增加无限等待或静默禁用失败用例使阶段通过。

## 13. 尚需实施证据的问题

这些问题已有默认处置，不阻止评审本方案；证据不足会阻止对应能力发布。

| 问题 | 默认处置 | 所需证据 |
| --- | --- | --- |
| Firefox 各版本能否在导航发生前覆盖 popup、跨源 frame 和重定向拦截 | 不宣告该组合满足基础策略 | 固定版本的真实浏览器场景及请求计数 |
| BiDi 严格私网策略的实际连接地址证据 | 要求该证据的策略组合不可用 | 浏览器实现依据与 DNS 变更/缓存/重定向用例 |
| Firefox 的回环端点是否满足某部署的本机隔离要求 | 未满足时不可用；不称作私有通道 | 对应 OS 的隔离实现和其他本机主体连接测试 |
| canonical 命名和现有 AX 输出的差异 | 保持 v1 投影，差异归入显式 profile | 固定命名语料、当前 24 场景与版本迁移测试 |
| 支持的最低浏览器版本及 ARM 平台原生可执行文件 | 不凭版本号猜测，不以模拟运行代替原生验收 | 冻结的版本/能力/平台清单及实测记录 |

协议参考以查阅日的官方资料为依据，实施时还须固定浏览器版本及使用的定义版本：[CDP DOM](https://raw.githubusercontent.com/ChromeDevTools/devtools-protocol/master/pdl/domains/DOM.pdl)、[CDP Fetch](https://raw.githubusercontent.com/ChromeDevTools/devtools-protocol/master/pdl/domains/Fetch.pdl)、[WebDriver BiDi](https://w3c.github.io/webdriver-bidi/)。协议定义存在某个命令，不等于目标浏览器版本已经实现或满足 RDH 的行为规约。
