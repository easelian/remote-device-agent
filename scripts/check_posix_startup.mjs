#!/usr/bin/env node
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, writeFile, rm, readFile, readdir } from 'node:fs/promises';
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
      await run('sudo', ['systemctl', 'start', `user@${uid}.service`]);
    }
  }
  const suffix = `${mac ? 'darwin' : 'linux'}-${process.arch}${mac ? '' : '-gnu'}.tar.gz`;
  const archive = (await readdir(directory)).find((name) => name.endsWith(suffix));
  assert.ok(archive, `Missing archive for ${suffix}`);
  for (const mode of system ? ['none', 'user', 'service'] : ['none']) {
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
      if (mode === 'none') {
        assert.equal(existsSync(userUnit), false);
        assert.equal(existsSync(systemUnit), false);
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
          await manager('bootout', `${serviceDomain}/${label}`);
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
          await manager('bootstrap', serviceDomain, mode === 'service' ? systemUnit : userUnit);
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
  for (const socket of server.clients) {
    socket.terminate();
  }
  await new Promise((resolve) => server.close(resolve));
  await rm(temporary, { recursive: true, force: true });
}
