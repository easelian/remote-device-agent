# Firefox BiDi D2b 实现说明

本文描述 BiDi 通道的 Linux 原生寿命守护、私有端点、并发命令、公共提交门和迟到响应结算。代码由 `experimental-browser-bidi` 功能开关控制，默认关闭。页面观察、输入和站点策略由[共同语义实现](browser-semantic-implementation-zhCN.md)提供；公开选择及命名资料、文件能力见 [capability v2](browser-capability-v2-zhCN.md)。省略选择参数的请求仍使用 CDP/v1。

原生验证组合为 Linux x86_64、Firefox 160.0、独立临时资料。其他 OS、CPU 架构和 Firefox 版本需要各自实测。八目标构建验证编译、单元测试和打包，不代替这些组合的真实 Firefox 验收。每次运行的提交、浏览器摘要和测试结果保存在外部验证产物中。

## 1. 原生寿命和端点

`process/firefox.rs` 启动同一 Agent 二进制的内部监督入口。入口在 Tokio 初始化前运行，通过继承的匿名 Unix `SOCK_SEQPACKET` 接收配置，并在独立的 user、network、PID 和 mount namespace 中启动 Firefox。该路径不分配 CDP 管道描述符。

监督链包含外层 anchor 和 namespace 的 PID 1。anchor 绑定 Agent 的父进程死亡信号；PID 1 绑定 anchor，并通过 pidfd 检查设置死亡信号之前父进程已经退出的窗口。PID 1 的启动身份持久化成功后，才允许它启动 Firefox。Agent 退出、anchor 退出或 PID 1 退出都会触发该命名空间的终止路径；PID 1 的退出由 Linux 内核终止其中其余进程，包括改变进程组的后代。原生退出证明仍须经过监督记录与回收检查，不能由 WebSocket 断开推断。[Linux PID namespace 语义](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html)、[父进程死亡信号语义](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html)。

Firefox 的 BiDi 监听器只存在于私有网络 namespace 的回环接口。监督进程在该 namespace 内连接端点，再通过 `SCM_RIGHTS` 把已连接 socket 交给 Agent。主机网络 namespace 不增加调试或代理监听端口；控制连接不使用主机 DNS、HTTP 重定向或环境代理。端点声明中的精确 `localhost` 被转换为数值 `127.0.0.1`，其他主机、凭据、查询参数和冲突端点均被拒绝。

这一边界针对其他 OS 用户和受限 Agent shell。root、具有相应管理能力的进程以及不受限制的同 UID 进程不在保证范围内。mount namespace 用于隔离挂载传播并为 PID namespace 挂载 `/proc`，不提供完整的文件系统容器；浏览器资料仍依赖既有私有存储规则。需要的 namespace、pidfd 或挂载能力不可用时启动失败，不退回主机回环监听。[Linux network namespace 语义](https://man7.org/linux/man-pages/man7/network_namespaces.7.html)。

## 2. 网站出站连接

私有网络 namespace 只启用回环接口，没有 veth 或默认路由。Firefox 使用该 namespace 内的 SOCKS5 监听器；监听器描述符交给 Agent，由 Agent 内部的原生 TCP 服务解析目标并建立出站连接。该服务不需要独立代理程序，TLS 握手和证书校验仍由 Firefox 完成。

出站服务仅接受 SOCKS5 TCP CONNECT，支持数值 IPv4、IPv6 和受限主机名。并发连接上限为 32；额外连接立即关闭。握手和目标解析/连接分别有 3 秒期限，每个方向使用固定 8 KiB 转发缓冲。关闭会话时停止监听并等待所有转发任务结束。Firefox 配置关闭直接连接回退、DoH 和 HTTP/3；当前实现不提供 UDP 出站。

该出站路径提供 namespace 内的网站访问。共同页面层在文档请求发送前实施站点许可及重定向检查，但当前 adapter 不提供实际连接地址证据；要求严格私网连接检查的策略在打开前拒绝。文档请求被拦截不代表没有发生 TCP 预连接，出站连通性本身也不证明策略兼容。

## 3. 命令、事件和控制权

`engine/bidi` 拥有 BiDi 编解码和原生身份。当前命令是封闭枚举：状态、建立会话、订阅核心事件、读取上下文树、创建标签页和关闭上下文。关闭整个浏览器使用独立的保留通道。没有任意方法名或脚本入口。

每条连接拥有独立的上下文作用域。原生 context 字符串相同也不能跨连接使用；外来身份在入队前拒绝。启动先建立会话、订阅上下文创建/销毁事件，再取得树快照。读任务记录树响应对应的事件边界，初始化合并该边界以前的订阅历史，避免快照恢复已销毁的上下文。内部 Ready 仅表示这段启动完成。

读任务和写任务独立运行。正常提交经过共享的 `control::Submission`，最后授权检查、同步入队和副作用标记位于同一控制区间。待决表独占命令的容量许可、输入交接屏障和原始回执；调用者取消或超时不会释放它们。迟到成功和协议错误结算原命令，断线则结算为未知，均不重放。已提交的副作用标记保持为真。交付还检查控制代次：旧代次不能取得响应内容，原始回执仍保留结算元数据。

只有当前订阅的核心上下文事件进入关键队列。生产者无法入队、超过字节预算或发现关键事件损坏时，立即关闭新提交入口并通知原生所有者清理；无需等待消费者调用 `recv`。正常关闭使用保留信号和保留命令 ID，普通队列满不能阻断关闭。读侧在原生退出前保留，以便结算关闭期间的迟到响应。

| 资源 | 固定边界 |
| --- | --- |
| WebSocket 完整消息与单帧 | 各 2 MiB；分片按完整消息累计 |
| 出站命令完整 JSON | 256 KiB |
| 普通命令队列、待决命令 | 各 32；容量不足同步拒绝，不等待空位 |
| 单个关键事件 | 64 KiB |
| 关键事件队列 | 256 项，累计编码字节最多 1 MiB |
| 启动上下文表 | 128 个活动项、128 个销毁标记，树深度最多 32 |
| 写入期限 | 3 秒；结果不明的已提交命令保持未知语义 |
| 启动日志读取 | 64 KiB；不向普通诊断输出原始端点或消息正文 |

## 4. 关闭和恢复

`Managed` 在独立任务中持有原生所有者。调用者丢弃会话、启动失败或关键事件缺口都触发关闭。正常关闭保留 2 秒优雅退出窗口；必要时停止监督链，原生关闭总预算为 3 秒。外层会话关闭在 3.5 秒内等待原生完成及两个传输任务退出，无法证明完成时返回 `CleanupPending` 并保留恢复记录。

记录类型 `linux-bidi-namespace-v1` 保存 anchor 与已发布 PID 1 的 PID 和启动身份。恢复先核实这些身份已结束，再使用既有 `retire` 路径回收目录；不能仅因 PID 已复用而判断旧工作仍活动，也不能删除仍由原生进程占用的资料。不启用该功能的构建会明确拒绝恢复此类记录，避免按旧 CDP 规则误删。

原生强杀测试通过明确的 IPC 屏障分别停止在 namespace 建立前、浏览器启动前、端点出现前、握手前和握手后。测试核实整棵 namespace 已终止、残留子进程已回收、目录可恢复，并保持独立会话存活。跨用户测试在首次握手前运行，覆盖实际允许网络的 Agent sandbox。

## 5. 复现

在仓库根目录运行。原生测试需要 Linux namespace/pidfd 支持、预先准备且核对摘要的 Firefox、bubblewrap，以及 `sudo -n -u nobody` 权限。证据目录须是新的私有空目录；测试不下载浏览器、不使用真实账号。夹具失败时保留目录供诊断，不能把跳过或人工清理记为成功。

```sh
export RDH_FIREFOX_EXECUTABLE=/absolute/path/to/firefox
export RDH_FIREFOX_VERSION=160.0
export CARGO_TARGET_DIR="$PWD/.build/bidi-d2b-target"
export RDH_D2B_EVIDENCE="$PWD/.build/bidi-d2b-evidence"
mkdir -m 700 -p "$RDH_D2B_EVIDENCE"

cargo +stable build --profile ci --locked --all-features --bin rdh
export RDH_BIDI_HELPER="$CARGO_TARGET_DIR/ci/rdh"

cargo +stable test --profile ci --locked --workspace --all-targets --all-features
cargo +stable test --profile ci --locked --all-features --lib \
  browser::process::firefox::tests:: -- --ignored --nocapture --test-threads=1
cargo +stable test --profile ci --locked --all-features --lib \
  browser::engine::bidi::session::tests:: -- --ignored --nocapture --test-threads=1
```

常规测试包含 10,000 个固定种子的有界 WebSocket 调度序列，覆盖取消、超时、控制换代、迟到结算、未知/重复 ID、断线和关键事件溢出。容量与事件压力下的 1,000 次提交门测量只表示内部传输夹具中的关闭延迟，不能替代公共 RPC 端到端时延或完整后端性能验收。

显式原生测试默认运行 100 次会话循环、各启动阶段共 100 次宿主 SIGKILL、27 次监督进程 SIGKILL、20 轮共 80 个真实响应迟到场景，以及出站、首会话隔离、取消和自动清理场景。结果应核实待决命令、事件字节、传输任务、自有进程和成功场景运行目录归零。资源计数与 RSS 分别记录；当前原生环境的描述性时延不等于 2 CPU/4 GiB 条件下的发布性能结论。

原有 `tests/browser_session.rs` 的 24 个 Chrome 场景须继续显式执行。D3 的页面/realm/文档身份、输入租约与站点策略，以及 D4 的平台矩阵、真实 MCP 和 v2 协商，仍按[后端验收指标](browser-backend-acceptance-zhCN.md)单独验收。未来 DevTools 扩展应通过有类型的命令共用现有协议连接、控制权和原生所有者；高流量诊断事件需要独立预算，不占用核心事件队列。
