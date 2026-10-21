/**
 * Self-update and rollback. The running installation is never modified in
 * place: a new tree is unpacked next to the old one, verified by running its
 * own --version, and only then does the `current` symlink move. A service that
 * fails to come back up after the switch is rolled back automatically.
 */
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { existsSync } from 'node:fs';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { compareVersions, decideUpgrade, hostPlatform, parseChannel } from './update.js';
import type { ChannelEntry, Decision } from './update.js';

const DEFAULT_CHANNEL = 'stable';
const DEFAULT_INDEX =
  'https://raw.githubusercontent.com/be-one-click/remote-device-agent/main/channels';

interface Options {
  channel: string;
  index?: string;
  check: boolean;
  force: boolean;
  dryRun: boolean;
  restart: boolean;
}

function usage(): string {
  return [
    'usage: rdh upgrade [options] | rdh rollback',
    '',
    '  --channel NAME   stable (default) or canary',
    '  --index URL      channel index; defaults to $RDH_INDEX or the published one',
    '  --check          report what would happen and exit',
    '  --force          ignore the rollout percentage and min_from',
    '  --dry-run        stop before touching the installation',
    '  --no-restart     do not restart a service after switching versions',
    '  --help           show this help',
  ].join('\n');
}

function parseOptions(argv: string[]): Options {
  const options: Options = {
    channel: DEFAULT_CHANNEL,
    check: false,
    force: false,
    dryRun: false,
    restart: true,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const item = argv[index];
    if (item === undefined) {
      break;
    }
    switch (item) {
      case '--channel': {
        const value = argv[++index];
        if (value === undefined || value.startsWith('--')) {
          throw new Error('--channel requires a value');
        }
        if (value !== 'stable' && value !== 'canary') {
          throw new Error('--channel must be stable or canary');
        }
        options.channel = value;
        break;
      }
      case '--index': {
        const value = argv[++index];
        if (value === undefined || value.startsWith('--')) {
          throw new Error('--index requires a value');
        }
        options.index = value;
        break;
      }
      case '--check':
        options.check = true;
        break;
      case '--force':
        options.force = true;
        break;
      case '--dry-run':
        options.dryRun = true;
        break;
      case '--no-restart':
        options.restart = false;
        break;
      case '--help':
      case '-h':
        throw new Error(usage());
      default:
        throw new Error(`Unknown argument: ${item}`);
    }
  }
  return options;
}

function installedPrefix(moduleUrl: string): string {
  const bundle = fileURLToPath(moduleUrl);
  const prefix = path.resolve(path.dirname(bundle), '../../..');
  if (!existsSync(path.join(prefix, 'versions')) || !existsSync(path.join(prefix, 'current'))) {
    throw new Error(
      'This command is not running from an installed directory. Install with install.sh before upgrading.',
    );
  }
  return prefix;
}

async function currentVersion(prefix: string): Promise<string> {
  const target = await fs.readlink(path.join(prefix, 'current'));
  return path.basename(target);
}

async function installId(prefix: string): Promise<string> {
  const file = path.join(prefix, 'install-id');
  try {
    const existing = (await fs.readFile(file, 'utf8')).trim();
    if (/^[0-9a-f]{32}$/.test(existing)) {
      return existing;
    }
  } catch (error) {
    if ((error as { code?: string }).code !== 'ENOENT') {
      throw error;
    }
  }
  const created = randomBytes(16).toString('hex');
  await fs.mkdir(prefix, { recursive: true, mode: 0o700 });
  await fs.writeFile(file, `${created}\n`, { mode: 0o600 });
  return created;
}

function localPath(location: string): string {
  return location.startsWith('file:') ? fileURLToPath(location) : location;
}

async function readIndex(location: string): Promise<ChannelEntry> {
  if (/^https?:/i.test(location)) {
    const response = await fetch(location, { redirect: 'follow' });
    if (!response.ok) {
      throw new Error(`Could not read the release index: HTTP ${response.status}`);
    }
    return parseChannel(await response.json());
  }
  const raw = await fs.readFile(localPath(location), 'utf8');
  return parseChannel(JSON.parse(raw));
}

function resolveArtifact(location: string, url: string): string {
  if (/^https?:/i.test(url) || url.startsWith('file:')) {
    return url;
  }
  if (/^https?:/i.test(location)) {
    return new URL(url, location).href;
  }
  return path.resolve(path.dirname(localPath(location)), url);
}

async function download(url: string, destination: string): Promise<void> {
  if (/^https?:/i.test(url)) {
    const response = await fetch(url, { redirect: 'follow' });
    if (!response.ok) {
      throw new Error(`Download failed: HTTP ${response.status}`);
    }
    // Artifacts are ~40 MB; buffering keeps the error paths simple.
    await fs.writeFile(destination, Buffer.from(await response.arrayBuffer()), { mode: 0o600 });
    return;
  }
  await fs.copyFile(localPath(url), destination);
}

async function sha256(file: string): Promise<string> {
  const data = await fs.readFile(file);
  return createHash('sha256').update(data).digest('hex');
}

function run(
  command: string,
  args: string[],
  options: { cwd?: string } = {},
): { status: number | null; stdout: string } {
  const result = spawnSync(command, args, { encoding: 'utf8', cwd: options.cwd });
  return { status: result.status, stdout: (result.stdout ?? '').trim() };
}

async function pointTo(prefix: string, name: string, target: string): Promise<void> {
  const link = path.join(prefix, name);
  const temporary = `${link}.tmp`;
  await fs.rm(temporary, { force: true });
  await fs.symlink(target, temporary);
  await fs.rename(temporary, link);
}

function unitFile(): string {
  const base = process.env.XDG_CONFIG_HOME ?? path.join(os.homedir(), '.config');
  return path.join(base, 'systemd/user/rdh.service');
}

function restartService(): boolean {
  if (!existsSync(unitFile())) {
    return false;
  }
  return run('systemctl', ['--user', 'restart', 'rdh']).status === 0;
}

async function waitForService(seconds: number): Promise<boolean> {
  if (!existsSync(unitFile())) {
    return true;
  }
  const deadline = Date.now() + seconds * 1000;
  while (Date.now() < deadline) {
    if (run('systemctl', ['--user', 'is-active', 'rdh']).stdout === 'active') {
      return true;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  return false;
}

async function unpack(archive: string, staging: string): Promise<void> {
  await fs.mkdir(staging, { recursive: true });
  const result = run('tar', ['-xzf', archive, '-C', staging, '--strip-components=1']);
  if (result.status !== 0) {
    throw new Error('Archive extraction failed');
  }
  await fs.chmod(path.join(staging, 'node'), 0o755);
  await fs.chmod(path.join(staging, 'rdh'), 0o755);
}

async function verifyTree(staging: string, version: string): Promise<void> {
  const result = run(path.join(staging, 'node'), [
    path.join(staging, 'bin/remote-device-hub.mjs'),
    '--version',
  ]);
  if (result.status !== 0 || result.stdout !== version) {
    throw new Error(`The new version failed its self-check (output: ${result.stdout || 'none'})`);
  }
}

export async function runUpgrade(command: 'upgrade' | 'rollback', argv: string[]): Promise<void> {
  const prefix = installedPrefix(import.meta.url);
  if (command === 'rollback') {
    await rollback(prefix, argv);
    return;
  }

  const options = parseOptions(argv);
  const channel = options.channel;
  const location = options.index ?? process.env.RDH_INDEX ?? `${DEFAULT_INDEX}/${channel}.json`;
  const current = await currentVersion(prefix);
  const platform = hostPlatform();

  console.log(`Current version: ${current} (${platform})`);
  const entry = await readIndex(location);
  const decision: Decision = decideUpgrade(entry, {
    current,
    platform,
    channel,
    installId: await installId(prefix),
    ...(options.force ? { force: true } : {}),
  });
  console.log(`Target version: ${entry.version} — ${decision.reason}`);

  if (decision.action !== 'upgrade') {
    return;
  }
  if (options.check || options.dryRun) {
    console.log(`Will download from ${resolveArtifact(location, decision.artifact.url)}`);
    return;
  }

  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-upgrade-'));
  const archive = path.join(temporary, 'bundle.tar.gz');
  const staging = path.join(prefix, 'versions', `${decision.version}.new`);
  try {
    console.log('Downloading…');
    await download(resolveArtifact(location, decision.artifact.url), archive);
    const digest = await sha256(archive);
    if (digest !== decision.artifact.sha256) {
      throw new Error('Download verification failed; upgrade aborted');
    }

    await fs.rm(staging, { recursive: true, force: true });
    await unpack(archive, staging);
    await verifyTree(staging, decision.version);

    const destination = path.join(prefix, 'versions', decision.version);
    await fs.rm(destination, { recursive: true, force: true });
    await fs.mkdir(path.join(prefix, 'versions'), { recursive: true });
    await fs.rename(staging, destination);
    await pointTo(prefix, 'previous', `versions/${current}`);
    await pointTo(prefix, 'current', `versions/${decision.version}`);
    console.log(`Switched to ${decision.version}`);

    if (options.restart && existsSync(unitFile())) {
      if (!restartService() || !(await waitForService(15))) {
        await pointTo(prefix, 'current', `versions/${current}`);
        restartService();
        throw new Error(`The service failed to start; rolled back to ${current}`);
      }
      console.log('The service restarted and passed its health check');
    } else if (options.restart) {
      console.log('No systemd service found; restart manually to apply the update');
    }
  } finally {
    await fs.rm(temporary, { recursive: true, force: true });
    await fs.rm(staging, { recursive: true, force: true });
  }
}

async function rollback(prefix: string, argv: string[]): Promise<void> {
  for (const item of argv) {
    if (item === '--help' || item === '-h') {
      throw new Error(usage());
    }
    throw new Error(`Unknown argument: ${item}`);
  }
  const previous = path.join(prefix, 'previous');
  if (!existsSync(previous)) {
    throw new Error('No previous version is available for rollback');
  }

  const target = await fs.readlink(previous);
  const current = await fs.readlink(path.join(prefix, 'current'));
  const currentVersionName = path.basename(current);
  const targetVersion = path.basename(target);
  if (compareVersions(targetVersion, currentVersionName) === 0) {
    throw new Error('The rollback target is the current version');
  }

  await pointTo(prefix, 'current', target);
  await pointTo(prefix, 'previous', current);
  console.log(`Rolled back to ${targetVersion}`);

  if (existsSync(unitFile())) {
    if (!restartService() || !(await waitForService(15))) {
      await pointTo(prefix, 'current', current);
      await pointTo(prefix, 'previous', target);
      restartService();
      throw new Error(`The service failed to start after rollback; restored ${currentVersionName}`);
    }
    console.log('The service restarted and passed its health check');
  }
}
