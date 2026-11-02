'use strict';

// Synthetic artifacts exercise assembly and rejection only, not native binaries.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');
const root = path.resolve(__dirname, '..');
const script = path.join(__dirname, 'prepare-release.cjs');
const version = '1.2.3-test.1';
const commit = 'a'.repeat(40);
const targets = {
  'x86_64-unknown-linux-gnu': 'linux-x64-gnu', 'aarch64-unknown-linux-gnu': 'linux-arm64-gnu',
  'x86_64-unknown-linux-musl': 'linux-x64-musl', 'aarch64-unknown-linux-musl': 'linux-arm64-musl',
  'x86_64-apple-darwin': 'darwin-x64', 'aarch64-apple-darwin': 'darwin-arm64',
  'x86_64-pc-windows-msvc': 'win32-x64', 'aarch64-pc-windows-msvc': 'win32-arm64',
};
const digest = (data) => crypto.createHash('sha256').update(data).digest('hex');

function fixture(temp) {
  const inputs = path.join(temp, 'inputs');
  for (const [target, platform] of Object.entries(targets)) {
    const base = `rdh-${version}-${platform}`;
    const dir = path.join(inputs, `native-development-${target}`);
    const tree = path.join(dir, 'verified', base);
    const binary = platform.startsWith('win32') ? 'rdh.exe' : 'rdh';
    const bytes = Buffer.from(`#!/bin/sh\nprintf '${version}\\n'\n`);
    const license = fs.readFileSync(path.join(root, 'LICENSE'));
    fs.mkdirSync(path.join(tree, 'licenses'), {recursive: true});
    fs.writeFileSync(path.join(tree, binary), bytes, { mode: 0o755 });
    fs.writeFileSync(path.join(tree, 'PROJECT_LICENSE'), license);
    const manifest = {kind: 'native-agent-release-candidate', version, target, platform, source_commit: commit,
      source_dirty: false, features: 'all', runtime_self_check: true, release_ready: false, project_license_included: true,
      binary: {path: binary, bytes: bytes.length, sha256: digest(bytes)}, files: {
        [binary]: {kind: 'file', bytes: bytes.length, sha256: digest(bytes)},
        PROJECT_LICENSE: {kind: 'file', bytes: license.length, sha256: digest(license)},
        licenses: {kind: 'directory'},
      }};
    fs.writeFileSync(path.join(tree, 'manifest.json'), JSON.stringify(manifest));
    const raw = `${base}${binary.endsWith('.exe') ? '.exe' : ''}`;
    const archive = `${base}${binary.endsWith('.exe') ? '.zip' : '.tar.gz'}`;
    for (const [name, data] of [[raw, bytes], [archive, Buffer.from('synthetic archive')]]) {
      fs.writeFileSync(path.join(dir, name), data);
      fs.writeFileSync(path.join(dir, `${name}.sha256`), `${digest(data)}  ${name}\n`);
    }
  }
  return inputs;
}

function run(inputs, output, temp) {
  return spawnSync(process.execPath, [script, inputs, output, version, commit, 'easelian/remote-device-agent'], {
    encoding: 'utf8', env: {...process.env, npm_config_cache: path.join(temp, 'npm-cache')},
  });
}

test('assembles eight checked inputs and rejects tampering or missing targets', () => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'rdh-release-assembly-'));
  try {
    const inputs = fixture(temp);
    const first = run(inputs, path.join(temp, 'ok'), temp);
    assert.equal(first.status, 0, first.stderr);
    const out = path.join(temp, 'ok');
    const outputNames = fs.readdirSync(out).sort();
    assert.equal(outputNames.length, 21);
    assert.ok(outputNames.includes('SHA256SUMS'));
    assert.ok(!outputNames.some((name) => name.endsWith('.sha256')));
    assert.ok(fs.readFileSync(path.join(out, 'install.sh'), 'utf8').includes(`\nrdh_default_version=${version}\n`));
    assert.ok(fs.readFileSync(path.join(out, 'install.ps1'), 'utf8').includes(`\n$rdhDefaultVersion = '${version}'\n`));
    const records = fs.readFileSync(path.join(out, 'SHA256SUMS'), 'utf8').trimEnd().split('\n');
    assert.equal(records.length, outputNames.length - 1);
    for (const record of records) {
      const match = /^([0-9a-f]{64})  ([^/]+)$/.exec(record);
      assert.ok(match, `Invalid checksum record: ${record}`);
      assert.equal(digest(fs.readFileSync(path.join(out, match[2]))), match[1]);
    }
    const tarball = path.join(out, `easelian-remote-device-agent-${version}.tgz`);
    assert.ok(fs.existsSync(tarball));
    const packageJson = spawnSync('tar', ['-xOf', tarball, 'package/package.json'], {encoding: 'utf8'});
    assert.equal(packageJson.status, 0, packageJson.stderr);
    assert.equal(JSON.parse(packageJson.stdout).license, 'Apache-2.0');
    const packageLicense = spawnSync('tar', ['-xOf', tarball, 'package/LICENSE']);
    assert.equal(packageLicense.status, 0, packageLicense.stderr.toString());
    assert.deepEqual(packageLicense.stdout, fs.readFileSync(path.join(root, 'LICENSE')));
    const install = path.join(temp, 'install');
    fs.mkdirSync(install);
    fs.writeFileSync(path.join(install, 'package.json'), '{"private":true}\n');
    const installed = spawnSync('npm', ['install', '--prefix', install, '--ignore-scripts', '--no-audit', '--no-fund', tarball], {
      encoding: 'utf8', env: {...process.env, npm_config_cache: path.join(temp, 'npm-cache')},
    });
    assert.equal(installed.status, 0, installed.stderr);
    const launched = spawnSync(path.join(install, 'node_modules', '.bin', 'rdh'), ['--version'], {encoding: 'utf8'});
    assert.equal(launched.status, 0, launched.stderr);
    assert.equal(launched.stdout.trim(), version);
    const channel = JSON.parse(fs.readFileSync(path.join(out, `rdh-${version}.canary.json`)));
    assert.equal(Object.keys(channel.platforms).length, 8);
    assert.equal(channel.platforms['linux-x64-gnu'].format, 'native-binary');
    const tree = path.join(inputs, 'native-development-x86_64-unknown-linux-gnu', 'verified', `rdh-${version}-linux-x64-gnu`);
    fs.writeFileSync(path.join(tree, 'rdh'), 'tampered');
    const bad = run(inputs, path.join(temp, 'bad'), temp);
    assert.notEqual(bad.status, 0);
    assert.match(bad.stderr, /Invalid (size|digest)/);
    fs.rmSync(path.join(inputs, 'native-development-x86_64-apple-darwin'), {recursive: true});
    const missing = run(inputs, path.join(temp, 'missing'), temp);
    assert.notEqual(missing.status, 0);
  } finally {
    fs.rmSync(temp, {recursive: true, force: true});
  }
});
