#!/usr/bin/env python3
"""Inspect a trusted old Agent archive in an offline user-systemd fixture.

This is a development check, not a migration or production acceptance test.
"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
import tarfile
import tempfile
import time

def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class FixtureDirectory:
    def __init__(self):
        self.path = Path(tempfile.mkdtemp(prefix='rdh-legacy-inspect-live-'))
        self.retain = False

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if not self.retain:
            shutil.rmtree(self.path)


async def run(*args, env, timeout=30):
    child = await asyncio.create_subprocess_exec(*map(str, args), env=env,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        out, err = await asyncio.wait_for(child.communicate(), timeout)
    except BaseException:
        if child.returncode is None:
            child.kill()
            await child.wait()
        raise
    return child.returncode, out.decode(errors='replace'), err.decode(errors='replace')


async def main(agent, archive, version):
    runtime = os.environ.get('XDG_RUNTIME_DIR') or f'/run/user/{os.getuid()}'
    manager_env = dict(os.environ, XDG_RUNTIME_DIR=runtime)
    manager_env.setdefault('DBUS_SESSION_BUS_ADDRESS', f'unix:path={runtime}/bus')
    unit = f'rdh-legacy-inspect-{secrets.token_hex(12)}.service'
    description = f'RDH legacy inspect fixture {unit}'
    inactive_unit = f'rdh-legacy-inactive-{secrets.token_hex(12)}.service'
    inactive_description = f'RDH inactive inspect fixture {inactive_unit}'
    inactive_attempted = False
    inactive_identity = None
    runner = None
    async def systemctl(*args):
        return await run('/usr/bin/systemctl', '--user', '--no-ask-password', *args,
                         env=manager_env, timeout=15)
    with FixtureDirectory() as fixture:
        root = fixture.path
        prefix = root / 'legacy'
        versions = prefix / 'versions'
        versions.mkdir(parents=True, mode=0o700)
        prefix.chmod(0o700)
        with tarfile.open(archive) as source:
            members = source.getmembers()
            require(0 < len(members) <= 4096, 'invalid fixture archive member count')
            roots = {Path(member.name).parts[0] for member in members if Path(member.name).parts}
            require(len(roots) == 1, 'fixture archive must have one top directory')
            top = next(iter(roots))
            require(top.startswith(f'rdh-{version}-linux-') and
                    re.fullmatch(r'[a-zA-Z0-9.-]+', top), 'unexpected fixture archive root')
            require(sum(member.size for member in members) <= 384 * 1024 * 1024,
                    'fixture archive is too large')
            for member in members:
                name = Path(member.name)
                require(not name.is_absolute() and '..' not in name.parts and
                        (member.isdir() or member.isfile()), 'unsafe fixture archive member')
            source.extractall(versions, filter='data')
        (versions / top).rename(versions / version)
        # Python's data filter creates directories with the current umask rather
        # than applying the archive's 0755 modes, unlike the legacy installer.
        for directory in (versions / version, versions / version / 'bin', versions / version / 'bundle'):
            directory.chmod(0o755)
        (prefix / 'current').symlink_to(f'versions/{version}')
        work, home, empty = root / 'work', root / 'home', root / 'empty'
        for directory in (work, home, empty):
            directory.mkdir()
        token = 'fixture-secret-never-print-' + secrets.token_hex(16)
        proxy_secret = 'fixture-proxy-never-print-' + secrets.token_hex(16)
        def redact(value):
            return value.replace(token, '[redacted]').replace(proxy_secret, '[redacted]')
        reserved = socket.socket()
        reserved.bind(('127.0.0.1', 0))
        origin = f'http://127.0.0.1:{reserved.getsockname()[1]}'
        config = root / 'device.json'
        config.write_text(json.dumps({'gateway_url':origin, 'device_id':'fixture',
                                      'token':token, 'roots':{'work':str(work)}}))
        config.chmod(0o600)
        before = config.read_bytes()
        node = versions / version / 'node'
        status, node_version, _ = await run(node, '--version', env={'PATH':'/usr/bin:/bin', 'HOME':str(home)})
        require(status == 0 and node_version.strip().startswith('v'), 'fixture Node cannot run')
        args = ['/usr/bin/systemd-run', '--user', '--unit', unit, '--wait', '--collect', '--quiet',
                '--property=Type=exec', '--property=Restart=no', f'--description={description}',
                f'--working-directory={root}', '--setenv=PATH=/usr/bin:/bin', f'--setenv=HOME={home}',
                f'--setenv=XDG_CONFIG_HOME={root}/config', f'--setenv=XDG_STATE_HOME={root}/state',
                '--setenv=NODE_OPTIONS=--no-warnings', '--setenv=NODE_USE_ENV_PROXY=1',
                f'--setenv=HTTP_PROXY=http://fixture:{proxy_secret}@127.0.0.1:1',
                str(prefix / 'current/rdh'), '--server', origin, '--config', str(config),
                '--no-browser']
        client_env = {'PATH':str(empty), 'HOME':str(home), 'XDG_RUNTIME_DIR':runtime,
                      'DBUS_SESSION_BUS_ADDRESS':manager_env['DBUS_SESSION_BUS_ADDRESS']}
        unit_file = Path(runtime) / 'systemd/user' / inactive_unit
        try:
            runner = await asyncio.create_subprocess_exec(*args, env=manager_env,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            deadline = time.monotonic() + 20
            pid = 0
            while time.monotonic() < deadline:
                rc, out, _ = await systemctl('show', unit, '--property=MainPID', '--value')
                if rc == 0 and out.strip().isdigit():
                    pid = int(out.strip())
                if pid and (root / 'device.json.node-lock/owner.json').is_file():
                    break
                if runner.returncode is not None:
                    raise RuntimeError('test service exited before its lock appeared')
                await asyncio.sleep(.05)
            if not pid or not (root / 'device.json.node-lock/owner.json').is_file():
                raise RuntimeError('test service did not start and acquire its lock')
            rc, desc, _ = await systemctl('show', unit, '--property=Description', '--value')
            require(rc == 0 and desc.strip() == description, 'fixture service identity changed')
            # A disabled, persistent runtime unit can be bound by the old
            # Agent. Its random name and inode are retained for safe cleanup.
            unit_contents = (
                f'[Unit]\nDescription={inactive_description}\n[Service]\nType=exec\nRestart=no\n'
                f'WorkingDirectory={root}\nEnvironment="PATH=/usr/bin:/bin"\n'
                f'Environment="HOME={home}"\n'
                f'ExecStart="{prefix}/current/rdh" --server {origin} --config "{config}" --no-browser\n'
                '[Install]\nWantedBy=default.target\n'
            )
            fd = os.open(unit_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            metadata = os.fstat(fd)
            inactive_identity = (metadata.st_dev, metadata.st_ino)
            inactive_attempted = True
            with os.fdopen(fd, 'w') as target:
                target.write(unit_contents)
            rc, _, _ = await systemctl('daemon-reload')
            require(rc == 0, 'could not reload the inactive fixture unit')
            rc, state, _ = await systemctl('is-enabled', inactive_unit)
            require(rc == 1 and state.strip() == 'disabled', 'fixture unit is not disabled')
            # Ask the old Agent to create its own startup sidecar. Binding is
            # local metadata; it does not enable, stop, or restart the unit.
            bind_env = dict(manager_env, PATH='/usr/bin:/bin', HOME=str(home),
                            XDG_CONFIG_HOME=str(root / 'config'), XDG_STATE_HOME=str(root / 'state'))
            rc, out, err = await run(prefix / 'current/rdh', 'startup', 'bind', 'user', inactive_unit,
                                     '--server', origin, '--config', config, env=bind_env)
            require(rc == 0, 'legacy startup binding failed: ' + redact(err))
            require(json.loads(out)['registration'] == {'mode':'user', 'name':inactive_unit},
                    'legacy startup binding did not name the fixture unit')
            startup_sidecar = Path(f'{config}.startup.json')
            startup_before = startup_sidecar.read_bytes()
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                     '--config', config, '--service-unit', unit, env=client_env)
            if rc != 0:
                raise RuntimeError('inspection failed: ' + redact(err))
            require(all(value not in out and value not in err for value in (token, proxy_secret)),
                    'inspector exposed a fixture secret')
            result = json.loads(out)
            service = result['service']
            require(service['pid'] == pid and service['exec_kind'] == 'current-launcher' and
                    service['transient'] is True and service['state'] == 'active' and
                    service['scope'] == 'running-process', 'wrong service observation')
            require(service['explicit_config_matches'] is True and
                    service['explicit_server_matches'] is True and
                    service['lock_owner_matches'] is True and service['lock_present'] is True and
                    service['recovery_guard_present'] is False, 'service identity did not match')
            startup = result['startup']
            require(startup['state'] == 'readable' and
                    startup['binding'] == {'mode':'user', 'name':inactive_unit} and
                    startup['binding_matches_user_unit'] is False and
                    startup['pending_mode'] is None and
                    startup['state_lock_present'] is False and
                    startup['recovery_guard_present'] is False,
                    'legacy startup binding was not observed')
            environment_keys = service['initial_network_environment_keys']
            require(environment_keys is not None and
                    {'HTTP_PROXY', 'NODE_OPTIONS', 'NODE_USE_ENV_PROXY'}.issubset(environment_keys),
                    'legacy network environment keys were not reported')
            require(result['hub_ready'] == result['active_work'] == 'unverified' and
                    result['migration_authorized'] is False and
                    result['offline_migration'] == 'requires_explicit_stopped_service_preflight',
                    'inspection claimed unproved state')
            require(result['version'] == version and
                    result['sha256']['node'] == hashlib.sha256(node.read_bytes()).hexdigest(),
                    'wrong version or Node digest')
            require(config.read_bytes() == before and startup_sidecar.read_bytes() == startup_before and
                    os.readlink(prefix / 'current') == f'versions/{version}',
                    'inspection changed the configuration, startup sidecar or version pointer')
            rc, after_pid, _ = await systemctl('show', unit, '--property=MainPID', '--value')
            require(rc == 0 and after_pid.strip().isdigit() and int(after_pid.strip()) == pid,
                    'inspection changed the service process')

            other = root / 'other.json'
            other_config = json.loads(before)
            other_config['gateway_url'] = 'http://127.0.0.1:1'
            other.write_text(json.dumps(other_config))
            other.chmod(0o600)
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                     '--config', other, '--service-unit', unit, env=client_env)
            require(rc == 0, 'different-config inspection failed: ' + redact(err))
            other_service = json.loads(out)['service']
            require(other_service['explicit_config_matches'] is False and
                    other_service['explicit_server_matches'] is False and
                    other_service['lock_owner_matches'] is None, 'different config was not reported')
            require(all(value not in out and value not in err for value in (token, proxy_secret)),
                    'inspector exposed a fixture secret')

            other_prefix = root / 'different-install'
            shutil.copytree(prefix, other_prefix, symlinks=True)
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', other_prefix,
                                     '--config', config, '--service-unit', unit, env=client_env)
            require(rc != 0, 'a different executable was accepted as the live service')
            require(all(value not in out and value not in err for value in (token, proxy_secret)),
                    'inspector exposed a fixture secret')

            # A separate loaded but never-started unit points to the same
            # configuration while the first process is still alive. It must
            # report the lock rather than implying this profile is idle.
            rc, state, _ = await systemctl('show', inactive_unit, '--property=ActiveState', '--value')
            require(rc == 0 and state.strip() == 'inactive', 'fixture unit is not inactive')

            async def inspect_inactive(selected=config):
                rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                         '--config', selected, '--service-unit', inactive_unit, env=client_env)
                if rc != 0:
                    _, diagnostic, _ = await systemctl(
                        'show', inactive_unit, '--property=Id', '--property=LoadState',
                        '--property=ActiveState', '--property=SubState', '--property=NeedDaemonReload',
                        '--property=Job', '--property=MainPID', '--property=ControlGroup',
                        '--property=ExecStart', '--no-pager')
                    _, lookup, lookup_error = await run(
                        '/usr/bin/busctl', '--user', '--json=short', 'call', 'org.freedesktop.systemd1',
                        '/org/freedesktop/systemd1', 'org.freedesktop.systemd1.Manager',
                        'GetUnit', 's', inactive_unit, env=manager_env, timeout=15)
                    raise RuntimeError('inactive inspection failed: ' + redact(err + diagnostic + lookup + lookup_error))
                require(all(value not in out and value not in err for value in (token, proxy_secret)),
                        'inactive inspection exposed a fixture secret')
                observed = json.loads(out)
                service = observed['service']
                if selected == config:
                    require(observed['startup']['binding_matches_user_unit'] is True,
                            'matching startup binding was not reported')
                else:
                    require(observed['startup']['state'] == 'missing' and
                            observed['startup']['binding_matches_user_unit'] is None,
                            'missing startup binding was not reported')
                require(service['state'] == 'inactive' and service['scope'] == 'unit-definition' and
                        service['pid'] is None and service['invocation_id'] is None and
                        service['lock_owner_matches'] is None and
                        service['initial_network_environment_keys'] is None,
                        'inactive service was described as a running instance')
                require(service['exec_kind'] == 'current-launcher' and
                        service['explicit_config_matches'] is (selected == config) and
                        service['explicit_server_matches'] is (selected == config) and
                        observed['migration_authorized'] is False and observed['active_work'] == 'unverified',
                        'inactive service definition did not match')
                return service

            inactive_with_live_lock = await inspect_inactive()
            require(inactive_with_live_lock['lock_present'] is True,
                    'inactive unit failed to report another Agent\'s lock')
            require((await inspect_inactive(other))['lock_present'] is False,
                    'inactive unit reported a lock for the wrong configuration')
            stopped, _, _ = await systemctl('stop', unit)
            require(stopped == 0, 'could not stop the transient fixture')
            await asyncio.wait_for(runner.communicate(), 15)
            require(runner.returncode == 0, 'transient fixture did not exit cleanly')
            runner = None
            rc, state, _ = await systemctl('show', unit, '--property=LoadState', '--value')
            require(rc == 0 and state.strip() == 'not-found', 'transient fixture remains loaded')
            inactive_after_stop = await inspect_inactive()
            require(inactive_after_stop['lock_present'] is False and
                    inactive_after_stop['recovery_guard_present'] is False,
                    'inactive fixture retained a lock or recovery guard')
            guard = Path(f'{config}.node-lock.reclaiming')
            guard.mkdir(mode=0o700)
            require((await inspect_inactive())['recovery_guard_present'] is True,
                    'inactive inspection did not report the recovery guard')
            guard.rmdir()
            wrapper = prefix / 'run-agent'
            wrapper.write_text(f'#!/bin/sh\nexec "{prefix}/current/rdh" --server {origin} --config "{config}"\n')
            wrapper.chmod(0o700)
            lines = unit_file.read_text().splitlines()
            lines = [f'ExecStart="{wrapper}"' if line.startswith('ExecStart=') else line for line in lines]
            unit_file.write_text('\n'.join(lines) + '\n')
            reloaded, _, _ = await systemctl('daemon-reload')
            require(reloaded == 0, 'could not reload wrapper fixture')
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                     '--config', config, '--service-unit', inactive_unit, env=client_env)
            require(rc == 0, 'wrapper inspection failed: ' + redact(err))
            wrapper_service = json.loads(out)['service']
            require(wrapper_service['scope'] == 'unit-definition' and
                    wrapper_service['exec_kind'] == 'installer-wrapper' and
                    wrapper_service['argument_source'] == 'unverified' and
                    wrapper_service['explicit_config_matches'] is None and
                    wrapper_service['explicit_server_matches'] is None,
                    'wrapper contents were treated as verified startup arguments')

            # Match the old installer's exact quoted shell format. Inspection
            # parses this file as data; it never executes it.
            def quote(value):
                return "'" + str(value).replace("'", "'\\''") + "'"
            args = [prefix / 'current/rdh', '--no-browser', '--config', config, '--server', origin]
            secret_state = root / f'state-{proxy_secret}'
            script = (f'#!/bin/sh\nexport XDG_CONFIG_HOME={quote(root / "config")}\n'
                      f'export XDG_STATE_HOME={quote(secret_state)}\ncd {quote(home)} || exit 1\n'
                      f'exec {" ".join(quote(arg) for arg in args)} --recover-lock\n')
            wrapper.write_text(script)
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                     '--config', config, '--service-unit', inactive_unit, env=client_env)
            require(rc == 0, 'canonical wrapper inspection failed: ' + redact(err))
            require(all(value not in out and value not in err for value in (token, proxy_secret)),
                    'canonical wrapper inspection exposed a fixture secret')
            static_service = json.loads(out)['service']
            require(static_service['argument_source'] == 'recognized-wrapper-format' and
                    static_service['explicit_config_matches'] is True and
                    static_service['explicit_server_matches'] is True,
                    'canonical wrapper arguments did not match')
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                     '--config', other, '--service-unit', inactive_unit, env=client_env)
            require(rc == 0, 'wrapper mismatch inspection failed: ' + redact(err))
            mismatch = json.loads(out)['service']
            require(mismatch['argument_source'] == 'recognized-wrapper-format' and
                    mismatch['explicit_config_matches'] is False and
                    mismatch['explicit_server_matches'] is False,
                    'canonical wrapper mismatch was not reported')

            sentinel = root / 'must-not-be-created'
            wrapper.write_text(script.replace(' --recover-lock\n', f' --recover-lock; touch {sentinel}\n'))
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                     '--config', config, '--service-unit', inactive_unit, env=client_env)
            require(rc == 0, 'unrecognized wrapper inspection failed: ' + redact(err))
            injected = json.loads(out)['service']
            require(injected['argument_source'] == 'unverified' and
                    injected['explicit_config_matches'] is None and not sentinel.exists(),
                    'inspection accepted or executed an additional shell command')

            wrapper.write_text(script)
            rc, _, _ = await systemctl('start', inactive_unit)
            require(rc == 0, 'could not start the canonical wrapper fixture')
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline and not Path(f'{config}.node-lock/owner.json').is_file():
                await asyncio.sleep(.05)
            require(Path(f'{config}.node-lock/owner.json').is_file(), 'wrapper fixture did not acquire its lock')
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                     '--config', config, '--service-unit', inactive_unit, env=client_env)
            require(rc == 0, 'running wrapper inspection failed: ' + redact(err))
            running_wrapper = json.loads(out)['service']
            require(running_wrapper['scope'] == 'running-process' and
                    running_wrapper['exec_kind'] == 'installer-wrapper' and
                    running_wrapper['argument_source'] == 'running-process' and
                    running_wrapper['explicit_config_matches'] is True and
                    running_wrapper['explicit_server_matches'] is True,
                    'running wrapper process arguments did not match its definition')
            stopped, _, _ = await systemctl('stop', inactive_unit)
            require(stopped == 0 and not Path(f'{config}.node-lock').exists(),
                    'canonical wrapper fixture did not stop cleanly')

            # The old startup manager also generates a per-profile wrapper
            # that invokes Node and the JS entry directly.
            profile_wrapper = Path(f'{config}.startup-run')
            profile_args = [node, versions / version / 'bin/remote-device-hub.mjs',
                            '--config', config, '--server', origin, '--no-browser', '--recover-lock']
            profile_wrapper.write_text(f'#!/bin/sh\ncd {quote(config.parent)} || exit 1\n'
                                       f'exec {" ".join(quote(arg) for arg in profile_args)}\n')
            profile_wrapper.chmod(0o700)
            lines = unit_file.read_text().splitlines()
            lines = [f'ExecStart="{profile_wrapper}"' if line.startswith('ExecStart=') else line for line in lines]
            unit_file.write_text('\n'.join(lines) + '\n')
            reloaded, _, _ = await systemctl('daemon-reload')
            require(reloaded == 0, 'could not reload profile wrapper fixture')
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                     '--config', config, '--service-unit', inactive_unit, env=client_env)
            require(rc == 0, 'profile wrapper inspection failed: ' + redact(err))
            profile_service = json.loads(out)['service']
            require(profile_service['exec_kind'] == 'profile-wrapper' and
                    profile_service['argument_source'] == 'recognized-wrapper-format' and
                    profile_service['explicit_config_matches'] is True and
                    profile_service['explicit_server_matches'] is True,
                    'canonical profile wrapper arguments did not match')
            profile_wrapper.unlink()
            profile_wrapper.symlink_to(wrapper)
            rc, out, err = await run(agent, 'migrate', '--inspect', '--prefix', prefix,
                                     '--config', config, '--service-unit', inactive_unit, env=client_env)
            require(rc == 0, 'symlink wrapper inspection failed: ' + redact(err))
            require(json.loads(out)['service']['argument_source'] == 'unverified',
                    'a symbolic link was accepted as a profile wrapper')
            require(config.read_bytes() == before and startup_sidecar.read_bytes() == startup_before and
                    os.readlink(prefix / 'current') == f'versions/{version}',
                    'wrapper inspection changed preserved state')
            return {'source_archive_sha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
                    'inspector_sha256': hashlib.sha256(agent.read_bytes()).hexdigest(),
                    'fixture_node_version': node_version.strip(),
                    'node_sha256': result['sha256']['node'],
                    'version': result['version'], 'device_id': result['device_id'],
                    'unit_kind': service['exec_kind'], 'main_pid_match': True,
                    'config_match': True, 'gateway_match': True, 'lock_owner_match': True,
                    'legacy_network_keys_present_without_values': True,
                    'legacy_created_startup_binding_observed': True,
                    'startup_binding_mismatch_reported': True,
                    'different_config_and_gateway_reported_false': True,
                    'different_executable_rejected': True, 'config_and_pointer_unchanged': True,
                    'inactive_definition_checked': True,
                    'inactive_definition_mismatch_detected': True,
                    'inactive_unit_reports_another_live_lock': True,
                    'inactive_unit_reports_recovery_guard': True,
                    'inactive_wrapper_arguments_remain_unverified': True,
                    'canonical_installer_wrapper_arguments_observed': True,
                    'unrecognized_wrapper_not_executed': True,
                    'wrapper_process_arguments_match': True,
                    'canonical_profile_wrapper_arguments_observed': True,
                    'symlink_profile_wrapper_unverified': True,
                    'hub_ready': 'unverified', 'active_work': 'unverified'}
        finally:
            try:
                if runner is not None:
                    rc, desc, _ = await systemctl('show', unit, '--property=Description', '--value')
                    if rc == 0 and desc.strip() == description:
                        stopped, _, _ = await systemctl('stop', unit)
                        require(stopped == 0, f'could not stop fixture unit {unit}')
                    elif rc == 0 and desc.strip():
                        raise RuntimeError('fixture unit identity changed')
                    try:
                        await asyncio.wait_for(runner.communicate(), 15)
                    except asyncio.TimeoutError:
                        runner.kill()
                        await runner.wait()
                        raise RuntimeError('fixture systemd-run did not exit')
                    await systemctl('reset-failed', unit)
                    rc, state, _ = await systemctl('show', unit, '--property=LoadState', '--value')
                    require(rc == 0 and state.strip() == 'not-found', 'fixture unit remains loaded')
                if inactive_attempted:
                    current = unit_file.lstat()
                    if (not unit_file.is_symlink() and current.st_uid == os.getuid() and
                            (current.st_dev, current.st_ino) == inactive_identity):
                        rc, desc, _ = await systemctl('show', inactive_unit, '--property=Description', '--value')
                        require(rc == 0 and desc.strip() == inactive_description,
                                'inactive fixture unit identity changed')
                        rc, active, _ = await systemctl('show', inactive_unit, '--property=ActiveState', '--value')
                        pid_rc, main_pid, _ = await systemctl('show', inactive_unit, '--property=MainPID', '--value')
                        group_rc, group, _ = await systemctl('show', inactive_unit, '--property=ControlGroup', '--value')
                        require(rc == pid_rc == group_rc == 0, 'cannot inspect inactive fixture state')
                        if not (active.strip() in ('inactive', 'failed') and main_pid.strip() == '0' and not group.strip()):
                            stopped, _, _ = await systemctl('stop', inactive_unit)
                            require(stopped == 0, 'could not stop inactive fixture unit')
                        unit_file.unlink()
                        reloaded, _, _ = await systemctl('daemon-reload')
                        require(reloaded == 0, 'could not reload removed fixture unit')
                    else:
                        raise RuntimeError('inactive fixture unit file identity changed')
                    await systemctl('reset-failed', inactive_unit)
                    rc, state, _ = await systemctl('show', inactive_unit, '--property=LoadState', '--value')
                    require(rc == 0 and state.strip() == 'not-found', 'inactive fixture remains loaded')
            except BaseException as error:
                fixture.retain = True
                raise RuntimeError(f'fixture cleanup is uncertain; inspect {unit}, {inactive_unit} and {root} manually') from error
            finally:
                reserved.close()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--agent', type=Path, required=True, help='native inspector executable')
    parser.add_argument('--archive', type=Path, required=True, help='trusted legacy Linux Agent archive')
    parser.add_argument('--version', required=True, help='expected legacy Agent version')
    args = parser.parse_args()
    require(re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?', args.version),
            'invalid expected version')
    agent, archive = args.agent.resolve(strict=True), args.archive.resolve(strict=True)
    require(agent.is_file() and os.access(agent, os.X_OK) and archive.is_file(),
            'inspector and archive must be regular files')
    print(json.dumps(asyncio.run(main(agent, archive, args.version)), indent=2))
