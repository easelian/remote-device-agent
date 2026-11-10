# 浏览器能力架构

浏览器功能由设备 Agent、Server 的 Core/Gate/Business 和 AI 客户端共同提供。Agent 的本地策略须显式启用浏览器，客户端通过 `capabilities.browser.version = 1` 判断协议支持。操作方法见[浏览器指南](browser-zhCN.md)。

## 模块与职责

| 当前实现 | 源码位置 | 使用边界 |
| --- | --- | --- |
| 本地策略显式启用浏览器；Linux、macOS 与启用原生进程/私有存储功能的 Windows 构建共用运行时 | [browser/mod.rs](../src/browser/mod.rs)、[policy.rs](../src/policy.rs) | 默认关闭；预检发现可执行文件后，仍需在创建会话时验证启动结果 |
| 浏览器工作、控制和普通文件/进程操作分别受限，关闭等待实际工作结束 | [rpc.rs](../src/rpc.rs)、[browser/runtime.rs](../src/browser/runtime.rs) | 响应期限不等于动作取消；只有完成停止后才释放私有存储排他锁 |
| 原生所有权确认后再回收目录；Windows 独立子进程在 Agent 退出后保存 Job 完成记录 | [browser/process.rs](../src/browser/process.rs)、[windows/supervisor.rs](../src/browser/process/windows/supervisor.rs) | 不以 PID 消失或固定等待代替完成；可靠记录缺失时继续隔离，不重放页面操作 |
| Core 从已验证请求构造 AI 主体，人工通路使用独立的 Auth application grant | Server `apps/core/src/runtime.rs`、`apps/core/src/browser.rs`、`crates/runtime/src/application.rs` | 模型参数不能指定用户或所有者；接纳和内容披露都核对权限 |
| 会话保存控制代次、frame 身份与当前段元素引用；观察支持有界续读 | [browser/runtime.rs](../src/browser/runtime.rs)、[browser/page.rs](../src/browser/page.rs)、[observe.js](../src/browser/observe.js) | 旧引用和游标不能用于控制变化后的页面；观察不是网站原子快照 |
| 页面动作使用有限 CDP 命令与固定隔离执行函数 | [browser/cdp.rs](../src/browser/cdp.rs)、[browser/page.rs](../src/browser/page.rs) | 不开放任意脚本、原始 CDP 或系统桌面输入；已发送未知输入仍保留在途记录 |
| Web 提供同会话接管、会话找回与文件交付 | Server `apps/business/web/src/Browser.vue`、`BrowserSessions.vue`、`BrowserFiles.vue`、`BrowserUpload.vue` | 只有明确接管才开放人工输入；交回前由用户确认内容分享范围 |
| `browser_open`、`browser_act`、`browser_control` 复用操作回执和安全摘要 | [receipts.rs](../src/receipts.rs)；Server `apps/core/src/mcp_tools.rs`、`crates/runtime/src/types.rs` | 去重与恢复不等于网站事务恰好一次；摘要不保存页面或输入正文 |
| 截图通过 MCP image 块交付；文件字节通过逐块重新授权的人工通路交付 | Server `apps/core/src/mcp_tools.rs`、`apps/core/src/browser.rs`；[browser/files.rs](../src/browser/files.rs) | 图片和文件有各自预算；实际 AI 宿主渲染仍待验证 |
| 登录资料、文件暂存和运行目录属于设备凭据的私有命名空间 | [browser/profiles.rs](../src/browser/profiles.rs)、[browser/files.rs](../src/browser/files.rs)、[files/native.rs](../src/files/native.rs) | 文件工具排除私有目录；shell 权限和同一 OS 账号不受这项排除约束 |

## 控制和恢复

Core 从经过认证的请求构造操作主体，Agent 按主体隔离会话、登录资料和文件暂存。AI 操作与人工接管使用独立通道。控制代次变化后，旧元素引用、观察游标和在途披露请求失效。

浏览器动作复用设备操作回执。请求超时不代表网页动作已取消，恢复时查询原操作和会话状态。Agent 重启后按原生进程所有权和完成记录清理私有目录，不重放网页动作。

## 扩展和验证

修改公共行为时检查 Agent 操作契约、Core 的主体与结果披露、Business 人工接管页面，以及客户端工具结果格式。`tests/browser_session.rs` 提供原生浏览器集成场景，Server 的 `tests/core/` 和 `tests/auth/` 提供 MCP、人工接管、文件及权限场景。平台、浏览器版本和实际 AI 客户端需要各自验证。
