#!/usr/bin/env node
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, writeFile, rm, readFile, readdir, stat } from 'node:fs/promises';
import { existsSync, realpathSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import WebSocketServer from '../vendor/ws/lib/websocket-server.js';

assert.ok(['linux', 'darwin'].includes(process.platform));
const execute = promisify(execFile);
const directory = path.resolve(process.argv[2] || 'dist');
const system = process.argv.includes('--system');
const temporary = realpathSync(await mkdtemp(path.join(os.tmpdir(), 'rdh startup ')));
const mac = process.platform === 'darwin';
const user = os.userInfo().username;
const uid = process.getuid();
const domain = `gui/${uid}`;
const label = 'com.be-one-click.rdh';
const servicePrefix = mac ? '/Library/RemoteDeviceHub' : '/opt/remote-device-hub';
const serviceData = mac
  ? '/Library/Application Support/RemoteDeviceHub'
  : '/var/lib/remote-device-hub';
const userUnit = mac
  ? path.join(os.homedir(), `Library/LaunchAgents/${label}.plist`)
  : path.join(os.homedir(), '.config/systemd/user/rdh.service');
const systemUnit = mac
  ? `/Library/LaunchDaemons/${label}.plist`
  : '/etc/systemd/system/rdh.service';
const env = { ...process.env };
if (!mac && system) {
  env.XDG_RUNTIME_DIR = `/run/user/${uid}`;
  env.DBUS_SESSION_BUS_ADDRESS = `unix:path=${env.XDG_RUNTIME_DIR}/bus`;
}
const run = async (command, args, options = {}) => {
  const result = await execute(command, args, {
    env,
    timeout: 120_000,
    maxBuffer: 2 * 1024 * 1024,
    ...options,
  });
  return result.stdout.trim();
};
const waitFor = async (condition, description) => {
  const deadline = Date.now() + 45_000;
  while (!(await condition())) {
    assert.ok(Date.now() < deadline, `Timed out: ${description}`);
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
};
const token = 'posix-startup-fixture-'.repeat(3);
const server = new WebSocketServer({
  port: 0,
  host: '127.0.0.1',
  verifyClient: ({ req }, done) => done(req.headers.authorization === `Bearer ${token}`, 401),
});
await new Promise((resolve) => server.once('listening', resolve));
const gateway = `http://127.0.0.1:${server.address().port}`;
const completed = new Map();
let originalOpt;
const requestStartup = (socket, operation, params = {}) =>
  new Promise((resolve, reject) => {
    const id = `startup-${Date.now()}-${Math.random()}`;
    const timer = setTimeout(() => {
      socket.off('message', receive);
      reject(new Error('Startup RPC timed out'));
    }, 30_000);
    const receive = (raw) => {
      const message = JSON.parse(raw);
      if (message.id !== id) {
        return;
      }
      clearTimeout(timer);
      socket.off('message', receive);
      resolve(message);
    };
    socket.on('message', receive);
    socket.send(JSON.stringify({ type: 'request', id, operation, params }));
  });
server.on('connection', (socket, request) => {
  const id = request.url.split('/').at(-1);
  socket.deviceId = id;
  socket.on('message', (raw) => {
    const message = JSON.parse(raw);
    if (message.type === 'hello') {
      socket.send(JSON.stringify({ type: 'ready', version: 1, device_id: id }));
      socket.send(JSON.stringify({ type: 'request', id: 'info', operation: 'device_info' }));
    } else if (message.type === 'result' && message.id === 'info' && message.result?.roots?.work) {
      completed.set(id, (completed.get(id) || 0) + 1);
    }
  });
});
try {
  assert.equal(existsSync(userUnit), false, 'Refusing to replace an existing user installation');
  assert.equal(existsSync(systemUnit), false, 'Refusing to replace an existing boot service');
  if (system) {
    assert.equal(existsSync(servicePrefix), false);
    assert.equal(existsSync(serviceData), false);
    if (!mac) {
      // Hosted runners make /opt group-writable for tool installation. The
      // production installer must reject that. Normalize only this disposable
      // fixture, then restore it; never relax the installer's ownership checks.
      if (process.env.GITHUB_ACTIONS === 'true') {
        originalOpt = await stat('/opt');
        await run('sudo', ['chown', '0:0', '/opt']);
        await run('sudo', ['chmod', '0755', '/opt']);
      }
      await run('sudo', ['systemctl', 'start', `user@${uid}.service`]);
    }
  }
  const suffix = `${mac ? 'darwin' : 'linux'}-${process.arch}${mac ? '' : '-gnu'}.tar.gz`;
  const archive = (await readdir(directory)).find((name) => name.endsWith(suffix));
  assert.ok(archive, `Missing archive for ${suffix}`);
  for (const mode of system
    ? ['none', 'user', 'service']
    : process.argv.includes('--user')
      ? ['none', 'user']
      : ['none']) {
    const name = `posix-${mode}-${process.pid}`;
    const prefix = mode === 'service' ? servicePrefix : path.join(temporary, mode);
    const config = path.join(temporary, `${mode}.json`);
    await writeFile(
      config,
      JSON.stringify({
        gateway_url: gateway,
        device_id: name,
        token,
        roots: { work: temporary },
        allow_shell: false,
        allow_write: false,
      }),
      { mode: 0o600 },
    );
    const args = [
      path.join(directory, 'install.sh'),
      '--offline',
      path.join(directory, archive),
      '--config',
      config,
      '--server',
      gateway,
    ];
    if (mode === 'service') {
      args.push('--startup', 'service', '--service-user', user);
    } else {
      args.push('--prefix', prefix, '--bin-dir', path.join(prefix, 'bin'));
    }
    if (mode === 'user') {
      args.push('--startup', 'user');
    }
    let foreground;
    let foregroundExit;
    let diagnostics = '';
    const serviceDomain = mode === 'service' ? 'system' : domain;
    const manager = async (...operation) => {
      const command = mac ? 'launchctl' : 'systemctl';
      const params = mac ? operation : [...(mode === 'user' ? ['--user'] : []), ...operation];
      return mode === 'service' ? run('sudo', [command, ...params]) : run(command, params);
    };
    try {
      if (mode === 'none') {
        foreground = spawn('sh', args, { env, stdio: ['ignore', 'pipe', 'pipe'] });
        foregroundExit = new Promise((resolve) =>
          foreground.once('exit', (code, signal) => resolve({ code, signal })),
        );
        foreground.stdout.on('data', (chunk) => {
          diagnostics += chunk;
        });
        foreground.stderr.on('data', (chunk) => {
          diagnostics += chunk;
        });
      } else {
        console.log(
          await run(
            mode === 'service' ? 'sudo' : 'sh',
            mode === 'service' ? ['sh', ...args] : args,
          ),
        );
      }
      await waitFor(
        () => completed.get(name) >= 1,
        `${mode} authenticated Agent request: ${diagnostics}`,
      );
      if (mode !== 'none') {
        const socket = [...server.clients].find(
          (item) => item.deviceId === name && item.readyState === 1,
        );
        const current = await requestStartup(socket, 'get_startup');
        assert.equal(current.result?.mode, mode.toLowerCase(), JSON.stringify(current));
        const pending = await requestStartup(socket, 'set_startup', { mode: mode.toLowerCase() });
        if (mode.toLowerCase() === 'service') {
          assert.equal(pending.error_code, 'startup_permission_required', JSON.stringify(pending));
        } else {
          assert.equal(pending.result?.status, 'pending_local_approval', JSON.stringify(pending));
        }
        const rejected = await requestStartup(socket, 'set_startup', {
          mode: mode.toLowerCase(),
          approved: true,
        });
        assert.ok(rejected.error, 'remote caller cannot self-approve');
        const disabled = await requestStartup(socket, 'set_startup', { mode: 'none' });
        if (mode.toLowerCase() === 'user') {
          assert.equal(disabled.result?.mode, 'none', JSON.stringify(disabled));
        } else {
          assert.ok(
            disabled.result?.mode === 'none' ||
              disabled.error_code === 'startup_permission_required',
            JSON.stringify(disabled),
          );
        }
        if (mode === 'service') {
          const result = JSON.parse(
            await run('sudo', [
              path.join(prefix, 'current/node'),
              path.join(prefix, 'current/bin/remote-device-hub.mjs'),
              'startup',
              'system',
              'set',
              'none',
            ]),
          );
          assert.equal(result.mode, 'none');
        }
        assert.equal(socket.readyState, 1, 'disable must preserve the original socket');
        const after = await requestStartup(socket, 'device_info');
        assert.ok(after.result?.roots?.work, JSON.stringify(after));
        const state = await requestStartup(socket, 'get_startup');
        assert.equal(state.result?.pending, null, JSON.stringify(state));
        if (mac) {
          await manager('enable', `${serviceDomain}/${label}`);
        } else {
          await manager('enable', 'rdh');
        }
      }
      if (mode === 'service') {
        for (const executable of [
          'run-agent',
          'current/node',
          'current/bin/remote-device-hub.mjs',
        ]) {
          const metadata = await stat(path.join(prefix, executable));
          assert.equal(metadata.uid, 0, 'boot service executables must belong to root');
          assert.equal(
            metadata.mode & 0o022,
            0,
            'boot service executables must not be group/world writable',
          );
        }
      }
      if (mode === 'none') {
        assert.equal(existsSync(userUnit), false);
        assert.equal(existsSync(systemUnit), false);
        if (system) {
          const socket = [...server.clients].find(
            (item) => item.deviceId === name && item.readyState === 1,
          );
          const cli = path.join(prefix, 'current/bin/remote-device-hub.mjs');
          const node = path.join(prefix, 'current/node');
          try {
            await run('sudo', [node, cli, 'startup', 'grant', 'user', '--config', config]);
            const enabled = await requestStartup(socket, 'set_startup', { mode: 'user' });
            assert.equal(enabled.result?.mode, 'user', JSON.stringify(enabled));
            assert.equal(existsSync(userUnit), true);
            const off = await requestStartup(socket, 'set_startup', { mode: 'none' });
            assert.equal(off.result?.mode, 'none', JSON.stringify(off));
            assert.equal(socket.readyState, 1);
            await run('sudo', [node, cli, 'startup', 'revoke', '--config', config]);
            const pending = await requestStartup(socket, 'set_startup', { mode: 'user' });
            assert.equal(pending.result?.status, 'pending_local_approval', JSON.stringify(pending));
            await requestStartup(socket, 'set_startup', { mode: 'none' });
          } finally {
            await run('sudo', [node, cli, 'startup', 'revoke', '--config', config]);
            await rm(userUnit, { force: true });
            await rm(`${config}.startup-run`, { force: true });
            if (!mac) {
              await run('systemctl', ['--user', 'daemon-reload']);
            } else {
              await run('launchctl', ['enable', `${domain}/${label}`]);
            }
          }
        }
        foreground.kill('SIGINT');
        const ended = await Promise.race([
          foregroundExit,
          new Promise((_, reject) =>
            setTimeout(() => reject(new Error('foreground stop timeout')), 15_000).unref(),
          ),
        ]);
        assert.equal(ended.signal, null);
        assert.ok([0, 130].includes(ended.code), JSON.stringify(ended));
      } else {
        const definition = await readFile(mode === 'service' ? systemUnit : userUnit, 'utf8');
        if (mac) {
          assert.equal(definition.includes('<key>UserName</key>'), mode === 'service');
          await manager('kill', 'SIGTERM', `${serviceDomain}/${label}`);
        } else {
          assert.match(
            definition,
            mode === 'service' ? /WantedBy=multi-user.target/ : /WantedBy=default.target/,
          );
          if (mode === 'service') {
            assert.match(definition, new RegExp(`User=${user}\\n`));
          }
          await manager('stop', 'rdh');
        }
      }
      await waitFor(
        () =>
          [...server.clients].every(
            (socket) => socket.deviceId !== name || socket.readyState !== 1,
          ),
        `${mode} disconnects on stop`,
      );
      await waitFor(async () => {
        const processes = await run('ps', ['-axo', 'command=']);
        return !processes.split('\n').some((line) => line.includes(`${prefix}/current/`));
      }, `${mode} process exits after stop`);
      if (mode !== 'none') {
        if (mac) {
          // Process exit and launchd's completion of the stop are separate.
          // kickstart is a no-op while launchd still considers the job running.
          await waitFor(
            async () =>
              /state = not running/.test(await manager('print', `${serviceDomain}/${label}`)),
            `${mode} launchd acknowledges stop`,
          );
          await manager('kickstart', `${serviceDomain}/${label}`);
        } else {
          await manager('start', 'rdh');
        }
        await waitFor(() => completed.get(name) >= 2, `${mode} restart`);
      }
      console.log(
        `${mode}: native startup, authenticated request, stop and ${mode === 'none' ? 'no autostart registration' : 'restart'} passed.`,
      );
    } catch (error) {
      console.error(diagnostics);
      if (mode !== 'none') {
        if (mac) {
          console.error(await manager('print', `${serviceDomain}/${label}`).catch(String));
        } else {
          console.error(await manager('status', 'rdh', '--no-pager', '--full').catch(String));
          console.error(
            await run('journalctl', [
              ...(mode === 'user' ? ['--user'] : []),
              '-u',
              'rdh',
              '-n',
              '50',
              '--no-pager',
            ]).catch(String),
          );
        }
      }
      throw error;
    } finally {
      if (foreground && foreground.exitCode === null && foreground.signalCode === null) {
        foreground.kill('SIGTERM');
        await foregroundExit;
      }
      if (mode !== 'none') {
        const uninstall = [path.join(directory, 'install.sh'), '--uninstall', '--startup', mode];
        if (mode === 'service') {
          uninstall.push('--service-user', user);
        } else {
          uninstall.push('--prefix', prefix, '--bin-dir', path.join(prefix, 'bin'));
        }
        await run(
          mode === 'service' ? 'sudo' : 'sh',
          mode === 'service' ? ['sh', ...uninstall] : uninstall,
        );
        assert.equal(existsSync(mode === 'service' ? systemUnit : userUnit), false);
        if (mode === 'service') {
          await run('sudo', ['rm', '-rf', serviceData]);
        }
      }
    }
  }
} finally {
  if (originalOpt) {
    await run('sudo', ['chown', `${originalOpt.uid}:${originalOpt.gid}`, '/opt']);
    await run('sudo', ['chmod', (originalOpt.mode & 0o7777).toString(8), '/opt']);
  }
  for (const socket of server.clients) {
    socket.terminate();
  }
  await new Promise((resolve) => server.close(resolve));
  await rm(temporary, { recursive: true, force: true });
}
