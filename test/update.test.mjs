import assert from 'node:assert/strict';
import test from 'node:test';
import {
  cohortBucket, compareVersions, decideUpgrade, detectPlatform, parseChannel,
} from '../.build/update.js';

const artifact = { url: 'https://example.test/rdh-0.8.0-linux-x64-gnu.tar.gz', sha256: 'a'.repeat(64) };
const index = (overrides = {}) => ({
  version: '0.8.0',
  platforms: { 'linux-x64-gnu': artifact },
  ...overrides,
});

test('versions compare numerically, and a prerelease precedes its release', () => {
  assert.equal(compareVersions('0.7.0', '0.7.1'), -1);
  assert.equal(compareVersions('0.7.1', '0.7.1'), 0);
  assert.equal(compareVersions('0.10.0', '0.9.9'), 1);
  assert.equal(compareVersions('0.8.0-rc.1', '0.8.0'), -1);
  assert.equal(compareVersions('v0.8.0', '0.8.0'), 0);
});

test('cohort buckets are stable, bounded and spread out', () => {
  const bucket = cohortBucket('0123456789abcdef0123456789abcdef');
  assert.ok(bucket >= 0 && bucket < 100);
  assert.equal(bucket, cohortBucket('0123456789abcdef0123456789abcdef'));
  const seen = new Set();
  for (let i = 0; i < 200; i += 1) seen.add(cohortBucket(String(i)));
  assert.ok(seen.size > 50, `expected spread, got ${seen.size}`);
});

test('platform detection follows libc, architecture and OS', () => {
  assert.equal(detectPlatform('linux', 'x64', '2.38'), 'linux-x64-gnu');
  assert.equal(detectPlatform('linux', 'arm64', '2.28'), 'linux-arm64-gnu');
  assert.equal(detectPlatform('linux', 'x64', '2.17'), 'linux-x64-musl');
  assert.equal(detectPlatform('linux', 'x64', undefined), 'linux-x64-musl');
  assert.equal(detectPlatform('darwin', 'arm64', undefined), 'darwin-arm64');
});

test('channel indexes are validated', () => {
  assert.equal(parseChannel(index()).version, '0.8.0');
  assert.throws(() => parseChannel({ version: 'nope', platforms: {} }), /version/);
  assert.throws(() => parseChannel({ version: '0.8.0', platforms: {} }), /platforms/);
  assert.throws(() => parseChannel({ version: '0.8.0', platforms: { 'linux-x64-gnu': { url: 'x', sha256: 'short' } } }), /sha256/);
  assert.throws(() => parseChannel(index({ rollout: { percent: 101 } })), /percent/);
});

test('an older index never downgrades a device', () => {
  const decision = decideUpgrade(parseChannel(index({ version: '0.7.9' })), {
    current: '0.8.0', platform: 'linux-x64-gnu', installId: 'x', channel: 'stable',
  });
  assert.equal(decision.action, 'none');
});

test('a version without the device platform is blocked', () => {
  const decision = decideUpgrade(parseChannel(index()), {
    current: '0.7.0', platform: 'darwin-arm64', installId: 'x', channel: 'stable',
  });
  assert.equal(decision.action, 'blocked');
});

test('min_from prevents skipping intermediate versions', () => {
  const decision = decideUpgrade(parseChannel(index({ min_from: '0.7.5' })), {
    current: '0.7.0', platform: 'linux-x64-gnu', installId: 'x', channel: 'stable',
  });
  assert.equal(decision.action, 'blocked');
});

test('rollout covers only its percentage, unless forced or on canary', () => {
  const id = 'device-1';
  const bucket = cohortBucket(id);
  const percent = bucket; // this device is the first one outside the rollout
  const entry = parseChannel(index({ rollout: { percent } }));
  const context = { current: '0.7.0', platform: 'linux-x64-gnu', installId: id, channel: 'stable' };
  assert.equal(decideUpgrade(entry, context).action, 'none');
  assert.equal(decideUpgrade(entry, { ...context, channel: 'canary' }).action, 'upgrade');
  assert.equal(decideUpgrade(entry, { ...context, force: true }).action, 'upgrade');
});

test('a full rollout upgrades and carries the artifact', () => {
  const decision = decideUpgrade(parseChannel(index({ rollout: { percent: 100 } })), {
    current: '0.7.0', platform: 'linux-x64-gnu', installId: 'x', channel: 'stable',
  });
  assert.equal(decision.action, 'upgrade');
  assert.equal(decision.artifact.sha256, artifact.sha256);
});
