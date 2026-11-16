# Browser capability v2

本契约定义 Agent 与 Server 对 `semantic_dom_v1` 的显式选择。现有 CDP/v1 请求、观察算法和资料格式继续使用原接口。v2 首批运行范围为 Linux x86_64，要求编译 `experimental-browser-bidi`，并在本地配置中启用 `browser.enabled` 和 `browser.capability_v2`。

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

打开成功返回原有 `session_id`、`tab_id`、`control_epoch`、`state`，并增加实际 `selection`、`capabilities` 和 `stage: "session_ready"`。选择在会话内固定；后续请求通过会话 ID 路由，不再接受选择参数。核心能力在运行中丢失时暂停或关闭，不降级成 v1，也不更换后端。

## 会话操作

v2 复用 `browser_sessions`、`browser_tabs`、`browser_observe`、`browser_act`、`browser_wait`、`browser_screenshot` 和 `browser_control`，继续使用经过认证的 `browser_principal`、操作回执和控制 epoch。人工接口继续由 Server 绑定用户主体，并用 view、lease、epoch、sequence 和截图 frame 约束输入。人工输入内容不进入操作回执。

观察结果中的 `observation_profile` 为 `semantic_dom_v1`。`nodes` 包含 canonical `role`、`name`、`name_source`、`name_quality`、可选 `value` 和可选 `ref`；`coverage` 明确遍历完成状态、变化和遗漏原因。`ref`、`next_cursor`、tab/frame 标识都是 Agent 分配的不透明标识，不接受原生协议 ID、脚本、选择器或坐标作为 AI 引用。输入回执的 `input_acknowledged` 只代表协议确认，`business_result_verified` 保持 `false`。

会话核心能力包括生命周期、文档身份、共同观察与输入、截图、JavaScript 弹窗、人工交接、导航策略和保守恢复。公开会话限制、支持动作和可选功能随打开结果返回。命名资料、上传下载、原生 AX 和 DevTools 扩展在此阶段均为不可用；请求 `profile_name` 或相关文件动作时，在副作用前返回 `browser_feature_unsupported`。临时资料按家族存放，和 v1 目录分开，关闭及崩溃恢复仍须证明所属进程已结束。

## 兼容与验证

兼容测试同时覆盖：旧请求到新 Agent、新请求到旧 Agent、未编译或未启用 v2、单个后端缺失、错误选择及跨主体会话。原有 v1 实测断言继续运行。v2 测试必须从公开 MCP 进入真实 Server/RPC/Agent，再驱动 Chrome 和 Firefox，验证页面观察、输入、导航、截图、弹窗、人工控制、回执恢复和清理；内部 adapter 测试不能替代整条链路的证据。

严格站点策略需要实际连接地址证据时，当前两个 v2 后端均拒绝打开。文档 HTTP 请求被拦截不等于没有 TCP 预连接。引用最终检查与原生输入派发之间仍有竞态；不能将输入确认解释为目标业务对象已经成功更新。
