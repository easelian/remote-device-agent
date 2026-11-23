# 面向 AI 的浏览器接口

这份文档说明 AI 操作浏览器时使用的三个操作：`browser_do`、`browser_read` 和 `browser_see`。它们取代原来的十四个 `browser_*` 操作，目标是让 AI 用更少的调用、读更少的字节完成同样的任务，并且不会操作到错误的元素。

文档先给出接口，再说明目前已实现的范围和测量结果。

## 设计取舍

- **一次调用完成一件事。** 打开页面、填三个字段、提交，是一次 `browser_do`，不是五次往返。
- **按名字定位。** 步骤可以直接写“`Order C1` 那一行的 `Approve`”，不需要先读页面取引用。
- **不替调用方猜。** 描述对应多个元素时返回候选，一个都不点。
- **每一步都说明后果。** 回复里写出这一步改变了页面的哪一部分，不需要再读一次来确认。
- **回复是文本。** 一行一个元素，没有 JSON 外壳，也不重复请求里已有的内容。
- **调用里没有会话状态。** 只有一个会话时不用写会话，任何调用都不带 `control_epoch`。

站点策略、沙箱、人工接管和文件批准仍在下层生效，这三个操作只是它们之上的入口。

## `browser_do`：执行一串步骤

```json
{"steps": [
  {"open": "https://shop.example/orders"},
  {"click": {"name": "Next page"}},
  {"click": {"name": "Approve", "in": "Order P07"}}
]}
```

步骤按顺序执行，某一步失败后，其后的步骤不再执行。一次调用最多 16 步。

| 参数 | 含义 |
|---|---|
| `steps` | 必填，步骤列表 |
| `session` | 会话 ID 的开头几位。只有一个会话时省略。`"new"` 让 `open` 步骤在已有会话之外再开一个，见[多个会话](#多个会话) |
| `tab` | 标签页 ID。步骤在这个标签页里执行，它不是当前标签页时先切换过去，见[多个标签页](#多个标签页) |
| `read` | `changes`（默认）、`page` 或 `none`，见[回复](#回复) |

### 步骤

每个步骤是一个对象，恰好包含下表中的一个动词。

| 步骤 | 作用 |
|---|---|
| `{"open": 地址}` | 没有会话时打开一个会话，否则在当前标签页打开该地址 |
| `{"click": 目标}` | 点击 |
| `{"hover": 目标}` | 悬停 |
| `{"fill": 目标, "text": 文本}` | 把输入框的内容替换为文本 |
| `{"select": 目标, "value": 值}` | 选择下拉项，值是选项的 `value` |
| `{"check": 目标, "on": true}` | 勾选或取消勾选，`on` 省略时为勾选 |
| `{"press": 键名, "on": 目标}` | 在目标上按键，例如 `Enter` |
| `{"scroll": 像素, "in": 目标}` | 纵向滚动。省略 `in` 时滚动整个页面 |
| `{"drag": 目标, "to": 目标}` | 拖到另一个元素上 |
| `{"upload": 目标, "grant": 授权}` | 把用户批准的文件交给文件输入框 |
| `{"wait": {"text": 文本}}` | 等到页面出现这段文字。也可写 `{"url": 片段}` |
| `{"dialog": "accept"}` | 回答浏览器对话框，或写 `"dismiss"` |
| `{"tab": "new"}` | 新开标签页。`"close"` 关闭当前标签页，其他值是要切换到的标签页 ID |
| `{"pause": true}`、`{"resume": true}` | 把浏览器交给人，或从人手里收回 |
| `{"close": true}` | 关闭会话 |

### 目标

目标有两种写法。

**引用**是字符串，取自上一次回复中方括号里的内容，例如 `"r41886"`。

**描述**是对象：

| 字段 | 含义 |
|---|---|
| `name` | 必填，控件的名称，即回复中冒号后面的文字 |
| `in` | 控件所在记录中的文字，例如表格行里的订单号 |
| `role` | 角色，例如 `button`、`link`、`textbox` |
| `nth` | 仍有多个匹配时取第几个，从 1 开始 |

名称的匹配分三级：完全相同、仅大小写不同、包含。只要上一级有匹配，就不看下一级。同一级中有多个匹配而没有 `nth` 时，这一步失败，回复列出候选：

```
✗ 1 click: browser_target_ambiguous — narrow it with "in", "role" or "nth"
5 match
  button "Approve" in "Order P06"
  button "Approve" in "Order P07"
```

用描述定位的步骤会等待：目标还没出现，或页面正在重绘时，最多重试 4 秒。因此大多数情况下不需要显式的 `wait` 步骤。

### 回复

回复的 `text` 字段是给 AI 读的全部内容：

```
1 open http://shop.example/orders (session c92c99)
2 click button "Next page"
  changed:
  heading: Orders
  <tr> Order P06
    button [r9bf91]: Approve
  <tr> Order P07
    button [r9bf92]: Approve
  text: Page 2
3 click button "Approve" in "Order P07"
  (no visible change)
```

- 每个步骤占一行，行首是步骤序号。
- `changed:` 下面是这一步改变的页面部分，带有可用于下一次调用的引用。
- `(no visible change)` 表示动作已送达，但页面上看不出变化。
- 最后一步改变了页面、却无法单独列出变化时，回复末尾在 `--- page` 之后附上整页内容。`open`、切换标签页、回答对话框、`wait`，以及跳转到另一个文档的点击都属于这种情况。
- `read` 为 `page` 时总是附上整页，为 `none` 时不附页面内容。

失败的步骤以 `✗` 开头，后面是错误码和处理建议，再后面一行写明没有执行的步骤：

```
✗ 2 click: browser_not_actionable — the element is hidden, disabled or covered right now
not run: 3–4
```

结果不确定的步骤会注明“可能已生效，也可能没有”，这时应先读页面再决定下一步。会话不会因此停住。

密码等敏感输入框不能由 `fill` 填写，这一步以 `browser_sensitive_input` 失败，文本不会发到页面。这时用 `{"pause": true}` 把浏览器交给用户输入，Server 会在回复末尾给出用户接手的链接。

除 `text` 外，结果还有三个供程序使用的字段：`session_id`、`steps_done`，以及失败时的 `failed`（步骤序号、错误码、是否已发出）。步骤失败时调用本身仍然成功返回，失败写在 `text` 和 `failed` 里；只有参数不合法或指定的会话不存在时，调用才按失败返回。

### 浏览器对话框

页面弹出 `alert`、`confirm` 或 `prompt` 时，引起它的那一步立即结束，回复写出对话框的类型和文字，其后的步骤不执行：

```
2 click button "Delete" in "Order D1"
  a confirm dialog opened: "Delete order D1?" — answer it with {"dialog":"accept"} or {"dialog":"dismiss"}
not run: 3–3
```

对话框打开期间，除 `dialog` 和 `close` 之外的步骤都立即以 `browser_dialog_open` 失败。下一次调用以 `dialog` 步骤开头，其后的步骤照常执行。

### 页面出的问题

步骤执行期间页面出现的问题，直接写在这一步下面，不需要另外查询：

```
2 click button "Save order"
  problems:
    n6 500 POST /fail fetch 2ms 214B — {"error":"limit exceeded"}
    c7 error Uncaught (in promise) TypeError: order is not saved (http://shop.example/app.js:17:26)
```

- 列出的问题有三种：没有被捕获的脚本错误、`console.error` 的输出、失败或状态码不低于 400 的请求。被页面自己取消的请求和缺少的 `favicon.ico` 不列出。
- 状态码不低于 400 的请求，行尾破折号之后是服务器响应内容的开头，最多 160 个字符。响应还没有收完时，这个请求留到下一次报告，只等一次。
- 每个问题只报告一次。读页面期间才出现的问题写在 `--- page` 之前，行首不缩进。两次调用之间出现的问题，记在下一次调用的第一步下面。
- 一步最多列 5 个，其余只给数量。
- 行首的 `n6`、`c7` 是条目编号，用 `browser_read` 可以查到这一条的全部内容，见[控制台和网络](#控制台和网络)。

### 多个标签页

一个会话里有若干标签页，其中一个是当前标签页，步骤在它上面执行。

- **步骤打开了新标签页**：点击 `target="_blank"` 的链接或页面调用 `window.open` 之后，当前标签页自动变为新打开的那个，这一步下面注明怎样回去，回复末尾附上新标签页的内容：

  ```
  1 click link "Open report"
    (no visible change)
    a new tab opened and is worked on now; go back with {"tab":"7419bbb3ea79c581fd742bb33400d173"}
  --- page
  Report — http://shop.example/report
  ```

- **切换**：`{"tab": ID}` 步骤，或者给整次调用加 `tab` 参数。`{"tab": "new"}` 打开空白标签页，`{"tab": "close"}` 关闭当前标签页。
- **不切换而读取**：`browser_read` 和 `browser_see` 的 `tab` 参数读另一个标签页的页面或截图，当前标签页不变。
- **页面自己关闭了当前标签页**：下一步之前自动改到剩下的第一个标签页，并在回复里注明。
- **列出**：`browser_read` 的 `what: "tabs"`，当前标签页以 `*` 标记。有多个会话而没有指明会话时，按会话分组列出全部标签页。

诊断日志只跟随当前标签页，切换标签页后重新开始。新标签页是在打开之后才成为当前标签页的，所以它加载期间的问题不在日志里。

一个会话最多 8 个标签页。

### 多个会话

`open` 步骤在没有会话时打开一个会话，已有会话时在当前标签页打开地址。任务需要两套互不相干的登录状态时，给调用加上 `"session": "new"`，`open` 会另开一个会话。此后有多个会话，每次调用都要用 `session` 指明其中一个，否则以 `browser_session_ambiguous` 失败。`browser_read` 的 `what: "sessions"` 列出所有会话，`what: "tabs"` 不带 `session` 时列出所有会话的标签页，这两种读取都不需要指明会话。每个会话是一个独立的浏览器进程，有自己的窗口。

## `browser_read`：读页面

```json
{"find": "Order G1500"}
```

| 参数 | 含义 |
|---|---|
| `what` | `page`（默认）、`tabs`、`sessions`、`console`、`network`、`styles` 或 `performance` |
| `find` | 只读包含这段文字的部分 |
| `filter` | 读页面时：`interactive` 只列可操作的控件，`viewport` 只列视口内的内容。读控制台时：`errors`。读网络时：`failed` |
| `more` | 上一次回复末尾 `more:` 后面的值，接着读下一页 |
| `max_chars` | 这一页的字符上限，默认 20000 |
| `session` | 同 `browser_do`，但不接受 `"new"` |
| `tab` | 读这个标签页而不切换过去。只对读页面有效：控制台、网络、样式和页面加载只有当前标签页的 |

回复的第一行是标题和地址，之后是页面内容。内容没有读完时，最后一行是 `more: <值> (42% read)`。

### 控制台和网络

`what` 为 `console` 或 `network` 时，读的是当前标签页的诊断日志：页面写到控制台的内容，以及页面发出的请求。

```
network: 3 of 4 requests since http://shop.example/orders loaded (3 failed, 1 pending)
n2 404 GET /missing.js script net::ERR_ABORTED
n5 404 GET /favicon.ico other 2ms 177B
n6 500 POST /fail fetch
```

- 第一行是总数。列表只包含当前文档加载以来的条目，从请求该文档的那一条开始，包括它之前的重定向。
- 网络的一行依次是编号、状态码、方法、地址、资源类型、耗时和大小。与页面同源的地址只写路径。没有收到响应的请求，状态码的位置是 `✗`（失败）或 `…`（进行中）。
- 控制台的一行依次是编号、级别、文字的第一行、来源位置，连续重复的输出合并为一行并注明次数。
- `find` 是条目编号时，返回这一条的全部内容：控制台条目是完整文字，包括调用栈；网络条目是完整地址、状态、耗时、大小、发起请求的脚本位置、请求头和响应头；状态码不低于 400 的请求还有响应体的开头。
- `find` 是其他文字时，只列包含它的条目。`filter` 为 `errors`（控制台）或 `failed`（网络）时只列出问题。
- `max_chars` 默认 4000。放不下时保留最新的条目，并注明省略了多少条。这两种读取不支持 `more`。

日志的范围和限制：

- 从 `open` 步骤开始记录，所以第一个页面加载时的问题也在内。每个标签页最多保留 200 行控制台输出和 300 个请求，超出后丢弃最早的。
- 名称中含有 `cookie`、`auth`、`token`、`key`、`session`、`csrf` 等字样的请求头和响应头，只保留名称，不保留值。
- 请求体不保留。响应体只保留一种：状态码不低于 400、且不是文档本身的请求，保留开头 2000 个字符，因为服务器通常在这里说明失败原因。成功的响应不保留，所以登录等接口正常返回的令牌不在日志里。
- 响应体要等请求结束后才能取到。页面没有读完响应、响应超过 16 kB，或者不是文本时，没有响应体。
- 控制权每次交回 AI 时日志重新开始，所以用户接手期间以及之前的内容都读不到。切换标签页后，日志也从新标签页重新开始。
- 只记录顶层文档的控制台输出，以及标签页主进程发出的请求。跨站点 iframe 里的输出和请求不在其中。
- 页面在短时间内向控制台输出大量对象时，日志会中断，并在下一次调用时重新开始，中断期间的内容丢失。

### 样式

`what` 为 `styles` 时，`find` 必填，是元素上的文字。回复说明带有这段文字的元素画在哪里、是什么样子、点击能不能落到它上面：

```
styles: 1 element with the text "Hidden offer"
button "Hidden offer" — 200×40 at 8,151 in a 1280×633 viewport; covered at its centre by div "Notice"
  background-color: rgb(239, 239, 239); border: 2px outset rgb(0, 0, 0); color: rgb(0, 0, 0); cursor: default; display: inline-block; font-family: Arial; font-size: 13.3333px; opacity: 0.5; padding: 1px 6px; text-align: center
```

- 每个元素的第一行是标签、名称、大小、在视口中的位置，以及元素中心点的情况：在最上层、被哪个元素盖住、在视口之外，或者不占空间。步骤以 `browser_not_actionable` 失败时，这一行说明原因。
- 第二行是计算后的样式，共 22 个属性：显示与定位（`display`、`visibility`、`opacity`、`pointer-events`、`position`、`z-index`、`overflow`、`transform`）、文字（`font-family`、`font-size`、`font-weight`、`line-height`、`text-align`、`color`）、盒子（`margin`、`padding`、`border`、`border-radius`、`box-shadow`、`background-color`）和 `cursor`。取值是 CSS 初始值的属性不列出，宽和高已在第一行。
- 文字按包含关系匹配，区分大小写。最多说明前 3 个匹配的元素。
- 不说明某个值来自哪条 CSS 规则。

### 页面加载

`what` 为 `performance` 时，回复是当前文档加载过程的摘要，数据来自诊断日志，不接受其他参数：

```
performance: http://shop.example/orders
document: first byte after 120ms, parsed after 480ms, loaded after 1.3s
requests: 43, 2 failed, 1 pending, 1.2MB received
slowest:
  n12 200 GET /api/items fetch 890ms 12.0kB
largest:
  n7 200 GET /hero.jpg image 610ms 640.0kB
paint: first content after 310ms, largest content after 1.1s (img "Autumn sale")
layout shift: 0.12 in 3, the largest 0.08 after 2.3s (div#banner)
long tasks: 2 totalling 380ms, the longest 210ms after 1.2s
```

- `document` 一行的时间从请求文档算起，包括之前的重定向。
- `slowest` 和 `largest` 各列 5 个请求，编号可以用 `what: "network"` 查看详情。
- `paint` 一行是首次内容绘制（FCP）和最大内容绘制（LCP）的时间，括号里是最大内容所在的元素。
- `layout shift` 一行是累积布局偏移（CLS）的分数、偏移次数，以及最大的一次偏移发生的时间和移动的元素。用户输入后 500 毫秒内的偏移不计入。
- `long tasks` 一行是阻塞主线程超过 50 毫秒的任务：个数、总时长、最长一个的时长和开始时间。
- 后三行由页面里一段固定的只读脚本取得，时间从文档开始加载算起。脚本读的是浏览器已经记下的数据，所以不需要在加载前做准备。页面正被对话框挡住时取不到，回复里会说明。

## `browser_see`：截图

可选参数有 `session` 和 `tab`。返回当前标签页或 `tab` 指定的标签页的截图，用于画布和只能靠视觉判断的内容。

## 已实现的范围

这三个操作在 Agent 中实现，并已加入设备策略可以允许的操作列表。Server 的 `tools/list` 向 MCP 客户端提供这三个工具和 `browser_files`，被取代的旧工具不再列出，但按名称调用仍然有效；Server 一侧的行为见 Server 仓库的 `contracts/browser/front-zhCN.md`。只有 v2 可用的 Agent 才通告这三个操作。以下部分还没有完成：

| 尚未完成 | 现状 |
|---|---|
| 引用的有效期 | 引用在下一个动作之后失效，同一次调用里靠后的步骤要用描述定位。设计目标是元素存在期间引用不变 |
| 诊断的缺口 | 样式不说明值来自哪条 CSS 规则；没有交互延迟（INP）；诊断日志只跟随当前标签页，不保留请求体，也不保留成功请求的响应体。Firefox 会话没有诊断日志 |
| 按截图坐标点击 | Server 已删除旧的 `browser_act`，MCP 客户端不能再按坐标点击 |
| 命名资料 | Server 已删除旧的 `browser_profiles`，`open` 步骤也不能指定资料，所以每个会话都用临时资料 |
| 文件列表 | 仍通过 `browser_files`，它保留在工具列表中 |
| Firefox | `open` 目前总是启动 Chromium |
| 可用的平台 | 三个操作只在带 `experimental-browser-bidi` 构建、且策略中 `capability_v2` 为 `true` 的 Agent 上可用，目前即 Linux x86_64。其他 Agent 不通告这三个操作，Server 以 `browser_unavailable` 拒绝 |
| 删除文件和取消下载 | 没有对应的步骤，只能由用户在文件页面上操作 |
| 下载进行中的读取 | 点击触发下载后，默认的 `read: "changes"` 会等到下载结束。下载较慢时给这次调用加上 `read: "none"` |
| 旧操作和 v1 运行时 | Server 不再转发旧操作。它们在 Agent 中仍然存在：三个新操作是在 v2 会话操作之上实现的，v1 运行时也还在，但已没有调用方 |

实现位于 [front.rs](../src/browser/runtime/v2/front.rs)。每个步骤转换为现有会话操作执行，所以站点策略、人工接管和文件批准的行为与旧操作一致。`browser_do` 需要回执，`browser_read` 和 `browser_see` 不需要。

`drag` 和 `upload` 两种步骤只做了参数检查，还没有在真实浏览器上验证。

## 测量

[场景测试](../tests/browser_scenarios.rs)中的 `front_pool` 在 24 个场景页面上各执行一个任务，每个任务是一次 `browser_do` 调用，再加一次关闭会话的调用。断言有三项：所有步骤都执行了，站点收到的写入与任务要求一致，回复中包含结果文字。第 25 个任务 `confirm` 用五次调用检查浏览器对话框的处理。同一文件中的 `front_diagnostics` 在一个会出错的页面上检查诊断日志和第二个会话。

```bash
RDH_BROWSER_EXECUTABLE=/opt/google/chrome/chrome cargo test --features experimental-browser-bidi --test browser_scenarios front_pool -- --ignored --nocapture
```

设置 `RDH_SCENARIO_ONLY=pager,wizard` 只运行列出的场景，设置 `RDH_SCENARIO_DUMP` 把每个任务的回复写入文件。

2026-11-22 在调试构建下的结果：

| | 调用次数 | 回复字节 |
|---|---|---|
| 旧操作（`scenario_pool`，同样 24 个场景） | 124 | 57,172 |
| 新接口（`front_pool`） | 48 | 10,304 |

两组测试的步骤不完全相同：`scenario_pool` 还包含对过期引用的检查，所以它的调用次数高于完成任务所需的最少次数。与 agent-browser 的对比也有同样的问题，按任务对齐的对比还没有做。
