import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
import { parseArgs, serverFromArgs } from '../lib/bootstrap.mjs';

test('package runs directly on Node with no runtime dependencies or install hooks', () => {
  const metadata = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
  assert.match(metadata.version, /^\d+\.\d+\.\d+$/);
  assert.equal(metadata.engines.node, '>=22');
  assert.equal(metadata.private, undefined);
  assert.equal(metadata.bin['remote-device-agent'], 'bin/remote-device-hub.mjs');
  for (const field of ['dependencies', 'optionalDependencies', 'peerDependencies'])
    assert.deepEqual(metadata[field] ?? {}, {});
  for (const name of ['preinstall', 'install', 'postinstall', 'prepare'])
    assert.equal(metadata.scripts?.[name], undefined);
  assert(!metadata.files.includes('runtime/'));
});

test('gateway arguments reject credential-bearing or insecure remote origins', () => {
  assert.equal(serverFromArgs([]), 'https://xdistribute.site:8090');
  assert.equal(serverFromArgs(['--server', 'http://127.0.0.1:8765/']), 'http://127.0.0.1:8765');
  assert.equal(serverFromArgs(['--server=https://example.com']), 'https://example.com');
  for (const url of ['http://example.com', 'https://user:password@example.com', 'https://example.com/path',
    'https://example.com?token=secret', 'https://example.com#fragment'])
    assert.throws(() => serverFromArgs(['--server', url]));
});

test('repeated roots, profile and config survive argument parsing', () => {
  assert.deepEqual(parseArgs(['--root', 'work=/tmp/work', '--root=other=/tmp/other',
    '--profile', 'build', '--config=/tmp/agent.json', '--no-browser']),
  { server: 'https://xdistribute.site:8090', profile: 'build', noBrowser: true,
    root: ['work=/tmp/work', 'other=/tmp/other'], config: '/tmp/agent.json' });
  for (const args of [['--unknown'], ['--root'], ['--profile', '--no-browser']])
    assert.throws(() => parseArgs(args));
});

test('help and version run offline with empty PATH and no runtime setup', () => {
  const directory = mkdtempSync(join(tmpdir(), 'rdh-offline-help-'));
  const cli = fileURLToPath(new URL('../bin/remote-device-hub.mjs', import.meta.url));
  try {
    const env = { ...process.env, PATH: '', XDG_CACHE_HOME: directory, XDG_CONFIG_HOME: directory };
    const metadata = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
    assert.equal(execFileSync(process.execPath, [cli, '--version'], { env, cwd: directory, encoding: 'utf8' }).trim(), metadata.version);
    assert.match(execFileSync(process.execPath, [cli, '--help'], { env, cwd: directory, encoding: 'utf8' }), /直接运行 Node Agent/);
    assert.deepEqual(readdirSync(directory), []);
  } finally { rmSync(directory, { recursive: true, force: true }); }
});
