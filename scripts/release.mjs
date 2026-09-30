#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { gzipSync } from 'node:zlib';
import { existsSync, lstatSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, relative, resolve, sep } from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
export const MAX_ARCHIVE = 64 * 1024;
export const MAX_UNPACKED = 512 * 1024;
const PACKAGE = JSON.parse(readFileSync(join(ROOT, 'package.json'), 'utf8'));
const RELEASE_SCRIPT = fileURLToPath(import.meta.url);
export const WS_CLIENT_FILES = Object.freeze([
  'LICENSE', 'package.json', 'vendor.json',
  'lib/buffer-util.js', 'lib/constants.js', 'lib/event-target.js', 'lib/extension.js',
  'lib/limiter.js', 'lib/permessage-deflate.js', 'lib/receiver.js', 'lib/sender.js',
  'lib/validation.js', 'lib/websocket.js',
]);
export const REQUIRED_CHECKS = Object.freeze([
  'package-integrity', 'install-cli', 'binding-reuse', 'protocol-compatibility',
]);

function sha256(data) { return createHash('sha256').update(data).digest('hex'); }
function privateName(name) { return name.startsWith('.') || name.includes(`${sep}.`) || name.includes(`${sep}..`); }

export function clientFiles() {
  const manifestPath = join(ROOT, 'vendor/ws/vendor.json');
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  if (manifest.name !== 'ws' || manifest.version !== '8.22.0' || manifest.license !== 'MIT' ||
      !/^https:\/\/registry\.npmjs\.org\/ws\/-\/ws-[0-9.]+\.tgz$/.test(manifest.url) ||
      !/^sha512-[A-Za-z0-9+/]+=*$/.test(manifest.integrity) ||
      !/^[0-9a-f]{64}$/.test(manifest.archiveSha256) || JSON.stringify(manifest.clientFiles) !== JSON.stringify(WS_CLIENT_FILES)) {
    throw new Error('Vendored ws provenance is invalid');
  }
  return { manifest, names: [...WS_CLIENT_FILES] };
}

export function sourceFiles() {
  const files = new Map();
  const add = (name) => {
    const path = join(ROOT, name);
    if (!existsSync(path) || !lstatSync(path).isFile() || lstatSync(path).isSymbolicLink())
      throw new Error(`Missing or symlinked source file: ${name}`);
    files.set(name, readFileSync(path));
  };
  add('package.json');
  add('README.md');
  add('bin/remote-device-hub.mjs');
  for (const directory of ['lib']) {
    for (const entry of readdirSync(join(ROOT, directory)).sort()) {
      if (entry.endsWith('.mjs')) add(`${directory}/${entry}`);
    }
  }
  const vendor = clientFiles();
  for (const name of vendor.names) add(`vendor/ws/${name}`);
  return { files, vendorManifest: vendor.manifest };
}

function gitState() {
  const run = (args) => { try { return execFileSync('git', args, { cwd: ROOT, encoding: 'utf8' }).trim(); } catch { return 'unavailable'; } };
  return { head: run(['rev-parse', 'HEAD']), status: run(['status', '--porcelain', '--untracked-files=all']) };
}

function octal(value, length) { return value.toString(8).padStart(length - 1, '0') + '\0'; }
function tarEntry(name, data) {
  const header = Buffer.alloc(512, 0);
  header.write(name, 0, 100, 'utf8');
  header.write(octal(0o644, 8), 100, 8, 'ascii');
  header.write(octal(0, 8), 108, 8, 'ascii');
  header.write(octal(0, 8), 116, 8, 'ascii');
  header.write(octal(data.length, 12), 124, 12, 'ascii');
  header.write(octal(0, 12), 136, 12, 'ascii');
  header.fill(0x20, 148, 156);
  header[156] = 0x30;
  header.write('ustar\0', 257, 6, 'ascii');
  header.write('00', 263, 2, 'ascii');
  header.write('root', 265, 32, 'ascii');
  header.write('root', 297, 32, 'ascii');
  let checksum = 0;
  for (const byte of header) checksum += byte;
  header.write(checksum.toString(8).padStart(6, '0') + '\0 ', 148, 8, 'ascii');
  const padding = Buffer.alloc((512 - (data.length % 512)) % 512);
  return Buffer.concat([header, data, padding]);
}

export function makeArchive(files) {
  const entries = [...files.entries()].sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0);
  const unpackedBytes = entries.reduce((sum, [, data]) => sum + data.length, 0);
  if (unpackedBytes > MAX_UNPACKED) throw new Error(`Unpacked package exceeds ${MAX_UNPACKED} bytes`);
  const tar = Buffer.concat([...entries.map(([name, data]) => tarEntry(`package/${name}`, data)), Buffer.alloc(1024)]);
  const archive = gzipSync(tar, { level: 9, mtime: 0 });
  // zlib writes a platform-specific OS byte; 255 keeps the gzip header portable.
  archive[9] = 255;
  if (archive.length > MAX_ARCHIVE) throw new Error(`Compressed package exceeds ${MAX_ARCHIVE} bytes`);
  return { archive, unpackedBytes };
}

export function validatePackage(files, vendorManifest) {
  const metadata = JSON.parse(files.get('package.json').toString('utf8'));
  if (metadata.name !== 'remote-device-hub' || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(metadata.version) || metadata.engines?.node !== '>=22')
    throw new Error('Package identity/version/Node engine is invalid');
  for (const key of ['dependencies', 'optionalDependencies', 'peerDependencies'])
    if (metadata[key] && Object.keys(metadata[key]).length) throw new Error(`Runtime ${key} are forbidden`);
  for (const key of ['preinstall', 'install', 'postinstall', 'prepare', 'prepublish', 'postpublish'])
    if (metadata.scripts?.[key]) throw new Error(`Lifecycle hook ${key} is forbidden`);
  if (metadata.private === true || metadata.bin?.['remote-device-agent'] !== 'bin/remote-device-hub.mjs')
    throw new Error('Package must preserve its public identity and bin aliases');
  if (JSON.stringify(vendorManifest.clientFiles) !== JSON.stringify(WS_CLIENT_FILES))
    throw new Error('Vendored ws client allowlist differs from the fixed client-only boundary');
  for (const name of files.keys()) if (privateName(name) || name.includes('runtime/') || name.endsWith('.py') || name.endsWith('.node'))
    throw new Error(`Forbidden package entry: ${name}`);
  return metadata;
}

export async function build({ output = join(ROOT, 'dist'), record = join(ROOT, 'releases', `${PACKAGE.name}-${PACKAGE.version}.json`) } = {}) {
  const { files, vendorManifest } = sourceFiles();
  const metadata = validatePackage(files, vendorManifest);
  const { archive, unpackedBytes } = makeArchive(files);
  await mkdir(output, { recursive: true });
  await mkdir(dirname(record), { recursive: true });
  const artifact = join(output, `${metadata.name}-${metadata.version}.tgz`);
  if (existsSync(artifact) && !readFileSync(artifact).equals(archive)) throw new Error(`Refusing to replace ${artifact}; increase version`);
  if (!existsSync(artifact)) await writeFile(artifact, archive, { flag: 'wx' });
  const source = Object.fromEntries([...files.entries()].map(([name, data]) => [name, sha256(data)]));
  const sourceSha256 = sha256(Buffer.from(JSON.stringify(source, Object.keys(source).sort()) + '\n'));
  const release = { component: 'agent', name: metadata.name, version: metadata.version, stage: 'prepared',
    source: { sha256: sourceSha256, files: source },
    artifact: { path: relative(ROOT, artifact).split(sep).join('/'), sha256: sha256(archive), bytes: archive.length, unpackedBytes },
    build: { node: process.version, builderSha256: sha256(readFileSync(RELEASE_SCRIPT)),
      vendorManifestSha256: sha256(readFileSync(join(ROOT, 'vendor/ws/vendor.json'))), git: gitState() },
    checks: {}, identities: {}, events: [{ stage: 'prepared', at: new Date().toISOString() }] };
  if (existsSync(record)) {
    const previous = JSON.parse(readFileSync(record, 'utf8'));
    if (JSON.stringify(previous.artifact) !== JSON.stringify(release.artifact) || previous.source.sha256 !== sourceSha256)
      throw new Error(`Refusing to replace release record ${record}; increase version`);
  } else await writeFile(record, JSON.stringify(release, null, 2) + '\n', { flag: 'wx' });
  return { artifact, record, release };
}

export async function check(recordPath) {
  const record = JSON.parse(await readFile(recordPath, 'utf8'));
  const artifact = resolve(ROOT, record.artifact.path);
  if (!existsSync(artifact) || !lstatSync(artifact).isFile() || lstatSync(artifact).isSymbolicLink() || !artifact.endsWith('.tgz') ||
      sha256(await readFile(artifact)) !== record.artifact.sha256)
    throw new Error('Recorded artifact checksum does not match');
  const { files, vendorManifest } = sourceFiles();
  const metadata = validatePackage(files, vendorManifest);
  const currentBuilderSha256 = sha256(readFileSync(RELEASE_SCRIPT));
  const currentVendorManifestSha256 = sha256(readFileSync(join(ROOT, 'vendor/ws/vendor.json')));
  if (record.build?.builderSha256 !== currentBuilderSha256 || record.build?.vendorManifestSha256 !== currentVendorManifestSha256)
    throw new Error('Recorded builder or vendor manifest fingerprint does not match');
  const source = Object.fromEntries([...files.entries()].map(([name, data]) => [name, sha256(data)]));
  const sourceSha256 = sha256(Buffer.from(JSON.stringify(source, Object.keys(source).sort()) + '\n'));
  const rebuilt = makeArchive(files);
  if (metadata.name !== record.name || metadata.version !== record.version || sourceSha256 !== record.source.sha256 ||
      rebuilt.archive.length !== record.artifact.bytes || rebuilt.unpackedBytes !== record.artifact.unpackedBytes ||
      sha256(rebuilt.archive) !== record.artifact.sha256) throw new Error('Recorded source or deterministic artifact does not match');
  return record;
}

export async function recordPublication(recordPath, stage, { host, evidence = [], checks = {}, note = '' } = {}) {
  if (!host) throw new Error('A real publication/deployment host is required');
  const record = await check(recordPath);
  const allowed = {
    prepared: new Set(['prepared', 'published', 'deployed', 'failed']),
    published: new Set(['published', 'deployed', 'verified', 'failed']),
    deployed: new Set(['deployed', 'verified', 'failed']),
    verified: new Set(['verified']),
    failed: new Set(['published', 'deployed', 'failed']),
  };
  if (!allowed[record.stage]?.has(stage)) throw new Error(`Unsupported release transition ${record.stage} -> ${stage}`);
  const evidenceRecords = evidence.map((value) => {
    const path = resolve(ROOT, value);
    if (!path.startsWith(ROOT + sep) || !existsSync(path) || lstatSync(path).isSymbolicLink() || !lstatSync(path).isFile())
      throw new Error(`Evidence must be a regular file inside the repository: ${value}`);
    return { path: relative(ROOT, path).split(sep).join('/'), sha256: sha256(readFileSync(path)) };
  });
  if (!evidenceRecords.length) throw new Error('At least one evidence file is required');
  for (const [key, value] of Object.entries(checks)) {
    if (!['passed', 'failed', 'unverified'].includes(value)) throw new Error(`Invalid check result for ${key}`);
  }
  const nextChecks = { ...record.checks, ...checks };
  if (stage === 'verified' && REQUIRED_CHECKS.some((key) => nextChecks[key] !== 'passed')) {
    throw new Error(`Required checks have not all passed: ${REQUIRED_CHECKS.join(', ')}`);
  }
  record.stage = stage;
  record.checks = nextChecks;
  record.events.push({ stage, at: new Date().toISOString(), host, checks, evidence: evidenceRecords, note });
  const temporary = `${recordPath}.tmp`;
  await writeFile(temporary, JSON.stringify(record, null, 2) + '\n', { flag: 'wx' });
  const { rename } = await import('node:fs/promises');
  await rename(temporary, recordPath);
  return record;
}

const args = process.argv.slice(2);
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const command = args.shift();
  const value = (flag, fallback) => { const index = args.indexOf(flag); return index < 0 ? fallback : args[index + 1]; };
  try {
    const defaultRecord = join(ROOT, 'releases', `${PACKAGE.name}-${PACKAGE.version}.json`);
    if (command === 'build') { const result = await build({ output: value('--output', join(ROOT, 'dist')), record: value('--record', defaultRecord) }); console.log(`Prepared ${result.artifact}`); }
    else if (command === 'check') { const record = await check(args[0]); console.log(`Checksum OK; stage=${record.stage}`); }
    else if (command === 'record') {
      const recordPath = args[0];
      const stage = value('--stage');
      const host = value('--host');
      const flagValues = (flag) => args.flatMap((item, index) => item === flag && args[index + 1] ? [args[index + 1]] : []);
      const evidence = flagValues('--evidence');
      const checks = Object.fromEntries(flagValues('--check').map((item) => item.split('=', 2)));
      const result = await recordPublication(recordPath, stage, { host, evidence, checks, note: value('--note', '') });
      console.log(`Recorded ${result.stage}: ${recordPath}`);
    }
    else throw new Error('Usage: release.mjs build [--output DIR] [--record FILE] | check RECORD | record RECORD --stage STAGE --host HOST --evidence FILE');
  } catch (error) { console.error(`Release rejected: ${error.message}`); process.exitCode = 1; }
}
