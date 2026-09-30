import { readFileSync } from 'node:fs';
import { validateGateway } from './config.mjs';

const metadata = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
const defaultServer = 'https://xdistribute.site:8090';

export function parseArgs(argv) {
  const result = { server: defaultServer, profile: 'default', noBrowser: false };
  const values = new Set(['server', 'root', 'name', 'profile', 'config']);
  for (let index = 0; index < argv.length; index++) {
    const item = argv[index];
    if (item === '--') continue;
    if (item === '--no-browser') { result.noBrowser = true; continue; }
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
    console.log(`Remote Device Hub Agent ${metadata.version}\n\n直接运行 Node Agent，在浏览器中确认绑定此设备。\n\n--server URL       中心地址（默认 ${defaultServer}）\n--root NAME=PATH   文件目录，可重复；默认当前目录\n--name NAME        设备显示名称\n--profile NAME     独立的设备配置，默认 default\n--config PATH      自定义私有配置文件位置\n--no-browser       仅显示链接，不自动打开浏览器\n\n再次执行相同命令会复用绑定；按 Ctrl+C 停止 Agent。`);
    return;
  }
  const args = parseArgs(argv);
  const stop = new AbortController();
  const interrupt = () => stop.abort(new Error('Agent stopped'));
  // npm may forward the same signal that the terminal also delivered to our
  // process group. Keep handlers installed until jobs and profile locks close.
  process.on('SIGINT', interrupt);
  process.on('SIGTERM', interrupt);
  args.signal = stop.signal;
  try {
    // The bundled ws implementation must use its JavaScript paths even when
    // an unrelated package has installed optional native accelerators nearby.
    process.env.WS_NO_BUFFER_UTIL = '1';
    process.env.WS_NO_UTF_8_VALIDATE = '1';
    const { connectDevice } = await import('./connect.mjs');
    const { Agent } = await import('./agent.mjs');
    await connectDevice(args, { runAgent: (config, signal) => new Agent(config).run(signal) });
  } catch (error) {
    if (!stop.signal.aborted) throw error;
  } finally {
    process.removeListener('SIGINT', interrupt);
    process.removeListener('SIGTERM', interrupt);
  }
}
