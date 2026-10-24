#!/usr/bin/env node
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, writeFile, rm, readFile } from 'node:fs/promises';
import { realpathSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import WebSocketServer from '../vendor/ws/lib/websocket-server.js';

assert.equal(process.platform, 'win32');
const execute = promisify(execFile);
const directory = path.resolve(process.argv[2] || 'dist');
const temporary = realpathSync.native(await mkdtemp(path.join(os.tmpdir(), 'rdh startup ')));
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
const serviceCleanup = String.raw`$ErrorActionPreference = 'Stop'
if (!(Test-Path -LiteralPath $env:RDH_TEST_DATA)) { exit 0 }
$owner = (Get-Acl -LiteralPath $env:RDH_TEST_DATA).GetOwner([Security.Principal.SecurityIdentifier]).Value
if ($owner -ne 'S-1-5-19') {
    Remove-Item -LiteralPath $env:RDH_TEST_DATA -Recurse -Force
    exit 0
}
# Private runtime-created files intentionally belong only to LocalService.
# Remove test data as its owner rather than weakening the production ACLs.
$name = $env:RDH_TEST_NAME + '-cleanup'
$literal = "'" + $env:RDH_TEST_DATA.Replace("'", "''") + "'"
$body = '$ErrorActionPreference = "Stop"; Remove-Item -LiteralPath ' + $literal + ' -Recurse -Force'
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($body))
$action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument "-NoProfile -NonInteractive -EncodedCommand $encoded"
$principal = New-ScheduledTaskPrincipal -UserId 'S-1-5-19' -LogonType ServiceAccount
Register-ScheduledTask -TaskName $name -Action $action -Principal $principal | Out-Null
try {
    Start-ScheduledTask -TaskName $name
    $deadline = (Get-Date).AddSeconds(60)
    do {
        Start-Sleep -Milliseconds 250
        $task = Get-ScheduledTask -TaskName $name
        $info = Get-ScheduledTaskInfo -TaskName $name
        if ($task.State -eq 'Ready' -and $info.LastRunTime.Year -gt 2000) {
            if ($info.LastTaskResult -ne 0) { throw "Cleanup task failed: $($info.LastTaskResult)" }
            if (Test-Path -LiteralPath $env:RDH_TEST_DATA) { throw 'Service test data remains' }
            exit 0
        }
    } while ((Get-Date) -lt $deadline)
    throw 'Service test data cleanup timed out'
} finally {
    Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $name -Confirm:$false
}
`;
const token = 'startup-test-token-'.repeat(3);
const server = new WebSocketServer({
  port: 0,
  host: '127.0.0.1',
  verifyClient: ({ req }, done) => done(req.headers.authorization === `Bearer ${token}`, 401),
});
await new Promise((resolve) => server.once('listening', resolve));
const gateway = `http://127.0.0.1:${server.address().port}`;
const completed = new Map();
const children = new Map();
const commands = new Map();
const manifest = JSON.parse(await readFile(path.join(directory, 'rdh-manifest.json'), 'utf8'));
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
      socket.send(
        JSON.stringify({
          type: 'request',
          id: 'child',
          operation: 'start_process',
          params: {
            command: commands.get(id),
            root: 'work',
            cwd: '.',
            timeout_seconds: 300,
            wait_seconds: 0,
          },
        }),
      );
    } else if (message.id === 'info' && message.type === 'result' && message.result?.roots?.work) {
      completed.set(id, (completed.get(id) || 0) + 1);
    } else if (message.id === 'child' && message.result?.status === 'running') {
      children.set(id, (children.get(id) || 0) + 1);
    }
  });
});
const waitFor = async (condition, description) => {
  const deadline = Date.now() + 90_000;
  while (!(await condition())) {
    assert.ok(Date.now() < deadline, `Timed out: ${description}`);
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
};
const failures = [];
try {
  // With redirected input and no -Startup argument, the default must remain temporary.
  const temporaryName = `RDH-Test-None-${process.pid}`;
  const temporaryPrefix = path.join(temporary, 'temporary installation');
  const temporaryConfig = path.join(temporary, 'temporary.json');
  const temporaryNode = path.join(
    temporaryPrefix,
    'versions',
    `rdh-${manifest.version}-win32-${process.arch}`,
    'node.exe',
  );
  commands.set(temporaryName, `"${temporaryNode}" -e "setInterval(() => {}, 1000)"`);
  await writeFile(
    temporaryConfig,
    JSON.stringify({
      gateway_url: gateway,
      device_id: temporaryName,
      token,
      roots: { work: temporary },
      allow_shell: true,
      allow_write: false,
    }),
  );
  await run(
    [
      '-Command',
      String.raw`
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
    foreach ($p in @($env:RDH_TEST_PARENT, $env:RDH_TEST_CONFIG)) {
      $item = Get-Item -LiteralPath $p
      $acl = if ($item.PSIsContainer) { [Security.AccessControl.DirectorySecurity]::new() } else { [Security.AccessControl.FileSecurity]::new() }
      $acl.SetOwner($sid)
      $acl.SetAccessRuleProtection($true, $false)
      $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'Allow'))
      Set-Acl -LiteralPath $p -AclObject $acl
    }
  `,
    ],
    { RDH_TEST_PARENT: temporary, RDH_TEST_CONFIG: temporaryConfig },
  );
  const foreground = spawn(
    shell,
    [
      '-NoProfile',
      '-NonInteractive',
      '-ExecutionPolicy',
      'Bypass',
      '-File',
      path.join(directory, 'install.ps1'),
      '-Base',
      directory,
      '-Prefix',
      temporaryPrefix,
      '-Name',
      temporaryName,
      '-Config',
      temporaryConfig,
      '-Server',
      gateway,
    ],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  );
  const foregroundExit = new Promise((resolve) => foreground.once('exit', resolve));
  let foregroundLog = '';
  foreground.stdout.on('data', (data) => {
    foregroundLog += data;
  });
  foreground.stderr.on('data', (data) => {
    foregroundLog += data;
  });
  try {
    await waitFor(
      () => completed.get(temporaryName) >= 1 && children.get(temporaryName) >= 1,
      'default temporary Agent request',
    );
    assert.match(foregroundLog, /Running this time only/);
    assert.equal(
      await run(
        [
          '-Command',
          '@(Get-ScheduledTask -TaskName $env:RDH_TEST_NAME -ErrorAction SilentlyContinue).Count; @(Get-Service -Name $env:RDH_TEST_NAME -ErrorAction SilentlyContinue).Count',
        ],
        { RDH_TEST_NAME: temporaryName },
      ),
      '0\r\n0',
    );
    const socket = [...server.clients].find(
      (item) => item.deviceId === temporaryName && item.readyState === 1,
    );
    const node = path.join(
      temporaryPrefix,
      'versions',
      `rdh-${manifest.version}-win32-${process.arch}`,
      'node.exe',
    );
    const cli = path.join(path.dirname(node), 'bin/remote-device-hub.mjs');
    let createdName;
    try {
      await execute(node, [cli, 'startup', 'grant', 'user', '--config', temporaryConfig]);
      const enabled = await requestStartup(socket, 'set_startup', { mode: 'user' });
      assert.equal(enabled.result?.mode, 'user', JSON.stringify(enabled));
      createdName = enabled.result.registration.name;
      assert.match(createdName, /^RemoteDeviceHub-S-1-5-/);
      const off = await requestStartup(socket, 'set_startup', { mode: 'none' });
      assert.equal(off.result?.mode, 'none', JSON.stringify(off));
      assert.equal(socket.readyState, 1);
      await execute(node, [cli, 'startup', 'revoke', '--config', temporaryConfig]);
      const pending = await requestStartup(socket, 'set_startup', { mode: 'user' });
      assert.equal(pending.result?.status, 'pending_local_approval', JSON.stringify(pending));
      await requestStartup(socket, 'set_startup', { mode: 'none' });
      await execute('taskkill.exe', ['/PID', String(foreground.pid), '/T', '/F']);
      await foregroundExit;
      await run(
        [
          '-Command',
          'Enable-ScheduledTask -TaskName $env:RDH_CREATED_TASK | Out-Null; Start-ScheduledTask -TaskName $env:RDH_CREATED_TASK',
        ],
        { RDH_CREATED_TASK: createdName },
      );
      await waitFor(
        () => completed.get(temporaryName) >= 2 && children.get(temporaryName) >= 2,
        'newly created task reconnects to paired Hub',
      );
      const restarted = [...server.clients].find(
        (item) => item.deviceId === temporaryName && item.readyState === 1,
      );
      const disabled = await requestStartup(restarted, 'set_startup', { mode: 'none' });
      assert.equal(disabled.result?.mode, 'none', JSON.stringify(disabled));
      assert.equal(restarted.readyState, 1);
    } finally {
      await execute(node, [cli, 'startup', 'revoke', '--config', temporaryConfig]);
      if (createdName) {
        await run(
          [
            '-Command',
            'Stop-ScheduledTask -TaskName $env:RDH_CREATED_TASK; Unregister-ScheduledTask -TaskName $env:RDH_CREATED_TASK -Confirm:$false',
          ],
          { RDH_CREATED_TASK: createdName },
        );
      }
    }
    console.log(
      'None: managed grant creates future logon startup; disabling retains the foreground connection; revocation requires local approval again.',
    );
  } catch (error) {
    console.error(foregroundLog);
    throw error;
  } finally {
    if (foreground.exitCode === null) {
      await execute('taskkill.exe', ['/PID', String(foreground.pid), '/T', '/F']);
      await foregroundExit;
    }
  }
  for (const mode of ['User', 'Service']) {
    const name = `RDH-Test-${mode}-${process.pid}`;
    const prefix =
      mode === 'Service'
        ? path.join(process.env.ProgramFiles, name)
        : path.join(temporary, 'user installation');
    const data =
      mode === 'Service' ? path.join(process.env.ProgramData, name) : path.join(prefix, 'data');
    const config = path.join(temporary, `${mode}.json`);
    const bundledNode = path.join(
      prefix,
      'versions',
      `rdh-${manifest.version}-win32-${process.arch}`,
      'node.exe',
    );
    commands.set(name, `"${bundledNode}" -e "setInterval(() => {}, 1000)"`);
    // ProgramData is accessible to LocalService; no personal directory ACLs are broadened.
    await writeFile(
      config,
      JSON.stringify({
        gateway_url: gateway,
        device_id: name,
        token,
        roots: { work: mode === 'Service' ? process.env.ProgramData : temporary },
        allow_shell: true,
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
      await waitFor(
        () => completed.get(name) >= 1 && children.get(name) >= 1,
        `${mode} Agent handshake and request`,
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
        if (mode === 'Service') {
          const result = await execute(bundledNode, [
            path.join(path.dirname(bundledNode), 'bin/remote-device-hub.mjs'),
            'startup',
            'system',
            'set',
            'none',
            '--name',
            name,
          ]);
          assert.equal(JSON.parse(result.stdout).mode, 'none');
        }
        assert.equal(socket.readyState, 1, 'disable must preserve the original socket');
        const after = await requestStartup(socket, 'device_info');
        assert.ok(after.result?.roots?.work, JSON.stringify(after));
        const state = await requestStartup(socket, 'get_startup');
        assert.equal(state.result?.pending, null, JSON.stringify(state));
        await run(
          [
            '-Command',
            mode === 'Service'
              ? 'Set-Service -Name $env:RDH_TEST_NAME -StartupType Automatic'
              : 'Enable-ScheduledTask -TaskName $env:RDH_TEST_NAME | Out-Null',
          ],
          environment,
        );
      }
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
        () =>
          [...server.clients].every(
            (socket) => socket.deviceId !== name || socket.readyState !== 1,
          ),
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
      await waitFor(
        () => completed.get(name) >= 2 && children.get(name) >= 2,
        `${mode} restart handshake and request`,
      );
      console.log(
        `${mode}: autostart definition, authenticated Agent request, stop with no remaining process, and restart passed.`,
      );
    } catch (error) {
      console.error(error);
      failures.push(error);
    } finally {
      if (mode === 'User') {
        console.log(
          await run(
            [
              '-Command',
              'Get-ScheduledTaskInfo -TaskName $env:RDH_TEST_NAME | Format-List; Get-ScheduledTask -TaskName $env:RDH_TEST_NAME | Select-Object State | Format-List',
            ],
            environment,
          ).catch(String),
        );
      }
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
      ).catch((error) => failures.push(error));
      await waitFor(
        async () =>
          (await run(
            [
              '-Command',
              '@(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($env:RDH_TEST_PREFIX, [StringComparison]::OrdinalIgnoreCase) }).Count',
            ],
            environment,
          )) === '0',
        `${mode} cleanup waits for all processes`,
      ).catch((error) => failures.push(error));
      const cleanup = async () => {
        if (mode === 'Service') {
          await run(['-Command', serviceCleanup], environment);
        }
        await rm(prefix, { recursive: true, force: true, maxRetries: 5, retryDelay: 500 });
      };
      // Windows can retain executable/file handles briefly after process exit.
      // Retry only disposable test-file removal, never installation or pairing.
      let cleanupError;
      for (let attempt = 0; attempt < 5; attempt++) {
        try {
          await cleanup();
          cleanupError = undefined;
          break;
        } catch (error) {
          cleanupError = error;
          await new Promise((resolve) => setTimeout(resolve, 1000));
        }
      }
      if (cleanupError) {
        console.error(cleanupError);
        failures.push(cleanupError);
      }
    }
  }
  if (failures.length) {
    throw new AggregateError(failures, 'Windows startup verification failed');
  }
} finally {
  for (const socket of server.clients) {
    socket.terminate();
  }
  await new Promise((resolve) => server.close(resolve));
  await rm(temporary, { recursive: true, force: true, maxRetries: 5, retryDelay: 1000 }).catch(
    console.error,
  );
}
