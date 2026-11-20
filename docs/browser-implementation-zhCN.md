# 浏览器实现与维护

远程浏览器运行在设备上。AI 通过 MCP 读取页面、提交输入、核对网站结果。用户在同一个浏览器会话中输入敏感信息或完成本人验证。安装与本地策略见[使用手册](browser-zhCN.md)。新增能力的顺序和验收条件见[架构与路线](browser-design-zhCN.md)。

## 选择运行路径

| 路径 | 谁可以使用 | 页面能力 |
| --- | --- | --- |
| v1 / Chromium CDP | 在支持的平台安装 Chromium，启用本地 `browser.enabled`，打开时省略 `selection` | 既有观察、引用操作、截图、弹窗、人工交接、可选命名资料及文件传输 |
| v2 / Chromium CDP | Linux x86_64，构建启用 `experimental-browser-bidi`，本地启用 `browser.enabled` 和 `browser.capability_v2`，显式选择 `cdp` | `semantic_dom_v1` 共同页面模型及会话声明的扩展 |
| v2 / Firefox BiDi | 同一构建和策略，配置 `browser.firefox_executable` 的绝对路径，具备所需 Linux namespace 和 pidfd 能力，显式选择 `bidi` | 同一页面模型，后端差异由能力声明和覆盖范围表示 |

浏览器功能默认关闭。Agent 使用设备上已安装的浏览器，Chromium 的本地路径由 `browser.executable` 指定或按平台规则查找。`device_info.capabilities.browser.version` 保持 `1`，`v2.backends[]` 中的 `stage: "candidate"` 表示设备允许尝试。`browser_open` 完成启动、文档身份和截图检查后才返回 `session_ready`。客户端按本次打开结果的 `capabilities` 选择功能，也可从 `browser_sessions` 取回 v2 会话能力。两个 v2 后端的 `observed_connection_address` 和 `network_isolation` 均为 `false`，要求严格连接地址检查的策略会在创建浏览器前拒绝。[选择与设备声明](../src/browser/selection.rs) · [会话能力](../src/browser/runtime/v2.rs)

Linux 上的 Firefox 会话归 Agent 进程所有。守护进程用 pidfd 等待 Agent 或浏览器命名空间的 PID 1 退出。发起 RPC 的工作线程在空闲时回收，用户仍可继续操作原页面。Agent 退出时，守护进程终止并回收该命名空间。守护进程意外退出时，内核通知命名空间的 PID 1 退出。[进程归属与回收](../src/browser/process/firefox.rs)

如果任务依赖特定扩展，先查设备上的候选后端，再核对打开后的会话。例如，将卡片拖到另一列需要 `ref_drag`。

1. 调用 `browser_guidance`，传入 `{"device_id":"example-device","topic":"open","include_extensions":true}`。在返回的 `variants` 中查找 `extensions.ref_drag == 1` 的候选项。同一任务需要多种能力时，在同一个候选项内逐项核对。`null` 表示该候选没有可用的已知版本。诊断任务还可以在同一候选项的 `task_event_categories` 中检查 `console` 或 `network`。
2. 将该项完整的 `selection` 传给一次 `browser_open`。确认实际会话返回的 `selection` 与所选项相符，并检查 `capabilities.extensions.ref_drag` 的 `version == 1` 且 `available == true`，再从新观察中选择两端。打开响应丢失时按原操作回执找回会话。

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
2. 从最新观察取元素 `ref`，通过 `browser_act` 导航、操作控件、滚动、处理标签页或弹窗。v2 的 `fill` 走原生文本编辑，日期、时间、颜色和范围控件走 `select` 的固定设值路径。省略 `ref` 的 `scroll` 滚动主文档。会话声明 `extensions.scoped_scroll` 版本 1 时，观察可为具备名称、region 角色或焦点能力的垂直滚动容器签发 `kind: scrollable` 的 ref。使用 `{"kind":"scroll","ref":"…","delta_y":420}` 滚动该容器，正数向下，范围为 −2000 至 2000 CSS 像素。到达边缘时可返回 `changed: false`。重新观察可见记录后再操作；容器专用 ref 只授权滚动。会话声明 `extensions.ref_hover` 版本 1 时，对最新控件引用调用 `{"kind":"hover","ref":"…"}` 可展开悬停菜单。可以先用 `browser_inspect` 的 `action_kind: hover` 检查遮挡和几何稳定性。指针移动也可能触发站点脚本，应在新观察中找到要选择的菜单项，再单独点击。
3. 用 `browser_wait` 等待明确的 URL 或文本，再观察目标记录的业务状态。`input_acknowledged` 只确认原生输入，`business_result_verified` 保持 `false`。一次提交即使收到了成功回执，仍需从网站读到与目标记录对应的结果。

嵌套 iframe 中的语义动作先按目标文档的视口检查几何。需要滚动时，CDP 在子文档中将保留的目标滚到中央，再沿 frame 链映射命中点，复核节点身份、可见性、遮挡和位置稳定性。命中其他控件时，动作返回 `browser_not_actionable`。[CDP 语义操作](../src/browser/engine/cdp/semantic.rs)

动作回执确认输入后，恢复摘要保留 `input_acknowledged: true`、`business_result_verified: false` 和 `site_processing_count: null`。原生表单导航在断线时可能为一次点击发出多个 POST。恢复时用原操作 ID 查询回执，再通过只读页面核对指定记录。网站的处理次数需要独立的业务回执或审计数据。

v2 逐 frame 遍历 DOM 与 open shadow tree。名称带有来源和质量，记录材料有独立预算。closed shadow、canvas 内部像素和浏览器自身界面超出语义观察范围。相同 URL 重新加载、文档或 realm 替换、节点或所在记录改变，都可能使旧引用失效。续页替换上一页的动作绑定。出现 `browser_stale_reference` 时重新观察当前记录，按原 ref 派发的动作始终重新检查身份、敏感性、几何和命中条件。[页面算法](../src/browser/semantic/page.rs) · [共同语义语料](../tests/fixtures/semantic-dom-v1/oracle.json)

Canvas 自绘控件或 closed shadow 内的按钮可能只有图上可见的标签。显式打开 v2 会话，确认 `extensions.visual_click` 版本 1，然后调用 `browser_screenshot`。用返回的 MCP 图片定位目标，保留同一响应的 `frame_id`。点击使用图片左上角为原点的整数坐标。会话同时支持 `visual_gestures` 版本 1 时，还可悬停或在同一可视面内沿直线拖动。每个动作都需要新截图。Agent 在原生输入前重新截图、检查视口和文档，再命中测试目标。CDP 和 BiDi 只在设备端检查当前命中节点的类型和 closed shadow 标记。遇到遮挡或过期帧时重新定位。账户验证和敏感输入沿用人工接管流程。[截图与指针输入](../src/browser/runtime/v2/visual.rs)

支持 `extensions.submit_validation` 的 v2 会话在通过 `ref` 点击提交按钮或按 Enter 提交表单、并触发原生 `invalid` 事件时，随动作回执返回 `native_validation`。Agent 只在获得字段证据读取权限时观察该次提交，并按原表单过滤事件。`invalid` 提供有界的原因枚举，`omitted` 表示触发事件的字段受保护。支持 `submit_validation_field` 时，`invalid` 回执还可带 `node_id`。监听器按实际表单归属记录首个失败控件，包括用 `form=` 关联的外部字段。用同一标识调用 `read_node` 读字段名称、`read_validation` 复查约束。敏感控件只返回 `omitted` 和空原因列表。

在同源顶层 POST 表单上，把原记录的节点和唯一业务键放入 `rejection_record`，再点击一次显式提交按钮。Agent 在点击前保存页面基线，观察该表单的真实提交事件，并在当前窗口匹配唯一的请求。原生校验拦截时，先读动作回执的 `native_validation`。若动作回执丢失，用原 `operation_id` 查询 `get_operation_status`，从 `result_summary.capture_id` 恢复。

用同一窗口的 `capture_id` 调用 `browser_evidence`。旧查询 `read_submit_outcome` 只返回判定摘要，200 拒绝依赖预先指定的 `expected_text`。声明 `submit_feedback` 版本 1 的后端还支持 `read_submit_feedback`。422 对应 `http_422`；若本次变化的可见提示能唯一关联到原表单中标记无效的字段，`outcome.feedback` 会包含 `field_node_id`、`message_node_id` 和查询时的页面原文 `reason`。用 `read_node` 复查字段名。200 出现这样的字段错误，或原表单、记录进入明确拒绝状态并给出新提示，可返回 `business_rejected`。普通通知和变化的状态文字以 `page_feedback` 返回，由 AI 结合原文判断拒绝、成功或仍在处理。

同一表单有多处错误时，支持 `submit_fields` 版本 1 的会话可用原 `capture_id` 调用 `read_submit_fields`。逐项读取 `outcome.fields`，用各自的 `field_node_id` 复查字段名，结合 `association` 和 `reason` 判断原因。显式 ARIA 关联、带错误标记的相邻提示、普通相邻文字分别对应 `aria`、`adjacent_error`、`adjacent_text`。HTTP 200 只有普通相邻文字时返回 `page_feedback`；原记录内的成功提示或填写建议也可能出现在这个状态下。

为同一个原始字段补充视觉线索时，Agent 比较输入前后有宽度的可见边框、轮廓及纯色背景的 CSS 计算值，并以同一 `field_node_id` 发布 `visual_changes`。保护字段、替换后的字段以及焦点、悬停等交互状态改变的字段省略颜色证据。纯样式变化可以触发 HTTP 200 的 `page_feedback`，共有颜色或相同文字都不能单独确立字段归属。

启用 `field_indicators` 版本 1 后，`read_submit_fields` 可带上 `include_indicators: true`。Agent 在提交前记录原字段和最多四层单字段容器，读取回执时比较实际绘制的边框、轮廓、纯色背景和阴影，并检查新出现的有错误标记的图标。每条 `outcome.indicators` 包含字段句柄 `field_node_id`、视觉来源句柄 `source_node_id` 和类型 `kind`。焦点或激活状态变化时省略字段的样式差异。若重新排版只改变了悬停位置，来源节点还须改变样式类，并且同一字段须有明确的拒绝文案，Agent 才保留这条视觉线索。页面插入有错误标记的图标或移除原图标的隐藏标记时，可单独报告图标。一次最多返回 32 条视觉线索，超出时省略该列表。

需要诊断无标记的错误图标时，先确认会话支持 `pixel_indicators` 版本 1。在提交动作的 `pre_action_diagnostics` 中指定 `rejection_record`，启用 `track_requests` 和 `capture_field_pixels`。Agent 在点击前截取视口，仅保留原字段旁候选区域的像素和位置。提交后使用原 `capture_id` 调用 `read_submit_fields`，同时启用 `include_indicators` 和 `include_pixel_indicators`。

Agent 将同一记录、表单和字段内的局部像素变化与新出现的字段提示对应。`image_change` 表示图片区域发生变化，`pseudo_change` 表示伪元素区域发生变化。`source_node_id` 指向图片、承载新图片的原占位节点或伪元素宿主，可与 `field_node_id` 一起复查。字段随表单重排时按相对位置对齐。焦点状态变化、多个候选、缺少可比较的区域或缺少独立字段提示时，回执省略像素证据。回执只返回节点引用与已有的字段诊断。

会话能力 `icon_diagnostics` 版本 1 在上述查询中增加 `include_icon_candidates`。对于原记录唯一的 HTTP 422 POST，无可见字段错误文字的图标可以进入 `suspected_fields`。Agent 将已返回的字段和来源节点保存在这次提交的采集窗口内。显式动作 `hover_submit_icon` 再次核对节点、截图中的图标区域和请求归属，向该区域中心移动指针。提交前的标题和描述基线用于排除已有帮助文字；悬停前的提示状态用于识别新出现的本地 tooltip。后续读取按原字段归属返回可核实的原因。

同一表单顶部或底部的摘要可以用指向原字段的 `#id` 链接，也可以用唯一字段标签加冒号逐项说明原因。`summary_error` 表示页面明确标记了错误，`summary_text` 表示可定位字段的普通反馈。字段独占容器内的提示可在输入框之前或之后，适用于上方、右侧和下方等布局。比较 `fields` 与 `indicators` 的 `field_node_id` 后，再用 `read_node` 复查文案和来源。存在同名歧义或归属冲突时，保留能独立确认的证据。

异步提示可以在原窗口内重读。查询限定在原表单、原记录和同一文档，页面文字按不可信数据处理。记录被替换、读到邻单提示或多个错误归属不清时，保留网络层已知状态。导航或交还控制后，通过原操作和业务键重新观察记录。[提交判定](../src/browser/runtime/v2/submit.rs)

| 标识 | 可用于 | 失效与拒绝 |
| --- | --- | --- |
| `ref`、`next_cursor` | 操作当前元素或续读原扫描 | 绑定会话、tab、文档、控制代次和读取序列。旧标识不能按同名或同位置重新匹配 |
| `frame_id`（AI 截图） | 按当前图片点击、悬停或同面拖动 | 最长 30 秒。新截图、输入、导航或控制权变更后重新截图 |
| `node_id` | 读取节点诊断或等待同一记录的业务状态 | 与动作 ref 独立。会话注册表保留 128 项、32 KiB，创建后 60 秒过期。换代、节点替换或被回收后需重新查找 |
| `revision` | 受控接收端应用观察增量 | 不赋予输入资格。重新连接、换代或基线失效后从完整观察重建，每次替换全部 ref 和 frame 绑定 |
| `file_grant_id` | 给一个当前的文件输入控件选择经用户批准的文件 | 绑定原主体、会话、tab、站点与一次选择。结果不明也不会恢复次数 |

### 用紧凑格式读取页面

默认观察为每个节点重复输出所属记录和全部状态字段，一个节点约占 1 KB，行数多的页面要连续读取多页。v2 会话声明 `extensions.compact_observation` 版本 1 时，可向 `browser_observe` 传入 `"format":"compact"`，用更少的字符取得同一次观察。Core 在会话支持时把这个参数放进 `browser_open` 返回的 `first_observe`，客户端原样传回即可。

紧凑结果用 `format: "compact_v1"` 标明，以 `frames` 和 `records` 取代 `nodes` 与 `frame_overview`，`page`、`coverage`、`next_cursor` 不变。

| 字段 | 读法 |
| --- | --- |
| `frames[].nodes` | 按阅读顺序排列，归在所属 frame 下。同一 frame 被其他 frame 的内容隔开时会出现多次 |
| 省略的节点字段 | 取通常值：没有 `role` 的节点是页面文本，`kind` 为 `native`，`value`、`checked`、`disabled`、`readonly`、`editable`、`sensitive`、`offscreen` 均未设置。可编辑字段的空值仍会写出 |
| `record` | `records` 的下标。同一行或列表项的记录只写一次，同名控件靠各自的记录区分 |
| `records[i].entries` | 字符串是记录内的文本；`[tag]` 或 `[tag, {attributes, value, checked, text}]` 是元素。没有属性、值和状态的布局容器（如 `td`、`div`、`span`）不写出，记录自身的元素除外 |
| `ref` | 形如 `r6aa6558` 的短引用，用法与失效规则同普通引用。一个会话内不重复签发，前缀随会话变化 |

记录已经列出的文本不再单独成为节点：返回了该记录的控件时，文本只出现在 `entries` 中；记录只有文本时保留第一段文本，使记录留在页面中的原位置。记录不完整或文本被截断时，文本节点照常返回。

紧凑读取的 `max_nodes` 默认为 1,000，`max_chars` 仍默认为 20,000，并按紧凑输出的估算字符数计费，同一记录只计一次。页面程序发回 Agent 的原始批次另以 600,000 个 UTF-16 码元为限。续页沿用起始读取的格式。普通读取每次调用在一个文档内最多遍历 5,000 个节点；紧凑读取在预算未用完时继续遍历，最多 8 段，所以在几千行的表格里按文字查找通常一次调用即可返回。`format` 不能与 `presentation` 同时使用，v1 会话收到它返回 `browser_feature_unsupported`。[动作后视图](#动作后只读取变更的部分)也使用这一格式。[紧凑投影](../src/browser/runtime/v2/compact.rs) · [页面内计费](../src/browser/semantic/program.js)

### 动作后只读取变更的部分

会话声明 `extensions.after_action_view` 版本 2 时，带 `ref` 的 `browser_act` 可传 `"after_action_view": true`，在同一次回执里取得页面的变化，省去紧接着的一次观察。

每个文档的页面程序用 `MutationObserver` 记录自上次读取以来被改动的元素：子节点增删、文本变化，以及影响可见性或可操作性的属性（`class`、`style`、`hidden`、`disabled`、`aria-*` 等）。输入值和勾选状态是元素属性，不产生这类记录；页面程序另外监听 `input` 和 `change` 事件，把该控件的父容器记为变更区域，以便读到紧邻它、由样式规则或脚本联动的内容。动作确认后，Agent 把这些元素扩大到包住它们的记录，只扫描这些区域，按[紧凑格式](#用紧凑格式读取页面)返回。每个区域之前最近的标题也一并返回，用来说明区域所在的位置，例如卡片被移到了哪一列；这个标题本身未必发生过变化。

| 回执中的 `after_action_view` | 含义与下一步 |
| --- | --- |
| `state: "sampled"` | `frames`、`records` 列出变更区域，`scope` 为 `changed_regions`。其中的 `ref` 新签发，可直接使用。`removed` 表示有内容被移除或替换，`truncated` 表示变更区域超出 100 节点或 16,000 字符的预算。`settled: false` 表示采样前后页面仍在变化，例如带时钟或实时数据的页面 |
| `continue_observing` / `no_visible_change` | 没有记录到变更。上次观察的引用继续有效。只由样式规则或脚本属性改变、又不在被操作控件旁边的内容不在记录之内 |
| `continue_observing` / `view_unavailable_or_changing` | 文档未就绪或带 `aria-busy`、单个文档的变更元素超过 64 个、有文档被替换或新增，或读取超出约 900 ms。所有引用失效，需重新观察。页面用 `pushState` 改地址而文档不变时视图照常返回 |
| `continue_observing` / `control_changed` | 读取期间控制权变更，结果被丢弃 |

普通动作会作废上次观察的全部引用。请求视图的带引用动作和不带引用的文档滚动，在视图为 `sampled` 或 `no_visible_change` 时保留这些引用：未变更部分的引用可继续使用，落在已变更记录内的旧引用在派发前的指纹复核中返回 `browser_stale_reference`。动作本身失败时引用照常全部作废。

首次观察之前发生的变更没有记录。晚于采样的异步更新不在视图中：页面先显示成功、随后被服务端拒绝并回滚时，视图里看到的是已被撤回的成功，业务结果仍须稍后从目标记录核对。[视图读取](../src/browser/runtime/v2/action_view.rs) · [变更记录与区域扫描](../src/browser/semantic/program.js)

### 用场景池核对页面变化的各种方式

`tests/fixtures/browser-scenarios/pages` 下每个页面以一种方式改变自己，共 21 个场景：

| 类别 | 场景 |
| --- | --- |
| 元素被复用或替换 | `recycle`（虚拟列表滚动）、`pager`（翻页复用行元素）、`rerender`（整表重绘）、`sort`（行只移动） |
| 变化不经 DOM 或不在原地 | `css-state`（样式规则与属性）、`modal`（确认框渲染在别处）、`toplayer`（原生模态对话框）、`kanban`（卡片换列） |
| 异步与噪声 | `optimistic`（乐观更新后回滚）、`autocomplete`（迟到的建议）、`ticker`（持续变化）、`feed`（不断下推的信息流）、`lazy`（滚动加载） |
| 结构与规模 | `spa`（同一文档内换路由）、`shadow`（open shadow 组件）、`xframe`（跨站 iframe）、`sticky`（粘性表头遮挡）、`big`（两千行表格） |
| 表单流程 | `dependent`（联动字段）、`inline`（行内编辑）、`wizard`（多步向导） |
`server.py` 记录页面发出的每次写入，场景以站点实际收到的写入和页面终态判定，不采信工具自己的回执。

```sh
RDH_BROWSER_EXECUTABLE=/opt/google/chrome/chrome cargo test --locked \
  --features experimental-browser-bidi --test browser_scenarios -- --ignored --nocapture
scripts/browser-scenarios-agent-browser.sh /opt/google/chrome/chrome
```

第一条命令经 Agent RPC 驱动 v2/CDP 会话，逐项断言并打印每个场景的调用次数、返回字节数和写入；设置 `RDH_SCENARIO_REPORT` 可把结果存为 JSON，设置 `RDH_SCENARIO_ONLY=sort,pager` 只运行列出的第二批场景。第二条用 `agent-browser` 驱动同一批页面，每个场景输出一行 JSON 供对照，需要 `python3` 和已安装的 `agent-browser`。新增场景时加一个页面、在 [场景测试](../tests/browser_scenarios.rs) 中加一段步骤和断言，并在对照脚本中加同样的步骤。

### 用拖放移动看板卡片

在 v2/CDP 会话上检查 `capabilities.extensions.ref_drag` 的 `available` 和 `version`（版本 1）。调用 `browser_observe`，设置 `filter: "drag_endpoints"`。同一页观察中的 `drag_source` 表示显式可拖动的具名元素，`drop_target` 表示有名称和接收标记的候选区域。网页决定是否接受拖放。两端执行时都须在当前主文档的视口内，中心能命中。客户端可先滚动，再重新观察；第一版覆盖的后端为 Chromium CDP。

向 `browser_act` 传入 `{"kind":"drag","ref":"<源 ref>","target_ref":"<目标 ref>"}`。Agent 在按下前及最后一次移动后复核两端的文档、记录和命中位置。Chromium 确认指针移动后，Agent 还会在隔离环境中等待可信的原生拖动进入事件，确保接管时能够取消已经启动的拖动。端点发生变化或控制权被收回时，Agent 取消尚未释放的拖动。释放已经送出而回执丢失时，按原 `operation_id` 查询，恢复控制后读取同一条记录及目标列。人工交还后使用新控制代次和新观察。

已有会话未提供 `ref_drag` 时，先观察目标记录是否带有网站提供的“移至”按钮或菜单。例如两行都有同名按钮时，检查各自的 `record.entries`，用订单号区分 `B42` 和 `B42-copy`。在还没有派发写操作的情况下，可点击对应记录的按钮一次。页面刷新后重新核对目标记录的列。若前一次输入的结果不明，先按原操作 ID 和页面现状排查。

### 独立验证页面行为

排查 Chromium 页面问题时，给 Easelian 和 `agent-browser` 各准备一份初态相同的合成业务记录，固定同一浏览器二进制。记录操作前的订单号和状态。两边分别使用自己的会话和最新快照，每份记录只提交一次。在 Linux 测试机的 Bash 中，将 `chromium` 设为浏览器的绝对路径，将 `fixture_url` 设为本次合成页面地址。

```bash
session="easelian-compare-$(cat /proc/sys/kernel/random/uuid)"
compare() { ~/.local/bin/agent-browser --session "$session" --executable-path "$chromium" --json "$@"; }
compare open "$fixture_url"
compare snapshot
```

| 问题 | CLI 侧核对方式 | 判定依据 |
| --- | --- | --- |
| 相邻订单已经批准，目标订单仍待处理 | 用 `compare wait --fn` 检查目标订单自身的状态元素，限定等待时间。嵌套订单使用独立的状态元素 | 等待超时后读回原订单。只读刷新后再检查同一条件，核对期间的写入数 |
| 站点已记账，HTTP 回复丢失 | 用 `compare network requests --method POST` 核对提交次数，读取页面提示。只点击站点提供的只读刷新控件，再查看对应的 GET 和原订单状态 | 页面可以先显示结果不明。刷新读到该订单已批准、站点计数为一次写入和一次读取，即可确认提交生效 |
| 邮箱格式错误，提交后页面未跳转 | 在独立合成表单上用 CLI 的 `eval` 读取目标输入框的 `validity.typeMismatch` 和 `validity.valid`，用 `compare network requests` 核对请求。Easelian 侧在修改字段前读取同一字段的 `read_validation` | 错误状态应为 `invalid/type_mismatch`，当时没有提交请求。修正后再核对一次目标写入和原订单的新状态 |
| 原生拖放 | 从当前快照定位源和目标，调用 `compare drag`。记录两端的 ref、网页收到的投放和目标列 | 重新观察原卡片所在的列，并核对站点写入次数 |
| 控制台报错 | 在单次保存后用 `compare errors` 看未处理异常，`compare console` 查站点日志，`compare network requests` 查是否发出保存请求。对齐 Easelian 的一基位置时，把 CLI 0.43.0 的顶层 `line` 和 `column` 各加一 | 核对同一次动作和对应源位置，再判断错误是否解释了页面现象 |

结束时执行 `compare close`。Easelian 独有的回执和控制权仍通过公开 MCP 验证。工具回复丢失时按原 `operation_id` 查询，人工交还后取得新的观察，再核对原记录。Firefox 专属行为使用原生 BiDi 会话验证。

### 在虚拟列表中确认目标记录

带组合框的页面通常只挂载一小段选项，滚动时会把同一批 DOM 节点用于下一段。设备和会话声明 `extensions.virtual_list` 版本 1 后，展开组合框，观察具有 `role: "listbox"` 和 `kind: "scrollable"` 的容器。用该容器的 ref 定向滚动，每次重新观察当前选项。在选项的 `record.entries[].attributes` 中读取页面提供的 `aria-posinset`（从 1 开始的逻辑位置）和 `aria-setsize`（集合大小，`-1` 表示未知）。位置只在对应列表内有意义。页面声明的位置非法或大于已知集合大小时，该选项的记录将标为不完整，并且不会签发动作 ref。同名项缺少可区分的标记时，先向任务上下文核对。选中前使用最新的选项 ref；若节点已被页面复用或移到另一个列表，重新观察并核对目标。

要确认提交或选择后的页面结果，先用 `browser_evidence` 的 `find_nodes` 取得目标记录的 `node_id`。设备和会话声明 `extensions.record_wait` 版本 1 时，可对当前 tab 调用 `browser_wait`，传入 `record: {"node_id":"…","anchor_text":"订单 B42","expected_text":"已批准"}`。两个字符串分别匹配同一条记录普通 DOM 子树中的独立可见文本节点，不跨 shadow root。单次采样最多遍历 256 个后代、4,096 个 Unicode 标量，遍历深度不超过 64 层；覆盖不完整时继续等待，直到条件成立或期限届满。选稳定且能区分业务记录的文本作为锚；状态出现在相邻记录、内嵌记录或输入框时，不能据此确认成功。页面整体换代时要重新定位记录。超时后检查回执和页面现状，再决定下一步；页面结果只能证明网站当时展示的状态。

同名按钮位于嵌套订单时，用 `browser_observe(find=订单号, filter="interactive")` 比对按钮的 `record.entries`。记录内查找只使用最近一层记录自己的内容。HTML 的 `article`、`tr`、`li`、显式 `data-rdh-record` 以及首个 ARIA 角色为 `row`、`listitem`、`article`、`option` 的容器可形成记录。若记录角色只出现在后备角色中，按不确定边界处理，外层等待会跳过该子树。动作派发还会复核外层记录的归属。页面把同一按钮或整条子记录移到另一订单后，重新观察以取得新的引用。

### 可用的诊断与增量展示

`browser_guidance` 提供 `open`、`observe`、`recover`、`handoff`、`files` 五个主题。Core 从已认证连接的会话元数据选择静态模板。模板的 `content_trust` 为 `server_template`，网页观察和诊断中的网页数据为 `untrusted_page`。指引里的示例仍需实际调用时的授权、参数和状态。

- **操作前置条件。** v2 的 `actionability` 扩展使用 `browser_inspect(ref, action_kind)` 检查一个动作。结果为 `ready_now`、`needs_scroll`、`blocked` 或 `indeterminate`。检查不触发滚动、聚焦、输入或文件授权消费，也不延长 ref。对返回 `needs_scroll` 的点击检查，普通 ref 点击会按需滚动，复核目标、命中点和位置稳定性后再派发。[动作判定](../src/browser/semantic/actionability.rs)
- **DOM 与布局。** v2 两个后端的 `dom_evidence` 扩展使用 `browser_evidence`。`find_nodes` 以 tag、role 或文本查找诊断节点，`read_node` 读取受限的节点事实，`read_layout` 读取 frame 视口内的布局、计算样式子集和局部命中情况。父 frame 遮挡、字段缺失和预算截断通过 `coverage` 表达。用 `node_id` 查读不会替换当前观察或增量基线。[证据采集](../src/browser/semantic/evidence.rs) · [注册表](../src/browser/runtime/v2/evidence.rs)
- **观察增量。** `observation_delta` 是展示扩展。受控客户端在每次调用的外层 `params._meta` 中声明 `com.easelian/browser-observation-delta` 的版本 1 和 reducer `rdh-observation-v1`，并显式传 `presentation`。Agent 执行完整观察，Core 按真实 MCP 编码在 `full`、`delta` 和 `unchanged` 中选择。只有候选至多为同次 full 的 70% 才发送较小格式。Server `clients/browser-observation-delta.mjs` 重建并校验当前绑定。普通客户端继续使用完整观察。[Agent 展示缓存](../src/browser/runtime/v2/presentation.rs)

#### 动作期间的事件

v2 会话通过 `extensions.task_events` 公布 `version: 1` 和 `categories`。当前两个后端均可声明 `console` 和 `network`。客户端只请求所选会话公布的类别，旧版 CDP Agent 可继续使用 `network`。若需要独立采集窗口，AI 在操作前调用 `browser_capture` 开窗，执行一次已授权操作后用 `browser_evidence` 读取。两种工具都要求 `version: 1`、同一会话、tab 和当前控制代次。启停使用独立的操作回执，启动响应丢失时按原操作 ID 找回 `capture_id`。动作本身继续使用原有的输入回执。

诊断一次提交时，若当前后端和会话都报告 `extensions.action_capture.version == 1` 且可用，客户端可将采集选项放入同一个 `browser_act.pre_action_diagnostics`。例如，`{"categories":["console"],"track_exception_locations":true}` 要求已启用异常位置扩展。Agent 先核对会话、tab 和控制代次，再开窗并派发一次带 ref 的点击。回包内的 `capture_id` 可用于 `read_events` 和 `read_exception_location`。若客户端丢失成功回包，可从原操作回执恢复窗口标识。原生输入结果未知时，先查原回执并只读核对记录；缺少窗口标识时应报告诊断证据不足。

本次点击产生的异步事件可在窗口有效期内继续到达。Agent 在处理下一次 `browser_act` 前停止该点击的采集，也允许显式停止。已经单独开启的窗口仍覆盖其间的动作；如果它还在采集，本次附带诊断的点击会收到 `browser_capture_active`，网站不会收到这次输入。导航或控制代次变化后，旧窗口失效。即使在同一窗口内，后台请求仍可能与点击无关。

窗口只保存下列事实。每条都有从 1 开始的 `sequence` 和相对开窗时间 `elapsed_ms`。

| 事件 | 可读信息 |
| --- | --- |
| `response`、`failed` | HTTP 状态码，或受限的传输错误类别。CDP 以目标 tab 的原生 session 归属，可能包括同一进程的子 frame。BiDi 只接收可归属该顶层 context 的事件 |
| `console`、`exception` | 来自顶层页面的控制台级别和浏览器上报的 JavaScript 异常标记。CDP 只接受顶层文档默认执行环境，BiDi 按顶层 context 过滤。日志正文、参数、错误文本、请求地址和认证信息均不进入窗口 |

`read_events` 的 `after_sequence` 为排他下界。首次读取返回 `through_sequence`，续页原样传回并用 `next_sequence` 接着读。达到末序号后，可省略 `through_sequence` 读取新增事件。`read_event` 使用同一窗口和序号复查一条摘要。窗口最长采集 30 秒，按序保留最近 128 条固定大小的记录。覆盖旧记录时，`dropped_count` 给出累计丢弃数；读到空页也可能是所查区间已全部淘汰。主动停止或到期后最多保留 30 秒，开始新窗口会替换旧窗口。控制权变化或文档替换会使旧窗口失效。人工操作结束后须重新开窗。

仅请求网络事实时，Chromium 沿用页面的网络订阅。请求控制台时，Agent 额外挂接一个 browser session 及其页面子 session。订阅启用前的回放和早于窗口起点的事件均不交付。对于当前文档的 `console.error`，AI 收到 `kind: console` 和 `level: error`。收到 `Runtime.exceptionThrown` 时，AI 收到 `kind: exception`。停止、换页、换代、超时和取消都由会话所有者收尾。[订阅与回收](../src/browser/engine/cdp/wire/console.rs) · [事件归属](../src/browser/engine/cdp/wire/console_route.rs)

会话声明 `extensions.exception_locations` 版本 1 且可用时，AI 可在含 `console` 的开窗动作中设置 `"track_exception_locations":true`。例如，点击保存后页面停在原订单，先从 `read_events` 找到 `kind: exception` 的 `sequence`，再向 `browser_evidence` 提交以下查询。使用原会话、tab、控制代次及版本 1。

```json
{"kind":"read_exception_location","capture_id":"<原窗口>","sequence":7}
```

`frames` 按异常现场到调用者的顺序，最多给出三个同步帧。每帧的 `line` 和 `column` 都从 1 开始；浏览器可能把同一表达式定位在不同列。`truncated: true` 表示部分栈未取得或未展开。`source_id` 是窗口内的来源编号，CDP 按原生脚本标识区分，BiDi 按浏览器报告的资源地址区分，同一页面的多个内联脚本可能共用编号。每个窗口最多编号 32 个来源，缺少来源或超出预算时返回 `null`。编号仅适用于当前窗口。Agent 在设备上将有界来源键转换为摘要；公开结果只包含编号和位置。[位置投影](../src/browser/semantic/task_events/locations.rs)

异常位置与事件一起淘汰。停止窗口后可在保留期限内复查，交给人工或文档替换后必须重新开窗。Firefox 按原生事件时间过滤早于本次开窗以及与开窗同一毫秒的异常。若还要判断保存请求是否到达网站，可以在同一窗口同时启用 `network` 和 `track_requests`，并在操作后只读刷新原订单。调用栈能说明浏览器在哪一行抛错，提交结果需要单独核对。

当前会话声明 `extensions.network_progress.version == 1` 且可用时，可在包含 `network` 类别的开窗动作内增加 `"track_requests":true`。使用 `{"kind":"read_requests","capture_id":"…"}` 读取快照。窗口最多保留 32 条请求记录，每次重定向另占一条，按窗口内的整数 `request_id` 排列。Chromium 观察目标原生页面 session，可能包含同进程子 frame。Firefox 只计入原顶层 context。两个后端从原生响应头事件取得 HTTP 状态，随后收到传输失败时更新为 `failed`。

`start_observed:false` 表示当前行缺少开始证据，此时省略方法。已观察开始时只返回标准方法枚举或 `OTHER`。重定向的下一跳通过 `redirected_from` 指向保留的前一跳，缺失前一跳时标记 `redirect_gap:true`。`evicted_count` 统计淘汰的行，`untracked_count` 统计无法归属或解析的进度事件。普通事件读取继续使用原有的序号和分页。控制代次改变后重新开窗。新窗口只披露从该窗口开始观察到的请求链，避免把上个控制者产生的延迟响应交给 AI。[请求关联与淘汰](../src/browser/semantic/task_events/requests.rs)

排查提交后页面迟迟没有变化时，可用可重置的合成订单对照两条路径。固定 Chromium 版本和订单初态，在各自的独立会话中只提交一次。Easelian 在提交前开启请求跟踪；参考工具在同一 CLI 会话中用 `network requests --method POST` 分别读取响应获准返回前后的状态。核对各自会话内的请求标识，再只读刷新目标订单，并检查测试站点收到的写入次数。例如，同一个 POST 从等待响应变为 HTTP 409，刷新后 B42 显示需要复核，而测试站只收到一次提交。这样可以区分等待服务端、业务拒绝和客户端重复提交。CLI 会话的启动步骤见[独立验证页面行为](#独立验证页面行为)。

每条控制台消息最多检查 16 个参数和 8 个不同原生对象，回收队列最多容纳 16 个句柄。超出预算时，Agent 终止该诊断窗口，主页面可以继续操作。超过 2 MiB 的私有 Runtime 通知只有在能按原生消息格式核实其子 session 时才允许流式丢弃，最长 2 秒、最多 16 MiB。其他超限消息沿用连接级故障处理。窗口返回 `browser_capture_unavailable` 时，AI 应重新观察并核对网站结果。若新窗口仍受历史大消息影响，可继续使用网络和页面证据。[大消息归属与丢弃](../src/browser/engine/cdp/wire/oversized_console.rs)

脚本异常、请求失败和页面业务拒绝各自回答不同问题。即使收到 HTTP 200，也要从原目标记录读取结果；缺少对应证据时应报告哪项结果尚待确认。更多样式来源、调用栈位置和性能采样见[架构路线](browser-design-zhCN.md)。[定型事件与分页](../src/browser/semantic/task_events.rs) · [窗口生命周期](../src/browser/runtime/v2/capture.rs)

#### 定位影响控件的样式

设备和 CDP 会话都声明 `extensions.matched_styles` 版本 1 时，客户端可以定位影响当前任务的 CSS 声明。先取 `find_nodes` 的证据 `node_id`。用 `read_layout` 检查计算值，再对同一节点传入 `{"kind":"read_styles","node_id":"…","property":"pointer-events"}`。属性还可选 `display`、`visibility`。BiDi 会话可继续使用自己的布局证据。

从 `styles.candidates` 查看来源、字面值和 `important`。有源码位置时，`line`、`column` 从 1 开始，`sheet` 是单次结果内的样式表编号。`value: null` 表示声明使用了此接口不展开的值，例如 CSS 变量。候选只覆盖目标元素自身，行内声明先取样，随后从原生匹配列表末端取样。最多检查 32 条规则、128 项属性，最多返回 16 项候选。到达边界时读取 `styles.truncated`。结合计算值和命中测试解释候选，`cascade` 目前固定为 `undetermined`。

#### 读取祖先样式

在同一节点需要判断祖先是否影响命中或可见性时，CDP 会话可使用 `read_ancestor_styles`。请求仍使用原 `node_id`，并指定 `property` 为 `pointer-events` 或 `visibility`；`display` 等属性不属于该接口。设备的 CDP 候选和实际会话均须启用版本 1 的 `dom_evidence`、`matched_styles` 和 `ancestor_styles`。例如，可向 `browser_evidence` 传入 `query: {"kind":"read_ancestor_styles","node_id":"<find_nodes 返回的 node_id>","property":"pointer-events"}`。

Agent 从当前节点沿普通 DOM 父链读取，最多检查 4 层祖先。shadow root、`slot`、`assignedSlot` 或跨文档边界会使这条链不可证明；不会把 shadow/slot 两侧拼成一条祖先链。返回的候选沿用直接样式的来源、字面值、`important` 和可选源码位置，单次结果共享 32 条规则、128 项属性和 16 项候选的预算，`cascade` 保持 `undetermined`。`styles.ancestors_examined` 是本次检查的层数，`ancestor_depth: 1` 表示父元素。若还有更远的父节点或超出采样预算，`styles.truncated` 为 `true`。

遇到敏感控件或带敏感标记的可编辑祖先时，结果的检查层数为 0，候选为空，`coverage.omitted` 包含 `sensitive_style_rules`。采集期间节点重挂或文档身份变化时，丢弃本次结果并重新定位。元素使用行内样式覆盖祖先声明时，仍可同时读取直接样式和祖先样式；例如祖先的 `pointer-events: none` 与节点自身的 `pointer-events: auto` 都可以作为候选交给 AI，但不能据此宣称完整层叠结论。

Agent 在隔离执行环境中复查证据节点和敏感标记。敏感节点返回空候选和 `coverage.omitted` 中的 `sensitive_style_rules`。Chrome 需要先以 `DOM.getDocument(depth=0)` 登记当前文档，再用 `DOM.requestNode` 为已绑定的包装对象取得临时前端 ID，才能调用 `CSS.getMatchedStylesForNode`。同一浏览器连接的样式读取等待上一次 DOM/CSS domain 关闭确认。取消、控制变更和迟到回包要覆盖包装对象释放、后续读取与旧结果拒绝。[原生读取](../src/browser/engine/cdp/styles.rs) · [结果投影](../src/browser/engine/cdp/styles_project.rs)

#### 表单提交前的字段校验

提交后页面留在原处时，可以先检查目标记录中的字段。设备和会话都声明 `extensions.form_validation` 版本 1 后，使用 `browser_evidence.find_nodes` 取得字段的 `node_id`，再向同一工具传入 `{"kind":"read_validation","node_id":"…"}` 作为 `query`。该读取覆盖 Chromium 和 Firefox 的原生 input、select 和 textarea。字段处于 `invalid` 时，`reasons` 列出浏览器报告的原因，例如必填项为空（`value_missing`）、电子邮件格式不符（`type_mismatch`）或站点设置了自定义错误（`custom_error`）。客户端可以据此定位字段，再核对页面给出的操作提示。 如果要向维护者解释上一次提交被拦截的原因，应先保留该字段的校验原因，再修改字段。输入新值后，原状态就无法从当前字段读回。

`valid` 表示当前字段通过原生约束。禁用字段以及普通页面元素返回 `not_applicable`，敏感控件返回 `omitted`。读取不提交表单，也不触发 `invalid` 事件。输入值、站点自定义错误文字和密码状态不进入诊断。自绘表单和服务端业务规则仍需观察目标记录，必要时结合请求与异常摘要判断。

## 单节点原生无障碍证据

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

遇到密码、一次性验证码或需要本人完成的验证时，AI 暂停任务，提供 Core 返回的 `human_url`。v2 的暂停和人工接管会先拦住旧代次的新输入。遇到只读等待时，在本次调用期限内等待它退出。若原生拖动等输入仍在执行，调用会返回 `browser_input_pending`。保留原操作回执，等输入结算后再请求接管，避免重复提交。人工已接管时再次请求暂停会收到 `browser_paused`，原人工租约仍可用。用户登录并明确接管后，AI 的页面读取、截图和输入受阻。用户核对可分享的页面和文件元数据后明确交回并同意披露。原对话继续时，AI 重新查询会话、tab 和新代次，从新观察取得 ref。普通网络或页面故障走诊断与回执恢复。人工输入受 view、租约、代次、序号和截图帧约束。系统级生物识别、硬件安全密钥界面不在远程网页视口内。[人工输入与交还](../src/browser/runtime/human.rs)

`browser.persistent_profiles`、`browser.uploads`、`browser.downloads` 分别启用，默认均关闭。独立文件批准还需设备的 `capabilities.browser.extensions.file_authorization` 版本 1 可用。用户从 `files_url` 或会话列表进入“批准文件使用”，确认 tab、目标 origin 和本地文件。AI 保留控制，批准后从 `browser_files` 取得授权，用最新主文档文件输入 ref 执行 `attach_file`。Agent 在原生派发前持久化消费。旧版租约上传仍可用于已接管的会话。文件字节只走授权的人工接口。[文件批准](../src/browser/runtime/file_approval.rs) · [上传存储与消费](../src/browser/files/upload.rs)

下载接收原始 HTTP 200 附件响应，要求唯一 `Content-Length` 且无压缩编码。Firefox 在私有 namespace 内处理原下载，取消进行中的下载影响整个会话。Chromium 的取消范围为单文件。`cancellation_scope` 告知用户影响范围，收到取消请求后继续核对终态。命名资料绑定主体与后端，v1、v2 Chromium、v2 Firefox 各自独占。[文件与下载](../src/browser/files) · [资料命名空间](../src/browser/profiles.rs)

## 资源边界与回退

| 资源 | 当前上限 | 源码 |
| --- | --- | --- |
| 会话、tab、frame | 每设备 v1/v2 合计 2 个会话。每会话 8 个 tab。v2 32 个 frame | [runtime](../src/browser/runtime.rs)、[v2](../src/browser/runtime/v2.rs)、[types](../src/browser/semantic/types.rs) |
| v1/CDP 拓扑与连接证据 | 页面最多 32 个 frame。会话最多 40 个附着子 target。连接地址证据最多 1,024 项并淘汰最旧记录 | [CDP page](../src/browser/engine/cdp/page.rs)、[session](../src/browser/engine/cdp/session.rs)、[runtime](../src/browser/runtime.rs) |
| 观察 | 默认 200 节点/20,000 字符，最大 1,000/64,000。紧凑格式默认 1,000 节点。每段遍历 5,000 节点，待遍历队列 50,000 项 | [page](../src/browser/semantic/page.rs)、[program](../src/browser/semantic/program.js) |
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
| 待决命令与发送队列 | CDP 普通 32 槽，另有 4 个供对象、流、诊断域和诊断会话清理的槽，发送队列 36。BiDi 普通 32 | [CDP wire](../src/browser/engine/cdp/wire.rs)、[BiDi wire](../src/browser/engine/bidi/wire.rs) |
| CDP 原生 session | 每连接 128 个 attachment。单 ID 256 字节。ID 与父 ID 合计 32 KiB。退出后未结算工作最多保留 3 秒局部宽限，再关闭连接准入并恢复 | [CDP sessions](../src/browser/engine/cdp/wire/sessions.rs) |
| 协议租约 | CDP 对象 256、流 16，分类不可借用。包括 pending 预留。BiDi root handle 0 | [CDP leases](../src/browser/engine/cdp/wire/leases.rs)、[BiDi wire](../src/browser/engine/bidi/wire.rs) |
| 事件 | CDP 256 条/合计 8 MiB 已编码字节/最多两个关键消费者，单消息受 2 MiB 限制。BiDi 256 条/合计 1 MiB/单事件 64 KiB | [CDP events](../src/browser/engine/cdp/wire/events.rs)、[BiDi wire](../src/browser/engine/bidi/wire.rs) |
| 导航决定 | 32 个并发、4,096 个去重项、2 秒期限 | [policy](../src/browser/semantic/policy.rs) |
| BiDi 启动上下文与写入 | 128 活动项、128 销毁标记、深度 32。写入 3 秒。启动日志 64 KiB | [BiDi](../src/browser/engine/bidi)、[Firefox process](../src/browser/process/firefox.rs) |
| 截图 | viewport JPEG，宽高各不超过 1920 CSS px，base64 最多 512 KiB。完整响应另外受限 | [image](../src/browser/image.rs)、[v2 native](../src/browser/runtime/v2/native.rs) |
| 命名资料与文件 | 每命名空间 16 个资料。文件/暂存合计 16 项/128 MiB，单文件 32 MiB。读取块 64 KiB。完成文件保留 24 小时 | [profiles](../src/browser/profiles.rs)、[files](../src/browser/files.rs) |

回退 Agent 前，用支持当前格式的版本关闭会话并确认所属浏览器进程、下载与清理都已结算。按完整 `selection` 清点 v2 资料与下载，保留仍需使用的数据。对明确退役的数据使用相应删除接口，再停止 Agent，只用 `rmdir` 从最深层移除已确认空的 v2 目录。未使用的版本 2 上传批准由当前版本撤销或完成重启清理。旧版遇到不认识的目录或元数据会保留数据并返回 `browser_cleanup_pending`。用目标版本支持的本地配置启动并验证原有 v1 资料。[资料](../src/browser/profiles.rs) · [文件格式](../src/browser/files/upload.rs)

回退 Server 时先排空受影响 Core，再用目标版本的 `rdh-migrate activate-runtime-transactions ENV CORE_ROLE` 切换配套 Runtime 函数契约。`ENV` 是保存维护数据库连接的环境变量名。目标 Core 的 `check-storage CONFIG.json` 成功后再恢复服务。Gate、Core 工具目录与数据库函数按各自契约核对。
