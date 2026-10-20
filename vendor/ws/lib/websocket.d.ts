import { EventEmitter } from 'node:events';
/** Client-only surface consumed by the Agent; source provenance is vendor.json. */
declare class WebSocket extends EventEmitter {
  constructor(address: string | URL, options?: Record<string, unknown>);
  readyState: number;
  bufferedAmount: number;
  send(data: string, callback?: (error?: Error) => void): void;
  close(code?: number, reason?: string): void;
  terminate(): void;
  ping(): void;
}
export = WebSocket;
