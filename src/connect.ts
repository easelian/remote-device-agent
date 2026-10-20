import * as fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { createHash, randomBytes } from 'node:crypto';
import { spawn } from 'node:child_process';
import { AgentConfig, configPath, configPaths, localRoots, readConfig, validateGateway, writeConfig } from './config.js';
import { acquireProfileLocks, reclaimProfileLocks } from './profile-lock.js';
import { DIRECT_SERVER, FALLBACK_SERVER, sameHub } from './server-selection.js';
import { defaultShell, platformName, ensurePrivatePath } from './platform.js';
import type { LaunchArgs, DeviceConfig } from './types.js';

const sleep = (ms: any, signal: any) => new Promise<void>((resolve, reject) => {
  if (signal?.aborted) return reject(signal.reason || new Error('操作已取消'));
  let onAbort: (() => void) | undefined;
  const timer = setTimeout(() => {
    signal?.removeEventListener('abort', onAbort!);
    resolve();
  }, ms);
  onAbort = () => { clearTimeout(timer); reject(signal.reason || new Error('操作已取消')); };
  signal?.addEventListener('abort', onAbort, { once: true });
});
const sameRoots = (left: Record<string, string>, right: Record<string, string>) => {
  const lk = Object.keys(left).sort();
  const rk = Object.keys(right).sort();
  return lk.length === rk.length && lk.every((key: any, index: any) => key === rk[index] && left[key] === right[key]);
};

function requestSignal(signal?: AbortSignal) {
  return AbortSignal.any(signal ? [signal, AbortSignal.timeout(20_000)] : [AbortSignal.timeout(20_000)]);
}

export async function pairDevice(server: string, roots: Record<string, string>, name: string, { openBrowser = true, signal }: { openBrowser?: boolean; signal?: AbortSignal | undefined } = {}): Promise<DeviceConfig> {
  const token = randomBytes(32).toString('base64url');
  const tokenHash = createHash('sha256').update(token).digest('hex');
  const proposal = { name, hostname: os.hostname() || 'device', os: platformName(),
    machine: os.machine(), roots, token_hash: tokenHash };
  const origin = new URL(server);
  const post = async (pathname: any, body: any) => fetch(new URL(pathname, origin), {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
    redirect: 'error', signal: requestSignal(signal),
  });
  const response = await post('/api/pair/start', proposal);
  if (response.status !== 201) throw new Error(`中心暂时无法创建绑定请求（HTTP ${response.status}）`);
  const pending = await response.json();
  const verification = new URL(pending.verification_uri_complete);
  if (!sameHub(verification.origin, origin.origin) || verification.username || verification.password || verification.hash ||
      verification.pathname !== '/admin/pair' ||
      verification.searchParams.get('code') !== pending.user_code || [...verification.searchParams.keys()].length !== 1)
    throw new Error('中心返回了无效的绑定地址');
  console.log(`\n绑定验证码：${pending.user_code}\n打开页面：${verification.href}\n`);
  console.log('请在页面核对设备与验证码，选择权限并确认绑定。验证码 10 分钟内有效。');
  if (openBrowser && !process.env.SSH_CONNECTION && (['darwin', 'win32'].includes(process.platform) || process.env.DISPLAY || process.env.WAYLAND_DISPLAY)) {
    const command = process.platform === 'win32' ? path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'rundll32.exe')
      : process.platform === 'darwin' ? 'open' : 'xdg-open';
    const browserArgs = process.platform === 'win32' ? ['url.dll,FileProtocolHandler', verification.href] : [verification.href];
    const child = spawn(command, browserArgs, { detached: true, stdio: 'ignore', windowsHide: true });
    child.on('error', () => {});
    child.unref();
  }
  const deadline = Date.now() + Math.min(Number(pending.expires_in), 600) * 1000;
  const interval = Math.max(3, Math.min(Number(pending.interval), 10)) * 1000;
  while (Date.now() < deadline) {
    await sleep(interval, signal);
    let result;
    try { result = await post('/api/pair/poll', { device_code: pending.device_code }); }
    catch (error: any) { if (error.name === 'AbortError' || signal?.aborted) throw error; continue; }
    if (result.status === 200) {
      const body = await result.json();
      // Older Hubs supply /bin/sh as a schema default even for Windows roots.
      const model = AgentConfig.validate({ ...body.config, token,
        ...(process.platform === 'win32' && body.config.shell === '/bin/sh' ? { shell: defaultShell() } : {}) });
      if (!sameHub(model.gateway_url, server) || !sameRoots(model.roots, roots))
        throw new Error('绑定响应中的中心或目录与本机请求不一致');
      return model;
    }
    if (result.status === 202 || result.status === 429) continue;
    if (result.status === 403) throw new Error('绑定被拒绝或撤销，请重新运行命令');
    if (result.status === 410) break;
    throw new Error(`绑定请求失败（HTTP ${result.status}）`);
  }
  throw new Error('绑定已超时，请重新运行命令取得新验证码');
}

export async function connectDevice(args: LaunchArgs, { locks, runAgent = async () => {}, canReleaseLocks = () => true }: any = {}) {
  const signal = args.signal;
  if (!['linux', 'darwin', 'win32'].includes(process.platform)) throw new Error('仅支持 Windows、Linux 和 macOS');
  const server = validateGateway(args.server);
  const profile = args.profile || 'default';
  const profileServers: [string, ...string[]] = sameHub(server, FALLBACK_SERVER) ? [FALLBACK_SERVER, DIRECT_SERVER] : [server];
  // Keep both entrances under the same lock set and reuse an existing binding
  // when startup chooses a different route. Credentials never cross custom hubs.
  let destination = args.config ? path.resolve(args.config.replace(/^~(?=\/|$)/, os.homedir())) : configPath(profileServers[0], profile);
  if (!args.config) {
    for (const candidate of profileServers.map((item: any) => configPath(item, profile))) {
      try { await fs.lstat(candidate); destination = candidate; break; }
      catch (error: any) { if (error.code !== 'ENOENT') throw error; }
    }
  }
  const lockTargets = args.config ? [destination] : profileServers.flatMap((item: any) => configPaths(item, profile));
  for (const target of lockTargets) {
    if (process.platform === 'win32') ensurePrivatePath(path.dirname(target), true);
    else await fs.mkdir(path.dirname(target), { recursive: true, mode: 0o700 });
  }
  if (args.recoverLock) {
    if (locks?.reclaim) await locks.reclaim(lockTargets);
    else await reclaimProfileLocks(lockTargets);
  }
  const lockHandle = locks?.acquire ? await locks.acquire(lockTargets) : await acquireProfileLocks(lockTargets);
  try {
    let config;
    try {
      config = await readConfig(destination);
      if (!sameHub(config.gateway_url, server)) throw new Error('已有配置属于其他中心，请选择不同 --profile');
      if (args.root && !sameRoots(config.roots, await localRoots(args.root)))
        throw new Error('此配置已经绑定其他目录，请用 --profile 新建配置');
      console.log(`使用已保存的绑定：${config.device_id}`);
    } catch (error: any) {
      if (error.code !== 'ENOENT') throw error;
      const roots = await localRoots(args.root);
      console.log(`目标设备：${os.hostname() || 'device'}`);
      console.log(`文件目录：${Object.values(roots).join(', ')}`);
      config = await pairDevice(server, roots, args.name || os.hostname() || 'device', { openBrowser: !args.noBrowser, signal });
      await writeConfig(destination, config);
      console.log(`绑定成功：${config.device_id}\n配置已保存在：${destination}`);
    }
    console.log('正在连接设备中心；保持此终端运行，按 Ctrl+C 停止。');
    return await runAgent({ ...config, gateway_url: server }, signal, { configFile: destination });
  } finally {
    // Unfinished I/O must not overlap a second Agent using this profile. A
    // forced exit leaves a stale lock for explicit recovery after inspection.
    if (await canReleaseLocks())
      await (lockHandle.release ? lockHandle.release() : locks.release(lockHandle));
  }
}
