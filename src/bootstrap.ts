import { Startup } from './startup.js';
import type { LaunchArgs } from './types.js';
import type { Agent } from './agent.js';
import { isRecord } from './boundary.js';
import { readFileSync } from 'node:fs';
import { validateGateway } from './config.js';
import { PUBLIC_SERVER } from './server-selection.js';

const metadata: unknown = JSON.parse(
  readFileSync(new URL('../package.json', import.meta.url), 'utf8'),
);
if (!isRecord(metadata) || typeof metadata.version !== 'string') {
  throw new Error('Invalid Agent package metadata');
}
const version = metadata.version;
const defaultServer = PUBLIC_SERVER;

export function parseArgs(argv: string[], env = process.env): LaunchArgs {
  const result: LaunchArgs = {
    server: env.RDH_SERVER ?? defaultServer,
    profile: 'default',
    noBrowser: false,
    recoverLock: false,
  };
  const values = new Set(['server', 'root', 'name', 'profile', 'config']);
  for (let index = 0; index < argv.length; index++) {
    const item = argv[index];
    if (item === undefined) {
      break;
    }
    if (item === '--') {
      continue;
    }
    if (item === '--no-browser') {
      result.noBrowser = true;
      continue;
    }
    if (item === '--recover-lock') {
      result.recoverLock = true;
      continue;
    }
    if (item === '--pair-only') {
      result.pairOnly = true;
      continue;
    }
    const match = /^--([a-z-]+)(?:=(.*))?$/.exec(item);
    const key = match?.[1];
    if (key === undefined || !values.has(key)) {
      throw new Error(`Unknown argument: ${item}`);
    }
    const value = match?.[2] ?? argv[++index];
    if (!value || value.startsWith('--')) {
      throw new Error(`Option --${key} requires a value`);
    }
    if (key === 'root') {
      (result.root ||= []).push(value);
    } else {
      result[key as 'server' | 'profile' | 'name' | 'config'] = value;
    }
  }
  result.server = validateGateway(result.server);
  return result;
}

export function serverFromArgs(argv: string[]) {
  return parseArgs(argv).server;
}

export async function launch(argv: string[]) {
  if (Number(process.versions.node.split('.')[0]) < 22) {
    throw new Error('The Node Agent requires Node.js 22 or later');
  }
  if (argv.includes('--version')) {
    console.log(version);
    return;
  }
  if (argv.includes('--help') || argv.includes('-h')) {
    console.log(
      `Remote Device Hub Agent ${version}\n\nRun the Node Agent and approve device pairing in your browser.\n\n--server URL       Use a specific Hub (otherwise RDH_SERVER or ${defaultServer})\n--root NAME=PATH   Expose a file root; repeat as needed (default: current directory)\n--name NAME        Device display name\n--profile NAME     Separate device profile (default: default)\n--config PATH      Custom private configuration file\n--no-browser       Print the pairing URL without opening a browser\n--recover-lock     Recover a stale configuration lock after its process has exited\n--pair-only        Exit after pairing without starting the Agent (used by the installer)\n\nCommands:\n  rdh startup --help                                Manage future startup and local approvals\n  rdh upgrade [--check] [--channel stable|canary]   Upgrade to the version in the release index\n  rdh rollback                                      Restore the previous version\n\nRun the same command again to reuse the pairing. Press Ctrl+C to stop the Agent.`,
    );
    return;
  }
  const args = parseArgs(argv);
  const stop = new AbortController();
  let shutdownTimer: ReturnType<typeof setTimeout> | undefined;
  const state: { agent?: Agent } = {};
  const setShutdownDeadline = (exitCode: number) => {
    if (shutdownTimer) {
      return;
    }
    shutdownTimer = setTimeout(() => {
      console.error(
        'Agent shutdown deadline exceeded; an operation outcome may be unknown. Inspect before retrying.',
      );
      process.exit(exitCode);
    }, 10_000);
    shutdownTimer.unref();
  };
  const interrupt = (signal: NodeJS.Signals) => {
    if (stop.signal.aborted) {
      return;
    }
    stop.abort(new Error('Agent stopped'));
    // Kernel filesystem I/O cannot always be cancelled. Stop accepting work
    // immediately, then give jobs, searches and profile locks ten seconds to
    // close before forcing this foreground Agent to exit.
    setShutdownDeadline(signal === 'SIGTERM' ? 143 : 130);
  };
  // npm may forward the same signal that the terminal also delivered to our
  // process group. Keep handlers installed until jobs and profile locks close.
  process.on('SIGINT', interrupt);
  process.on('SIGTERM', interrupt);
  args.signal = stop.signal;
  try {
    // The bundled ws implementation must use its JavaScript paths even when
    // an unrelated package has installed optional native accelerators nearby.
    Object.assign(process.env, { WS_NO_BUFFER_UTIL: '1', WS_NO_UTF_8_VALIDATE: '1' });
    const { connectDevice } = await import('./connect.js');
    const { Agent } = await import('./agent.js');
    const { buildHardening } = await import('./hardening.js');
    await connectDevice(args, {
      runAgent: async (config, signal, { configFile }) => {
        // Throws (and the Agent does not start) when a configured sandbox or
        // required audit log cannot be provided.
        state.agent = new Agent(config, {
          ...buildHardening(config, configFile),
          startup: new Startup(configFile),
        });
        return state.agent.run(signal);
      },
      canReleaseLocks: () => !state.agent || state.agent.cleanShutdown,
    });
  } catch (error) {
    if (state.agent && !state.agent.cleanShutdown) {
      setShutdownDeadline(1);
    }
    if (!stop.signal.aborted) {
      throw error;
    }
  } finally {
    if (!stop.signal.aborted && (!state.agent || state.agent.cleanShutdown)) {
      clearTimeout(shutdownTimer);
    }
    process.removeListener('SIGINT', interrupt);
    process.removeListener('SIGTERM', interrupt);
  }
}
