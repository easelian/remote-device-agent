export interface SenderSocket {
  readyState: number;
  bufferedAmount: number;
  send(text: string, callback: (error?: Error) => void): void;
  terminate(): void;
}

export interface TransportLimits {
  maxFrameBytes: number;
  maxBufferedBytes: number;
  maxQueuedMessages: number;
  sendTimeoutMs: number;
}

export const TRANSPORT_LIMITS: Readonly<TransportLimits> = Object.freeze({
  maxFrameBytes: 2 * 1024 * 1024,
  maxBufferedBytes: 4 * 1024 * 1024,
  maxQueuedMessages: 16,
  sendTimeoutMs: 5000,
});

/** One bounded writer for results, handshake and heartbeats. */
export class BoundedSender {
  readonly socket: SenderSocket;
  readonly limits: TransportLimits;
  tail: Promise<void>;
  queuedBytes: number;
  queuedMessages: number;
  closed: boolean;
  cancelWrite: (() => void) | null;

  constructor(socket: SenderSocket, limits: Partial<TransportLimits> = {}) {
    this.socket = socket;
    this.limits = { ...TRANSPORT_LIMITS, ...limits };
    this.tail = Promise.resolve();
    this.queuedBytes = 0;
    this.queuedMessages = 0;
    this.closed = false;
    this.cancelWrite = null;
    for (const value of Object.values(this.limits)) {
      if (!Number.isSafeInteger(value) || value < 1) throw new Error('Invalid transport limits');
    }
  }

  send(value: Record<string, unknown>): Promise<void> {
    if (this.closed || this.socket.readyState !== 1) return Promise.reject(new Error('Connection closed'));
    let text = JSON.stringify(value);
    let bytes = Buffer.byteLength(text);
    if (bytes > this.limits.maxFrameBytes && value.type === 'result') {
      text = JSON.stringify({ type: 'result', id: value.id,
        error: 'Result exceeds the transport limit; request a smaller range or narrower directory' });
      bytes = Buffer.byteLength(text);
    }
    if (bytes > this.limits.maxFrameBytes || this.queuedMessages >= this.limits.maxQueuedMessages ||
        Math.max(this.queuedBytes, this.socket.bufferedAmount || 0) + bytes > this.limits.maxBufferedBytes) {
      this.close();
      this.socket.terminate();
      return Promise.reject(new Error('Connection send budget exceeded'));
    }
    this.queuedBytes += bytes;
    this.queuedMessages += 1;
    const sent = this.tail.then(() => this.write(text));
    const released = sent.finally(() => { this.queuedBytes -= bytes; this.queuedMessages -= 1; });
    this.tail = released.catch(() => undefined);
    return released;
  }

  write(text: string): Promise<void> {
    if (this.closed || this.socket.readyState !== 1) return Promise.reject(new Error('Connection closed'));
    return new Promise<void>((resolve, reject) => {
      let finished = false;
      let timer: ReturnType<typeof setTimeout> | undefined;
      const done = (error?: unknown) => {
        if (finished) return;
        finished = true;
        clearTimeout(timer);
        this.cancelWrite = null;
        if (error) reject(error); else resolve();
      };
      this.cancelWrite = () => done(new Error('Connection closed'));
      timer = setTimeout(() => {
        done(new Error('Connection send timed out'));
        this.close();
        this.socket.terminate();
      }, this.limits.sendTimeoutMs);
      try { this.socket.send(text, done); } catch (error) { done(error); }
    });
  }

  close() {
    this.closed = true;
    this.cancelWrite?.();
  }
}
