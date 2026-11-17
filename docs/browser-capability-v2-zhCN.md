# Browser capability v2

本契约定义 Agent 与 Server 对 `semantic_dom_v1` 的显式选择。现有 CDP/v1 请求、观察算法和资料格式继续使用原接口。v2 首批运行范围为 Linux x86_64，要求编译 `experimental-browser-bidi`，并在本地配置中启用 `browser.enabled` 和 `browser.capability_v2`。

本地策略配置示例；将两个可执行文件路径替换为设备上的实际安装位置。此开关不安装浏览器，也不更改宿主的隔离策略。Firefox 需要允许其 helper 使用所需的非特权 user、mount、PID 和 network namespace。

```json
{
  "version": 1,
  "browser": {
    "enabled": true,
    "capability_v2": true,
    "executable": "/opt/google/chrome/chrome",
    "firefox_executable": "/opt/firefox/firefox"
  }
}
```

## 能力发现与打开

`device_info.capabilities.browser.version` 保持 `1`。新增的 `capabilities.browser.v2` 是独立声明，包含 `version: 2`、`available`、`reason`、`observation_profiles` 和 `backends`。每个后端条目包含 `id`、`browser_family`、`available`、`reason` 和 `stage: "candidate"`。候选可用表示配置、平台、文件与策略允许尝试；不表示已经完成协议握手。

`browser_open` 增加一个可选对象 `selection`：

```json
{
  "device_id": "test",
  "selection": {
    "version": 2,
    "observation_profile": "semantic_dom_v1",
    "backend": "bidi"
  },
  "url": "https://example.com/"
}
```

`selection` 必须且只能包含这三个字段；后端只能是 `cdp` 或 `bidi`。CDP 使用 `browser.executable`，BiDi 使用绝对路径配置 `browser.firefox_executable`。未提供 `selection` 的请求按 v1 执行；`null`、不完整选择、未知字段、未知版本/profile/后端均拒绝。没有自动选择或失败后换用其他后端。

Server 先验证选择结构，再检查设备的 v2 声明和目标后端，满足后才分配或执行变更操作。旧 Agent 缺少 v2 时返回 `browser_capability_unsupported`。Agent 独立重复验证，不信任 Server 已做检查。未知选择返回 `browser_selection_invalid`；配置、文件、平台或站点策略不满足时返回 `browser_backend_unavailable`。这些拒绝均发生在进程、临时资料和页面副作用之前。

打开成功返回原有 `session_id`、`tab_id`、`control_epoch`、`state`，并增加实际 `selection`、`capabilities` 和 `stage: "session_ready"`。选择在会话内固定；后续会话请求通过会话 ID 路由，不再接受选择参数；资料列举和删除单独接受 `selection`。核心能力在运行中丢失时暂停或关闭，不降级成 v1，也不更换后端。

## 会话操作

v2 复用 `browser_sessions`、`browser_tabs`、`browser_observe`、`browser_act`、`browser_wait`、`browser_screenshot` 和 `browser_control`，继续使用经过认证的 `browser_principal`、操作回执和控制 epoch。人工接口继续由 Server 绑定用户主体，并用 view、lease、epoch、sequence 和截图 frame 约束输入。人工输入内容不进入操作回执。

观察结果中的 `observation_profile` 为 `semantic_dom_v1`。`nodes` 包含 canonical `role`、`name`、`name_source`、`name_quality`、可选 `value` 和可选 `ref`，并保留控件的 `kind`、`tag`、`input_type`、敏感标记和有界 `record` 材料；记录条目不包含内部节点编号。`coverage` 明确遍历完成状态、变化和遗漏原因。`ref`、`next_cursor`、tab/frame 标识都是 Agent 分配的不透明标识，不接受原生协议 ID、脚本、选择器或坐标作为 AI 引用。输入回执的 `input_acknowledged` 只代表协议确认，`business_result_verified` 保持 `false`。

`fill` 使用原生文本编辑；原生日期、时间、颜色及范围控件使用 `select` 的固定表单设值路径。`scroll` 只滚动主文档，人工视口滚轮则按命中位置路由。

会话核心能力包括生命周期、文档身份、共同观察与输入、截图、JavaScript 弹窗、人工交接、导航策略和保守恢复。公开会话限制、支持动作和可选功能随打开结果返回。命名资料、上传和下载分别遵循 `browser.persistent_profiles`、`browser.uploads`、`browser.downloads` 三个本地开关，默认均关闭；`device_info` 和打开结果声明这些可选能力。命名资料被本地策略关闭时返回 `browser_profile_disabled`。原生 AX 和 DevTools 扩展仍不可用。关闭及崩溃恢复必须证明所属进程已结束，才能释放命名资料租约。

v1 与 v2 共用每设备 2 个会话的上限，每个 v2 会话最多 8 个标签页和 32 个 frame。截图为有界 viewport JPEG。人工视图租约为 120 秒；刷新截图或完成输入可续期，过期视图不能继续输入，返回 AI 必须显式确认披露页面。敏感输入只经已认证的人工接口传输，观察和回执不返回密码值。

人工文本输入核对所选标签页内的 DOM 焦点及逐层 frame 所有者，不依赖 headless 浏览器的窗口级 `document.hasFocus()`。BiDi 的截图坐标先按原生 frame 所有者和内容区几何换算，再在对应 context 发送原生输入；跨 frame 只接受可证明的正向、轴对齐映射，旋转和透视等几何在输入前拒绝。

弹窗可能使触发它的原生输入返回 `unknown`，会话随之暂停，原生命令仍由通道持有。客户端从 `browser_tabs` 获取当前 epoch 和 `dialog_id`，明确处理弹窗，再在待决输入结算后恢复并重新观察；不能重放触发弹窗的点击。输入执行前拒绝与已提交但结果不明分别保持 `not_sent` 和 `unknown`。

回执仅保存已有的有界会话标识及严格的三字段 `selection`。Agent、Server Rust 和 PostgreSQL 结算函数执行同一白名单；URL、标题、页面内容、输入文本和未知选择字段不进入恢复摘要。Server 的数据库函数契约须与服务二进制一起更新，启动时的契约哈希检查会拒绝不一致的组合。

## 命名资料与文件传输

`browser_open` 接受 `profile_name`。v1、v2 Chromium、v2 Firefox 的目录、资料 ID 和独占租约相互隔离；同一名称不能共享 Cookie、站点存储或浏览器数据库。v1 的资料 ID、元数据格式和默认请求保持不变。每个命名空间最多保留 16 个资料，未显式删除时持续保留。资料只属于创建它的认证 AI 主体，同名资料不能同时打开，使用中的资料不能删除。

列举 v2 资料时，向 `browser_profiles` 传入打开时的完整 `selection`；删除时，向 `browser_control` 传入 `action: "delete_profile"`、`profile_name` 和相同 `selection`。省略 `selection` 只访问 v1 资料。其它会话操作不接受 `selection`，以 `session_id` 确定后端。资料列举和删除不消耗任务配额。

上传沿用人工网页中的分块暂存和显式授权：批准的文件绑定 AI 主体、用户、会话、标签页、来源站点和一次使用次数。AI 的 `attach_file` 必须提供新观察中的 `ref` 和 `file_grant_id`。Agent 在共同输入租约内重新验证文档、主 frame、原生文件输入框、几何和站点，并在原生派发前持久化授权消耗。CDP 使用 `DOM.setFileInputFiles`，BiDi 使用 `input.setFiles`；调用者不能指定设备路径。结果不明时不恢复授权或自动重试。上传字节不进入模型观察、文件列表或操作回执。

下载只接收 HTTP 200 attachment 响应，必须有唯一的 `Content-Length`，且未使用压缩编码。单文件最多 32 MiB，全部文件及上传暂存共用 128 MiB 和 16 项配额，完成文件保留 24 小时。读取每块最多 64 KiB，并校验完整长度与 SHA-256。文件列表只返回元数据，实际字节通过认证人工文件接口读取。人工接管立即隐藏该会话的文件；显式返回并披露后才重新对 AI 可见。

Chrome 使用已经暂停的原始响应流。Firefox 由原生下载器处理同一响应；不重新请求 URL，因此保留原始 Cookie、POST 请求体和服务器的一次性操作语义。Firefox 的下载目录挂载在所属浏览器的私有 namespace 内，tmpfs 数据容量为 32 MiB，inode 上限为 128；Agent 仅通过继承的目录描述符读取完成文件，并拒绝路径逃逸、符号链接及非普通文件。每个 Firefox 会话同一时间只接收一个附件。

Firefox 下载开始及结束事件必须匹配已接纳的主 frame、navigation、URL 和 download ID。未知下载、无法归属的事件或清理失败会关闭所属会话。Firefox 没有可用的单文件取消路径，因此取消正在接收的附件会关闭该会话；其 `cancellation_scope` 为 `session`，网页在原有删除确认处说明这一影响。Chrome 的取消范围为单文件。取消确认只表示收到请求，客户端应继续核对文件列表；只有确认原生进程结束后，Firefox 才释放失败下载的配额。独立会话不受影响。

## 兼容与验证

兼容测试同时覆盖：旧请求到新 Agent、新请求到旧 Agent、未编译或未启用 v2、单个后端缺失、错误选择及跨主体会话。原有 v1 实测断言继续运行。v2 测试必须从公开 MCP 进入真实 Server/RPC/Agent，再驱动 Chrome 和 Firefox，验证页面观察、输入、导航、截图、弹窗、人工控制、回执恢复和清理；内部 adapter 测试不能替代整条链路的证据。

严格站点策略需要实际连接地址证据时，当前两个 v2 后端均拒绝打开。文档 HTTP 请求被拦截不等于没有 TCP 预连接。引用最终检查与原生输入派发之间仍有竞态；不能将输入确认解释为目标业务对象已经成功更新。

资料位于原有私有 browser 根目录的 `v2/chromium-temporary-v1` 或 `v2/firefox-temporary-v1`；命名资料保存在各自目录下的独立命名空间。旧程序不认识 `v2` 时返回 `browser_cleanup_pending`，拒绝恢复和打开，不会接管或删除目录。关闭最后一个 v2 会话不会移除这些空目录，直接替换成旧程序仍可能被拒绝。

### 回退前处理浏览器资料

回退会影响新格式资料和文件的可用性。先清点所有认证主体的 v2 资料与下载；需要保留新资料时继续使用支持它们的版本，并按既有私有数据备份流程处理。以下步骤适用于已明确决定退役相应 v2 资料和文件的设备，不能用来绕过 `browser_cleanup_pending`：

1. 使用支持 v2 的 Agent 关闭会话，并核对原生进程、待决下载及清理状态。清理未确定时停止回退。
2. 按完整 `selection` 列举并显式删除已确认退役的 v2 命名资料；通过原有文件接口删除已确认退役的下载。Firefox 的 `cancellation_scope` 元数据不属于旧文件格式，不能只处理资料目录而留下这些文件。
3. 停止 Agent，确认没有所属浏览器进程或有效所有权记录。从最深层开始，仅用非递归 `rmdir` 移除已核实为空的 v2 目录，最后移除 `v2` 本身。任何文件、符号链接或非空目录都使此步骤停止；不删除所有权记录或递归清空目录。
4. 使用旧版接受的本地配置启动旧 Agent；旧版配置不包含 `capability_v2` 和 `firefox_executable`。重新打开原有 v1 命名资料，验证资料 ID 和合成持久状态；实际业务数据按设备的验收流程核对。

Server 回退是独立步骤：停止并排空受影响的 Core，再用目标版本的迁移器离线安装其 Runtime 函数契约，执行目标 Core 的 `check-storage` 后才恢复服务。旧 Core 不接受新的 `selection` 参数。程序和 SQL 函数来自不同版本时不能继续启动；Agent 的资料清理不会修复 Server 契约不一致。

Server 的 `tests/core/test_browser_v2.py` 使用 `RDH_TEST_CLUSTER_AGENT`、`RDH_TEST_BROWSER_EXECUTABLE` 和 `RDH_TEST_FIREFOX_EXECUTABLE` 指定同一候选的 Agent、Chrome 与 Firefox。测试通过真实 Auth/Gate/Core/Agent 和独立 PostgreSQL 夹具执行；人工控制还经过 Business 网页会话认证。`test_browser_v2_files.py` 覆盖命名资料与文件；`test_browser_v2_readiness.py` 使用独立构建的固定旧版本和默认构建，验证升级/回退及公开会话、标签页、frame 限额。回退场景只退役测试自行创建的合成资料。

兼容测试还要求 `RDH_TEST_OLD_AGENT`、`RDH_TEST_DEFAULT_AGENT`、`RDH_TEST_OLD_CORE` 和 `RDH_TEST_OLD_MIGRATE`；未指定时跳过相应场景，发布验收须拒绝这些跳过结果。新旧源码使用不同 Cargo target 目录，避免把共享增量产物误记为旧版本证据。以上入口之外继续运行原有 v1 测试及共同页面语义测试。资源循环、受限性能和平台原生运行分别记录，不能从短用例通过推导为 D4 整体完成。
