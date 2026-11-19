'use strict';

const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { packageVersions, tarballName } = require('./packages.cjs');
const { publishRelease } = require('./publish.cjs');

function fixture(t, version) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'rdh-npm-publish-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const packages = packageVersions(version);
  const entries = new Map();
  for (const pkgVersion of packages) {
    const file = path.join(directory, tarballName(pkgVersion));
    fs.writeFileSync(file, pkgVersion);
    const integrity = `sha512-${createHash('sha512').update(pkgVersion).digest('base64')}`;
    entries.set(file, { pkgVersion, integrity });
  }
  const registry = new Map();
  const writes = [];
  const publish = (file, tag) => {
    const { pkgVersion, integrity } = entries.get(file);
    writes.push([pkgVersion, tag]);
    registry.set(pkgVersion, integrity);
  };
  const getIntegrity = async (pkgVersion) => registry.get(pkgVersion) ?? null;
  return { directory, packages, writes, registry, getIntegrity, publish };
}

for (const [version, tag] of [['1.2.3', 'latest'], ['1.2.3-rc.1', 'next']]) {
  test(`publishes every platform before the launcher at ${tag}, and can resume`, async (t) => {
    const { directory, packages, writes, getIntegrity, publish } = fixture(t, version);
    const nativeBuilds = packages.slice(0, -1);
    publish(path.join(directory, tarballName(nativeBuilds[0])), 'native');
    writes.length = 0;
    await publishRelease(directory, version, { getIntegrity, publish });
    assert.deepEqual(writes, [...nativeBuilds.slice(1).map((item) => [item, 'native']), [version, tag]]);
    writes.length = 0;
    await publishRelease(directory, version, { getIntegrity, publish });
    assert.deepEqual(writes, []);
  });
}

test('waits for the platform version to be readable before advancing', async (t) => {
  const version = '1.2.3';
  const { directory, packages, writes, getIntegrity, publish } = fixture(t, version);
  let polls = 0;
  let waits = 0;
  await publishRelease(directory, version, {
    publish,
    wait: async () => { waits++; },
    getIntegrity: async (pkgVersion) => {
      if (pkgVersion === packages[0] && ++polls < 3) return null;
      if (pkgVersion !== packages[0]) assert.equal(polls, 3);
      return getIntegrity(pkgVersion);
    },
  });
  assert.equal(waits, 1);
  assert.deepEqual(writes.at(-1), [version, 'latest']);
});

test('blocks the launcher when a platform version is absent or has different bytes', async (t) => {
  const version = '1.2.3';
  const { directory, packages, registry, getIntegrity, publish } = fixture(t, version);
  await assert.rejects(publishRelease(directory, version, {
    getIntegrity, publish: () => {}, wait: async () => {},
  }), /is not visible/);
  registry.set(packages[0], 'sha512-mismatch');
  await assert.rejects(publishRelease(directory, version, { getIntegrity, publish }), /integrity differs/);
  assert.equal(registry.has(version), false);
});
