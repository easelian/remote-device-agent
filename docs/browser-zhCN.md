# 在远程设备上操作网页

[English](browser.md) | 简体中文

通过已连接 RDH 的 AI 客户端，可以在已绑定的 Linux、macOS 或 Windows 设备上读取和操作网页，在同一个浏览器中人工登录，并取得受支持的文件。浏览器能力为预览功能，默认关闭。本指南适用于报告 `capabilities.browser.version = 1` 的 Agent，以及配套的 Gate、Core 和 Business 网页构建。模块职责和源码位置见[浏览器实现现状](browser-implementation-zhCN.md)。

服务端需将 Core 的 `http.browser_user_url` 设为用户网站的 origin，在 Gate 的网页登录动作列表允许 `browser:control`，并应用包含 `device-browser` 用途的配套 Auth application-access 契约。Server 示例配置已包含这些设置；发布 Agent 不会部署配套服务。

浏览器会话目前只在 Linux x86_64 上、以 `experimental-browser-bidi` 特性构建的 Agent 中运行，本地策略启用 `enabled` 即可；策略中把 `capability_v2` 设为 `false` 会关闭会话运行时，这时设备没有浏览器操作。其他平台和构建的 Agent 在 `device_info.capabilities.browser` 中报告 `available: false` 和 `reason: "platform_or_build_unsupported"`，不提供浏览器操作。本指南中 macOS 和 Windows 的安装、加密与沙箱说明，对应这些平台上已有的浏览器存储和进程监督代码；会话运行时移植到这些平台之后，它们才适用。实现细节见[浏览器实现现状](browser-implementation-zhCN.md)。

## 启用浏览器

远程设备需要能以 Agent 所属操作系统账号、在启用沙箱的条件下运行的 Chromium 可执行文件。Agent 使用独立的浏览器资料目录和私有协议管道，不接管日常浏览器。共用运行时支持 Linux 和 macOS；Windows 构建需要原生进程及私有存储功能，默认构建已启用。原生验证覆盖 Linux x64、macOS ARM64 和 Windows 11 ARM64，其他架构与浏览器安装包仍需各自运行证据。

1. 正常运行候选 `rdh`，按需完成设备绑定。终端会显示 `AI browser sessions` 和确切的 `Local browser policy` 路径；已绑定设备继续使用原身份。
2. 停止 Agent，打开这个策略文件；文件不存在时再创建。保留已有配置，并添加下方的 `browser` 对象。文件必须属于 Agent 用户：Linux/macOS 使用 `0600`，Windows 使用关闭继承的私有 ACL；不接受符号链接或 Windows 重解析路径，详见[本地策略文件](hardening.md#policy-file)。
3. 按需将 `executable` 设为已安装浏览器的绝对可执行路径；省略时按下表自动查找。找到文件不代表启动包装器、依赖和沙箱已经验证；首次创建会话时才检查实际启动结果。 设备必须经代理才能上网时，再加上 `"proxy": {"server": "http://主机:端口"}`：浏览器不读取系统或环境变量里的代理设置。`server` 也可以是 `socks5://主机:端口`，不能带用户名和密码；`bypass` 列出直接连接的主机。
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

## 限定可以打开的站点

默认情况下，浏览器会话可以打开任何公网站点、内网站点和设备本机上的站点。云元数据服务的地址和链路本地地址（`169.254.0.0/16`、`fe80::/10`）会被拒绝：网页没有正常理由去打开它们，而有些云会在那里发放凭证。要改变这些规则，在策略文件的 `browser` 下加一个 `sites` 对象：

```json
{
  "version": 1,
  "browser": {
    "enabled": true,
    "sites": {
      "public": "allow",
      "allow": ["*.example.org", "intranet.example.com"],
      "deny": ["*.bank.example"],
      "private_networks": "allow",
      "private_allow": ["localhost:3000", "wiki.corp", "192.168.10.0/24"]
    }
  }
}
```

| 字段 | 默认 | 含义 |
| --- | --- | --- |
| `public` | `allow` | 没有被任何名单提到的公网站点如何处置。设为 `deny` 后，只有 `allow` 里的站点能打开。 |
| `allow` | 空 | `public` 为 `deny` 时可以打开的公网站点。它不放行内网地址。 |
| `deny` | 空 | 永远拒绝的站点，优先于其他所有字段。 |
| `private_networks` | `allow` | 内网和设备本机如何处置。`deny` 表示只有 `private_allow` 里的主机能打开；目前设为 `deny` 的设备不提供浏览器会话，原因见下文的限度。 |
| `private_allow` | 空 | 不受 `private_networks` 限制的主机，同时也豁免对元数据和链路本地地址的拒绝。 |

每一项可以是主机名（`example.com`）、某个名字之下的所有主机（`*.example.com`，不含 `example.com` 自身）、地址（`192.168.1.10`、`[fd00::1]`）或网段（`10.0.0.0/8`）。网段只匹配直接写在 URL 里的地址，不匹配解析到该网段的主机名；要按名字放行一个内网站点，就写这个名字。每一项可以带端口（`localhost:3000`），不带端口时匹配所有端口。不写协议和路径。每个名单最多 256 项；有读不懂的项时 Agent 拒绝启动。Agent 启动时会打印当前生效的规则。

规则管的是页面导航：AI 给出的地址、链接、表单、重定向、frame 和新标签页。被拒的地址向 AI 报告为 `browser_site_denied`；标签页自身的导航被拒时，它会带上 `blocked: "site_policy"`。人工接管会话时受同样的规则约束。

依赖它之前要了解它的限度：

- 它不管页面自己发出的请求，例如图片、脚本和 `fetch`。Chrome 142 及以上会自己拒绝公网页面对内网地址的这类请求，更早的版本不会。
- 主机名按 Agent 查到的地址来判定。浏览器会再查一次，所以对两者给出不同答案的名字拦不住。`private_networks` 设为 `deny` 时，这个缺口无法容忍，而目前的会话运行时还不能核对浏览器实际连上的地址，所以 Agent 在这种策略下不提供浏览器会话，`device_info.capabilities.browser` 报告 `reason: "browser_policy_unsupported"`。
- 默认设置下，被网页误导的 AI 仍然到得了你的内网。设备开了 shell 权限时，AI 不经过浏览器也到得了。

## 决定 shell 与浏览器能否同时提供

shell 权限和浏览器会话是两个独立的开关。设备两者都开时，网页上的文字可能误导 AI 去执行命令。不在沙箱里的命令以 Agent 用户的身份运行：它能读到浏览器会话保存的登录数据，能修改这个策略文件，也能不经站点规则直接访问你的网络。

`browser` 对象里的 `shell_coexistence` 表示你接受哪一种：

| 取值 | 含义 |
| --- | --- |
| `allow`（默认） | shell 和浏览器会话同时提供。现有设备没有任何变化。 |
| `sandboxed_only` | 只有 shell 命令运行在[沙箱](hardening.md#shell-sandbox)里时，才在有 shell 的设备上提供浏览器会话。沙箱会对命令隐藏浏览器的存储和策略文件。 |
| `deny` | 设备有 shell 时不提供浏览器会话。 |

条件不满足时，shell 照常可用，浏览器报告自己不可用，原因是 `shell_not_sandboxed` 或 `shell_enabled`。没有 shell 的设备不受影响。沙箱只在 Linux 上有，所以在 macOS 和 Windows 上 `sandboxed_only` 的效果等同 `deny`。

两者都开时，Agent 启动会打印这意味着什么。`device_info.capabilities.browser.shell` 报告 shell 是否开着、是否在沙箱里，以及当前生效的取值。

要了解它的限度。取 `allow` 时，没有任何东西阻止被误导的 AI 执行命令。`sandboxed_only` 让命令够不到浏览器的存储和策略；`sandbox.network` 为 `true` 时，命令仍然能访问你的网络。

## 开始任务并核对结果

向 AI 指明设备、网站和任务，例如：“在测试设备上打开开发站点，读取待处理记录，并说明覆盖了哪些页面。”设备名称不唯一时，先确定目标设备。

客户端列出 `browser_guidance` 时，可以按设备或现有会话请求 `open`、`observe`、`recover`、`handoff`、`files` 中的一个主题。指引根据当前明确能力提供示例；占位值按 `bindings` 从本次认证结果取得。读取指引不打开页面或授权动作，实际调用仍独立检查状态和权限。

AI 通过三个操作使用浏览器。`browser_do` 执行一串步骤，例如打开页面、点击、填写、等待和关闭会话；`browser_read` 读取页面、标签页、会话或诊断信息；`browser_see` 取得当前视口的截图。参数、回复格式和每种失败的处理方法见[面向 AI 的浏览器接口](browser-ai-interface-zhCN.md)。

此前的 `browser_open`、`browser_observe`、`browser_act`、`browser_control` 等操作已经删除，Agent 把这些名称当作未知操作拒绝。本地策略的 `allowed_operations` 中如果还列着其中某些名称，策略文件照常加载，但这些名称不再允许任何操作，也不代表取代它们的新操作：要使用浏览器，需要在列表里写上 `browser_do`、`browser_read`、`browser_see` 中所需的名称，其中 `browser_do` 必须与 `browser_read` 一起允许。

当前支持普通文本框、原生单选下拉框、复选框、链接、按钮、页面滚动、标签页及受支持的 frame。日期、时间、颜色和滑块按值填写，值须符合该字段自身的格式，例如 `2026-03-04`、`09:30`、`#ff8800`、`7`。密码框不能由 AI 填写：这一步以 `browser_sensitive_input` 失败，文本不会发到页面。密码和验证码应在人工网页输入，不放进 AI 工具参数。

步骤按名称指定元素，例如“`Order C1` 那一行的 `Approve`”。描述对应多个元素时，回复列出候选，这一步不执行。每一步的回复说明页面的哪一部分发生了变化；某一步失败后，其后的步骤不再执行。一步的结果不明时，回复会标明，页面可能已经收到输入：先用 `browser_read` 确认页面状态，不要直接重复这一步。

读取可以只取页面的一部分，用 `find` 查找含有某段文字的各处，或用 `filter` 只读控件。长页面没有读完时，回复给出继续读取的方法；没有读全不能证明记录不存在。页面可能在读取期间发生变化，读取器不会把持续变化的网站冻结为同一时刻的快照。

Canvas 绘制的控件和 closed Shadow DOM 的内部节点不能按名称指定，目前也不能按截图坐标点击。遇到这类控件，或遇到需要本人验证的内容时，交给用户处理。

`browser_do` 是写操作，需要操作回执。集成客户端可在调用前用 `prepare_operation` 预留并保存操作 ID，以便响应丢失时查询同一次操作。省略 ID 时 Core 会自动准备。浏览器确认收到输入后，仍需检查网站上的业务结果。

## 人工登录与交回 AI

打开 AI 返回的 `human_url`，或从“设备 → 查看浏览器会话 → 打开远程浏览器”进入。页面要求当前 RDH 登录和设备访问权，仅打开页面不会取得控制权。

点击“接管 / 重新接管”后，AI 的读取和动作暂停。先点击远程页面中的文本框，再使用受保护的文本输入完成支持的登录步骤。输入只作用于这个浏览器视口；操作系统对话框、硬件安全密钥、生物识别窗口和自动解决 CAPTCHA 不受支持。

交回前检查网页，移除不应披露的内容，确认页面及相关文件元数据可以分享，再点击“交回 AI”。回到原 AI 对话说“继续”；交回操作不会自动唤醒已经结束的对话。AI 应查询原会话并取得新引用。

输入结果不确定时，页面会停止后续输入。先点击“查询浏览器状态”，在允许时明确重新接管，不重复不确定的输入。“关闭远程浏览器”关闭所属浏览器，不能撤回网站已经收到的请求。

## 保留登录与传输文件

命名资料目前不可用：`open` 步骤不能指定资料，每个会话都使用临时资料，浏览器进程退出后清理。`persistent_profiles` 暂时没有效果。此前版本用旧操作创建的命名资料仍保留在设备的私有浏览器存储中，Agent 不再列出或删除它们；解绑设备时，它们随这个绑定的浏览器数据一起删除。

启用 `downloads` 后，受支持的 HTTP 文档附件会出现在 `browser_files` 和“浏览器文件”页面中，可在登录后的页面保存到本地。附件必须明确声明长度、状态码为 200、没有内容压缩；Blob 和无长度流式下载尚不支持。文件字节不返回给 AI。

启用 `uploads` 且设备声明 `file_authorization` 版本 1 可用时，通过 `files_url` 或会话列表选择“批准文件使用”。确认目标站点和本地文件，批准一次选择。AI 保留控制，从 `browser_files` 取得 `file_grant_id`，用 `browser_do` 的 `upload` 步骤把文件交给当前主文档的文件输入框。授权创建后十分钟到期，选择结果未知也不恢复次数。选择文件可能立即上传，之后提交表单是另一个动作。旧版租约上传面板在已接管的会话中仍可使用。

## 限制与清理

- 最多两个会话，每个会话最多八个标签页。十分钟无活动后关闭；设备断连后暂停，超过 Agent 的五分钟离线宽限后关闭。
- 临时资料在所属浏览器进程退出后清理。命名资料保留到明确删除，每个设备绑定最多十六份；浏览器资料、缓存与内存没有硬配额。
- 上传和下载共用暂存预算：单文件 32 MiB、合计 128 MiB、最多十六份。完成的下载保留 24 小时；已经使用的上传副本最迟保留到创建后 24 小时。关闭会话不删除已完成下载。
- 解绑确认后，待所属工作停止，再删除这个绑定的私有浏览器资料和暂存文件。无法确认清理时保留绑定记录及待处理状态，供重试；不影响其他绑定或已经保存到本地的文件。
- Agent 重启会丢弃未完成下载和全部上传副本，不恢复旧会话或重放网站动作。POSIX 恢复会核验原进程组。Windows 的独立回收子进程会在 Agent 异常退出后终止浏览器 Job，确认其中的活动进程归零并保存完成记录；下一次浏览器请求据此清理旧运行目录，允许重新打开资料。清理尚未完成时返回 `browser_cleanup_pending`。如果 Agent 和回收子进程都被结束，或可靠完成记录缺失，资料仍保持隔离，复用和解绑删除会被拒绝；重启系统本身不能补足这项证明。
- `.rdh-browser` 是 Agent 私有存储，文件读取、写入、列表、搜索和共享根配置均排除这个目录。名称匹配忽略大小写，Windows 还核对规范目录路径，拒绝短文件名别名。Shell 权限仍代表 Agent 所属操作系统账号的权限，这项排除不是 shell 沙箱，也不能隔离同一系统账号的直接访问。

源码职责见[浏览器实现现状](browser-implementation-zhCN.md)。图片展示、任务续接、性能和人工体验需在目标 AI 客户端、操作系统及浏览器组合中分别验证。首次权限配置和浏览器安装目前需要在远程设备本地完成；网页已提供状态说明和已有会话入口。
