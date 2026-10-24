// A fixed pool of render threads with a FIFO of jobs.
import { Worker } from "node:worker_threads";
import type { RenderJob } from "./render-worker.ts";

type Pending = { job: Omit<RenderJob, "job">; resolve: (b: Uint8Array) => void; reject: (e: Error) => void };

export class RenderPool {
  private idle: Worker[] = [];
  private queue: Pending[] = [];
  private inflight = new Map<number, Pending>();
  private byWorker = new Map<Worker, number>();
  private all: Worker[] = [];
  private next = 1;

  constructor(threads: number) {
    for (let i = 0; i < threads; i++) {
      const w = new Worker(new URL("./render-worker.ts", import.meta.url));
      w.on("message", (m: { job: number; data: Uint8Array }) => {
        const p = this.inflight.get(m.job);
        this.inflight.delete(m.job);
        this.byWorker.delete(w);
        this.idle.push(w);
        p?.resolve(m.data);
        this.pump();
      });
      w.on("error", (e) => {
        const id = this.byWorker.get(w);
        if (id !== undefined) this.inflight.get(id)?.reject(e);
      });
      w.unref();
      this.idle.push(w);
      this.all.push(w);
    }
  }

  /** Jobs waiting for a thread. */
  get backlog(): number {
    return this.queue.length;
  }

  render(job: Omit<RenderJob, "job">): Promise<Uint8Array> {
    return new Promise((resolve, reject) => {
      this.queue.push({ job, resolve, reject });
      this.pump();
    });
  }

  private pump(): void {
    while (this.idle.length > 0 && this.queue.length > 0) {
      const w = this.idle.pop()!;
      const p = this.queue.shift()!;
      const id = this.next++;
      this.inflight.set(id, p);
      this.byWorker.set(w, id);
      w.postMessage({ ...p.job, job: id });
    }
  }

  async close(): Promise<void> {
    await Promise.all(this.all.map((w) => w.terminate()));
  }
}
