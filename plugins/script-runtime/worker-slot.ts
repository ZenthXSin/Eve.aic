import type { FromScript } from "../../contracts/script-wire.ts";

/** A single-use, empty sandbox. No user module is sent until admission and checking finish. */
export class ScriptWorkerSlot {
  readonly worker: Worker;
  readonly ready: Promise<boolean>;
  failed = false;
  #resolve!: (ready: boolean) => void;
  #timer: ReturnType<typeof setTimeout>;
  constructor() {
    this.ready = new Promise((resolve) => this.#resolve = resolve);
    this.worker = new Worker(new URL("./worker.ts", import.meta.url), {
      type: "module",
      name: "eve-script-sandbox",
      deno: {
        permissions: {
          read: [
            new URL("../../contracts/", import.meta.url).pathname,
            new URL("../../kernel/protocol.ts", import.meta.url).pathname,
            new URL("./worker.ts", import.meta.url).pathname,
          ],
          write: false,
          net: false,
          run: false,
          env: false,
          ffi: false,
          sys: false,
        },
      },
    } as unknown as WorkerOptions);
    this.#timer = setTimeout(() => this.close(), 10_000);
    this.worker.addEventListener("message", (event: MessageEvent<FromScript>) => {
      if (event.data.t !== "ready") return;
      clearTimeout(this.#timer);
      this.#resolve(true);
    });
    this.worker.addEventListener("error", (event) => {
      event.preventDefault();
      this.close();
    });
  }
  close() {
    this.failed = true;
    clearTimeout(this.#timer);
    this.#resolve(false);
    this.worker.terminate();
  }
}
