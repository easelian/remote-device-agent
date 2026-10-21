#!/usr/bin/env node
/**
 * Build a channel index (channels/<channel>.json) from a release's
 * SHA256SUMS. Promotion is a data change: clients decide from this file
 * whether an update applies to them, including the rollout percentage.
 *
 *   node scripts/promote.mjs --channel stable --version 0.7.1 \
 *     --base https://github.com/o/r/releases/download/v0.7.1 --percent 10
 */
import { mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

const PLATFORMS = [
  'linux-x64-gnu',
  'linux-arm64-gnu',
  'linux-x64-musl',
  'linux-arm64-musl',
  'darwin-x64',
  'darwin-arm64',
];

function usage() {
  console.error(`usage: promote.mjs --channel stable|canary --version VERSION --base URL [options]

  --percent N       rollout percentage, 0-100 (default 100)
  --min-from V      refuse to upgrade devices older than V
  --from DIR        directory holding SHA256SUMS (default dist)
  --out FILE        output file (default channels/<channel>.json)
  --allow-partial   do not require every platform
`);
  process.exit(2);
}

const options = {
  channel: '',
  version: '',
  base: '',
  percent: 100,
  from: 'dist',
  out: '',
  minFrom: '',
  allowPartial: false,
};
for (let index = 2; index < process.argv.length; index += 1) {
  const flag = process.argv[index];
  const value = process.argv[index + 1];
  switch (flag) {
    case '--channel':
      options.channel = value ?? usage();
      index += 1;
      break;
    case '--version':
      options.version = (value ?? usage()).replace(/^v/, '');
      index += 1;
      break;
    case '--base':
      options.base = value ?? usage();
      index += 1;
      break;
    case '--percent':
      options.percent = Number(value);
      index += 1;
      break;
    case '--min-from':
      options.minFrom = (value ?? usage()).replace(/^v/, '');
      index += 1;
      break;
    case '--from':
      options.from = value ?? usage();
      index += 1;
      break;
    case '--out':
      options.out = value ?? usage();
      index += 1;
      break;
    case '--allow-partial':
      options.allowPartial = true;
      break;
    case '--help':
    case '-h':
      usage();
      break;
    default:
      console.error(`unknown argument: ${flag}`);
      usage();
  }
}

if (!['stable', 'canary'].includes(options.channel)) {
  console.error('--channel must be stable or canary');
  process.exit(2);
}
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(options.version)) {
  console.error('--version must look like 1.2.3');
  process.exit(2);
}
if (!/^https?:\/\//.test(options.base)) {
  console.error('--base must be an http(s) URL');
  process.exit(2);
}
if (!Number.isFinite(options.percent) || options.percent < 0 || options.percent > 100) {
  console.error('--percent must be 0-100');
  process.exit(2);
}

const sums = readFileSync(join(options.from, 'SHA256SUMS'), 'utf8').trim().split('\n');
const platforms = {};
for (const line of sums) {
  const [sha256, file] = line.trim().split(/\s+/);
  if (!file) {
    continue;
  }
  const match = new RegExp(`^rdh-${options.version.replace(/\./g, '\\.')}-(.+)\\.tar\\.gz$`).exec(
    file,
  );
  if (!match) {
    continue;
  }
  const platform = match[1];
  if (!PLATFORMS.includes(platform)) {
    continue;
  }
  let bytes;
  try {
    bytes = statSync(join(options.from, file)).size;
  } catch {
    bytes = undefined;
  }
  platforms[platform] = {
    url: `${options.base.replace(/\/$/, '')}/${file}`,
    sha256,
    ...(bytes === undefined ? {} : { bytes }),
  };
}

const missing = PLATFORMS.filter((platform) => platforms[platform] === undefined);
if (missing.length && !options.allowPartial) {
  console.error(`missing artifacts for: ${missing.join(', ')}`);
  process.exit(1);
}
if (!Object.keys(platforms).length) {
  console.error('no artifacts matched the version');
  process.exit(1);
}

const sorted = Object.fromEntries(
  Object.keys(platforms)
    .sort()
    .map((key) => [key, platforms[key]]),
);
const entry = {
  schema: 1,
  channel: options.channel,
  version: options.version,
  released_at: new Date().toISOString(),
  ...(options.minFrom ? { min_from: options.minFrom } : {}),
  rollout: { percent: options.percent },
  platforms: sorted,
};

const out = options.out || join('channels', `${options.channel}.json`);
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, `${JSON.stringify(entry, null, 2)}\n`);
console.log(
  `${out}: ${options.channel} ${options.version} at ${options.percent}% (${Object.keys(sorted).length} platforms)`,
);
