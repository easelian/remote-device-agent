import { PUBLIC_URL, MCP_PUBLIC_URL } from './deploymentDefaults.js';

export const PUBLIC_SERVER = PUBLIC_URL;
export const MCP_SERVER = MCP_PUBLIC_URL;

// Only the exact origins shipped together in this deployment share credentials.
// User-supplied servers retain same-origin isolation. DNS similarity is not trust.
export function sameHub(left: string, right: string): boolean {
  return (
    left === right ||
    ([PUBLIC_SERVER, MCP_SERVER].includes(left) && [PUBLIC_SERVER, MCP_SERVER].includes(right))
  );
}

export function profileServers(server: string): [string, ...string[]] {
  return sameHub(server, PUBLIC_SERVER)
    ? [PUBLIC_SERVER, ...[MCP_SERVER].filter((value) => value !== PUBLIC_SERVER)]
    : [server];
}
