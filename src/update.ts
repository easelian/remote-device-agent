/**
 * Update policy: channel index parsing, version comparison and rollout
 * selection. Everything here is pure so it can be tested without touching a
 * real installation.
 */
import { createHash } from 'node:crypto';

export interface PlatformArtifact {
  url: string;
  sha256: string;
  bytes?: number | undefined;
  node?: string | undefined;
}

export interface ChannelEntry {
  schema?: number | undefined;
  channel?: string | undefined;
  version: string;
  released_at?: string | undefined;
  min_from?: string | undefined;
  rollout?: { percent?: number | undefined } | undefined;
  platforms: Record<string, PlatformArtifact>;
}

export type Decision =
  | { action: 'upgrade'; reason: string; version: string; artifact: PlatformArtifact }
  | { action: 'none'; reason: string }
  | { action: 'blocked'; reason: string };

const VERSION_RE = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

/** Compare two SemVer strings; a prerelease sorts before its release. */
export function compareVersions(left: string, right: string): number {
  const split = (value: string) => {
    const clean = value.replace(/^v/, '');
    const dash = clean.indexOf('-');
    const core = dash === -1 ? clean : clean.slice(0, dash);
    const pre = dash === -1 ? '' : clean.slice(dash + 1);
    return { parts: core.split('.').map((part) => Number.parseInt(part, 10) || 0), pre };
  };
  const a = split(left);
  const b = split(right);
  for (let index = 0; index < 3; index += 1) {
    const diff = (a.parts[index] ?? 0) - (b.parts[index] ?? 0);
    if (diff !== 0) {
      return diff < 0 ? -1 : 1;
    }
  }
  if (a.pre === b.pre) {
    return 0;
  }
  if (a.pre === '') {
    return 1;
  }
  if (b.pre === '') {
    return -1;
  }
  return a.pre < b.pre ? -1 : 1;
}

/** Stable 0..99 bucket for an install id; monotonic as the percentage grows. */
export function cohortBucket(installId: string): number {
  const digest = createHash('sha256').update(installId).digest();
  return (((digest[0] ?? 0) << 8) | (digest[1] ?? 0)) % 100;
}

/** Platform key used by the release artifacts, e.g. linux-x64-gnu. */
export function detectPlatform(platform: string, arch: string, glibc: string | undefined): string {
  const os = platform === 'darwin' ? 'darwin' : platform === 'linux' ? 'linux' : platform;
  const cpu =
    arch === 'x64' || arch === 'amd64'
      ? 'x64'
      : arch === 'arm64' || arch === 'aarch64'
        ? 'arm64'
        : arch;
  if (os !== 'linux') {
    return `${os}-${cpu}`;
  }
  if (glibc === undefined) {
    return `linux-${cpu}-musl`;
  }
  if (compareVersions(`${glibc}.0`, '2.28.0') < 0) {
    return `linux-${cpu}-musl`;
  }
  return `linux-${cpu}-gnu`;
}

/** Detect the platform of the current process. */
export function hostPlatform(): string {
  return detectPlatform(process.platform, process.arch, runtimeGlibc());
}

/** glibc version reported by Node, undefined on musl systems. */
export function runtimeGlibc(): string | undefined {
  try {
    const report = process.report?.getReport() as
      | { header?: { glibcVersionRuntime?: string } }
      | undefined;
    return report?.header?.glibcVersionRuntime;
  } catch {
    return undefined;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/** Validate a channel index; throws with a readable message when malformed. */
export function parseChannel(raw: unknown): ChannelEntry {
  if (!isRecord(raw)) {
    throw new Error('版本索引不是对象');
  }
  const version = raw.version;
  if (typeof version !== 'string' || !VERSION_RE.test(version)) {
    throw new Error('版本索引缺少有效 version');
  }
  const platforms = raw.platforms;
  if (!isRecord(platforms) || Object.keys(platforms).length === 0) {
    throw new Error('版本索引缺少 platforms');
  }
  const parsed: Record<string, PlatformArtifact> = {};
  for (const [key, value] of Object.entries(platforms)) {
    if (!isRecord(value)) {
      throw new Error(`平台 ${key} 的条目无效`);
    }
    const url = value.url;
    const sha256 = value.sha256;
    if (typeof url !== 'string' || url === '') {
      throw new Error(`平台 ${key} 缺少下载地址`);
    }
    if (typeof sha256 !== 'string' || !/^[0-9a-f]{64}$/.test(sha256)) {
      throw new Error(`平台 ${key} 的 sha256 无效`);
    }
    parsed[key] = {
      url,
      sha256,
      ...(typeof value.bytes === 'number' ? { bytes: value.bytes } : {}),
      ...(typeof value.node === 'string' ? { node: value.node } : {}),
    };
  }
  const entry: ChannelEntry = { version, platforms: parsed };
  if (typeof raw.channel === 'string') {
    entry.channel = raw.channel;
  }
  if (typeof raw.released_at === 'string') {
    entry.released_at = raw.released_at;
  }
  if (typeof raw.min_from === 'string') {
    entry.min_from = raw.min_from;
  }
  if (typeof raw.schema === 'number') {
    entry.schema = raw.schema;
  }
  if (isRecord(raw.rollout)) {
    const percent = raw.rollout.percent;
    if (typeof percent !== 'number' || percent < 0 || percent > 100) {
      throw new Error('rollout.percent 必须在 0 到 100 之间');
    }
    entry.rollout = { percent };
  }
  return entry;
}

export interface UpgradeContext {
  current: string;
  platform: string;
  installId: string;
  channel: string;
  force?: boolean;
}

/** Decide what to do for one device against one channel entry. */
export function decideUpgrade(entry: ChannelEntry, context: UpgradeContext): Decision {
  const artifact = entry.platforms[context.platform];
  if (artifact === undefined) {
    return { action: 'blocked', reason: `该版本没有 ${context.platform} 的构建` };
  }

  if (compareVersions(entry.version, context.current) <= 0) {
    return { action: 'none', reason: `已是 ${context.current}，无需更新` };
  }
  if (
    context.force !== true &&
    entry.min_from !== undefined &&
    compareVersions(context.current, entry.min_from) < 0
  ) {
    return {
      action: 'blocked',
      reason: `需要先升级到 ${entry.min_from}，再升级到 ${entry.version}`,
    };
  }
  const percent = entry.rollout?.percent ?? 100;
  if (context.channel !== 'canary' && context.force !== true && percent < 100) {
    const bucket = cohortBucket(context.installId);
    if (bucket >= percent) {
      return { action: 'none', reason: `灰度未覆盖本设备（${bucket} >= ${percent}%）` };
    }
  }
  return { action: 'upgrade', reason: `升级到 ${entry.version}`, version: entry.version, artifact };
}
