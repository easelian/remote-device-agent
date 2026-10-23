# Agent distribution

The Agent supports runtime archives for Linux/macOS and an npm package for devices with Node.js 22 or later. Both contain the same bundled Agent implementation.

| Format | Supported targets | Runtime requirement |
| --- | --- | --- |
| `rdh-<version>-<platform>.tar.gz` | Linux glibc/musl on x64/arm64; macOS on x64/arm64 | Includes Node.js |
| `remote-device-hub-<version>.tgz` | Windows, Linux, and macOS | Node.js 22+ already installed |

## Runtime archives

The release workflow publishes runtime archives, `SHA256SUMS`, `rdh-manifest.json`, and `install.sh` as GitHub Release assets. Install with:

```sh
curl -fsSL https://github.com/be-one-click/remote-device-agent/releases/latest/download/install.sh | sh
```

The installer selects the platform archive, verifies its checksum, extracts it into a version directory, and switches the `current` symlink. Pairing and service registration follow unless disabled with `--no-pair` or `--no-service`. Linux uses a systemd user service; macOS uses launchd. See the [installation options](../README.md#install-and-run) for offline installation and removal.

## npm packages

Distribute the TGZ built by `scripts/release.mjs` through an internal artifact service or copy it to the device. For the current package version:

```sh
npm install --global --ignore-scripts --offline ./remote-device-hub-0.7.0.tgz
remote-device-agent --version
```

The command should print the installed package version. The `remote-device-hub` alias starts the same Agent. The package has no runtime npm dependencies or installation scripts, so installing a local TGZ requires no registry access. Keep an immutable download URL and a digest for each distributed version.

## Internal mirrors and rollout channels

An internal mirror for runtime installation must serve the release files together. Set the installer's `--base` to that directory. Devices with an existing runtime installation can use `rdh upgrade --index URL` to read an internal channel index.

The `Promote` workflow writes `channels/stable.json` or `channels/canary.json` with artifact URLs, checksums, an optional minimum source version, and a rollout percentage. A device's installation ID determines its rollout bucket. Promotion changes the index; it does not create another package. See the [upgrade and rollback commands](../README.md#releases-and-staged-rollouts).
