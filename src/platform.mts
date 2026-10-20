import fs, { type Stats } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

type Environment = NodeJS.ProcessEnv;

export function platformName(platform: string = process.platform): string {
  if (platform === 'win32') return 'Windows';
  if (platform === 'darwin') return 'Darwin';
  if (platform === 'linux') return 'Linux';
  throw new Error(`Unsupported platform: ${platform}`);
}

function systemRoot(env: Environment): string {
  const root = env.SystemRoot || env.SYSTEMROOT || env.windir;
  if (!root || !/^[a-z]:\\/i.test(root) || /[\0\r\n]/.test(root) ||
      root.slice(2).includes(':') || root.split(/[\\/]/).some(part => part === '..' || part === '.')) {
    throw new Error('A trusted absolute SystemRoot is required on Windows');
  }
  return path.win32.normalize(root);
}

export function defaultShell(platform: string = process.platform, env: Environment = process.env): string {
  return platform === 'win32' ? path.win32.join(systemRoot(env), 'System32', 'cmd.exe') : '/bin/sh';
}

export function configBase(env: Environment = process.env, home = os.homedir(), platform: string = process.platform): string {
  if (env.XDG_CONFIG_HOME) return env.XDG_CONFIG_HOME;
  return platform === 'win32'
    ? env.LOCALAPPDATA || path.win32.join(home, 'AppData', 'Local')
    : path.posix.join(home, '.config');
}

export function stateBase(env: Environment = process.env, home = os.homedir(), platform: string = process.platform): string {
  if (env.XDG_STATE_HOME) return env.XDG_STATE_HOME;
  return platform === 'win32'
    ? env.LOCALAPPDATA || path.win32.join(home, 'AppData', 'Local')
    : path.posix.join(home, '.local', 'state');
}

export function validateRelativePath(value: string, platform: string = process.platform): string[] {
  if (typeof value !== 'string' || value.length > 4096 || value.includes('\0') || path.posix.isAbsolute(value)) {
    throw new Error('Path must be relative and contain no NUL bytes');
  }
  const parts = value.split('/');
  if (parts.some(part => part === '..')) throw new Error("Path must be relative to a root, with no '..' or NUL");
  if (platform === 'win32') {
    if (/[\\:<>\"|?*\x00-\x1f]/.test(value) || parts.some(part =>
      (part !== '.' && /[ .]$/.test(part)) ||
      /^(?:con|prn|aux|nul|com[1-9¹²³]|lpt[1-9¹²³]|conin\$|conout\$)(?:[ .]|$)/i.test(part))) {
      throw new Error('Path contains an invalid Windows name');
    }
  }
  return parts.filter(part => part !== '' && part !== '.');
}

export function absoluteDeviceRoot(value: string, platform: string = process.platform): boolean {
  if (typeof value !== 'string' || value.length > 4096 || value.includes('\0')) return false;
  if (platform !== 'win32') return path.posix.isAbsolute(value);
  const normalized = value.replaceAll('\\', '/');
  if (/^\/(?:\/\?|\/\.|\?\?)(?:\/|$)/.test(normalized)) return false;
  if (/^[A-Za-z]:\//.test(normalized)) return true;
  const share = /^\/\/([^/]+)\/([^/]+)(?:\/|$)/.exec(normalized);
  return Boolean(share && ![share[1], share[2]].some(part =>
    ['.', '..', '?', '??'].includes(part) || /[<>:"|?*\x00-\x1f]/.test(part)));
}

// Windows Stats has no DACL information and must never authorize secret reads.
export function privateStat(stat: Stats): boolean {
  return process.platform !== 'win32' && typeof process.getuid === 'function' &&
    stat.uid === process.getuid() && (stat.mode & 0o077) === 0;
}

function missing(error: unknown): boolean {
  return error instanceof Error && 'code' in error && error.code === 'ENOENT';
}

function existingStat(filename: string): Stats | undefined {
  try { return fs.lstatSync(filename); }
  catch (error) { if (missing(error)) return undefined; throw error; }
}

function assertKind(filename: string, directory: boolean): Stats {
  const stat = fs.lstatSync(filename);
  if (stat.isSymbolicLink() || !(directory ? stat.isDirectory() : stat.isFile())) {
    throw new Error(`Private storage must be a regular ${directory ? 'directory' : 'file'}: ${filename}`);
  }
  return stat;
}

// POSIX keeps legitimate system aliases such as macOS /var -> /private/var.
// Windows additionally rejects parent aliases/reparse points. Node cannot hold
// directory handles for all subsequent path operations: an attacker able to
// rename ancestors concurrently remains outside this guarantee.
function assertAncestors(filename: string): void {
  if (process.platform !== 'win32') {
    if (existingStat(filename)?.isSymbolicLink()) {
      throw new Error(`Private storage cannot be a symbolic link: ${filename}`);
    }
    return;
  }
  let current = path.resolve(filename);
  for (;;) {
    const stat = existingStat(current);
    if (stat?.isSymbolicLink()) throw new Error(`Private storage cannot traverse symbolic links: ${current}`);
    const parent = path.dirname(current);
    if (parent === current) break;
    if (stat && current !== path.resolve(filename) && !stat.isDirectory()) {
      throw new Error(`Private storage ancestor is not a directory: ${current}`);
    }
    current = parent;
  }
  if (existingStat(filename)) {
    const resolved = path.resolve(filename);
    if (fs.realpathSync.native(resolved).toLowerCase() !== resolved.toLowerCase()) {
      throw new Error('Private storage cannot traverse redirected paths');
    }
  }
}

// Run only the Windows inbox PowerShell, without profiles or command strings
// containing caller data. The target arrives through an environment variable.
// A fresh process per operation intentionally avoids caching stale ACL results.
const ACL_SCRIPT = String.raw`
$ErrorActionPreference = 'Stop'
$p = $env:RDH_PRIVATE_PATH
$action = $env:RDH_PRIVATE_ACTION
$directory = $env:RDH_PRIVATE_DIRECTORY -eq '1'
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
function Check-Ancestors([string]$name) {
  $cursor = [System.IO.Path]::GetFullPath($name)
  while ($cursor) {
    if (Test-Path -LiteralPath $cursor) {
      $item = Get-Item -LiteralPath $cursor -Force
      if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw 'Private storage cannot traverse reparse points'
      }
    }
    $parent = [System.IO.Directory]::GetParent($cursor)
    if ($null -eq $parent) { break }
    $cursor = $parent.FullName
  }
}
function New-PrivateAcl {
  if ($directory) {
    $acl = [System.Security.AccessControl.DirectorySecurity]::new()
    $inherit = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
  } else {
    $acl = [System.Security.AccessControl.FileSecurity]::new()
    $inherit = [System.Security.AccessControl.InheritanceFlags]::None
  }
  $acl.SetOwner($sid)
  $acl.SetAccessRuleProtection($true, $false)
  $rule = [System.Security.AccessControl.FileSystemAccessRule]::new(
    $sid, [System.Security.AccessControl.FileSystemRights]::FullControl,
    $inherit, [System.Security.AccessControl.PropagationFlags]::None,
    [System.Security.AccessControl.AccessControlType]::Allow)
  $acl.AddAccessRule($rule)
  return $acl
}
Check-Ancestors $p
if ($action -eq 'create') {
  if (Test-Path -LiteralPath $p) { throw 'Private storage target already exists' }
  $acl = New-PrivateAcl
  if ($directory) {
    [System.IO.DirectoryInfo]::new($p).Create($acl)
  } else {
    $stream = [System.IO.FileStream]::new($p, [System.IO.FileMode]::CreateNew,
      [System.Security.AccessControl.FileSystemRights]::FullControl,
      [System.IO.FileShare]::None, 4096, [System.IO.FileOptions]::None, $acl)
    $stream.Dispose()
  }
} elseif ($action -eq 'secure-new') {
  $acl = New-PrivateAcl
  Set-Acl -LiteralPath $p -AclObject $acl
} elseif ($action -ne 'assert') {
  throw 'Invalid private storage action'
}
Check-Ancestors $p
$item = Get-Item -LiteralPath $p -Force
if ($item.PSIsContainer -ne $directory) { throw 'Unexpected private storage type' }
$actual = Get-Acl -LiteralPath $p
if ($actual.GetOwner([System.Security.Principal.SecurityIdentifier]).Value -ne $sid.Value) {
  throw 'Private storage owner is not the current user'
}
if (-not $actual.AreAccessRulesProtected) { throw 'Private storage ACL must disable inheritance' }
$allowed = @($sid.Value, 'S-1-5-18', 'S-1-5-32-544')
$userControl = $false
$rules = $actual.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])
foreach ($rule in $rules) {
  if ($rule.AccessControlType -ne [System.Security.AccessControl.AccessControlType]::Allow) {
    throw 'Private storage ACL contains a deny rule'
  }
  if ($allowed -notcontains $rule.IdentityReference.Value) { throw 'Private storage ACL grants another principal access' }
  if ($rule.IdentityReference.Value -eq $sid.Value -and
      ($rule.PropagationFlags -band [System.Security.AccessControl.PropagationFlags]::InheritOnly) -eq 0 -and
      ($rule.FileSystemRights -band [System.Security.AccessControl.FileSystemRights]::FullControl) -eq
        [System.Security.AccessControl.FileSystemRights]::FullControl) { $userControl = $true }
}
if (-not $userControl) { throw 'Private storage ACL does not grant the current user full control' }
[Console]::Out.Write('RDH_PRIVATE_OK')
`;

function windowsAcl(filename: string, directory: boolean, action: 'assert' | 'create' | 'secure-new'): void {
  const shellDirectory = path.win32.join(systemRoot(process.env), 'System32', 'WindowsPowerShell', 'v1.0');
  const executable = path.win32.join(shellDirectory, 'powershell.exe');
  const env: Environment = {};
  // PowerShell autoloads Get-Acl/Set-Acl. Do not search user-supplied modules.
  for (const [key, value] of Object.entries(process.env)) {
    if (key.toLowerCase() !== 'psmodulepath') env[key] = value;
  }
  env.PSModulePath = path.win32.join(shellDirectory, 'Modules');
  env.RDH_PRIVATE_PATH = path.resolve(filename);
  env.RDH_PRIVATE_ACTION = action;
  env.RDH_PRIVATE_DIRECTORY = directory ? '1' : '0';
  const result = spawnSync(executable, ['-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand',
    Buffer.from(ACL_SCRIPT, 'utf16le').toString('base64')], {
    encoding: 'utf8', windowsHide: true, timeout: 15_000, maxBuffer: 64 * 1024,
    env,
  });
  if (result.error || result.status !== 0 || result.stdout.trim() !== 'RDH_PRIVATE_OK') {
    throw new Error(`Windows private storage ACL ${action} failed: ${result.error?.message || result.stderr.trim() || 'verification unavailable'}`);
  }
}

export function assertPrivatePath(filename: string, directory: boolean): void {
  assertAncestors(filename);
  const stat = assertKind(filename, directory);
  if (process.platform === 'win32') windowsAcl(filename, directory, 'assert');
  else if (!privateStat(stat)) throw new Error(`Private storage must be owned by the current user with no group/other access (${directory ? '0700' : '0600'}): ${filename}`);
}

// Only call immediately after the caller's successful exclusive creation, and
// before writing secrets. Never use this to repair untrusted existing storage.
export function secureNewPath(filename: string, directory: boolean): void {
  assertAncestors(filename);
  assertKind(filename, directory);
  if (process.platform === 'win32') windowsAcl(filename, directory, 'secure-new');
  else {
    const stat = fs.lstatSync(filename);
    if (typeof process.getuid !== 'function' || stat.uid !== process.getuid()) {
      throw new Error('New private storage is not owned by the current user');
    }
    fs.chmodSync(filename, directory ? 0o700 : 0o600);
    assertPrivatePath(filename, directory);
  }
}

export function ensurePrivatePath(filename: string, directory: boolean): void {
  assertAncestors(filename);
  if (existingStat(filename)) return assertPrivatePath(filename, directory);
  const parent = path.dirname(path.resolve(filename));
  if (!existingStat(parent)) ensurePrivatePath(parent, true);
  if (process.platform === 'win32') {
    windowsAcl(filename, directory, 'create');
    return;
  }
  try {
    if (directory) fs.mkdirSync(filename, { mode: 0o700 });
    else fs.closeSync(fs.openSync(filename, fs.constants.O_WRONLY | fs.constants.O_CREAT |
      fs.constants.O_EXCL | fs.constants.O_NOFOLLOW, 0o600));
  } catch (error) {
    if (!(error instanceof Error && 'code' in error && error.code === 'EEXIST')) throw error;
  }
  assertPrivatePath(filename, directory);
}
