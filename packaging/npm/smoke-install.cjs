'use strict';

// Serve the exact release tarballs from a read-only, loopback npm registry.
const assert = require('node:assert/strict');
const { execFile, execFileSync } = require('node:child_process');
const { createHash } = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { promisify } = require('node:util');
const { name, packageVersions, platformVersion, tarballName } = require('./packages.cjs');

async function smokeInstall(directory, version, platforms, executePlatform) {
  const tag = version.includes('-') ? 'next' : 'latest';
  const entry = tag === 'next' ? `${name}@next` : name;
  const downloaded = [];
  const versions = {};
  const tarballs = new Map();
  const server = http.createServer((request, response) => {
    const url = decodeURIComponent(request.url);
    if (request.method !== 'GET') {
      response.writeHead(405).end();
    } else if (url === `/${name}`) {
      response.setHeader('Content-Type', 'application/json');
      response.end(JSON.stringify({ name, 'dist-tags': { [tag]: version }, versions }));
    } else if (tarballs.has(url)) {
      const content = tarballs.get(url);
      downloaded.push(url);
      response.setHeader('Content-Length', content.length);
      response.end(content);
    } else {
      response.writeHead(404).end();
    }
  });
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'rdh-npm-smoke-'));
  try {
    await new Promise((resolve, reject) => {
      server.once('error', reject);
      server.listen(0, '127.0.0.1', resolve);
    });
    const registry = `http://127.0.0.1:${server.address().port}`;
    for (const pkgVersion of packageVersions(version)) {
      const file = path.join(directory, tarballName(pkgVersion));
      const manifest = JSON.parse(execFileSync('tar', ['-xOf', file, 'package/package.json']));
      assert.equal(manifest.name, name);
      assert.equal(manifest.version, pkgVersion);
      const content = fs.readFileSync(file);
      const url = `/${tarballName(pkgVersion)}`;
      tarballs.set(url, content);
      versions[pkgVersion] = { ...manifest, dist: {
        tarball: registry + url,
        integrity: `sha512-${createHash('sha512').update(content).digest('base64')}`,
      } };
    }
    const npmrc = path.join(temp, 'npmrc');
    fs.writeFileSync(npmrc, `${name.split('/')[0]}:registry=${registry}\n`);
    for (const platform of platforms) {
      const [os, cpu, linuxAbi] = platform.split('-');
      const prefix = path.join(temp, platform);
      const platformFlags = platform === executePlatform ? [] : [
        '--os', os, '--cpu', cpu, ...(linuxAbi ? ['--libc', linuxAbi === 'gnu' ? 'glibc' : linuxAbi] : []),
      ];
      downloaded.length = 0;
      await promisify(execFile)('npm', [
        'install', '--global', '--prefix', prefix, '--cache', path.join(prefix, 'cache'),
        '--registry', registry, '--userconfig', npmrc, '--globalconfig', '/dev/null',
        '--ignore-scripts', '--no-audit', '--no-fund',
        ...platformFlags, entry,
      ], { timeout: 60000 });
      assert.deepEqual(downloaded.sort(), [
        `/${tarballName(version)}`, `/${tarballName(platformVersion(version, platform))}`,
      ].sort(), `Unexpected npm tarball downloads for ${platform}`);
      if (platform === executePlatform) {
        const actual = execFileSync(path.join(prefix, 'bin', 'rdh'), ['--version'], { encoding: 'utf8' });
        assert.equal(actual.trim(), version);
      }
      console.log(`npm fetched only the launcher and ${platform}`);
    }
  } finally {
    server.close();
    fs.rmSync(temp, { recursive: true, force: true });
  }
}

if (require.main === module) {
  const [directory, version] = process.argv.slice(2);
  const platform = `linux-${process.arch}-${process.report.getReport().header.glibcVersionRuntime ? 'gnu' : 'musl'}`;
  smokeInstall(directory, version, [platform], platform).catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
}

module.exports = { smokeInstall };
