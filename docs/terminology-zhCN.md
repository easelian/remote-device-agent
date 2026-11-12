# Remote Device Hub 统一术语

Remote Device Hub（RDH）是 Easelian 的产品名称。Remote Device Server 是服务端实现；Remote Device Agent（简称 Agent）是在设备上运行的原生程序，可执行文件名为 `rdh`。客户端插件或扩展负责将 AI 客户端接入 RDH，并提供宿主集成与操作指引。

| 术语 | 含义 |
| --- | --- |
| Auth | 客户身份、登录、OIDC 与 MCP OAuth。 |
| Gate | 设备配对、长期设备凭据、管理授权、分配与 MCP 路由。 |
| Core | Agent WSS 连接、执行准入、实际消费与操作回执。 |
| Business | 产品网站、网站会话、商业业务、支持与版本化权益导出。Web 按上下文指网站或 Business 服务。 |
| Admin | 工作人员身份与管理，与客户身份分开。 |
| 配对 / 绑定 / 解绑 | 配对是批准流程；绑定是配对后保存的设备身份和配置；解绑撤销设备访问。 |
| 授权 / 本地策略 | 服务端检查账户和设备访问授权；Agent 本地策略限制设备操作；客户端批准提示是宿主额外的控制。 |
| 文件根目录 / 共享目录 | 文件工具使用的具名路径边界；Shell 命令以 Agent 操作系统账户权限执行，文件根目录不构成命令沙箱。 |
| 权益 / 调用额度 / 实际消费 | 权益声明可用权利和额度窗口；调用额度是调用次数预算；实际消费由 Core 维护。Agent 的资源限制另行计算。 |
| 操作 / 回执 / 受管任务 | 操作使用稳定操作 ID 查询结果，回执记录执行事实；进程和搜索任务可在启动请求结束后继续运行，使用各自的任务 ID。超时可能留下未知结果。 |

客户端通过 Auth 授权，向 Gate 发起 MCP 调用，Gate 将已授权执行路由到 Core。Agent 用长期设备凭据向 Gate 发现并获取分配，再用短期、一次性准入票据主动通过 WSS 连接 Core。设备凭据、准入票据、MCP OAuth 令牌和网站会话各有用途。

浏览器会话为可选择启用的预览功能。浏览器资料保存浏览器登录状态，与 Agent 绑定、RDH 账户不同；人工接管是浏览器会话内明确发起的控制交接。

术语以 Server 的[统一术语](https://github.com/easelian/remote-device-server/blob/main/docs/terminology.md)和[五服务架构](https://github.com/easelian/remote-device-server/blob/main/docs/product-architecture.md)为准。用户说明使用产品名称，职责、凭据和网络路径说明使用具体服务名；API、配置键和运维命令使用实际名称。
