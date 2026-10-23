#!/usr/bin/env node
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import WebSocketServer from '../vendor/ws/lib/websocket-server.js';

assert.equal(process.platform, 'win32');
const execute = promisify(execFile);
const directory = path.resolve(process.argv[2] || 'dist');
const temporary = await mkdtemp(path.join(os.tmpdir(), 'rdh startup '));
const shell = path.join(process.env.SystemRoot, 'System32/WindowsPowerShell/v1.0/powershell.exe');
const run = async (args, env = {}) => {
  const result = await execute(
    shell,
    ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', ...args],
    {
      env: { ...process.env, ...env },
      timeout: 180_000,
      maxBuffer: 2 * 1024 * 1024,
    },
  );
  return result.stdout.trim();
};
const token = 'startup-test-token-'.repeat(3);
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
  socket.on('message', (raw) => {
    const message = JSON.parse(raw);
    if (message.type === 'hello') {
      socket.send(JSON.stringify({ type: 'ready', version: 1, device_id: id }));
      socket.send(JSON.stringify({ type: 'request', id: 'info', operation: 'device_info' }));
    } else if (message.id === 'info') {
      completed.set(id, (completed.get(id) || 0) + 1);
    }
  });
});
const waitFor = async (condition, description) => {
  const deadline = Date.now() + 60_000;
  while (!(await condition())) {
    assert.ok(Date.now() < deadline, `Timed out: ${description}`);
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
};
try {
  for (const mode of ['User', 'Service']) {
    const name = `RDH-Test-${mode}-${process.pid}`;
    const prefix =
      mode === 'Service'
        ? path.join(process.env.ProgramFiles, name)
        : path.join(temporary, 'user installation');
    const data =
      mode === 'Service' ? path.join(process.env.ProgramData, name) : path.join(prefix, 'data');
    const config = path.join(temporary, `${mode}.json`);
    // ProgramData is accessible to LocalService; no personal directory ACLs are broadened.
    await writeFile(
      config,
      JSON.stringify({
        gateway_url: gateway,
        device_id: name,
        token,
        roots: { work: mode === 'Service' ? process.env.ProgramData : temporary },
        allow_shell: false,
        allow_write: false,
      }),
    );
    const environment = { RDH_TEST_NAME: name, RDH_TEST_PREFIX: prefix, RDH_TEST_DATA: data };
    const stop =
      mode === 'Service'
        ? 'Stop-Service -Name $env:RDH_TEST_NAME'
        : 'Stop-ScheduledTask -TaskName $env:RDH_TEST_NAME';
    const start =
      mode === 'Service'
        ? 'Start-Service -Name $env:RDH_TEST_NAME'
        : 'Start-ScheduledTask -TaskName $env:RDH_TEST_NAME';
    try {
      console.log(
        await run([
          '-File',
          path.join(directory, 'install.ps1'),
          '-Base',
          directory,
          '-Startup',
          mode,
          '-Name',
          name,
          '-Prefix',
          prefix,
          '-Config',
          config,
          '-Server',
          gateway,
          '-NoPair',
        ]),
      );
      await waitFor(() => completed.get(name) >= 1, `${mode} Agent handshake and request`);
      const definition = JSON.parse(
        await run(
          [
            '-Command',
            mode === 'Service'
              ? 'Get-CimInstance Win32_Service -Filter "Name=\'$env:RDH_TEST_NAME\'" | Select-Object StartMode,StartName | ConvertTo-Json -Compress'
              : 'Get-ScheduledTask -TaskName $env:RDH_TEST_NAME | Select-Object -Property @{n="LogonType";e={$_.Principal.LogonType.ToString()}},@{n="Trigger";e={$_.Triggers[0].CimClass.CimClassName}} | ConvertTo-Json -Compress',
          ],
          environment,
        ),
      );
      if (mode === 'Service') {
        assert.equal(definition.StartMode, 'Auto');
        assert.match(definition.StartName, /LocalService/i);
        const owner = await run(
          [
            '-Command',
            '(Get-Acl -LiteralPath (Join-Path $env:RDH_TEST_DATA "agent.json")).GetOwner([Security.Principal.SecurityIdentifier]).Value',
          ],
          environment,
        );
        assert.equal(owner, 'S-1-5-19');
      } else {
        assert.equal(definition.LogonType, 'Interactive');
        assert.equal(definition.Trigger, 'MSFT_TaskLogonTrigger');
      }
      await run(['-Command', stop], environment);
      await waitFor(
        () => [...server.clients].every((socket) => socket.readyState !== 1),
        `${mode} connection closes on stop`,
      );
      const processes = await run(
        [
          '-Command',
          '@(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($env:RDH_TEST_PREFIX, [StringComparison]::OrdinalIgnoreCase) }).Count',
        ],
        environment,
      );
      assert.equal(processes, '0', `${mode} left a background process after stop`);
      await run(['-Command', start], environment);
      await waitFor(() => completed.get(name) >= 2, `${mode} restart handshake and request`);
      console.log(
        `${mode}: autostart definition, authenticated Agent request, stop with no remaining process, and restart passed.`,
      );
    } finally {
      // Dump only diagnostics, never the pairing configuration.
      await run(
        [
          '-Command',
          `Get-ChildItem -LiteralPath $env:RDH_TEST_DATA -Filter '*.log' -ErrorAction SilentlyContinue | ForEach-Object { Get-Content -LiteralPath $_.FullName -Tail 30 }`,
        ],
        environment,
      )
        .then(console.log)
        .catch(() => {});
      await run(
        [
          '-Command',
          mode === 'Service'
            ? 'Stop-Service -Name $env:RDH_TEST_NAME -ErrorAction SilentlyContinue; & "$env:SystemRoot\\System32\\sc.exe" delete $env:RDH_TEST_NAME | Out-Null'
            : 'Stop-ScheduledTask -TaskName $env:RDH_TEST_NAME -ErrorAction SilentlyContinue; Unregister-ScheduledTask -TaskName $env:RDH_TEST_NAME -Confirm:$false -ErrorAction SilentlyContinue',
        ],
        environment,
      );
      await run(
        [
          '-Command',
          'Remove-Item -LiteralPath $env:RDH_TEST_PREFIX -Recurse -Force -ErrorAction SilentlyContinue; Remove-Item -LiteralPath $env:RDH_TEST_DATA -Recurse -Force -ErrorAction SilentlyContinue',
        ],
        environment,
      );
    }
  }
} finally {
  for (const socket of server.clients) {
    socket.terminate();
  }
  await new Promise((resolve) => server.close(resolve));
  await rm(temporary, { recursive: true, force: true });
}
