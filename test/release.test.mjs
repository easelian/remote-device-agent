import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { gunzipSync } from 'node:zlib';
import test from 'node:test';
import {
  build,
  check,
  MAX_ARCHIVE,
  MAX_UNPACKED,
  recordPublication,
  ROOT,
  sourceFiles,
} from '../scripts/release.mjs';
const metadata = JSON.parse(await readFile(join(ROOT, 'package.json'), 'utf8'));

test('build is deterministic, bounded, and records the current artifact', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'rdh-release-'));
  try {
    const first = await build({
      output: join(dir, 'dist'),
      record: join(dir, 'releases', 'agent.json'),
    });
    const bytes = await readFile(first.artifact);
    const second = await build({
      output: join(dir, 'dist'),
      record: join(dir, 'releases', 'agent.json'),
    });
    assert.deepEqual(await readFile(second.artifact), bytes);
    assert.equal(bytes[9], 255, 'gzip OS must be platform independent');
    assert.equal(first.release.version, metadata.version);
    assert.equal(first.artifact.endsWith(`${metadata.name}-${metadata.version}.tgz`), true);
    assert.ok(bytes.length <= MAX_ARCHIVE);
    assert.ok(first.release.artifact.unpackedBytes <= MAX_UNPACKED);
    await check(first.record);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('check rejects a tampered artifact', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'rdh-release-tamper-'));
  try {
    const result = await build({
      output: join(dir, 'dist'),
      record: join(dir, 'releases', 'agent.json'),
    });
    const bytes = await readFile(result.artifact);
    await writeFile(result.artifact, Buffer.concat([bytes, Buffer.from('tamper')]));
    await assert.rejects(() => check(result.record), /checksum/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('source allowlist excludes tests, scripts, runtime, and private files', async () => {
  const result = await build({
    output: join(tmpdir(), 'rdh-release-allowlist-dist'),
    record: join(tmpdir(), 'rdh-release-allowlist.json'),
  });
  const tar = gunzipSync(await readFile(result.artifact));
  const entries = [];
  for (let offset = 0; offset + 512 <= tar.length; ) {
    if (tar.subarray(offset, offset + 512).every((byte) => byte === 0)) {
      break;
    }
    entries.push(tar.toString('utf8', offset, offset + 100).replace(/\0.*$/, ''));
    const size = parseInt(
      tar
        .toString('ascii', offset + 124, offset + 136)
        .replace(/\0.*$/, '')
        .trim() || '0',
      8,
    );
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  assert.equal(entries.includes('package/test/release.test.mjs'), false);
  assert.equal(entries.includes('package/vendor/ws/lib/websocket-server.js'), false);
  assert.deepEqual(
    entries.filter((name) => name.startsWith('package/vendor/')),
    [],
  );
  assert.ok(entries.includes('package/bundle/agent.mjs'));
  assert.ok(entries.includes('package/THIRD_PARTY_LICENSES'));
  assert.deepEqual(
    entries
      .filter((name) => name.startsWith('package/'))
      .map((name) => name.slice('package/'.length))
      .sort(),
    [...sourceFiles().files.keys()].sort(),
  );
  await rm(result.artifact, { force: true });
  await rm(result.record, { force: true });
});

test('publication record enforces ordered stages and requires evidence', async () => {
  const dir = await mkdtemp(join(ROOT, '.release-stage-'));
  try {
    const evidence = join(dir, 'evidence.txt');
    await writeFile(evidence, 'local verification\n');
    const result = await build({
      output: join(dir, 'dist'),
      record: join(dir, 'releases', 'agent.json'),
    });
    await assert.rejects(
      () => recordPublication(result.record, 'verified', { host: 'local', evidence: [evidence] }),
      /transition/,
    );
    const published = await recordPublication(result.record, 'published', {
      host: 'local',
      evidence: [evidence],
      checks: { tests: 'passed' },
    });
    assert.equal(published.stage, 'published');
    await assert.rejects(
      () => recordPublication(result.record, 'verified', { host: 'local', evidence: [evidence] }),
      /Required checks/,
    );
    const verified = await recordPublication(result.record, 'verified', {
      host: 'local',
      evidence: [evidence],
      checks: {
        'package-integrity': 'passed',
        'install-cli': 'passed',
        'binding-reuse': 'passed',
        'protocol-compatibility': 'passed',
      },
    });
    assert.equal(verified.stage, 'verified');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('CLI record accepts repeated evidence and check options', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'rdh-release-cli-'));
  const evidenceDir = await mkdtemp(join(ROOT, '.release-cli-evidence-'));
  try {
    const evidence = join(evidenceDir, 'readback.txt');
    await writeFile(evidence, 'passed\n');
    const result = await build({
      output: join(dir, 'dist'),
      record: join(dir, 'releases', 'agent.json'),
    });
    execFileSync(
      process.execPath,
      [
        'scripts/release.mjs',
        'record',
        result.record,
        '--stage',
        'published',
        '--host',
        'local',
        '--evidence',
        evidence,
        '--check',
        'tests=passed',
      ],
      { cwd: ROOT, encoding: 'utf8' },
    );
    const updated = JSON.parse(await readFile(result.record, 'utf8'));
    assert.equal(updated.stage, 'published');
    assert.equal(updated.checks.tests, 'passed');
    assert.equal(updated.events.at(-1).evidence[0].path.endsWith('/readback.txt'), true);
  } finally {
    await rm(dir, { recursive: true, force: true });
    await rm(evidenceDir, { recursive: true, force: true });
  }
});
