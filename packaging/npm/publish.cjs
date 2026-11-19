'use strict';

const { execFileSync } = require('node:child_process');
const { createHash } = require('node:crypto');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { setTimeout: delay } = require('node:timers/promises');
const { name, packageVersions, tarballName } = require('./packages.cjs');

const registry = 'https://registry.npmjs.org/';
const validVersion = /^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

async function registryIntegrity(version) {
  const response = await fetch(`${registry}${encodeURIComponent(name)}/${version}`, {
    headers: { 'Cache-Control': 'no-cache' },
    signal: AbortSignal.timeout(15000),
  });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`npm registry returned HTTP ${response.status} for ${name}@${version}`);
  const record = await response.json();
  if (record.name !== name || record.version !== version || !record.dist?.integrity) {
    throw new Error(`Unexpected npm registry response for ${name}@${version}`);
  }
  return record.dist.integrity;
}

function npmPublish(file, tag) {
  execFileSync('npm', ['publish', file, '--registry', registry, '--access', 'public', '--tag', tag, '--ignore-scripts'], {
    stdio: 'inherit',
  });
}

async function publishRelease(directory, version, {
  getIntegrity = registryIntegrity, publish = npmPublish, wait = delay,
} = {}) {
  const packages = packageVersions(version).map((pkgVersion) => {
    const file = join(directory, tarballName(pkgVersion));
    const integrity = `sha512-${createHash('sha512').update(readFileSync(file)).digest('base64')}`;
    const tag = pkgVersion !== version ? 'native' : version.includes('-') ? 'next' : 'latest';
    return { pkgVersion, file, integrity, tag };
  });

  function verify(pkg, existing) {
    if (existing === null) return false;
    if (existing !== pkg.integrity) {
      throw new Error(`Published integrity differs for ${name}@${pkg.pkgVersion}`);
    }
    console.log(`Verified ${name}@${pkg.pkgVersion} in npm`);
    return true;
  }

  async function publishBatch(batch) {
    const existing = await Promise.all(batch.map(({ pkgVersion }) => getIntegrity(pkgVersion)));
    let pending = batch.filter((pkg, index) => !verify(pkg, existing[index]));
    for (const { file, tag } of pending) publish(file, tag);

    // npm scans newly uploaded packages before making them visible.
    for (let attempt = 0; pending.length && attempt <= 40; attempt++) {
      if (attempt) await wait(30000);
      const available = await Promise.all(pending.map(({ pkgVersion }) => getIntegrity(pkgVersion)));
      pending = pending.filter((pkg, index) => !verify(pkg, available[index]));
    }
    if (pending.length) {
      throw new Error(`Versions not visible in npm: ${pending.map(({ pkgVersion }) => `${name}@${pkgVersion}`).join(', ')}`);
    }
  }

  await publishBatch(packages.slice(0, -1));
  await publishBatch(packages.slice(-1));
}

if (require.main === module) {
  const [command, version, directory] = process.argv.slice(2);
  if (!validVersion.test(version || '')) throw new Error('Expected a release version');
  if (command === 'list' && !directory) {
    console.log(packageVersions(version).map(tarballName).join('\n'));
  } else if (command === 'publish' && directory) {
    publishRelease(directory, version).catch((error) => {
      console.error(error.message);
      process.exitCode = 1;
    });
  } else {
    throw new Error('Usage: publish.cjs list VERSION | publish VERSION VERIFIED_DIRECTORY');
  }
}

module.exports = { publishRelease };
