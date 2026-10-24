import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
  existsSync,
  realpathSync,
} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

const installer = path.resolve('packaging/install.sh');
function fixture(platform = 'Linux') {
  const dir = realpathSync(mkdtempSync(path.join(os.tmpdir(), 'rdh install ')));
  const home = path.join(dir, 'home');
  const commands = path.join(dir, 'commands');
  const tree = path.join(dir, 'runtime');
  for (const name of [home, commands, path.join(tree, 'bin')]) {
    mkdirSync(name, { recursive: true });
  }
  const quote = (s) => `'${s.replaceAll("'", "'\\''")}'`;
  const cli = path.join(dir, 'fixture.cjs');
  writeFileSync(
    cli,
    `const fs = require('node:fs');
const args = process.argv.slice(2);
if (args.includes('--version')) { console.log('1.2.3'); } else {
fs.appendFileSync(process.env.INSTALL_CALLS, JSON.stringify(args) + '\\n');
if (process.env.FAIL_PAIR && args.includes('--pair-only')) process.exit(7);
}
`,
  );
  writeFileSync(
    path.join(tree, 'rdh'),
    `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(cli)} "$@"\n`,
    { mode: 0o755 },
  );
  writeFileSync(path.join(tree, 'node'), '#!/bin/sh\necho 1.2.3\n');
  writeFileSync(path.join(tree, 'bin/remote-device-hub.mjs'), '');
  const arch = process.arch === 'arm64' ? 'arm64' : 'x64';
  const archive = path.join(
    dir,
    `rdh-1.2.3-${platform === 'Darwin' ? 'darwin' : 'linux'}-${arch}${platform === 'Linux' ? '-gnu' : ''}.tar.gz`,
  );
  execFileSync('tar', ['-czf', archive, '-C', dir, 'runtime']);
  writeFileSync(
    path.join(commands, 'uname'),
    `#!/bin/sh\ncase "$1" in -s) echo ${platform};; -m) echo ${arch === 'arm64' ? 'arm64' : 'x86_64'};; esac\n`,
    { mode: 0o755 },
  );
  for (const command of ['systemctl', 'launchctl', 'loginctl']) {
    writeFileSync(
      path.join(commands, command),
      `#!/bin/sh\nprintf '%s\\n' "${command} $*" >> "$MANAGER_CALLS"\ncase "$*" in *enable*|*bootstrap*) [ -z "$FAIL_START" ] || exit 9;; esac\nexit 0\n`,
      { mode: 0o755 },
    );
  }
  const env = {
    ...process.env,
    HOME: home,
    XDG_CONFIG_HOME: path.join(home, '.config'),
    PATH: `${commands}:${process.env.PATH}`,
    INSTALL_CALLS: path.join(dir, 'calls'),
    MANAGER_CALLS: path.join(dir, 'manager'),
  };
  const prefix = path.join(home, "runtime ' & $ %");
  const unit =
    platform === 'Linux'
      ? path.join(home, '.config/systemd/user/rdh.service')
      : path.join(home, 'Library/LaunchAgents/com.be-one-click.rdh.plist');
  return {
    dir,
    env,
    prefix,
    unit,
    run(args = [], extra = {}) {
      return spawnSync(
        'sh',
        [
          installer,
          '--offline',
          archive,
          '--prefix',
          prefix,
          '--bin-dir',
          path.join(home, 'bin'),
          ...args,
        ],
        { env: { ...env, ...extra }, encoding: 'utf8' },
      );
    },
    calls() {
      return readFileSync(env.INSTALL_CALLS, 'utf8').trim().split('\n').map(JSON.parse);
    },
    close() {
      rmSync(dir, { recursive: true, force: true });
    },
  };
}
for (const platform of ['Linux', 'Darwin']) {
  test(`${platform}: default runs once after pairing and never registers startup`, () => {
    const f = fixture(platform);
    try {
      const result = f.run(['--root', 'work=/tmp']);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(f.calls().length, 2);
      assert.ok(f.calls()[0].includes('--pair-only'));
      assert.deepEqual(f.calls()[1], ['--no-browser', '--root', 'work=/tmp']);
      assert.equal(existsSync(f.env.MANAGER_CALLS), false);
      assert.equal(existsSync(f.unit), false);
      assert.match(result.stdout, /Running this time only/);
    } finally {
      f.close();
    }
  });
  test(`${platform}: explicit user mode persists correctly quoted arguments`, () => {
    const f = fixture(platform);
    try {
      const args = ['--config', path.join(f.dir, "paired ' & $.json"), '--root', 'work=/tmp'];
      const result = f.run(['--startup', 'user', ...args]);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(f.calls().length, 2);
      assert.deepEqual(f.calls()[1].slice(0, 3), ['startup', 'bind', 'user']);
      assert.ok(existsSync(f.unit));
      execFileSync(path.join(f.prefix, 'run-agent'), [], { env: f.env });
      assert.deepEqual(f.calls()[2], ['--no-browser', ...args, '--recover-lock']);
      const definition = readFileSync(f.unit, 'utf8');
      if (platform === 'Linux') {
        assert.match(definition, /WantedBy=default.target/);
        assert.match(definition, /\$\$ %%/);
      } else {
        assert.match(definition, /&amp;/);
        assert.equal(definition.includes('<key>UserName</key>'), false);
      }
      const second = f.run();
      assert.notEqual(second.status, 0);
      assert.match(second.stderr, /startup is already configured/);
    } finally {
      f.close();
    }
  });
  test(`${platform}: pairing/start failures never leave startup enabled`, () => {
    for (const failure of ['FAIL_PAIR', 'FAIL_START']) {
      const f = fixture(platform);
      try {
        const result = f.run(['--startup', 'user'], { [failure]: '1' });
        assert.notEqual(result.status, 0);
        assert.match(
          result.stderr,
          failure === 'FAIL_PAIR' ? /pairing failed/ : /start failed|registration failed/,
        );
        assert.equal(existsSync(f.unit), false);
        if (failure === 'FAIL_PAIR') {
          assert.equal(
            existsSync(f.env.MANAGER_CALLS) &&
              readFileSync(f.env.MANAGER_CALLS, 'utf8').includes('enable --now'),
            false,
          );
        }
      } finally {
        f.close();
      }
    }
  });
}
test('install-only stays inactive and invalid startup modes fail before download', () => {
  const f = fixture();
  try {
    assert.equal(f.run(['--no-pair']).status, 0);
    assert.equal(existsSync(f.env.INSTALL_CALLS), false);
    assert.notEqual(f.run(['--startup', 'invalid']).status, 0);
    assert.match(f.run(['--startup', 'user', '--no-pair']).stderr, /requires pairing/);
  } finally {
    f.close();
  }
});
