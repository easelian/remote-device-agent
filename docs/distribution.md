# 自托管 npm 包分发

2026-10-01 按用户最新要求，先通过 xdistribute 的 HTTPS 下载入口分发冻结的
`remote-device-hub@0.6.2` TGZ。npmjs registry 发布留待后续，自托管安装不需要 npm
发布者账户认证。

Linux/macOS 使用 Node.js 22 或更新版本，可执行：

```sh
npm install -g https://mcp.xdistribute.site/downloads/remote-device-hub-0.6.2.tgz
remote-device-agent --version
```

版本输出应为 `0.6.2`。`remote-device-hub` 是同一包提供的另一个命令别名。

包大小为 53,839 字节，SHA-256 为：

```text
39d36e8b51c8c1fd913ce4d357bb3ad9452a6718edce7763985db107864ded35
```

同一冻结包的发行归档地址为
[2026-10-01 TGZ](https://mcp.xdistribute.site/releases/20261001/remote-device-hub-0.6.2.tgz)。
既有端口入口也可下载同一包：
[xdistribute:8090 TGZ](https://xdistribute.site:8090/downloads/remote-device-hub-0.6.2.tgz)。

在 xdistribute 的 Node v22.13.1 / npm 10.9.2 环境中，使用独立临时 prefix 从上述
canonical HTTPS 下载地址完成 `npm install`。安装的 27 个源码文件全部匹配冻结
release record，两个命令别名的 `--version` / `--help` 均成功，临时安装目录已删除。
该验证未启动新 Agent 绑定。

证据：[下载回读](../releases/evidence/selfhost-download-0.6.2-20261001-fresh.json)、
[xdistribute 安装验证](../releases/evidence/selfhost-npm-install-on-xdistribute-0.6.2-20261001.json)。
各渠道状态以 [0.6.2 发布记录](../releases/remote-device-hub-0.6.2.json) 为准。
