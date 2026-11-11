// 每个插件 worker 的入口：加载插件模块，驱动三段生命周期，收发调用。
// 这里的任何失败都如实报告给内核，由内核决定回滚。

import type { CallOptions, Handler, PluginContext, PluginModule } from "../sdk/plugin.ts";
import {
  type FromWorker,
  type Json,
  KernelError,
  type Manifest,
  toWireError,
  type ToWorker,
} from "./protocol.ts";

const scope = self as unknown as {
  postMessage(message: FromWorker): void;
  onmessage: ((event: MessageEvent<ToWorker>) => void) | null;
};

const send = (message: FromWorker) => scope.postMessage(message);

let plugin: PluginModule = {};
let manifest: Manifest;
let state: Json | undefined;
let loading = false;
const provided = new Map<string, Record<string, Handler>>();
const cleanups: Array<() => void | Promise<void>> = [];
const pending = new Map<number, { resolve(value: Json): void; reject(error: Error): void }>();
let nextCallId = 1;

const ctx: PluginContext = {
  get manifest() {
    return manifest;
  },
  provide(contract, handlers) {
    if (!loading) throw new Error("provide 只能在 load 阶段调用");
    if (!(contract in manifest.provides)) {
      throw new Error(`清单的 provides 里没有声明契约 ${contract}`);
    }
    provided.set(contract, handlers);
  },
  call<T extends Json = Json>(
    contract: string,
    method: string,
    args: Json = null,
    options: CallOptions = {},
  ): Promise<T> {
    const id = nextCallId++;
    return new Promise<T>((resolve, reject) => {
      pending.set(id, { resolve: resolve as (value: Json) => void, reject });
      send({ t: "call", id, contract, method, args, ...options });
    });
  },
  getState<T extends Json = Json>() {
    return state as T | undefined;
  },
  setState(next) {
    // 经过一次结构化复制，保证之后对原对象的修改不会悄悄改变已提交的状态。
    state = structuredClone(next);
    send({ t: "state", data: state });
  },
  onDisable(cleanup) {
    cleanups.push(cleanup);
  },
  log(message) {
    send({ t: "log", message });
  },
};

function describe(error: unknown): string {
  return error instanceof Error ? `${error.name}: ${error.message}` : String(error);
}

async function init(message: Extract<ToWorker, { t: "init" }>): Promise<void> {
  manifest = message.manifest;
  const imported = await import(message.entry);
  plugin = (imported.default ?? {}) as PluginModule;
  state = message.state;
  if (state !== undefined && message.stateVersion !== manifest.stateVersion) {
    if (!plugin.migrate) {
      throw new Error(
        `状态格式是第 ${message.stateVersion} 版，插件需要第 ${manifest.stateVersion} 版，且没有提供 migrate`,
      );
    }
    state = await plugin.migrate(state, message.stateVersion ?? 0);
    send({ t: "state", data: state });
  }
  loading = true;
  try {
    await plugin.load?.(ctx);
  } finally {
    loading = false;
  }
  send({ t: "loaded", provides: [...provided.keys()] });
}

async function disable(): Promise<void> {
  await plugin.disable?.(ctx);
  while (cleanups.length > 0) await cleanups.pop()!();
  send({ t: "disabled" });
}

async function handleCall(message: Extract<ToWorker, { t: "call" }>): Promise<void> {
  try {
    const handler = provided.get(message.contract)?.[message.method];
    if (!handler) {
      throw new KernelError("not_found", `${message.contract} 没有方法 ${message.method}`);
    }
    const value = await handler(message.args);
    send({ t: "result", id: message.id, ok: true, value: value ?? null });
  } catch (error) {
    send({ t: "result", id: message.id, ok: false, error: toWireError(error) });
  }
}

scope.onmessage = (event) => {
  const message = event.data;
  switch (message.t) {
    case "init":
      init(message).catch((error) =>
        send({ t: "failed", stage: "load", message: describe(error) })
      );
      break;
    case "enable":
      Promise.resolve()
        .then(() => plugin.enable?.(ctx))
        .then(() => send({ t: "enabled" }))
        .catch((error) => send({ t: "failed", stage: "enable", message: describe(error) }));
      break;
    case "disable":
      disable().catch((error) => send({ t: "failed", stage: "disable", message: describe(error) }));
      break;
    case "call":
      handleCall(message);
      break;
    case "reply": {
      const waiter = pending.get(message.id);
      if (!waiter) break;
      pending.delete(message.id);
      if (message.ok) waiter.resolve(message.value);
      else waiter.reject(new KernelError(message.error.kind, message.error.message));
      break;
    }
    case "probe":
      Promise.resolve()
        .then(() => plugin.health?.(ctx))
        .then(() => send({ t: "probed", id: message.id, ok: true }))
        .catch((error) =>
          send({ t: "probed", id: message.id, ok: false, message: describe(error) })
        );
      break;
    case "flush":
      send({ t: "flushed", id: message.id });
      break;
  }
};
