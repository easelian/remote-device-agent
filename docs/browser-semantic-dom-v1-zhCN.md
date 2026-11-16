# semantic_dom_v1 共同语义

本规约冻结 D3 内部页面 profile 的共同语义与测试预期。CDP、BiDi 和 Fake 使用同一公共协调器和断言；原有 browser v1 仍使用原有 profile。新 profile 不改变公开 capability、浏览器默认选择或资料格式。

## 观察范围与名称

范围为各已枚举文档的 DOM 与 open shadow tree。遍历不跨文档猜测关联；每个 frame 单独采集。closed shadow 的内容、canvas 像素和浏览器 UI 不在范围内，coverage 固定列出这些限制，不能声称知道未开放内容的数量。最多 32 个 frame；超过上限拒绝，不返回伪完整结果。

候选包括原生控件、受支持的显式 ARIA 控件、标题和文本。显式控件 role 支持 button、link、textbox、searchbox、combobox、checkbox、radio、spinbutton、slider、switch、menuitem、tab、option；heading 为只读记录。原生映射包括 button、有效 href 的 a、input 的非隐藏类型、textarea、select、summary、img、h1–h6 与 contenteditable。其他 tabindex、onclick 或 pointer 元素可标为 inferred，只允许 click/press。generic、none、presentation、caption、code、deletion、emphasis、insertion、paragraph、strong、subscript、superscript 禁止命名，也不因此取得输入资格。未知 role 保留限制，不伪造原生质量。

命名依次采用有效 aria-labelledby 的文本、非空 aria-label、适用控件的关联 label、img/input-image 的 alt、允许内容命名的角色或控件文本、title。input-button/submit/reset 使用显式 value；没有显式 value 的本地化默认文本不推测。textbox 等不采用自身文本作为名称。多 label 按文档顺序；多引用按声明顺序。仅折叠 ASCII 空白 U+0009/U+000A/U+000C/U+000D/U+0020；保留大小写、NBSP 和其他 Unicode。

关联只能在当前 tree root 内解析。隐藏的显式引用可以提供文本。缺失 ID、重复 ID、循环引用、空关联、跨 shadow 关联以及预算不足均给出明确质量限制；有效的部分名称可显示，但不获得可执行引用。最多处理 32 个关联、递归 16 层、256 个命名节点，名称最多 512 个 Unicode 标量。名称规则是有界产品语义，不宣称完整 AccName 或原生 AX 树兼容。

固定语料在 `tests/fixtures/semantic-dom-v1/oracle.json`。实现不得根据浏览器结果修改期望或按 backend kind 忽略公共字段。原生可访问性增强与 canonical 数据分开，不能覆盖 canonical 名称。

## 记录、分页和引用

记录边界为最近的 `data-rdh-record`、tr、li、article 或 role=row/listitem 容器；无此祖先时为控件自身。材料包括该范围内受支持的文本、结构、控件类型、命名依据、目标 URL、表单状态和选项。密码及敏感输入值不进入材料、观察或日志。公共 Rust 层校验材料并散列；不接受后端提供的任意 signature 字符串。

每条记录最多 256 个节点、4,096 个 Unicode 标量，预算不足时返回限制且不签发可执行 ref。邻接记录变化不使未变化记录的 ref 失效；同一记录变化、控件替换、脱离文档或类型变化必须拒绝。身份来自已保留节点，不能按名称、选择器或位置重新匹配旧引用。

每页最多 1,000 项、64,000 字符；默认 200/20,000。每段扫描最多访问 5,000 个节点，find/interactive/viewport 不通过无限重启取得完整。查询与 frame 范围在首次扫描固定。续页替换上一页引用；新观察、控制权换代、300 秒空闲过期或扫描释放使旧 cursor/ref 失效。变更中读取返回 changed_during_read，发生语义变化的记录不取得 ref；未扫描部分不标 complete。

会话、上下文、文档与 realm 身份分开。新文档、同 URL reload、BFCache 恢复及无法证明连续性时换文档代次；hash/history 更新地址而不据 URL 猜测新文档。realm 重建释放扫描与脚本资源，旧 AI ref 不复活。节点仅在仍能证明同一文档与原生节点时重新解析。动作前必须再次查询身份与材料，不能仅靠事件缓存。

## 输入与控制

公共动作依次校验 ref、文档、记录、参数、敏感性、可操作性，必要时滚动，再检查并提交输入。坐标使用 CSS 像素，跨 frame 路由与映射由 adapter 负责。已观察到的遮挡、错误目标、禁用、只读或嵌套真实控件拒绝输入。最后检查与事件派发之间的网页变化不具有原子保证，单独验证和报告。

普通 ReplaceText 使用原生编辑/按键路径，最终 Unicode 值必须相等，产生编辑事件，没有额外提交或导航。必测中文、组合字符、非 BMP、换行以及 contenteditable；按键事件数量和 inputType 可以不同，原始日志保留。不得使用 DOM value 赋值、剪贴板或桌面输入替代普通 fill。选择、日期、颜色、范围使用独立 SetFormValue 固定程序，明确报告合成 input/change；已满足状态返回 changed=false 且不产生新输入。inferred 仅接受 click/press；敏感字段拒绝 AI fill。

contenteditable 的编辑值采用显示文本并将 CRLF 规范为 LF；没有子节点或仅有一个 BR 占位节点的空编辑区表示空字符串。占位 BR 的原始 HTML 与 innerText 保留在合成测试证据中。焦点操作可把选择区折叠到已保留编辑节点内部，随后仍由原生按键或编辑命令替换内容；不以设置 DOM 内容模拟编辑。

每个副作用子命令复用公共提交门与绝对 deadline。输入任务由会话拥有，调用 future 被取消不取消原生工作或其清理。输入租约仅释放本次可能按下的键与指针；清理不确定时暂停，不能向下一操作者发放输入许可。旧代次不能继续下一输入子命令；已提交副作用单调保留 unknown，不重放。人工交接要求所有输入与清理结算，交回后重新观察和确认披露。

## 导航与能力

复用 `browser/sites.rs` 的站点与解析规则。拦截必须在导航开放前安装，独立事件任务处理顶层、frame、popup、脚本导航和重定向，不持有普通页面锁等待 DNS 或请求决定。决定是绑定连接、上下文、请求和 hop 的一次性 token；期限 2 秒，无法归属或超时拒绝。最多 32 个并发决定，超限停止会话，不丢弃关键事件。

首跳拒绝与前序重定向请求已经发出分开报告。验收依据合成 HTTP 服务的连接/请求计数，不能只依据创建事件顺序。导航响应仅表示该协议阶段，不等于 load 或业务完成。等待采用明确文档状态，不使用通用 network-idle。

能力区分导航拦截、DNS 解析检查、实际连接地址证据。DNS 不能替代实际连接证据；连接后证据也不能证明此前没有连接私网。Firefox 没有等价证据时，要求严格连接检查的策略在创建浏览器前拒绝。声明可用的范围必须以真实首请求计数证明；未通过的组合保持不可用。

## 验收与预算

共同断言覆盖命名、质量、完整记录、身份、分页、查询、引用、输入、控制权与导航。Fake 用于确定性变更和取消调度，不作为浏览器执行证据。Linux x86_64 的 Chromium 与 Firefox 记录准确版本，关键控制权和未知结果场景各连续执行 20 次；失败不通过重跑至成功抹去。原有 24 项 Chrome 场景保留。

每会话最多 32 个 frame、每 frame 一个扫描、每扫描最多 1,000 个强节点租约；续页回收上一页节点，文档/realm 变更或 close 清空。协议上限沿用入站 2 MiB、出站 256 KiB、32 个 pending；脚本返回结构必须在解码后进一步校验。100 次会话和 10,000 次观察替换验证租约、pending、工作任务及目录归零。故意无法清理时保留 CleanupPending，不能删除记录制造零残留。

Firefox 体验门槛沿用已列目标并在输入实现前冻结：2 CPU/4 GiB、5,000 DOM 节点、最多 20 frame；启动 30 次 p95 ≤10 秒，观察与稳定输入各 100 次 p95 分别 ≤3 秒、≤1 秒。首次测量不足以满足资源条件时记录 not_run，不改写门槛。D3 不据 Linux 结果声明 macOS 主修饰键、Windows 或 ARM 浏览器运行已验证；公开 v2 和完整平台矩阵属于 D4。

协议依据：[WebDriver BiDi 编辑草案](https://w3c.github.io/webdriver-bidi/)、[CDP Runtime](https://chromedevtools.github.io/devtools-protocol/tot/Runtime/)、[CDP Input](https://chromedevtools.github.io/devtools-protocol/tot/Input/)。协议定义与浏览器实际实现分别验证。
