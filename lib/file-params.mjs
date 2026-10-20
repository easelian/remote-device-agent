// Normalize both supported move formats once, before policy checks or I/O.
export function moveParameters(params) {
  if (!params || typeof params !== 'object' || Array.isArray(params)) throw new Error('Invalid move parameters');
  const modern = Object.hasOwn(params, 'source') || Object.hasOwn(params, 'destination');
  if (modern && ['path', 'destination_root', 'destination_path'].some(key => Object.hasOwn(params, key))) {
    throw new Error('Mixed move parameter formats are not allowed');
  }
  const normalized = modern
    ? { root: params.root, path: params.source, destination_root: params.root, destination_path: params.destination }
    : { root: params.root, path: params.path, destination_root: params.destination_root, destination_path: params.destination_path };
  if (Object.values(normalized).some(value => typeof value !== 'string')) throw new Error('Invalid move parameters');
  return normalized;
}
