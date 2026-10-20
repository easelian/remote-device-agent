export interface MoveParameters {
  root: string;
  path: string;
  destination_root: string;
  destination_path: string;
}

// Normalize both supported move formats once, before policy checks or I/O.
export function moveParameters(params: unknown): MoveParameters {
  if (!params || typeof params !== 'object' || Array.isArray(params)) throw new Error('Invalid move parameters');
  const values = params as Record<string, unknown>;
  const modern = Object.hasOwn(params, 'source') || Object.hasOwn(params, 'destination');
  if (modern && ['path', 'destination_root', 'destination_path'].some(key => Object.hasOwn(params, key))) {
    throw new Error('Mixed move parameter formats are not allowed');
  }
  const normalized = modern
    ? { root: values.root, path: values.source, destination_root: values.root, destination_path: values.destination }
    : { root: values.root, path: values.path, destination_root: values.destination_root, destination_path: values.destination_path };
  if (Object.values(normalized).some(value => typeof value !== 'string')) throw new Error('Invalid move parameters');
  return normalized as MoveParameters;
}
