// Server-Sent Events driven by PRAGMA data_version polling (PRD §3, the proven
// Ametrite pattern). One shared poller watches the ledger; each client stream
// is notified only when data_version changes, so idle fleets cost ~nothing.
import type { Ledger } from "./ledger.ts";

type Listener = (version: number) => void;

export class VersionPoller {
  private ledger: Ledger;
  private intervalMs: number;
  private listeners = new Set<Listener>();
  private timer: ReturnType<typeof setInterval> | null = null;
  private last = -1;

  constructor(ledger: Ledger, intervalMs = 750) {
    this.ledger = ledger;
    this.intervalMs = intervalMs;
  }

  get version(): number {
    // ensure a real read even before the interval starts (first SSE event)
    if (this.last < 0) this.last = this.ledger.dataVersion();
    return this.last;
  }

  private start(): void {
    if (this.timer) return;
    this.last = this.ledger.dataVersion();
    this.timer = setInterval(() => {
      const v = this.ledger.dataVersion();
      if (v !== this.last) {
        this.last = v;
        for (const l of this.listeners) l(v);
      }
    }, this.intervalMs);
    // don't keep the process alive purely for polling
    (this.timer as { unref?: () => void }).unref?.();
  }

  private stop(): void {
    if (this.timer && this.listeners.size === 0) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn);
    this.start();
    return () => {
      this.listeners.delete(fn);
      this.stop();
    };
  }
}

/**
 * Build a `text/event-stream` Response for one client. Emits an initial
 * `version` event immediately, then a `version` event on every change, plus a
 * `ping` heartbeat so proxies don't drop the connection.
 */
export function sseResponse(poller: VersionPoller): Response {
  let unsub: (() => void) | null = null;
  let heartbeat: ReturnType<typeof setInterval> | null = null;

  const stream = new ReadableStream({
    start(controller) {
      const enc = new TextEncoder();
      const send = (event: string, data: string) => {
        try {
          controller.enqueue(enc.encode(`event: ${event}\ndata: ${data}\n\n`));
        } catch {
          /* client gone */
        }
      };
      send("version", String(poller.version));
      unsub = poller.subscribe((v) => send("version", String(v)));
      heartbeat = setInterval(() => send("ping", String(Date.now())), 20_000);
      (heartbeat as { unref?: () => void }).unref?.();
    },
    cancel() {
      unsub?.();
      if (heartbeat) clearInterval(heartbeat);
    },
  });

  return new Response(stream, {
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache, no-transform",
      connection: "keep-alive",
      "x-accel-buffering": "no",
    },
  });
}
