# 浏览器能力架构

浏览器功能由设备 Agent、Server 的 Core/Gate/Business 和 AI 客户端共同提供。Agent 的本地策略须显式启用浏览器，客户端通过 `capabilities.browser.version = 1` 判断协议支持。操作方法见[浏览器指南](browser-zhCN.md)。

## 模块与职责

| 当前实现 | 源码位置 | 使用边界 |
| --- | --- | --- |
| 本地策略显式启用浏览器；Linux、macOS 与启用原生进程/私有存储功能的 Windows 构建共用运行时 | [browser/mod.rs](../src/browser/mod.rs)、[policy.rs](../src/policy.rs) | 默认关闭；预检发现可执行文件后，仍需在创建会话时验证启动结果 |
| 浏览器工作、控制和普通文件/进程操作分别受限，关闭等待实际工作结束 | [rpc.rs](../src/rpc.rs)、[browser/runtime.rs](../src/browser/runtime.rs) | 响应期限不等于动作取消；只有完成停止后才释放私有存储排他锁 |
| 原生所有权确认后再回收目录；Windows 独立子进程在 Agent 退出后保存 Job 完成记录 | [browser/process.rs](../src/browser/process.rs)、[windows/supervisor.rs](../src/browser/process/windows/supervisor.rs) | 不以 PID 消失或固定等待代替完成；可靠记录缺失时继续隔离，不重放页面操作 |
| Core 从已验证请求构造 AI 主体，人工通路使用独立的 Auth application grant | RDH 服务端（不在本仓库） | 模型参数不能指定用户或所有者；接纳和内容披露都核对权限 |
| 会话保存控制代次、frame 身份与当前段元素引用；观察支持有界续读 | [browser/runtime.rs](../src/browser/runtime.rs)、[browser/page.rs](../src/browser/page.rs)、[observe.js](../src/browser/observe.js) | 旧引用和游标不能用于控制变化后的页面；观察不是网站原子快照 |
| 传输层与文件交付共用控制代次及暂停、恢复状态 | [browser/control.rs](../src/browser/control.rs) | 控制变化与命令入队前的最后一次授权检查串行执行 |
| 页面动作使用有限 CDP 命令与固定隔离执行函数 | [browser/cdp.rs](../src/browser/cdp.rs)、[browser/page.rs](../src/browser/page.rs) | 不开放任意脚本、原始 CDP 或系统桌面输入；已发送未知输入仍保留在途记录 |
| Web 提供同会话接管、会话找回与文件交付 | RDH 服务端（不在本仓库） | 只有明确接管才开放人工输入；交回前由用户确认内容分享范围 |
| `browser_open`、`browser_act`、`browser_control` 复用操作回执和安全摘要 | [receipts.rs](../src/receipts.rs)；RDH 服务端（不在本仓库） | 去重与恢复不等于网站事务恰好一次；摘要不保存页面或输入正文 |
| 截图通过 MCP image 块交付；文件字节通过逐块重新授权的人工通路交付 | RDH 服务端（不在本仓库）；[browser/files.rs](../src/browser/files.rs) | 图片和文件有各自预算；实际 AI 宿主渲染仍待验证 |
| 登录资料、文件暂存和运行目录属于设备凭据的私有命名空间 | [browser/profiles.rs](../src/browser/profiles.rs)、[browser/files.rs](../src/browser/files.rs)、[files/native.rs](../src/files/native.rs) | 文件工具排除私有目录；shell 权限和同一 OS 账号不受这项排除约束 |

## 控制和恢复

Core 从经过认证的请求构造操作主体，Agent 按主体隔离会话、登录资料和文件暂存。AI 操作与人工接管使用独立通道。控制代次变化后，旧元素引用、观察游标和在途披露请求失效。

浏览器动作复用设备操作回执。请求超时不代表网页动作已取消，恢复时查询原操作和会话状态。没有发出滚动、输入或导航命令的失败报告为未发送，不改变控制代次；发出之后的失败按未知结果处理，会话暂停并进入新的控制代次。Agent 重启后按原生进程所有权和完成记录清理私有目录，不重放网页动作。

## 原生后端演进（计划）

当前可用后端仍是 Chromium/CDP；`control.rs` 已独立于 CDP 模块，Firefox/BiDi 的启动、传输和页面适配尚未实现。后续保持 Rust 原生实现，协议适配代码随 Agent 二进制交付。运行时使用设备上已安装且经过验证的浏览器，不要求用户安装 Node、Puppeteer、ChromeDriver 或 GeckoDriver。

Chromium 继续使用私有调试管道。Firefox 计划由 Agent 以独立资料目录启动，通过浏览器原生 WebDriver BiDi WebSocket 建立会话；Mozilla 的[直接连接说明](https://developer.mozilla.org/en-US/docs/Web/WebDriver/How_to/Create_BiDi_connection)介绍了自动分配调试端口和创建 BiDi 会话的方法。端口发现、回环地址限制、进程所有权、启动失败和异常退出后的清理，均需由原生启动器验证，不能只凭 WebSocket 连接成功就宣告浏览器可用。

按以下顺序推进：

1. 增加 Firefox 启动和 BiDi 传输，验证命令与事件分流、有界队列、超时后的迟到响应，以及控制变化时停止接纳输入。
2. 建立浏览器无关的页面身份模型，适配 frame、文档、执行环境和元素引用。CDP 的节点 ID 与 BiDi 的节点引用不能直接互换；导航、frame 分离和人工接管后仍需校验引用有效性。
3. 接入导航、观察、点击、填写、截图和人工接管，复用现有站点规则、操作回执、未知结果和披露规则；上传下载等尚未满足契约的能力保持不可用。
4. 按浏览器版本和平台执行同一组场景，再通过能力查询报告已验证的操作。MCP 契约与 Server 同步更新后，才对客户端开放新的后端能力。

## 扩展和验证

修改公共行为时检查 Agent 操作契约、Core 的主体与结果披露、Business 人工接管页面，以及客户端工具结果格式。`tests/browser_session.rs` 提供原生浏览器集成场景，Server 的 `tests/core/` 和 `tests/auth/` 提供 MCP、人工接管、文件及权限场景。平台、浏览器版本和实际 AI 客户端需要各自验证。
