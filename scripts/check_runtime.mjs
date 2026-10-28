#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const platforms = [
  'linux-x64-gnu',
  'linux-arm64-gnu',
  'linux-x64-musl',
  'linux-arm64-musl',
  'darwin-x64',
  'darwin-arm64',
  'win32-x64',
  'win32-arm64',
];
const directory = path.resolve(process.argv[2] || 'dist');
const platform = process.argv[3];
const manifest = JSON.parse(readFileSync(path.join(directory, 'rdh-manifest.json'), 'utf8'));
assert.deepEqual(manifest.artifacts.map((a) => a.platform).sort(), [...platforms].sort());
const sums = readFileSync(path.join(directory, 'SHA256SUMS'), 'utf8');
for (const artifact of manifest.artifacts) {
  const extension = artifact.platform.startsWith('win32-') ? 'zip' : 'tar.gz';
  assert.equal(artifact.file, `rdh-${manifest.version}-${artifact.platform}.${extension}`);
  const bytes = readFileSync(path.join(directory, artifact.file));
  assert.equal(bytes.length, artifact.bytes);
  assert.equal(createHash('sha256').update(bytes).digest('hex'), artifact.sha256);
  assert.ok(sums.split('\n').includes(`${artifact.sha256}  ${artifact.file}`));
}
console.log('All eight runtime archives and checksums verified.');
if (platform) {
  const artifact = manifest.artifacts.find((a) => a.platform === platform);
  assert.ok(artifact, `Unknown platform: ${platform}`);
  const [expectedOS, expectedArch] = platform.split('-');
  assert.equal(process.platform, expectedOS);
  assert.equal(process.arch, expectedArch);
  const temporary = mkdtempSync(path.join(os.tmpdir(), 'rdh runtime check '));
  const windows = process.platform === 'win32';
  const powershell = path.join(
    process.env.SystemRoot || '',
    'System32/WindowsPowerShell/v1.0/powershell.exe',
  );
  const run = (command, args, options = {}) => {
    const result = spawnSync(command, args, { encoding: 'utf8', timeout: 120_000, ...options });
    assert.equal(result.error, undefined, result.error?.message);
    assert.equal(result.status, 0, result.stderr || result.stdout);
    return result.stdout.trim();
  };
  try {
    const archive = path.join(directory, artifact.file);
    if (windows) {
      run(
        powershell,
        [
          '-NoProfile',
          '-NonInteractive',
          '-Command',
          'Expand-Archive -LiteralPath $env:RDH_ARCHIVE -DestinationPath $env:RDH_TARGET',
        ],
        { env: { ...process.env, RDH_ARCHIVE: archive, RDH_TARGET: temporary } },
      );
    } else {
      run('tar', ['-xzf', archive, '-C', temporary]);
    }
    const tree = path.join(temporary, `rdh-${manifest.version}-${platform}`);
    const node = path.join(tree, windows ? 'node.exe' : 'node');
    assert.equal(run(node, ['-p', 'process.arch']), expectedArch);
    assert.equal(run(node, ['-p', 'process.versions.node']), manifest.node);
    const noNode = path.join(temporary, 'no-system-node');
    mkdirSync(noNode);
    writeFileSync(path.join(noNode, 'node'), '#!/bin/sh\nexit 99\n', { mode: 0o755 });
    const env = {
      ...process.env,
      PATH: windows ? '' : `${noNode}${path.delimiter}${process.env.PATH}`,
      NODE_PATH: '',
    };
    const command = windows
      ? path.join(process.env.SystemRoot, 'System32/cmd.exe')
      : path.join(tree, 'rdh');
    const args = windows ? ['/d', '/c', 'rdh.cmd', '--version'] : ['--version'];
    assert.equal(run(command, args, { cwd: tree, env }), manifest.version);
    const prefix = path.join(temporary, 'installed runtime');
    const bin = path.join(temporary, 'installed bin');
    if (windows) {
      run(powershell, [
        '-NoProfile',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        path.join(directory, 'install.ps1'),
        '-Base',
        directory,
        '-Prefix',
        prefix,
        '-BinDir',
        bin,
        '-NoPair',
      ]);
      assert.equal(
        run(command, ['/d', '/c', 'rdh.cmd', '--version'], { cwd: bin, env }),
        manifest.version,
      );
    } else {
      run('sh', [
        path.join(directory, 'install.sh'),
        '--base',
        pathToFileURL(directory).href,
        '--prefix',
        prefix,
        '--bin-dir',
        bin,
        '--no-pair',
      ]);
      assert.equal(run(path.join(bin, 'rdh'), ['--version'], { env }), manifest.version);
    }
    console.log(`${platform}: bundled Node.js, launcher without PATH, and installer passed.`);
  } finally {
    // Every fixture command has exited via spawnSync before this cleanup.
    // Allow bounded Windows EBUSY/EPERM cleanup retries, as in the startup
    // fixture. Retry only this tree; a persistent lock still fails the check.
    rmSync(temporary, {
      recursive: true,
      force: true,
      maxRetries: windows ? 5 : 0,
      retryDelay: 1000,
    });
  }
}
