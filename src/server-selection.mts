export interface ServerSelectionOptions {
  fetchImpl?: typeof globalThis.fetch;
  signal?: AbortSignal;
  timeoutMs?: number;
}

export const DIRECT_SERVER = 'https://xdistribute.site:8090';
export const FALLBACK_SERVER = 'https://mcp.xdistribute.site';

// These exact TLS origins are two operator-controlled entrances to one hub.
// Custom servers retain strict same-origin pairing and credential isolation.
export function sameHub(left: string, right: string): boolean {
  return left === right || ([DIRECT_SERVER, FALLBACK_SERVER].includes(left) &&
    [DIRECT_SERVER, FALLBACK_SERVER].includes(right));
}

export async function selectServer({ fetchImpl = globalThis.fetch, signal, timeoutMs = 500 }: ServerSelectionOptions = {}): Promise<string> {
  signal?.throwIfAborted();
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<string>((resolve) => {
    timer = setTimeout(() => { controller.abort(); resolve(FALLBACK_SERVER); }, timeoutMs);
  });
  const probe = (async () => {
    try {
      const response = await fetchImpl(`${DIRECT_SERVER}/healthz`, {
        redirect: 'error', signal: AbortSignal.any([controller.signal, ...(signal ? [signal] : [])]),
      });
      if (!response.ok) { await response.body?.cancel(); return FALLBACK_SERVER; }
      const body = await response.json();
      return body.status === 'ok' && body.service === 'remote-device-hub' ? DIRECT_SERVER : FALLBACK_SERVER;
    } catch { return FALLBACK_SERVER; }
  })();
  try {
    const result = await Promise.race([probe, deadline]);
    signal?.throwIfAborted();
    return result;
  } finally { clearTimeout(timer); controller.abort(); }
}
