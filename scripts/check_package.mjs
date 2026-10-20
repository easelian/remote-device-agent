#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtemp, readFile, readdir, rm, stat } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { ROOT, sourceFiles, MAX_ARCHIVE, MAX_UNPACKED } from './release.mjs';

const npm = process.env.npm_execpath;
if (!npm) throw new Error('Run npm run pack:check so the active npm CLI is explicit.');
const directory = await mkdtemp(path.join(os.tmpdir(), 'rdh-package-check-'));
const run = (command, args, options = {}) => {
  const result = spawnSync(command, args, { encoding: 'utf8', cwd: ROOT, timeout: 60_000,
    maxBuffer: 2 * 1024 * 1024, ...options });
  if (result.error || result.status !== 0) throw new Error(result.error?.message || result.stderr || result.stdout);
  return result.stdout;
};
try {
  const [packed] = JSON.parse(run(process.execPath, [npm, 'pack', '.', '--ignore-scripts', '--json',
    '--offline', '--cache', path.join(directory, 'cache'), '--pack-destination', directory]));
  assert.deepEqual(packed.files.map(file => file.path).sort(), [...sourceFiles().files.keys()].sort());
  assert.ok(packed.size <= MAX_ARCHIVE, `npm TGZ exceeds ${MAX_ARCHIVE} bytes: ${packed.size}`);
  assert.ok(packed.unpackedSize <= MAX_UNPACKED);
  const archive = path.join(directory, packed.filename);
  const prefix = path.join(directory, 'installed');
  run(process.execPath, [npm, 'install', '--global', '--prefix', prefix, '--cache', path.join(directory, 'empty-cache'),
    '--registry', 'http://127.0.0.1:9', '--offline', '--ignore-scripts', '--no-audit', '--no-fund', archive]);
  const installed = process.platform === 'win32' ? path.join(prefix, 'node_modules', packed.name)
    : path.join(prefix, 'lib', 'node_modules', packed.name);
  assert.ok(!(await readdir(installed)).includes('node_modules'), 'Consumers must not install runtime dependencies');
  const metadata = JSON.parse(await readFile(path.join(installed, 'package.json'), 'utf8'));
  const env = { ...process.env, PATH: '', NODE_PATH: '', XDG_CONFIG_HOME: path.join(directory, 'state'),
    XDG_CACHE_HOME: path.join(directory, 'state') };
  for (const [alias, bin] of Object.entries(metadata.bin)) {
    const cli = path.join(installed, bin);
    assert.equal(run(process.execPath, [cli, '--version'], { cwd: directory, env }).trim(), metadata.version);
    assert.match(run(process.execPath, [cli, '--help'], { cwd: directory, env }), /Node Agent/);
    const shim = process.platform === 'win32' ? path.join(prefix, `${alias}.cmd`) : path.join(prefix, 'bin', alias);
    assert.ok((await stat(shim)).isFile(), 'npm must create both CLI aliases');
    if (process.platform !== 'win32') assert.equal(run(shim, ['--version'], { cwd: directory,
      env: { ...env, PATH: path.dirname(process.execPath) } }).trim(), metadata.version);
  }
  console.log(JSON.stringify({ npmArchiveBytes: packed.size, unpackedBytes: packed.unpackedSize,
    entries: packed.files.length, offlineInstall: 'passed', runtimeDependencies: 0,
    installScripts: 'disabled', cliAliases: 'passed', integrity: packed.integrity }, null, 2));
} finally {
  await rm(directory, { recursive: true, force: true });
}
