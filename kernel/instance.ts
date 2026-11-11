// 一个正在运行的插件版本：一个带独立权限的 worker，加上与它之间的全部往来。

import {
  type FromWorker,
  type Json,
  KernelError,
  type Manifest,
  type StateReply,
  type ToWorker,
} from "./protocol.ts";

export class LifecycleError extends Error {
  constructor(readonly stage: string, readonly timedOut: boolean, message: string) {
    super(message);
    this.name = "LifecycleError";
  }
}

export interface InstanceHooks {
  onCall(call: Extract<FromWorker, { t: "call" }>): Promise<Json>;
  onState(data: Json): Promise<void>;
  onLog(message: string): void;
  /** 生命周期之外的崩溃或失败。 */
  onCrash(reason: string): void;
}

interface Waiter<T> {
  resolve(value: T): void;
  reject(error: Error): void;
  timer?: ReturnType<typeof setTimeout>;
}

export class Instance {
  #worker: Worker;
  #alive = true;
  #nextId = 1;
  #calls = new Map<number, Waiter<Json>>();
  #probes = new Map<number, Waiter<void>>();
  #flushes = new Map<number, Waiter<void>>();
  #lifecycle: { expect: string; stage: string; waiter: Waiter<FromWorker> } | undefined;

  constructor(
    readonly manifest: Manifest,
    readonly dir: string,
    codeRoots: string[],
    private hooks: InstanceHooks,
  ) {
    const permissions = manifest.permissions;
    const list = (items: string[] | undefined) => items && items.length > 0 ? items : false;
    this.#worker = new Worker(new URL("./worker_host.ts", import.meta.url), {
      type: "module",
      name: `${manifest.name}@${manifest.version}`,
      deno: {
        permissions: {
          read: [dir, ...codeRoots, ...(permissions.read ?? [])],
          write: list(permissions.write),
          net: list(permissions.net),
          env: false,
          run: false,
          ffi: false,
          sys: false,
        },
      },
    } as unknown as WorkerOptions);
    this.#worker.addEventListener("message", (event) => this.#receive(event.data as FromWorker));
    this.#worker.addEventListener("error", (event) => {
      event.preventDefault();
      this.#fail(`worker 出错：${event.message}`);
    });
    this.#worker.addEventListener("messageerror", () => this.#fail("收到无法解析的消息"));
  }

  get alive(): boolean {
    return this.#alive;
  }

  #post(message: ToWorker | StateReply): void {
    if (this.#alive) this.#worker.postMessage(message);
  }

  #fail(reason: string): void {
    if (!this.#alive) return;
    const lifecycle = this.#lifecycle;
    this.kill(reason);
    if (lifecycle) {
      lifecycle.waiter.reject(new LifecycleError(lifecycle.stage, false, reason));
    } else {
      this.hooks.onCrash(reason);
    }
  }

  #receive(message: FromWorker): void {
    if (!this.#alive) return;
    switch (message.t) {
      case "loaded":
      case "enabled":
      case "disabled": {
        const lifecycle = this.#lifecycle;
        if (lifecycle?.expect === message.t) {
          this.#lifecycle = undefined;
          clearTimeout(lifecycle.waiter.timer);
          lifecycle.waiter.resolve(message);
        }
        break;
      }
      case "failed": {
        const lifecycle = this.#lifecycle;
        if (lifecycle) {
          this.#lifecycle = undefined;
          clearTimeout(lifecycle.waiter.timer);
          lifecycle.waiter.reject(new LifecycleError(message.stage, false, message.message));
        } else {
          this.#fail(`${message.stage} 阶段失败：${message.message}`);
        }
        break;
      }
      case "result": {
        const waiter = this.#calls.get(message.id);
        if (!waiter) break;
        this.#calls.delete(message.id);
        clearTimeout(waiter.timer);
        if (message.ok) waiter.resolve(message.value);
        else waiter.reject(new KernelError(message.error.kind, message.error.message));
        break;
      }
      case "call":
        this.hooks.onCall(message).then(
          (value) => this.#post({ t: "reply", id: message.id, ok: true, value }),
          (error) =>
            this.#post({
              t: "reply",
              id: message.id,
              ok: false,
              error: error instanceof KernelError
                ? { kind: error.kind, message: error.message }
                : { kind: "remote", message: String(error) },
            }),
        );
        break;
      case "probed": {
        const waiter = this.#probes.get(message.id);
        if (!waiter) break;
        this.#probes.delete(message.id);
        clearTimeout(waiter.timer);
        if (message.ok) waiter.resolve();
        else waiter.reject(new Error(message.message ?? "健康探测失败"));
        break;
      }
      case "flushed": {
        const waiter = this.#flushes.get(message.id);
        if (!waiter) break;
        this.#flushes.delete(message.id);
        clearTimeout(waiter.timer);
        waiter.resolve();
        break;
      }
      case "state":
        this.hooks.onState(message.data).then(
          () => this.#post({ t: "state_saved", id: message.id, ok: true }),
          (error) =>
            this.#post({
              t: "state_saved",
              id: message.id,
              ok: false,
              error: { kind: "storage", message: `状态保存失败：${(error as Error).message}` },
            }),
        );
        break;
      case "log":
        this.hooks.onLog(message.message);
        break;
    }
  }

  #awaitLifecycle(expect: string, stage: string, timeoutMs: number): Promise<FromWorker> {
    return new Promise<FromWorker>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#lifecycle = undefined;
        reject(new LifecycleError(stage, true, `${stage} 阶段在 ${timeoutMs} 毫秒内没有完成`));
      }, timeoutMs);
      this.#lifecycle = { expect, stage, waiter: { resolve, reject, timer } };
    });
  }

  /** 加载并启用。失败或超时时终止 worker 并抛出 LifecycleError。 */
  async start(state: Json | undefined, stateVersion: number | undefined): Promise<void> {
    const entry = new URL(this.manifest.entry, `file://${this.dir}/`).href;
    try {
      const loaded = this.#awaitLifecycle("loaded", "load", this.manifest.deadlines.loadMs);
      this.#post({ t: "init", manifest: this.manifest, entry, state, stateVersion });
      const message = await loaded as Extract<FromWorker, { t: "loaded" }>;
      const missing = Object.keys(this.manifest.provides).filter((contract) =>
        !message.provides.includes(contract)
      );
      if (missing.length > 0) {
        throw new LifecycleError(
          "load",
          false,
          `清单声明提供 ${missing.join("、")}，但插件没有实现`,
        );
      }
      const enabled = this.#awaitLifecycle("enabled", "enable", this.manifest.deadlines.enableMs);
      this.#post({ t: "enable" });
      await enabled;
    } catch (error) {
      this.kill("启动失败");
      throw error;
    }
  }

  /** 健康探测；不健康或超时都抛错。 */
  probe(): Promise<void> {
    if (!this.#alive) return Promise.reject(new Error("插件不在运行"));
    const id = this.#nextId++;
    const timeoutMs = this.manifest.deadlines.probeMs;
    return new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#probes.delete(id);
        reject(new Error(`健康探测在 ${timeoutMs} 毫秒内没有回应`));
      }, timeoutMs);
      this.#probes.set(id, { resolve, reject, timer });
      this.#post({ t: "probe", id });
    });
  }

  /** 等 worker 把此前发出的状态更新都送达。返回是否在期限内完成。 */
  flush(timeoutMs: number): Promise<boolean> {
    if (!this.#alive) return Promise.resolve(false);
    const id = this.#nextId++;
    return new Promise<boolean>((resolve) => {
      const timer = setTimeout(() => {
        this.#flushes.delete(id);
        resolve(false);
      }, timeoutMs);
      this.#flushes.set(id, { resolve: () => resolve(true), reject: () => resolve(false), timer });
      this.#post({ t: "flush", id });
    });
  }

  call(contract: string, method: string, args: Json): Promise<Json> {
    if (!this.#alive) {
      return Promise.reject(new KernelError("unavailable", `${this.manifest.name} 不在运行`));
    }
    const id = this.#nextId++;
    return new Promise<Json>((resolve, reject) => {
      // 等待期限由内核管理。调用方不再等待后，仍接收真正的迟到结果。
      this.#calls.set(id, { resolve, reject });
      this.#post({ t: "call", id, contract, method, args });
    });
  }

  /** 正常停止：执行清理后终止。返回清理阶段的问题描述；一切正常返回 undefined。 */
  async stop(): Promise<string | undefined> {
    if (!this.#alive) return undefined;
    let problem: string | undefined;
    try {
      const disabled = this.#awaitLifecycle(
        "disabled",
        "disable",
        this.manifest.deadlines.disableMs,
      );
      this.#post({ t: "disable" });
      await disabled;
    } catch (error) {
      problem = (error as Error).message;
    }
    this.kill("已停止");
    return problem;
  }

  /** 立刻终止 worker；在途调用的结果记为未知。 */
  kill(reason: string): void {
    if (!this.#alive) return;
    this.#alive = false;
    this.#worker.terminate();
    if (this.#lifecycle) clearTimeout(this.#lifecycle.waiter.timer);
    this.#lifecycle = undefined;
    for (const waiter of this.#calls.values()) {
      clearTimeout(waiter.timer);
      waiter.reject(
        new KernelError(
          "unknown",
          `${this.manifest.name} 已终止（${reason}），在途调用是否已执行未知`,
        ),
      );
    }
    this.#calls.clear();
    for (const waiter of [...this.#probes.values(), ...this.#flushes.values()]) {
      clearTimeout(waiter.timer);
      waiter.reject(new Error(reason));
    }
    this.#probes.clear();
    this.#flushes.clear();
  }
}
