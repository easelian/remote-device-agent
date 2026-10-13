import { readFileSync } from 'node:fs';
import { validateGateway } from './config.mjs';
import { DIRECT_SERVER, FALLBACK_SERVER, selectServer } from './server-selection.mjs';

const metadata = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
const defaultServer = DIRECT_SERVER;

export function parseArgs(argv) {
  const result = { server: defaultServer, profile: 'default', noBrowser: false, recoverLock: false };
  const values = new Set(['server', 'root', 'name', 'profile', 'config']);
  for (let index = 0; index < argv.length; index++) {
    const item = argv[index];
    if (item === '--') continue;
    if (item === '--no-browser') { result.noBrowser = true; continue; }
    if (item === '--recover-lock') { result.recoverLock = true; continue; }
    const match = /^--([a-z-]+)(?:=(.*))?$/.exec(item);
    if (!match || !values.has(match[1])) throw new Error(`未知参数：${item}`);
    const value = match[2] ?? argv[++index];
    if (!value || value.startsWith('--')) throw new Error(`参数 --${match[1]} 需要一个值`);
    if (match[1] === 'root') (result.root ||= []).push(value);
    else result[match[1]] = value;
  }
  result.server = validateGateway(result.server);
  return result;
}

export function serverFromArgs(argv) { return parseArgs(argv).server; }

export async function launch(argv) {
  if (Number(process.versions.node.split('.')[0]) < 22) throw new Error('Node Agent 需要 Node.js 22 或更新版本');
  if (argv.includes('--version')) { console.log(metadata.version); return; }
  if (argv.includes('--help') || argv.includes('-h')) {
    console.log(`Remote Device Hub Agent ${metadata.version}\n\n直接运行 Node Agent，在浏览器中确认绑定此设备。\n\n--server URL       固定中心地址；省略时先探测 ${defaultServer}，500 ms 内失败切换 ${FALLBACK_SERVER}\n--root NAME=PATH   文件目录，可重复；默认当前目录\n--name NAME        设备显示名称\n--profile NAME     独立的设备配置，默认 default\n--config PATH      自定义私有配置文件位置\n--no-browser       仅显示链接，不自动打开浏览器\n--recover-lock     显式恢复已退出进程留下的配置锁\n\n再次执行相同命令会复用绑定；按 Ctrl+C 停止 Agent。`);
    return;
  }
  const args = parseArgs(argv);
  const stop = new AbortController();
  let shutdownTimer;
  let runningAgent;
  const setShutdownDeadline = (exitCode) => {
    if (shutdownTimer) return;
    shutdownTimer = setTimeout(() => {
      console.error('Agent shutdown deadline exceeded; an operation outcome may be unknown. Inspect before retrying.');
      process.exit(exitCode);
    }, 10_000);
    shutdownTimer.unref();
  };
  const interrupt = (signal) => {
    if (stop.signal.aborted) return;
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
    if (!argv.some((item) => item === '--server' || item.startsWith('--server='))) {
      args.autoServer = true;
      args.server = await selectServer({ signal: stop.signal });
      console.log(`设备中心：${args.server}${args.server === FALLBACK_SERVER ? '（直连入口 500 ms 内未通过健康检查，使用备用入口）' : '（直连入口）'}`);
    }
    // The bundled ws implementation must use its JavaScript paths even when
    // an unrelated package has installed optional native accelerators nearby.
    process.env.WS_NO_BUFFER_UTIL = '1';
    process.env.WS_NO_UTF_8_VALIDATE = '1';
    const { connectDevice } = await import('./connect.mjs');
    const { Agent } = await import('./agent.mjs');
    await connectDevice(args, {
      runAgent: (config, signal) => { runningAgent = new Agent(config); return runningAgent.run(signal); },
      canReleaseLocks: () => !runningAgent || runningAgent.cleanShutdown,
    });
  } catch (error) {
    if (runningAgent && !runningAgent.cleanShutdown) setShutdownDeadline(1);
    if (!stop.signal.aborted) throw error;
  } finally {
    if (!stop.signal.aborted && (!runningAgent || runningAgent.cleanShutdown)) clearTimeout(shutdownTimer);
    process.removeListener('SIGINT', interrupt);
    process.removeListener('SIGTERM', interrupt);
  }
}
