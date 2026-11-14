import { dirname } from "node:path";
import { writeDurableJson } from "./durable-json.ts";

/** A small, private startup projection; never a replacement for journal originals. */
export class RecoveryReporter {
  #phase: "recovering" | "ready" | "failed" = "recovering";
  #timer?: ReturnType<typeof setInterval>;
  #pending: Promise<void> = Promise.resolve();
  #writing = false;
  readonly startedAt = Date.now();
  constructor(
    readonly path: string,
    readonly service: string,
    readonly progress: () => unknown,
  ) {}
  async start() {
    await Deno.mkdir(dirname(this.path), { recursive: true, mode: 0o700 });
    await this.#write();
    this.#timer = setInterval(() => {
      if (!this.#writing) void this.#write().catch(() => {});
    }, 5000);
  }
  #write() {
    this.#writing = true;
    this.#pending = writeDurableJson(this.path, {
      service: this.service,
      pid: Deno.pid,
      startedAt: this.startedAt,
      updatedAt: Date.now(),
      phase: this.#phase,
      progress: this.progress(),
    }).finally(() => this.#writing = false);
    return this.#pending;
  }
  async close() {
    clearInterval(this.#timer);
    await this.#pending.catch(() => {});
  }
  async finish(phase: "ready" | "failed") {
    clearInterval(this.#timer);
    await this.#pending.catch(() => {});
    this.#phase = phase;
    await this.#write();
  }
}
