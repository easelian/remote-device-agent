import fs from 'node:fs';
import path from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { readSidecar } from './hardening.js';
import { secureNewPath } from './platform.js';
import { acquireProfileLocks } from './profile-lock.js';
import { isRecord, errorCode, errorMessage } from './boundary.js';
import {
  NativeStartup,
  powershell,
  type StartupBinding,
  type StartupMode,
  type StartupBackend,
} from './startup-native.js';

export class StartupError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}
export function startupMode(mode: unknown): StartupMode {
  if (mode !== 'none' && mode !== 'user' && mode !== 'service') {
    throw new Error('mode must be none, user, or service');
  }
  return mode;
}
interface Pending {
  request_id: string;
  mode: 'user' | 'service';
  expires_at: number;
}
interface State {
  version: 1;
  binding: StartupBinding | null;
  pending: Pending | null;
}
function binding(value: unknown): StartupBinding {
  if (
    !isRecord(value) ||
    !['user', 'service'].includes(String(value.mode)) ||
    typeof value.name !== 'string' ||
    !/^[A-Za-z0-9][A-Za-z0-9_.-]{0,99}$/.test(value.name)
  ) {
    throw new Error('Invalid startup binding');
  }
  return { mode: value.mode as 'user' | 'service', name: value.name };
}
function writePrivate(file: string, value: unknown) {
  const temp = `${file}.${randomUUID()}.tmp`;
  try {
    fs.writeFileSync(temp, JSON.stringify(value, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
    secureNewPath(temp, false);
    fs.renameSync(temp, file);
  } finally {
    fs.rmSync(temp, { force: true });
  }
}
const ps = (s: string) => `'${s.replaceAll("'", "''")}'`;
function grantPath(config: string) {
  const root =
    process.platform === 'win32'
      ? path.join(process.env.ProgramData || 'C:\\ProgramData', 'RemoteDeviceHubStartupGrants')
      : process.platform === 'darwin'
        ? '/private/etc/remote-device-hub/startup-grants'
        : '/etc/remote-device-hub/startup-grants';
  return path.join(root, `${createHash('sha256').update(config).digest('hex')}.json`);
}
function trustedGrant(file: string) {
  if (process.platform === 'win32') {
    powershell(
      `$p=${ps(file)}; $first=Get-Item -LiteralPath $p -Force; $boundary=if ($first.PSIsContainer) {$p} else {Split-Path -Parent $p};
$trusted=@('S-1-5-18','S-1-5-32-544','S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464');
while ($p) {
  $i=Get-Item -LiteralPath $p -Force;
  if ($i.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Untrusted grant path' }
  $a=Get-Acl -LiteralPath $p;
  if ($a.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin $trusted) { throw 'Untrusted grant owner' }
  $mask=if ($p -eq ${ps(file)} -or $p -eq $boundary) {0xD0156} else {0xC0040};
  foreach ($r in $a.GetAccessRules($true,$true,[Security.Principal.SecurityIdentifier])) {
    if (($r.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) -eq 0 -and $r.AccessControlType -eq 'Allow' -and ($r.FileSystemRights -band $mask) -and $r.IdentityReference.Value -notin $trusted) { throw 'Writable grant path' }
  }
  $parent=Split-Path -Parent $p; if ($parent -eq $p) {break}; $p=$parent
}`,
    );
  } else {
    let cursor = file;
    while (cursor !== '/') {
      const stat = fs.lstatSync(cursor);
      if (stat.isSymbolicLink() || stat.uid !== 0 || stat.mode & 0o022) {
        throw new Error('Untrusted managed startup grant');
      }
      cursor = path.dirname(cursor);
    }
  }
}
export function managedStartupGrant(config: string, mode: StartupMode) {
  const file = grantPath(config);
  try {
    fs.statSync(file);
    trustedGrant(file);
    if (fs.statSync(file).size > 4096) {
      throw new Error('Managed startup grant is too large');
    }
    const raw: unknown = JSON.parse(fs.readFileSync(file, 'utf8'));
    return (
      isRecord(raw) &&
      raw.version === 1 &&
      raw.config === config &&
      Array.isArray(raw.modes) &&
      raw.modes.includes(mode)
    );
  } catch (error) {
    if (errorCode(error) === 'ENOENT' || !fs.existsSync(file)) {
      return false;
    }
    throw error;
  }
}
export function setManagedStartupGrant(config: string, mode: StartupMode) {
  const file = grantPath(config);
  const dir = path.dirname(file);
  if (process.platform === 'win32') {
    powershell(
      `$p=[Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent()); if (!$p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Administrator required' }; if (!(Test-Path -LiteralPath ${ps(dir)})) { New-Item -ItemType Directory -Path ${ps(dir)} | Out-Null; $a=[Security.AccessControl.DirectorySecurity]::new(); $a.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')); $a.SetAccessRuleProtection($true,$false); foreach ($s in @('S-1-5-18','S-1-5-32-544')) { $a.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($s),'FullControl','ContainerInherit,ObjectInherit','None','Allow')) }; $a.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-5-11'),'ReadAndExecute','ContainerInherit,ObjectInherit','None','Allow')); Set-Acl -LiteralPath ${ps(dir)} -AclObject $a }`,
    );
  } else {
    if (process.getuid?.() !== 0) {
      throw new StartupError(
        'startup_permission_required',
        'Administrator privileges are required to grant managed startup',
      );
    }
    fs.mkdirSync(dir, { recursive: true, mode: 0o755 });
  }
  trustedGrant(dir);
  if (fs.existsSync(file)) {
    trustedGrant(file);
  }
  if (mode === 'none') {
    fs.rmSync(file, { force: true });
    return;
  }
  const temp = path.join(dir, `${randomUUID()}.tmp`);
  try {
    fs.writeFileSync(temp, JSON.stringify({ version: 1, config, modes: [mode] }) + '\n', {
      flag: 'wx',
      mode: 0o644,
    });
    if (process.platform === 'win32') {
      powershell(
        `$a=Get-Acl -LiteralPath ${ps(temp)}; $a.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')); Set-Acl -LiteralPath ${ps(temp)} -AclObject $a`,
      );
    }
    fs.renameSync(temp, file);
  } finally {
    fs.rmSync(temp, { force: true });
  }
}
export class Startup {
  readonly file: string;
  readonly config: string;
  readonly backend: StartupBackend;
  constructor(
    config: string,
    backend?: StartupBackend,
    readonly granted = managedStartupGrant,
    readonly now = Date.now,
  ) {
    this.config = fs.existsSync(config)
      ? fs.realpathSync.native(config)
      : path.join(fs.realpathSync.native(path.dirname(config)), path.basename(config));
    this.backend = backend ?? new NativeStartup(this.config);
    this.file = `${this.config}.startup.json`;
  }
  private read(): State {
    const raw = readSidecar(this.file);
    if (raw === null) {
      return { version: 1, binding: null, pending: null };
    }
    if (!isRecord(raw) || raw.version !== 1) {
      throw new Error('Invalid startup state');
    }
    let pending: Pending | null = null;
    if (raw.pending !== null) {
      const p = raw.pending;
      if (
        !isRecord(p) ||
        typeof p.request_id !== 'string' ||
        !/^[a-f0-9-]{36}$/.test(p.request_id) ||
        (p.mode !== 'user' && p.mode !== 'service') ||
        typeof p.expires_at !== 'number' ||
        !Number.isSafeInteger(p.expires_at)
      ) {
        throw new Error('Invalid startup approval request');
      }
      if (p.expires_at > this.now()) {
        pending = { request_id: p.request_id, mode: p.mode, expires_at: p.expires_at };
      }
    }
    return { version: 1, binding: raw.binding === null ? null : binding(raw.binding), pending };
  }
  status() {
    const state = this.read();
    return {
      mode: state.binding
        ? this.backend.enabled(state.binding)
          ? state.binding.mode
          : 'none'
        : 'unknown',
      registration: state.binding,
      pending: state.pending,
      current_connection: 'unchanged',
      authorization: 'local_confirmation_or_administrator_grant',
      shell_authority:
        'Commands running as the Agent OS user can change user startup independently',
    };
  }
  private async locked<T>(action: () => T): Promise<T> {
    let lock;
    try {
      lock = await acquireProfileLocks([this.file]);
    } catch {
      throw new StartupError(
        'startup_busy',
        'Startup settings are being changed or a stale startup lock needs local inspection',
      );
    }
    try {
      return action();
    } finally {
      await lock.release();
    }
  }
  async bind(mode: 'user' | 'service', name: string) {
    return this.locked(() => {
      const state = this.read();
      const next = binding({ mode, name });
      if (state.binding && (state.binding.mode !== mode || state.binding.name !== name)) {
        throw new StartupError(
          'startup_binding_conflict',
          'This profile is already bound to a different startup registration',
        );
      }
      this.backend.enabled(next); // Verify the native registration before trusting it.
      writePrivate(this.file, { ...state, binding: next });
      return this.status();
    });
  }
  private apply(state: State, mode: StartupMode) {
    if (mode !== 'none' && state.binding && state.binding.mode !== mode) {
      throw new StartupError(
        'startup_binding_conflict',
        'Changing service identity requires local administrator migration; the current connection is unchanged',
      );
    }
    if (!state.binding && mode === 'service') {
      throw new StartupError(
        'startup_service_install_required',
        'Install and bind the advanced boot service locally before enabling it',
      );
    }
    if (!state.binding && mode === 'user') {
      state.binding = this.backend.createUser();
      writePrivate(this.file, state); // Preserve recovery binding even if native enable fails.
    }
    if (state.binding) {
      try {
        this.backend.set(state.binding, mode !== 'none');
      } catch (error) {
        console.error(`Startup manager operation failed: ${errorMessage(error)}`);
        throw new StartupError(
          'startup_permission_required',
          'The operating system did not allow or confirm the startup change; inspect native state locally. No stop was requested.',
        );
      }
    }
    state.pending = null;
    writePrivate(this.file, state);
    return { status: state.binding ? 'applied' : 'unbound', ...this.status() };
  }
  async set(mode: StartupMode, local = false) {
    return this.locked(() => {
      const state = this.read();
      if (mode === 'none') {
        // Revocation cancels a pending enable even if native disable fails.
        state.pending = null;
        writePrivate(this.file, state);
        return this.apply(state, mode);
      }
      if (
        mode === 'service' &&
        state.binding?.mode === 'service' &&
        this.backend.serviceAuthority?.() === false
      ) {
        throw new StartupError(
          'startup_permission_required',
          'Use the local administrator command rdh startup system set service; this service account cannot change system startup',
        );
      }
      if (local || this.granted(this.config, mode)) {
        return this.apply(state, mode);
      }
      if (state.pending && state.pending.mode !== mode) {
        throw new StartupError(
          'startup_busy',
          'A different startup request is awaiting local approval',
        );
      }
      state.pending ??= { request_id: randomUUID(), mode, expires_at: this.now() + 10 * 60_000 };
      writePrivate(this.file, state);
      return {
        status: 'pending_local_approval',
        ...state.pending,
        current_connection: 'unchanged',
      };
    });
  }
  async approve(id: string, accept: boolean) {
    return this.locked(() => {
      const state = this.read();
      if (!state.pending || state.pending.request_id !== id) {
        throw new StartupError(
          'startup_request_expired',
          'Approval request is missing, replaced, or expired',
        );
      }
      if (!accept) {
        state.pending = null;
        writePrivate(this.file, state);
        return { status: 'denied', ...this.status() };
      }
      return this.apply(state, state.pending.mode);
    });
  }
}
