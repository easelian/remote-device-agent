/** Validated device configuration; the wire schema stays compatible with Hub v1. */
export interface DeviceConfig {
  gateway_url: string;
  device_id: string;
  token: string;
  roots: Record<string, string>;
  allow_shell: boolean;
  allow_write: boolean;
  shell: string;
  max_processes: number;
  max_process_seconds: number;
}

export interface LaunchArgs {
  server: string;
  profile: string;
  noBrowser: boolean;
  recoverLock: boolean;
  root?: string[];
  config?: string;
  name?: string;
  signal?: AbortSignal;
  autoServer?: boolean;
}

export type ErrorCode = Error & { code?: string };

export interface SandboxSettings {
  mode: 'none' | 'bwrap';
  limits: Record<string, number>;
  readonlyRoots?: string[];
  network?: boolean;
  hideHome?: boolean;
  home?: string;
  expose?: string[];
  hiddenPaths?: string[];
  hiddenFiles?: string[];
}
