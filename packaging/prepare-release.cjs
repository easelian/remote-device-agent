#!/usr/bin/env node
'use strict';

// Assemble only artifacts produced by the eight successful native CI jobs.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

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
const [inputs, output, version, commit, repository] = process.argv.slice(2);
function fail(message) { throw new Error(message); }
if (process.argv.length !== 7 || !/^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/.test(version || '') ||
    !/^[0-9a-f]{40}$/.test(commit || '') || !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository || '')) {
  fail('Usage: prepare-release.cjs INPUT_DIR NEW_OUTPUT_DIR VERSION COMMIT OWNER/REPOSITORY');
}
if (fs.existsSync(output)) fail('Output directory must be new');
const sha = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const equal = (a, b, message) => { if (a !== b) fail(message); };
function verifyInventory(tree, inventory) {
  if (!inventory || typeof inventory !== 'object' || Array.isArray(inventory)) fail('Missing artifact inventory');
  const seen = new Set();
  function visit(directory, prefix) {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (relative === 'manifest.json') continue;
      if (seen.size >= 4096 || relative.split('/').length > 8) fail('Artifact inventory exceeds limits');
      const expected = inventory[relative];
      if (!expected) fail(`Unexpected artifact entry: ${relative}`);
      seen.add(relative);
      const actual = path.join(directory, entry.name);
      if (entry.isDirectory() && expected.kind === 'directory') visit(actual, relative);
      else if (entry.isFile() && expected.kind === 'file') {
        equal(fs.statSync(actual).size, expected.bytes, `Invalid size: ${relative}`);
        equal(sha(actual), expected.sha256, `Invalid digest: ${relative}`);
      } else fail(`Invalid artifact entry: ${relative}`);
    }
  }
  visit(tree, '');
  equal(seen.size, Object.keys(inventory).length, 'Artifact inventory is incomplete');
}
const assets = [];
const channels = {};
fs.mkdirSync(output, { recursive: true });
const staging = path.join(output, '.npm-stage');
fs.mkdirSync(path.join(staging, 'native'), { recursive: true });
try {
  for (const [target, platform] of Object.entries(targets)) {
    const base = `rdh-${version}-${platform}`;
    const dir = path.join(inputs, `native-development-${target}`);
    const tree = path.join(dir, 'verified', base);
    const binaryName = platform.startsWith('win32-') ? 'rdh.exe' : 'rdh';
    const rawName = base + (binaryName.endsWith('.exe') ? '.exe' : '');
    const archiveName = base + (binaryName.endsWith('.exe') ? '.zip' : '.tar.gz');
    const raw = path.join(dir, rawName);
    const archive = path.join(dir, archiveName);
    const manifest = JSON.parse(fs.readFileSync(path.join(tree, 'manifest.json'), 'utf8'));
    for (const [key, expected] of Object.entries({ kind: 'native-agent-release-candidate', version, target, platform, source_commit: commit, source_dirty: false, features: 'all', runtime_self_check: true, release_ready: false, project_license_included: true })) {
      if (manifest[key] !== expected) fail(`${platform}: invalid manifest field ${key}`);
    }
    verifyInventory(tree, manifest.files);
    if (!manifest.binary || manifest.binary.path !== binaryName || manifest.binary.bytes > 128 * 1024 * 1024) fail(`${platform}: invalid binary metadata`);
    const rawHash = sha(raw);
    equal(manifest.binary.sha256, rawHash, `${platform}: binary digest differs from manifest`);
    equal(manifest.binary.bytes, fs.statSync(raw).size, `${platform}: binary size differs from manifest`);
    equal(sha(path.join(tree, binaryName)), rawHash, `${platform}: extracted executable differs`);
    for (const name of [rawName, archiveName]) {
      const file = path.join(dir, name);
      const expected = `${sha(file)}  ${name}\n`;
      equal(fs.readFileSync(`${file}.sha256`, 'utf8'), expected, `${platform}: invalid checksum for ${name}`);
      for (const entry of [name, `${name}.sha256`]) {
        fs.copyFileSync(path.join(dir, entry), path.join(output, entry), fs.constants.COPYFILE_EXCL);
        assets.push(entry);
      }
    }
    const dest = path.join(staging, 'native', platform);
    fs.mkdirSync(dest);
    fs.copyFileSync(raw, path.join(dest, binaryName));
    if (binaryName === 'rdh') fs.chmodSync(path.join(dest, binaryName), 0o755);
    fs.cpSync(path.join(tree, 'licenses'), path.join(dest, 'licenses'), { recursive: true, errorOnExist: true });
    fs.copyFileSync(path.join(tree, 'PROJECT_LICENSE'), path.join(dest, 'PROJECT_LICENSE'));
    equal(sha(path.join(tree, 'PROJECT_LICENSE')), sha(path.resolve(__dirname, '..', 'LICENSE')), `${platform}: project license differs`);
    channels[platform] = { format: 'native-binary', url: `https://github.com/${repository}/releases/download/v${version}/${rawName}`, sha256: rawHash, bytes: fs.statSync(raw).size };
  }
  const root = path.resolve(__dirname, '..');
  fs.copyFileSync(path.join(root, 'LICENSE'), path.join(staging, 'LICENSE'));
  fs.copyFileSync(path.join(__dirname, 'npm', 'rdh.cjs'), path.join(staging, 'rdh.cjs'));
  fs.writeFileSync(path.join(staging, 'package.json'), `${JSON.stringify({
    name: '@easelian/remote-device-agent', version, description: 'Native Remote Device Hub Agent',
    license: 'MIT', repository: { type: 'git', url: `https://github.com/${repository}.git` },
    engines: { node: '>=22.14.0' }, bin: { rdh: 'rdh.cjs', 'remote-device-agent': 'rdh.cjs', 'remote-device-hub': 'rdh.cjs' },
    files: ['rdh.cjs', 'native', 'LICENSE'], publishConfig: { access: 'public' },
  }, null, 2)}\n`);
  const packed = JSON.parse(execFileSync('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', path.resolve(output)], { cwd: staging, encoding: 'utf8' }));
  if (packed.length !== 1 || !/^easelian-remote-device-agent-.*\.tgz$/.test(packed[0].filename)) fail('Unexpected npm pack output');
  assets.push(packed[0].filename);
  for (const name of ['install.sh', 'install.ps1']) {
    fs.copyFileSync(path.join(__dirname, name), path.join(output, name), fs.constants.COPYFILE_EXCL);
    assets.push(name);
  }
  const indexName = `rdh-${version}.canary.json`;
  fs.writeFileSync(path.join(output, indexName), `${JSON.stringify({schema: 2, channel: 'canary', version, rollout: {percent: 100}, platforms: channels}, null, 2)}\n`);
  assets.push(indexName);
  const sorted = assets.sort();
  fs.writeFileSync(path.join(output, 'SHA256SUMS'), sorted.map((name) => `${sha(path.join(output, name))}  ${name}\n`).join(''));
  console.log(`Prepared ${sorted.length} checked assets for ${version} at ${commit}`);
} finally {
  fs.rmSync(staging, { recursive: true, force: true });
}
