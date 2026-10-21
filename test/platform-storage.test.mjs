import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import {
  platformName,
  defaultShell,
  configBase,
  stateBase,
  validateRelativePath,
  privateStat,
  absoluteDeviceRoot,
  assertPrivatePath,
  ensurePrivatePath,
  secureNewPath,
} from '../.build/platform.js';

const windows = process.platform === 'win32';
function temporary(t) {
  const root = fs.mkdtempSync(path.join(fs.realpathSync(os.tmpdir()), 'rdh-private-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  return root;
}

test('platform defaults use native names, explicit system executables and standard storage', () => {
  assert.equal(platformName('win32'), 'Windows');
  assert.equal(platformName('darwin'), 'Darwin');
  assert.equal(platformName('linux'), 'Linux');
  assert.throws(() => platformName('freebsd'), /Unsupported/);
  assert.equal(
    defaultShell('win32', { SystemRoot: 'C:\\Windows', ComSpec: 'malicious.exe' }),
    'C:\\Windows\\System32\\cmd.exe',
  );
  assert.throws(() => defaultShell('win32', { SystemRoot: 'relative' }), /SystemRoot/);
  assert.throws(
    () => defaultShell('win32', { SystemRoot: 'C:\\Windows\\..\\other' }),
    /SystemRoot/,
  );
  assert.equal(defaultShell('darwin', {}), '/bin/sh');
  assert.equal(configBase({}, '/home/me', 'linux'), '/home/me/.config');
  assert.equal(stateBase({}, '/Users/me', 'darwin'), '/Users/me/.local/state');
  assert.equal(configBase({}, 'C:\\Users\\me', 'win32'), 'C:\\Users\\me\\AppData\\Local');
  assert.equal(stateBase({ LOCALAPPDATA: 'D:\\Local' }, 'C:\\Users\\me', 'win32'), 'D:\\Local');
  assert.equal(configBase({ XDG_CONFIG_HOME: '/custom' }, '/home/me', 'linux'), '/custom');
  assert.equal(stateBase({ XDG_STATE_HOME: '/state' }, '/home/me', 'linux'), '/state');
});

test('protocol paths reject traversal and Windows aliases while retaining POSIX names', () => {
  for (const platform of ['linux', 'darwin', 'win32']) {
    assert.deepEqual(validateRelativePath('a/./b//', platform), ['a', 'b']);
    assert.deepEqual(validateRelativePath('', platform), []);
    for (const value of ['/etc/passwd', '../x', 'a/../b', 'a\0b']) {
      assert.throws(() => validateRelativePath(value, platform));
    }
  }
  for (const value of [
    'C:/x',
    'a\\b',
    '\\\\server\\share',
    'x:stream',
    'NUL',
    'con.txt',
    'aux',
    'COM1.log',
    'lpt9',
    'LPT¹.txt',
    'CONIN$',
    'con .txt',
    'file.',
    'file ',
    'a/b?',
    'a/<b>',
    'a/pipe|',
  ]) {
    assert.throws(() => validateRelativePath(value, 'win32'), undefined, value);
  }
  for (const value of ['NUL', 'file.', 'file ', 'x:stream', 'a\\b', 'C:/x']) {
    assert.ok(validateRelativePath(value, 'linux').length);
  }
  assert.deepEqual(validateRelativePath('console/com10.txt/a..b', 'win32'), [
    'console',
    'com10.txt',
    'a..b',
  ]);
});

test('Windows device roots are fully qualified independently of the test host', () => {
  for (const root of ['C:/work', 'C:\\work', '//host/share/work', '\\\\host\\share']) {
    assert.equal(absoluteDeviceRoot(root, 'win32'), true, root);
  }
  for (const root of [
    'C:work',
    '/work',
    '\\work',
    '//host',
    '//?/C:/work',
    '\\\\.\\pipe\\agent',
    '\\??\\C:\\work',
  ]) {
    assert.equal(absoluteDeviceRoot(root, 'win32'), false, root);
  }
});

test('private storage creation and validation preserve secret content', (t) => {
  const root = temporary(t);
  const directory = path.join(root, 'nested', 'private');
  ensurePrivatePath(directory, true);
  const filename = path.join(directory, 'token.json');
  ensurePrivatePath(filename, false);
  fs.writeFileSync(filename, 'secret');
  assertPrivatePath(filename, false);
  ensurePrivatePath(filename, false);
  assert.equal(fs.readFileSync(filename, 'utf8'), 'secret');
  assert.throws(() => assertPrivatePath(filename, true), /regular directory/);
  assert.throws(() => assertPrivatePath(directory, false), /regular file/);
  assert.equal(privateStat(fs.statSync(filename)), !windows);
});

test('POSIX refuses preexisting permissions without repairing them', { skip: windows }, (t) => {
  const root = temporary(t);
  const filename = path.join(root, 'existing');
  fs.writeFileSync(filename, 'secret', { mode: 0o644 });
  fs.chmodSync(filename, 0o644);
  assert.throws(() => ensurePrivatePath(filename, false), /no group\/other access/);
  assert.equal(fs.statSync(filename).mode & 0o777, 0o644);
  const ownNew = path.join(root, 'exclusive');
  fs.closeSync(fs.openSync(ownNew, 'wx', 0o644));
  secureNewPath(ownNew, false);
  assert.equal(fs.statSync(ownNew).mode & 0o777, 0o600);
});

test('POSIX refuses symlink files while preserving system directory aliases', {
  skip: windows,
}, (t) => {
  const root = temporary(t);
  const real = path.join(root, 'real');
  ensurePrivatePath(real, true);
  fs.symlinkSync(real, path.join(root, 'alias'));
  ensurePrivatePath(path.join(root, 'alias', 'token'), false);
  assert.equal(fs.existsSync(path.join(real, 'token')), true);
  const file = path.join(real, 'token');
  ensurePrivatePath(file, false);
  fs.symlinkSync(file, path.join(root, 'token-link'));
  assert.throws(() => assertPrivatePath(path.join(root, 'token-link'), false), /symbolic link/);
});

function powershell(script, filename) {
  const executable = path.win32.join(
    process.env.SystemRoot,
    'System32',
    'WindowsPowerShell',
    'v1.0',
    'powershell.exe',
  );
  const result = spawnSync(
    executable,
    [
      '-NoLogo',
      '-NoProfile',
      '-NonInteractive',
      '-EncodedCommand',
      Buffer.from(`$ErrorActionPreference = 'Stop'\n${script}`, 'utf16le').toString('base64'),
    ],
    {
      encoding: 'utf8',
      windowsHide: true,
      timeout: 15_000,
      env: { ...process.env, RDH_TEST_PATH: filename },
    },
  );
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, result.stderr);
}

test('Windows rejects broad ACL grants and leaves them unchanged', { skip: !windows }, (t) => {
  const root = temporary(t);
  const file = path.join(root, "literal ' $ & token.json");
  ensurePrivatePath(file, false);
  fs.writeFileSync(file, 'secret');
  powershell(
    `
$acl = Get-Acl -LiteralPath $env:RDH_TEST_PATH
$sid = [System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')
$acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($sid, 'Read', 'Allow'))
Set-Acl -LiteralPath $env:RDH_TEST_PATH -AclObject $acl
`,
    file,
  );
  assert.throws(() => assertPrivatePath(file, false), /another principal/);
  assert.throws(() => ensurePrivatePath(file, false), /another principal/);
  powershell(
    `
$acl = Get-Acl -LiteralPath $env:RDH_TEST_PATH
$rules = $acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])
if (-not ($rules | Where-Object { $_.IdentityReference.Value -eq 'S-1-1-0' })) { throw 'ACL was changed' }
`,
    file,
  );
});

test('Windows secures exclusively created children and rejects junction ancestors', {
  skip: !windows,
}, (t) => {
  const root = temporary(t);
  const directory = path.join(root, 'private');
  ensurePrivatePath(directory, true);
  const filename = path.join(directory, 'new');
  fs.closeSync(fs.openSync(filename, 'wx'));
  secureNewPath(filename, false);
  assertPrivatePath(filename, false);
  const alias = path.join(root, 'junction');
  fs.symlinkSync(directory, alias, 'junction');
  assert.throws(
    () => ensurePrivatePath(path.join(alias, 'other'), false),
    /symbolic|redirected|reparse/,
  );
  assert.equal(fs.existsSync(path.join(directory, 'other')), false);
});

test('Windows rejects deny ACLs rather than treating allow entries as effective access', {
  skip: !windows,
}, (t) => {
  const root = temporary(t);
  const filename = path.join(root, 'denied');
  ensurePrivatePath(filename, false);
  powershell(
    `
$acl = Get-Acl -LiteralPath $env:RDH_TEST_PATH
$sid = [System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-546')
$acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($sid, 'WriteData', 'Deny'))
Set-Acl -LiteralPath $env:RDH_TEST_PATH -AclObject $acl
`,
    filename,
  );
  assert.throws(() => assertPrivatePath(filename, false), /deny rule/);
});
