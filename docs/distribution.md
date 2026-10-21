# Self-hosted npm package distribution

As requested on 2026-10-01, the frozen `remote-device-hub@0.6.2` TGZ is initially distributed through xdistribute's HTTPS download endpoint. Publishing to the npmjs registry is deferred; self-hosted installation does not require npm publisher authentication.

On Linux/macOS with Node.js 22 or later, run:

```sh
npm install -g https://mcp.xdistribute.site/downloads/remote-device-hub-0.6.2.tgz
remote-device-agent --version
```

The version command should print `0.6.2`. `remote-device-hub` is another command alias provided by the same package.

The package is 53,839 bytes. Its SHA-256 digest is:

```text
39d36e8b51c8c1fd913ce4d357bb3ad9452a6718edce7763985db107864ded35
```

The same frozen package is archived at [2026-10-01 TGZ](https://mcp.xdistribute.site/releases/20261001/remote-device-hub-0.6.2.tgz). It is also available through the existing port-based endpoint: [xdistribute:8090 TGZ](https://xdistribute.site:8090/downloads/remote-device-hub-0.6.2.tgz).

Installation from the canonical HTTPS URL was verified on xdistribute with Node v22.13.1 and npm 10.9.2, using a separate temporary prefix. All 27 installed source files matched the frozen release record. `--version` and `--help` succeeded for both command aliases. The temporary installation directory was removed. This check did not initiate a new Agent pairing.

Evidence: [download readback](../releases/evidence/selfhost-download-0.6.2-20261001-fresh.json) and [xdistribute installation validation](../releases/evidence/selfhost-npm-install-on-xdistribute-0.6.2-20261001.json). See the [0.6.2 release record](../releases/remote-device-hub-0.6.2.json) for each distribution channel's status.
