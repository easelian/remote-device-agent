# Firefox BiDi D2a 开发验证

本阶段验证 Linux 上的 Firefox 原生通道、进程寿命和本机隔离，为 D2b 确定实施条件。夹具只在 `cfg(all(test, target_os = "linux"))` 中编译，生产 Agent 不提供 Firefox 选择或 BiDi 能力。D1 的 CDP/v1 行为继续由原有测试验证。

## 1. 实现边界

入口为 `src/browser/process/bidi_probe.rs` 及其 `client`、`tests` 子模块。夹具直接连接 Firefox 的原生 WebSocket，使用现有 Rust 依赖，不引入浏览器驱动、Node 运行时、扩展或外部代理。

进程使用现有 POSIX `Child`、PID 启动身份、进程组回收、私有存储、namespace 锁及 `recover`/`retire` 路径。临时 Firefox 数据位于独立的 `firefox-v1` 子目录。D2a 不实现命名资料，也不读取或转换已有 Chromium 资料。

现有 `Child::spawn` 会配置 CDP 描述符。Firefox 不使用这些描述符，夹具立即关闭父端；这一复用只适用于开发验证。D2b 接入产品前须显式区分私有管道和受管 WebSocket 的启动通道，不能借此声称 Firefox 使用了私有管道。

端点只从本次子进程的私有 stderr 读取，启动日志读取上限为 64 KiB。解析等待完整行，接受明确的 `ws://127.0.0.1:<非零端口>`，拒绝外部地址、凭据、查询参数和多个端点。连接前通过 Linux socket inode 与本次进程组复核监听器归属。连接使用数值回环地址，不使用代理、DNS 或 HTTP 重定向。

串行客户端只验证会话、上下文事件和退出。完整消息与单帧上限为 2 MiB，事件缓存最多 64 项。超时、取消或解码错误后禁止继续使用该连接，调用方回收浏览器。它尚未实现 D2b 的并发待决表、公共提交门、迟到响应结算或输入能力，不能用这些测试替代完整 D2 传输验收。

## 2. 已确定的部署限制

验证组合为 Linux x86_64、Firefox 160.0、独立测试资料、原生回环 WebSocket；浏览器精确摘要和每次运行结果保存在外部验证产物中。

| 场景 | 实测行为 | 对产品的含义 |
| --- | --- | --- |
| 正常启动与会话 | 启动、`session.new`、上下文创建/销毁事件及关闭可以完成 | 原生通道路线可用于后续开发 |
| 其他 OS 用户、已有会话 | 私有文件不可读，但可建立 WebSocket 并调用 `session.status`；无会话的连接不能直接读取上下文树 | 文件权限不构成端点访问控制 |
| 其他 OS 用户、Agent 握手前 | 可先建立 BiDi 会话并读取合成页面的上下文树 | 存在首个会话被其他主体占用的窗口；不能以会话已建立后的行为证明认证成立 |
| 实际 Agent sandbox，允许网络 | 浏览器私有文件被隐藏，调试端点仍可连接 | `sandboxed_only` 及文件系统隔离不足以开放 BiDi |
| 实际 Agent sandbox，禁止网络 | 网络 namespace 不同，无法连接主机回环端点 | 只证明该 shell 被阻断，不能替代其他本机用户的隔离 |
| 丢弃进程所有者 future | 原生 guard 停止本次进程组，保留记录，后续恢复可以完成 | 有序取消路径可复用 |
| 对宿主发送 SIGKILL | 端点出现前、出现后和握手后三个阶段，Firefox 均可继续存活；现有恢复正确拒绝删除活动资料 | Firefox 不具备 CDP 管道断开后的同等退出行为，需要独立的寿命守护机制 |

上述隔离和宿主崩溃场景中的成功探测，是对限制的复现，不是相应产品验收通过。测试清理仅处理本次已验证归属的浏览器进程，并保留另一个独立会话验证不会误杀。

正常关闭在 `browser.close` 响应后仍保留连接，直到原生进程监督确认退出；响应成功不能代替进程退出。关闭继续使用现有 2 秒优雅退出、随后强制停止、总计 3 秒的预算。停止中的浏览器也必须经过强制回收测试，无法证明回收完成时保留目录和记录。

Mozilla 明确说明 Remote Agent 不提供认证或消息加密。回环地址、Host/Origin 检查和随机端口都不能充当主体认证。[Remote Agent 安全模型](https://firefox-source-docs.mozilla.org/remote/Security.html)

## 3. 与 D2b 的关系

本夹具保留直接回环监听和现有 POSIX 进程组的验证方式，用于说明该组合的隔离与寿命限制。[D2b 实现](browser-bidi-d2b-zhCN.md)另行采用独立监督链、Linux PID/network namespace、继承的私有控制 socket 和有界 TCP 出站服务；它还实现并发待决表、公共提交门与迟到响应结算。

D2a 的结果不能替代 D2b 实测，D2b 的 namespace 保障也不能推广到本夹具的直接回环组合。产品能力继续保持不可用；页面适配、站点策略和公开 v2 按后续阶段单独验收。

## 4. 复现方法

测试需要预先准备的 Firefox、Linux `/proc`、bubblewrap，以及用于切换到现有 `nobody` 用户的 `sudo -n` 权限。跨用户探测只访问夹具创建的空白会话；公开临时目录只放测试程序，浏览器资料仍保持私有。缺少这些条件时应记为 `not_run`，不能把跳过记成隔离通过。

在仓库根目录设置以下变量。其中 Firefox 路径必须指向已核对版本与摘要的可执行文件，证据目录必须是新的空目录。测试不会自动下载浏览器。

```sh
export RDH_FIREFOX_EXECUTABLE=/absolute/path/to/firefox
export RDH_FIREFOX_VERSION=160.0
export RDH_D2A_EVIDENCE="$PWD/.build/bidi-d2a-evidence"
mkdir -m 700 "$RDH_D2A_EVIDENCE"

cargo +stable test --profile ci --locked -p rdh-agent --all-features \
  --lib browser::process::bidi_probe:: -- --nocapture

cargo +stable test --profile ci --locked -p rdh-agent --all-features \
  --lib browser::process::bidi_probe::tests:: \
  -- --ignored --nocapture --test-threads=1
```

第一条命令运行端点校验及受控握手错误测试。第二条显式运行真实 Firefox 场景，输出独立的生命周期、隔离、首会话竞争、取消恢复及宿主 SIGKILL JSONL。夹具失败时保留诊断目录；成功结束必须确认进程和临时运行目录归零，并重新取得 namespace 锁。

这些场景使用空白页面及合成文件，不验证 `semantic_dom_v1`、文本输入、导航策略、文件传输或 macOS/Windows/ARM 的真实浏览器行为。
