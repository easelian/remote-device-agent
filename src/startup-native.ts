import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { configBase, defaultShell } from './platform.js';

export type StartupMode = 'none' | 'user' | 'service';
export interface StartupBinding {
  mode: 'user' | 'service';
  name: string;
}
export interface StartupBackend {
  enabled(binding: StartupBinding): boolean;
  set(binding: StartupBinding, enabled: boolean): void;
  createUser(): StartupBinding;
}
const literal = (s: string) => `'${s.replaceAll("'", "''")}'`;
const xml = (s: string) =>
  s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
const shell = (s: string) => `'${s.replaceAll("'", "'\\''")}'`;
export function powershell(script: string): string {
  const executable = path.join(
    path.dirname(defaultShell('win32')),
    'WindowsPowerShell/v1.0/powershell.exe',
  );
  return execFileSync(
    executable,
    [
      '-NoProfile',
      '-NonInteractive',
      '-EncodedCommand',
      Buffer.from(`$ErrorActionPreference='Stop'; ${script}`, 'utf16le').toString('base64'),
    ],
    { encoding: 'utf8', timeout: 12_000, windowsHide: true },
  ).trim();
}
export class NativeStartup implements StartupBackend {
  constructor(
    readonly config: string,
    readonly executable = process.execPath,
    readonly entry = path.resolve(process.argv[1] || ''),
  ) {
    for (const value of [config, executable, entry]) {
      if (!path.isAbsolute(value) || /[\x00-\x1f]/.test(value)) {
        throw new Error('Startup requires absolute paths without control characters');
      }
    }
  }
  private run(command: string, args: string[]) {
    return execFileSync(command, args, {
      encoding: 'utf8',
      timeout: 12_000,
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
  }
  private domain(b: StartupBinding) {
    return b.mode === 'service' ? 'system' : `gui/${process.getuid?.()}`;
  }
  enabled(b: StartupBinding): boolean {
    if (process.platform === 'win32') {
      return (
        powershell(
          b.mode === 'user'
            ? `(Get-ScheduledTask -TaskName ${literal(b.name)}).Settings.Enabled`
            : `(Get-Service -Name ${literal(b.name)}).StartType -eq 'Automatic'`,
        ).toLowerCase() === 'true'
      );
    }
    if (process.platform === 'darwin') {
      const definition =
        b.mode === 'service'
          ? path.join('/Library/LaunchDaemons', `${b.name}.plist`)
          : path.join(os.homedir(), 'Library/LaunchAgents', `${b.name}.plist`);
      if (!fs.existsSync(definition)) {
        throw new Error('Startup registration is missing');
      }
      const output = this.run('/bin/launchctl', ['print-disabled', this.domain(b)]);
      const escaped = b.name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      return !new RegExp(`"${escaped}"\\s*=>\\s*true`).test(output);
    }
    // is-enabled uses exit status 1 for a disabled unit. Other failures are not 'none'.
    try {
      return (
        this.run('/usr/bin/systemctl', [
          ...(b.mode === 'user' ? ['--user'] : []),
          'is-enabled',
          b.name,
        ]) === 'enabled'
      );
    } catch (error) {
      const e = error as { stdout?: string; status?: number };
      if (e.status === 1 && String(e.stdout).trim() === 'disabled') {
        return false;
      }
      throw error;
    }
  }
  set(b: StartupBinding, enabled: boolean) {
    // Never stop, restart, bootout, or use --now: the current connection survives.
    if (process.platform === 'win32') {
      powershell(
        b.mode === 'user'
          ? `${enabled ? 'Enable' : 'Disable'}-ScheduledTask -TaskName ${literal(b.name)} | Out-Null`
          : `Set-Service -Name ${literal(b.name)} -StartupType ${enabled ? 'Automatic' : 'Disabled'}`,
      );
    } else if (process.platform === 'darwin') {
      this.run('/bin/launchctl', [enabled ? 'enable' : 'disable', `${this.domain(b)}/${b.name}`]);
    } else {
      this.run('/usr/bin/systemctl', [
        '--no-ask-password',
        ...(b.mode === 'user' ? ['--user'] : []),
        enabled ? 'enable' : 'disable',
        b.name,
      ]);
    }
    if (this.enabled(b) !== enabled) {
      throw new Error(
        'Startup manager did not confirm the requested state; inspect before retrying',
      );
    }
  }
  createUser(): StartupBinding {
    const args = [this.entry, '--config', this.config, '--no-browser', '--recover-lock'];
    if (!fs.existsSync(this.entry)) {
      throw new Error('A persistent Agent executable is required');
    }
    if (process.platform === 'win32') {
      const name = powershell(
        '"RemoteDeviceHub-" + [Security.Principal.WindowsIdentity]::GetCurrent().User.Value',
      );
      const root = path.resolve(path.dirname(this.entry), '..');
      const runner = [
        path.join(root, 'windows-logon.ps1'),
        path.join(root, 'packaging/windows-logon.ps1'),
      ].find((file) => fs.existsSync(file));
      if (!runner) {
        throw new Error('Windows startup helper is missing; reinstall the Agent package');
      }
      const command = `& ${literal(runner)} -Node ${literal(this.executable)} -Log ${literal(`${this.config}.startup.log`)} -AgentArguments @(${args.map(literal).join(',')})`;
      const encoded = Buffer.from(command, 'utf16le').toString('base64');
      const host = path.join(
        path.dirname(defaultShell('win32')),
        'WindowsPowerShell/v1.0/powershell.exe',
      );
      powershell(`$id=[Security.Principal.WindowsIdentity]::GetCurrent();
if (Get-ScheduledTask -TaskName ${literal(name)} -ErrorAction SilentlyContinue) { throw 'Startup registration already exists; bind it locally after inspection' }
$a=New-ScheduledTaskAction -Execute ${literal(host)} -Argument ${literal(`-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand ${encoded}`)} -WorkingDirectory ${literal(path.dirname(this.config))};
$t=New-ScheduledTaskTrigger -AtLogOn -User $id.Name;
$p=New-ScheduledTaskPrincipal -UserId $id.Name -LogonType Interactive -RunLevel Limited;
$s=New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries;
$s.Enabled=$false;
Register-ScheduledTask -TaskName ${literal(name)} -Action $a -Trigger $t -Principal $p -Settings $s | Out-Null`);
      return { mode: 'user', name };
    }
    const mac = process.platform === 'darwin';
    const name = mac ? 'com.be-one-click.rdh' : 'rdh.service';
    const directory = mac
      ? path.join(os.homedir(), 'Library/LaunchAgents')
      : path.join(configBase(), 'systemd/user');
    fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
    const file = path.join(directory, mac ? `${name}.plist` : name);
    const wrapper = `${this.config}.startup-run`;
    // Exclusive creation avoids replacing another profile's startup definition.
    fs.writeFileSync(
      wrapper,
      `#!/bin/sh\ncd ${shell(path.dirname(this.config))} || exit 1\nexec ${[this.executable, ...args].map(shell).join(' ')}\n`,
      { flag: 'wx', mode: 0o700 },
    );
    try {
      const quote = (s: string) =>
        s
          .replaceAll('\\', '\\\\')
          .replaceAll('"', '\\"')
          .replaceAll('%', '%%')
          .replaceAll('$', '$$');
      fs.writeFileSync(
        file,
        mac
          ? `<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>Label</key><string>${name}</string><key>ProgramArguments</key><array><string>${xml(wrapper)}</string></array><key>RunAtLoad</key><true/></dict></plist>\n`
          : `[Unit]\nDescription=Remote Device Hub Agent\n[Service]\nExecStart="${quote(wrapper)}"\nRestart=on-failure\nRestartSec=5\n[Install]\nWantedBy=default.target\n`,
        { flag: 'wx', mode: 0o600 },
      );
    } catch (error) {
      fs.unlinkSync(wrapper);
      throw error;
    }
    try {
      if (mac) {
        this.run('/bin/launchctl', ['disable', `gui/${process.getuid?.()}/${name}`]);
      } else {
        this.run('/usr/bin/systemctl', ['--user', 'daemon-reload']);
      }
    } catch (error) {
      fs.unlinkSync(file);
      fs.unlinkSync(wrapper);
      throw error;
    }
    return { mode: 'user', name };
  }
}
