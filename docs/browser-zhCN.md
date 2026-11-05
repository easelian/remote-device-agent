# 在远程设备上操作网页

[English](browser.md) | 简体中文

通过已连接 RDH 的 AI 客户端，可以在已绑定的 Linux、macOS 或 Windows 设备上读取和操作网页，在同一个浏览器中人工登录，并取得受支持的文件。本指南适用于报告 `capabilities.browser.version = 1` 的 Agent，以及配套的 Rust Hub/MCP 和 Business 网页构建。平台实现与原生验证范围见[方案第 17.14 节](browser-plan-zhCN.md#1714-异常退出恢复启动修正与跨平台联调2026-11-05)。这些浏览器改动尚未发布或部署。

服务端需将 Core 的 `http.browser_user_url` 设为用户网站的 origin，在 Gate 的网页登录动作列表允许 `browser:control`，并应用包含 `device-browser` 用途的配套 Auth application-access 契约。Server 示例配置已包含这些设置；构建 worktree 不会修改已部署服务。

## 启用浏览器

远程设备需要能以 Agent 所属操作系统账号、在启用沙箱的条件下运行的 Chromium 可执行文件。Agent 使用独立的浏览器资料目录和私有协议管道，不接管日常浏览器。共用运行时支持 Linux 和 macOS；Windows 构建需要原生进程及私有存储功能，默认构建已启用。原生验证覆盖 Linux x64、macOS ARM64 和 Windows 11 ARM64，其他架构与浏览器安装包仍需各自运行证据。

1. 正常运行候选 `rdh`，按需完成设备绑定。终端会显示 `AI browser sessions` 和确切的 `Local browser policy` 路径；已绑定设备继续使用原身份。
2. 停止 Agent，打开这个策略文件；文件不存在时再创建。保留已有配置，并添加下方的 `browser` 对象。文件必须属于 Agent 用户：Linux/macOS 使用 `0600`，Windows 使用关闭继承的私有 ACL；不接受符号链接或 Windows 重解析路径，详见[本地策略文件](hardening.md#policy-file)。
3. 按需将 `executable` 设为已安装浏览器的绝对可执行路径；省略时按下表自动查找。找到文件不代表启动包装器、依赖和沙箱已经验证；首次创建会话时才检查实际启动结果。
4. 重新运行同一个 `rdh`。设备页面会显示浏览器未启用、不支持、未安装或已启用；AI 也可以读取 `device_info.capabilities.browser`。浏览器权限独立于文件和 shell 权限，不需要为此开启 shell 或通用文件写入。

下面的完整策略适用于没有其他本地设置的情况。三个可选功能均关闭，需要时分别改为 `true`；已有策略文件只合并 `browser` 字段，不覆盖其他设置。

```json
{
  "version": 1,
  "browser": {
    "enabled": true,
    "persistent_profiles": false,
    "downloads": false,
    "uploads": false
  }
}
```

自动查找按下列顺序检查文件。显式配置 `executable` 时应填写实际绝对路径，不使用环境变量占位符。

| 平台 | 自动查找位置 |
| --- | --- |
| Linux | `/usr/bin/chromium`、`/usr/bin/chromium-browser`、`/usr/bin/google-chrome`、`/opt/google/chrome/chrome` |
| macOS | `/Applications` 下的 Chrome、Edge、Chromium、Chrome for Testing 应用可执行文件 |
| Windows | 依次在 `ProgramFiles`、`ProgramFiles(x86)`、`LOCALAPPDATA` 下检查 Chrome、Edge、Chromium |

macOS 保留登录 Cookie 需要浏览器能正常访问用户已解锁的钥匙串。资料持久化用例在已登录用户的 GUI 启动会话中通过；由 SSH 启动时，站点存储保留，但 Cookie 丢失，浏览器报告加密不可用。需要保留登录时，应在相应的已登录用户会话中运行 Agent；Agent 不解锁钥匙串，也不绕过浏览器加密。

以 macOS 用户级 GUI 后台任务启动时，连接局域网 Hub 还需要本地网络权限。在“系统设置 → 隐私与安全性 → 本地网络”中允许对应的 Agent；更换构建或启动入口后重新核对。权限拒绝可能表现为 `No route to host`。参见 [Apple 本地网络权限说明](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)。

Windows 上过深的自定义配置目录可能超出浏览器派生文件的路径限制。测试中 Edge 报告 `The filename or extension is too long (0xCE)`，且未通过资料保留检查；同一候选改用较短的本机目录后通过。使用较短的本机配置位置，并在正常关闭、重新打开命名资料后核对登录状态。Agent 的启动检查尚不能发现浏览器所有存储路径限制。

如果设置了 `allowed_operations`，还需允许所需的浏览器工具；人工网页使用内部操作 `browser_human`。Agent 不会自动安装浏览器或开启这些权限。浏览器请求使用远程设备的网络，共享文件目录不限制可访问的网站。

## 开始任务并核对结果

向 AI 指明设备、网站和任务，例如：“在测试设备上打开开发站点，读取待处理记录，并说明覆盖了哪些页面。”设备名称不唯一时，先确定目标设备。

AI 通过 `browser_open` 打开会话，使用 `browser_observe` 读取页面，再以返回的元素引用调用 `browser_act`。操作后通过 `browser_wait` 等待预期状态，并重新观察结果。当前支持普通文本框、原生单选下拉框、复选框、链接、按钮、页面滚动、标签页及受支持的 frame。密码和验证码应在人工网页输入，不放进 AI 工具参数。

长页面返回 `next_cursor` 时继续读取，直到没有后续游标，再检查 `coverage.sequence_complete` 与 `omitted`。未读全不能证明记录不存在。每次续读会替换此前的元素引用；检测到文档变化、导航、执行动作、控制权改变、开始新读取或五分钟未继续时，旧游标失效。读取器不会把持续变化的网站冻结为同一时刻的快照。

集成客户端调用 `browser_open`、`browser_act` 和 `browser_control` 前，应通过 `prepare_operation` 取得操作 ID；这个 ID 只能用于原请求的恢复。浏览器确认收到输入不等于网站业务成功，仍需检查保存结果。超时后效果可能未知，应查询操作和会话状态，不重复提交。

## 人工登录与交回 AI

打开 AI 返回的 `human_url`，或从“设备 → 查看浏览器会话 → 打开远程浏览器”进入。页面要求当前 RDH 登录和设备访问权，仅打开页面不会取得控制权。

点击“接管 / 重新接管”后，AI 的读取和动作暂停。先点击远程页面中的文本框，再使用受保护的文本输入完成支持的登录步骤。输入只作用于这个浏览器视口；操作系统对话框、硬件安全密钥、生物识别窗口和自动解决 CAPTCHA 不受支持。

交回前检查网页，移除不应披露的内容，确认页面及相关文件元数据可以分享，再点击“交回 AI”。回到原 AI 对话说“继续”；交回操作不会自动唤醒已经结束的对话。AI 应查询原会话并取得新引用。

输入结果不确定时，页面会停止后续输入。先点击“查询浏览器状态”，在允许时明确重新接管，不重复不确定的输入。“关闭远程浏览器”关闭所属浏览器，不能撤回网站已经收到的请求。

## 保留登录与传输文件

将 `persistent_profiles` 设为 `true` 后，可以在 `browser_open.profile_name` 指定名称。命名资料绑定当前设备凭据和 AI 授权；正常关闭会请求浏览器写入 Cookie 与站点存储，持久化仍依赖上述浏览器和操作系统加密条件。其他 AI 授权不能直接复用。通过 `browser_profiles` 查看资料，以 `browser_control` 的 `delete_profile` 动作删除未使用的资料。占用中的资料不能重复打开或删除。

启用 `downloads` 后，受支持的 HTTP 文档附件会出现在 `browser_files` 和“浏览器文件”页面中，可在登录后的页面保存到本地。附件必须明确声明长度、状态码为 200、没有内容压缩；Blob 和无长度流式下载尚不支持。文件字节不返回给 AI。

启用 `uploads` 后，先取得人工控制权，选择本地文件，并明确批准将文件提供给页面显示的站点。页面暂存文件并核验摘要。交回后，AI 用返回的 `file_grant_id` 和新的主页面文件控件引用选择文件。授权仅使用一次，未使用时十分钟到期，选择结果未知也不恢复授权。选择文件可能立即上传，之后提交表单是另一个动作。当前不接受任意远程文件路径或 frame 内文件控件。

## 限制与清理

- 最多两个会话，每个会话最多八个标签页。十分钟无活动后关闭；设备断连后暂停，超过 Agent 的五分钟离线宽限后关闭。
- 临时资料在所属浏览器进程退出后清理。命名资料保留到明确删除，每个设备绑定最多十六份；浏览器资料、缓存与内存没有硬配额。
- 上传和下载共用暂存预算：单文件 32 MiB、合计 128 MiB、最多十六份。完成的下载保留 24 小时；已经使用的上传副本最迟保留到创建后 24 小时。关闭会话不删除已完成下载。
- 解绑确认后，待所属工作停止，再删除这个绑定的私有浏览器资料和暂存文件。无法确认清理时保留绑定记录及待处理状态，供重试；不影响其他绑定或已经保存到本地的文件。
- Agent 重启会丢弃未完成下载和全部上传副本，不恢复旧会话或重放网站动作。POSIX 恢复会核验原进程组。Windows 的独立回收子进程会在 Agent 异常退出后终止浏览器 Job，确认其中的活动进程归零并保存完成记录；下一次浏览器请求据此清理旧运行目录，允许重新打开资料。清理尚未完成时返回 `browser_cleanup_pending`。如果 Agent 和回收子进程都被结束，或可靠完成记录缺失，资料仍保持隔离，复用和解绑删除会被拒绝；重启系统本身不能补足这项证明。
- `.rdh-browser` 是 Agent 私有存储，文件读取、写入、列表、搜索和共享根配置均排除这个目录。名称匹配忽略大小写，Windows 还核对规范目录路径，拒绝短文件名别名。Shell 权限仍代表 Agent 所属操作系统账号的权限，这项排除不是 shell 沙箱，也不能隔离同一系统账号的直接访问。

源码、证据和剩余发布门槛见[浏览器方案与验收计划](browser-plan-zhCN.md)。实际 AI 宿主展示、其余 OS/架构组合、负载性能和人工体验验收仍待完成。首次权限配置和浏览器安装目前需要在远程设备本地完成；网页已提供状态说明和已有会话入口。
