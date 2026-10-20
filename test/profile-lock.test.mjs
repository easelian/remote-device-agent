import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import * as fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { acquireProfileLocks, reclaimProfileLocks } from '../.build/profile-lock.js';

const temp = () => fs.mkdtemp(path.join(os.tmpdir(), 'rdh-profile-lock-'));
const stale = (config, pid = 999999999) => fs.mkdir(`${config}.node-lock`, { mode: 0o700 }).then(() =>
  fs.writeFile(`${config}.node-lock/owner.json`, JSON.stringify({ pid, nonce: 'x'.repeat(43) }) + '\n', { mode: 0o600 }));

test('explicit reclaim removes a dead owner and permits a fresh acquire', async (t) => {
  const root = await temp(); t.after(() => fs.rm(root, { recursive: true, force: true }));
  const config = path.join(root, 'profile.json');
  await stale(config);
  assert.deepEqual((await reclaimProfileLocks([config])).reclaimed, [`${config}.node-lock`]);
  const handle = await acquireProfileLocks([config]);
  await handle.release();
});

test('reclaim refuses a live owner and never removes its lock', async (t) => {
  const root = await temp(); t.after(() => fs.rm(root, { recursive: true, force: true }));
  const config = path.join(root, 'profile.json');
  await stale(config, process.pid);
  await assert.rejects(() => reclaimProfileLocks([config]), /配置锁已存在/);
  assert.equal((await fs.stat(`${config}.node-lock`)).isDirectory(), true);
  await fs.rm(`${config}.node-lock`, { recursive: true });
});

test('reclaim guard blocks a fresh acquire and a second reclaimer', async (t) => {
  const root = await temp(); t.after(() => fs.rm(root, { recursive: true, force: true }));
  const config = path.join(root, 'profile.json');
  await stale(config);
  await fs.mkdir(`${config}.node-lock.reclaiming`, { mode: 0o700 });
  await assert.rejects(() => acquireProfileLocks([config]), /配置锁已存在/);
  await assert.rejects(() => reclaimProfileLocks([config]), /配置锁已存在/);
  await fs.rm(`${config}.node-lock.reclaiming`, { recursive: true });
  await reclaimProfileLocks([config]);
});

test('acquire and reclaim race through the shared guard without moving a fresh lock', async (t) => {
  const root = await temp(); t.after(() => fs.rm(root, { recursive: true, force: true }));
  const config = path.join(root, 'profile.json');
  await stale(config);
  const acquire = acquireProfileLocks([config]);
  await Promise.resolve();
  const reclaim = reclaimProfileLocks([config]);
  const outcomes = await Promise.allSettled([acquire, reclaim]);
  // The guard serializes the two operations.  Depending on which one wins,
  // reclaim may remove the stale lock before acquire creates a fresh one, so
  // both operations can legitimately fulfill.  A fresh lock must never be
  // moved out from under its owner.
  assert.ok(outcomes.some((item) => item.status === 'fulfilled'));
  if (outcomes[0].status === 'fulfilled') await outcomes[0].value.release();
  const fresh = await acquireProfileLocks([config]);
  await fresh.release();
});

test('SIGKILL leaves a recoverable lock without an automatic reclaim race', async (t) => {
  const root = await temp(); t.after(() => fs.rm(root, { recursive: true, force: true }));
  const config = path.join(root, 'profile.json');
  const modulePath = path.resolve('.build/profile-lock.js');
  const child = spawn(process.execPath, ['--input-type=module', '-e',
    `import { acquireProfileLocks } from ${JSON.stringify(modulePath)}; await acquireProfileLocks([${JSON.stringify(config)}]); console.log('ready'); setInterval(() => {}, 1000);`],
  { stdio: ['ignore', 'pipe', 'pipe'] });
  t.after(() => { try { child.kill('SIGKILL'); } catch {} });
  await once(child.stdout, 'data');
  child.kill('SIGKILL');
  await once(child, 'close');
  await assert.rejects(() => acquireProfileLocks([config]), /配置锁已存在/);
  await reclaimProfileLocks([config]);
  const handle = await acquireProfileLocks([config]);
  await handle.release();
});
