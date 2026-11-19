'use strict';

const name = '@easelian/remote-device-agent';
const targets = {
  'x86_64-unknown-linux-gnu': 'linux-x64-gnu',
  'aarch64-unknown-linux-gnu': 'linux-arm64-gnu',
  'x86_64-unknown-linux-musl': 'linux-x64-musl',
  'aarch64-unknown-linux-musl': 'linux-arm64-musl',
  'x86_64-apple-darwin': 'darwin-x64',
  'aarch64-apple-darwin': 'darwin-arm64',
  'x86_64-pc-windows-msvc': 'win32-x64',
  'aarch64-pc-windows-msvc': 'win32-arm64',
};

const platformVersion = (version, platform) => `${version}-${platform}`;
const tarballName = (version) => `easelian-remote-device-agent-${version}.tgz`;

function packageVersions(version) {
  return [...Object.values(targets).map((platform) => platformVersion(version, platform)), version];
}

module.exports = { name, targets, platformVersion, tarballName, packageVersions };
