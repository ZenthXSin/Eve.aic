// 内核：插件注册、按依赖顺序启动与逆序停止、调用转发、换版与回滚。
// 内核不含任何认知逻辑；Eve 自己也只是其中一个插件。

import { checkReplacement, plan } from "./graph.ts";
import { Instance, LifecycleError } from "./instance.ts";
import { readManifest } from "./manifest.ts";
import { type FromWorker, type Json, KernelError, type Manifest } from "./protocol.ts";
import { type StateRecord, Store } from "./store.ts";
import { type CallRecord, fingerprint } from "./calls.ts";
import { toWireError } from "./protocol.ts";

export interface KernelEvent {
  at: string;
  event: string;
  plugin?: string;
  detail?: string;
}

export interface KernelOptions {
  /** 持久化目录：状态、快照、失败版本留存、回执。 */
  dataDir: string;
  /** 换版前是否用 deno check 做类型检查，默认开启。 */
  typecheck?: boolean;
  /** 默认调用超时。 */
  callTimeoutMs?: number;
  /** 换版时等待在途调用排空的时间。 */
  drainMs?: number;
  log?: (event: KernelEvent) => void;
}

export interface CallOptions {
  timeoutMs?: number;
  idempotencyKey?: string;
}

export type SwapOutcome = "confirmed" | "rejected" | "rolled_back" | "rollback_failed";

export interface SwapReport {
  plugin: string;
  from: string;
  to: string;
  startedAt: string;
  finishedAt: string;
  outcome: SwapOutcome;
  /** 依次到达的信号：ready、healthy、confirmed:<方式>。 */
  signals: string[];
  /** 拒绝或回滚的原因。 */
  reason?: string;
  /** 换版前的状态快照文件。 */
  snapshot?: string;
  /** 失败版本写下的状态的留存位置。 */
  failedState?: string;
  /** 过程中需要如实说明的情况，例如在途调用没有排空。 */
  notes: string[];
}

export interface PluginStatus {
  name: string;
  version: string;
  status: "registered" | "running" | "failed" | "stopped";
}

interface Entry {
  manifest: Manifest;
  dir: string;
  status: PluginStatus["status"];
  instance?: Instance;
  record?: StateRecord;
  gate?: { promise: Promise<void>; release(): void };
  inflight: number;
  drained: Array<() => void>;
  observer?: (ok: boolean, error?: Error) => void;
  crashed?: (reason: string) => void;
}

const ROOT = new URL("..", import.meta.url).pathname;
const CODE_ROOTS = [`${ROOT}kernel`, `${ROOT}sdk`, `${ROOT}contracts`];
const WAIT_TIMEOUT = Symbol("wait_timeout");

function sleepUntil<T>(promise: Promise<T>, ms: number): Promise<T | typeof WAIT_TIMEOUT> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<typeof WAIT_TIMEOUT>((resolve) => {
    timer = setTimeout(() => resolve(WAIT_TIMEOUT), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

export class Kernel {
  #store: Store;
  #entries = new Map<string, Entry>();
  #order: string[] = [];
  #providers = new Map<string, string>();
  #idempotent = new Map<string, Promise<{ request: string; execution: Promise<Json> }>>();
  #swapping = false;
  #started = false;
  #stopping = false;
  #stopWork?: Promise<string[]>;
  #options: Required<Omit<KernelOptions, "log">> & { log: (event: KernelEvent) => void };

  constructor(options: KernelOptions) {
    this.#options = {
      typecheck: true,
      callTimeoutMs: 30_000,
      drainMs: 5_000,
      log: () => {},
      ...options,
    };
    this.#store = new Store(options.dataDir);
  }

  #log(event: string, plugin?: string, detail?: string): void {
    this.#options.log({ at: new Date().toISOString(), event, plugin, detail });
  }

  /** 登记一个插件目录（内含 manifest.json）。必须在 start 之前。 */
  async register(dir: string): Promise<Manifest> {
    if (this.#started) throw new KernelError("rejected", "内核启动后不能再用 register 登记插件");
    const absolute = await Deno.realPath(dir);
    const manifest = await readManifest(absolute);
    if (this.#entries.has(manifest.name)) {
      throw new KernelError("rejected", `插件名重复：${manifest.name}`);
    }
    this.#entries.set(manifest.name, {
      manifest,
      dir: absolute,
      status: "registered",
      inflight: 0,
      drained: [],
    });
    return manifest;
  }

  #replan(): void {
    const result = plan([...this.#entries.values()].map((entry) => entry.manifest));
    this.#order = result.order;
    this.#providers = result.providers;
  }

  #spawn(entry: Entry, manifest: Manifest, dir: string): Instance {
    const instance: Instance = new Instance(manifest, dir, CODE_ROOTS, {
      onCall: (call) => this.#fromPlugin(manifest, call),
      onState: async (data) => {
        const record: StateRecord = {
          schema: 1,
          plugin: manifest.name,
          version: manifest.version,
          stateVersion: manifest.stateVersion,
          updatedAt: new Date().toISOString(),
          data,
        };
        try {
          await this.#store.save(record);
        } catch (error) {
          this.#log("state_save_failed", manifest.name, (error as Error).message);
          throw error;
        }
        entry.record = record;
      },
      onLog: (message) => this.#log("plugin_log", manifest.name, message),
      onCrash: (reason) => {
        if (entry.instance !== instance) return;
        if (entry.crashed) {
          entry.crashed(reason);
          return;
        }
        entry.instance = undefined;
        entry.status = "failed";
        this.#log("plugin_crashed", manifest.name, reason);
      },
    });
    return instance;
  }

  /** 按依赖顺序启动全部插件。任何一个失败，已启动的按逆序停止，然后抛出原因。 */
  async start(): Promise<void> {
    if (this.#started) throw new KernelError("rejected", "内核已经启动");
    await this.#store.init();
    this.#replan();
    const started: Entry[] = [];
    try {
      for (const name of this.#order) {
        const entry = this.#entries.get(name)!;
        entry.record = await this.#store.load(name);
        const instance = this.#spawn(entry, entry.manifest, entry.dir);
        await instance.start(entry.record?.data, entry.record?.stateVersion);
        entry.instance = instance;
        entry.status = "running";
        started.push(entry);
        this.#log("plugin_started", name, entry.manifest.version);
      }
    } catch (error) {
      for (const entry of started.reverse()) {
        await entry.instance?.stop();
        entry.instance = undefined;
        entry.status = "stopped";
      }
      this.#log("start_failed", undefined, (error as Error).message);
      throw error;
    }
    this.#started = true;
  }

  /** 按启动的逆序停止全部插件，返回停止过程中遇到的问题汇总。 */
  stop(): Promise<string[]> {
    if (this.#stopWork) return this.#stopWork;
    const work = this.#stop();
    this.#stopWork = work;
    const clear = () => {
      this.#stopWork = undefined;
    };
    work.then(clear, clear);
    return work;
  }

  async #stop(): Promise<string[]> {
    const problems: string[] = [];
    this.#stopping = true;
    for (const name of [...this.#order].reverse()) {
      const entry = this.#entries.get(name)!;
      if (!entry.instance) continue;
      // 先排空依赖方，再封住其依赖。Disable中的真实收尾调用仍可到达后续服务。
      this.#pause(entry);
      const problem = await entry.instance.stop();
      if (problem) problems.push(`${name}：${problem}`);
      entry.instance = undefined;
      entry.status = "stopped";
      try {
        await this.#store.settle(name);
      } catch (error) {
        problems.push(`${name}：状态保存失败：${(error as Error).message}`);
      }
      this.#log("plugin_stopped", name);
    }
    this.#started = false;
    for (const entry of this.#entries.values()) this.#resume(entry);
    this.#stopping = false;
    return problems;
  }

  status(): PluginStatus[] {
    return [...this.#entries.values()].map((entry) => ({
      name: entry.manifest.name,
      version: entry.manifest.version,
      status: entry.status,
    }));
  }

  /** 从内核外部（宿主、测试、守护）调用某个契约。 */
  call<T extends Json = Json>(
    contract: string,
    method: string,
    args: Json = null,
    options: CallOptions = {},
  ): Promise<T> {
    return this.#dispatch("host", contract, method, args, options) as Promise<T>;
  }

  #fromPlugin(caller: Manifest, call: Extract<FromWorker, { t: "call" }>): Promise<Json> {
    if (!(call.contract in caller.requires) && !(call.contract in caller.optional)) {
      return Promise.reject(
        new KernelError("rejected", `${caller.name} 没有在清单里声明对 ${call.contract} 的依赖`),
      );
    }
    return this.#dispatch(`plugin:${caller.name}`, call.contract, call.method, call.args, {
      timeoutMs: call.timeoutMs,
      idempotencyKey: call.idempotencyKey,
    });
  }

  async callStatus(contract: string, method: string, key: string): Promise<CallRecord | undefined> {
    const record = await this.#store.loadCall(await fingerprint(["host", contract, method, key]));
    if (record?.status === "started") {
      return {
        ...record,
        status: "unknown",
        error: { kind: "unknown", message: "调用已开始，尚未确认结果；不会重新执行" },
      };
    }
    return record;
  }

  async #dispatch(
    caller: string,
    contract: string,
    method: string,
    args: Json,
    options: CallOptions,
  ): Promise<Json> {
    if (this.#stopping && caller === "host") {
      throw new KernelError("unavailable", "内核正在停止，不接新的宿主动作");
    }
    args = structuredClone(args);
    const timeoutMs = options.timeoutMs ?? this.#options.callTimeoutMs;
    if (!Number.isFinite(timeoutMs) || timeoutMs < 0) {
      throw new KernelError("rejected", "调用期限无效");
    }
    const deadline = Date.now() + timeoutMs;
    let execution: Promise<Json>;
    if (!options.idempotencyKey) execution = this.#forward(contract, method, args, deadline);
    else {
      const key = options.idempotencyKey;
      if (key.length > 512) throw new KernelError("rejected", "幂等键过长");
      const id = await fingerprint([caller, contract, method, key]);
      const request = await fingerprint(args);
      let pending = this.#idempotent.get(id);
      if (!pending) {
        pending = this.#prepareCall(id, request, caller, contract, method, key, args, deadline);
        this.#idempotent.set(id, pending);
        pending.catch(() => {});
      }
      const operation = await pending;
      if (request !== operation.request) {
        throw new KernelError("rejected", "相同幂等键不能对应不同参数");
      }
      execution = operation.execution;
    }
    // 超时仅结束等待，真实执行与迟到回执继续跟踪；不因超时释放幂等键。
    const result = await sleepUntil(execution, Math.max(0, deadline - Date.now()));
    if (result === WAIT_TIMEOUT) {
      throw new KernelError(
        "timeout",
        `${contract}.${method} 等待超时，执行结果未知；查询原调用，不要重放`,
      );
    }
    return result;
  }

  async #prepareCall(
    id: string,
    request: string,
    caller: string,
    contract: string,
    method: string,
    key: string,
    args: Json,
    deadline: number,
  ): Promise<{ request: string; execution: Promise<Json> }> {
    const previous = await this.#store.loadCall(id);
    if (previous) {
      const execution = previous.status === "completed"
        ? Promise.resolve(previous.value!)
        : Promise.reject(
          new KernelError(
            previous.status === "started" ? "unknown" : previous.error!.kind,
            previous.error?.message ?? "上次调用已开始但结果未知；原调用保留，不会重放",
          ),
        );
      execution.catch(() => {});
      return { request: previous.request, execution };
    }
    const record: CallRecord = {
      schema: 1,
      id,
      caller,
      contract,
      method,
      key,
      request,
      status: "started",
      updatedAt: new Date().toISOString(),
    };
    // 执行前必须记录动作；写入失败时不调用插件。
    await this.#store.saveCall(record);
    const execution = this.#forward(contract, method, args, deadline).then(async (value) => {
      await this.#store.saveCall({
        ...record,
        status: "completed",
        value,
        updatedAt: new Date().toISOString(),
      });
      return value;
    }, async (error) => {
      const wire = toWireError(error);
      await this.#store.saveCall({
        ...record,
        status: ["timeout", "unknown"].includes(wire.kind) ? "unknown" : "failed",
        error: wire,
        updatedAt: new Date().toISOString(),
      });
      throw error;
    });
    execution.catch(() => {});
    return { request, execution };
  }

  async #forward(
    contract: string,
    method: string,
    args: Json,
    deadline: number,
  ): Promise<Json> {
    const provider = this.#providers.get(contract);
    if (!provider) throw new KernelError("not_found", `没有插件提供契约 ${contract}`);
    const entry = this.#entries.get(provider)!;
    while (entry.gate) {
      const waited = await sleepUntil(entry.gate.promise, Math.max(0, deadline - Date.now()));
      if (waited === WAIT_TIMEOUT) {
        throw new KernelError("unavailable", `${provider} 正在换版，等待超时，调用没有执行`);
      }
    }
    const instance = entry.instance;
    if (!instance) throw new KernelError("unavailable", `${provider} 不在运行，调用没有执行`);
    entry.inflight++;
    try {
      const value = await instance.call(contract, method, args);
      if (entry.instance === instance) entry.observer?.(true);
      return value;
    } catch (error) {
      if (entry.instance === instance) entry.observer?.(false, error as Error);
      throw error;
    } finally {
      entry.inflight--;
      if (entry.inflight === 0) {
        for (const notify of entry.drained.splice(0)) notify();
      }
    }
  }

  #pause(entry: Entry): void {
    if (entry.gate) return;
    let release = () => {};
    const promise = new Promise<void>((resolve) => {
      release = resolve;
    });
    entry.gate = { promise, release };
  }

  #resume(entry: Entry): void {
    const gate = entry.gate;
    entry.gate = undefined;
    gate?.release();
  }

  async #drain(entry: Entry): Promise<boolean> {
    if (entry.inflight === 0) return true;
    const empty = new Promise<void>((resolve) => entry.drained.push(resolve));
    return (await sleepUntil(empty, this.#options.drainMs)) !== WAIT_TIMEOUT;
  }

  async #typecheck(manifest: Manifest, dir: string): Promise<string | undefined> {
    const command = new Deno.Command(Deno.execPath(), {
      args: ["check", "--quiet", `${dir}/${manifest.entry}`],
      stdout: "null",
      stderr: "piped",
    });
    const output = await command.output();
    if (output.success) return undefined;
    return new TextDecoder().decode(output.stderr).trim();
  }

  #confirm(entry: Entry, instance: Instance): Promise<string> {
    const policy = instance.manifest.confirm;
    if (policy.calls <= 0) return Promise.resolve("not_required");
    return new Promise<string>((resolve, reject) => {
      let successes = 0;
      const finish = () => {
        clearTimeout(timer);
        entry.observer = undefined;
        entry.crashed = undefined;
      };
      entry.observer = (ok, error) => {
        if (!ok) {
          finish();
          reject(new Error(`首批真实调用失败：${error?.message ?? "未知错误"}`));
        } else if (++successes >= policy.calls) {
          finish();
          resolve("real_calls");
        }
      };
      entry.crashed = (reason) => {
        finish();
        reject(new Error(`确认期间崩溃：${reason}`));
      };
      const timer = setTimeout(() => {
        finish();
        if (successes > 0) {
          resolve("partial_calls");
          return;
        }
        // 期限内没有真实流量：由内核主动探测，避免无流量的插件被误判。
        instance.probe().then(
          () => resolve("probe_only"),
          (error) => reject(new Error(`确认期限到达且探测失败：${(error as Error).message}`)),
        );
      }, policy.deadlineMs);
    });
  }

  /**
   * 把正在运行的插件换成 newDir 里的版本。
   * 成功信号三段：就绪、健康探测通过、首批真实调用成功；任何一段失败或超时都回到旧版本并恢复快照。
   * 结果总是写入回执 receipts/swaps.jsonl，不论成功、拒绝还是回滚。
   */
  async swap(name: string, newDir: string): Promise<SwapReport> {
    const entry = this.#entries.get(name);
    if (!entry) throw new KernelError("not_found", `没有登记插件 ${name}`);
    const report: SwapReport = {
      plugin: name,
      from: entry.manifest.version,
      to: "unknown",
      startedAt: new Date().toISOString(),
      finishedAt: "",
      outcome: "rejected",
      signals: [],
      notes: [],
    };
    const finish = async (outcome: SwapOutcome, reason?: string): Promise<SwapReport> => {
      report.outcome = outcome;
      report.reason = reason;
      report.finishedAt = new Date().toISOString();
      await this.#store.appendReceipt("swaps", report);
      this.#log(`swap_${outcome}`, name, reason ?? `${report.from} -> ${report.to}`);
      return report;
    };

    if (this.#stopping) return await finish("rejected", "内核正在停止，不能启动换版");
    if (this.#swapping) return await finish("rejected", "已有另一次换版在进行");
    if (!entry.instance) return await finish("rejected", `${name} 不在运行`);
    this.#swapping = true;
    try {
      // 第一道门：清单、契约兼容、类型检查。此时旧版本仍在正常服务。
      let next: Manifest;
      let absolute: string;
      try {
        absolute = await Deno.realPath(newDir);
        next = await readManifest(absolute);
      } catch (error) {
        return await finish("rejected", (error as Error).message);
      }
      report.to = next.version;
      if (next.name !== name) {
        return await finish("rejected", `新目录里的插件名是 ${next.name}，不是 ${name}`);
      }
      const manifests = [...this.#entries.values()].map((item) => item.manifest);
      const incompatible = checkReplacement(manifests, next);
      if (incompatible) return await finish("rejected", `契约检查不通过：${incompatible}`);
      if (this.#options.typecheck) {
        const problem = await this.#typecheck(next, absolute);
        if (problem) return await finish("rejected", `类型检查不通过：${problem}`);
      }

      // 停旧版本之前：暂停新调用、排空在途调用、收齐状态、拍快照。
      const old = entry.instance;
      const oldManifest = entry.manifest;
      const oldDir = entry.dir;
      this.#pause(entry);
      this.#log("swap_paused", name, `${oldManifest.version} -> ${next.version}`);
      if (!(await this.#drain(entry))) {
        report.notes.push("在途调用没有在期限内排空，它们的结果记为未知");
      }
      if (!(await old.flush(1000))) {
        report.notes.push("旧版本没有回应状态收齐请求，快照取自最后一次已知状态");
      }
      let snapshot: { path: string; record: StateRecord | undefined };
      try {
        snapshot = await this.#store.snapshot(name);
      } catch (error) {
        this.#resume(entry);
        return await finish("rejected", `拍快照失败，旧版本继续运行：${(error as Error).message}`);
      }
      report.snapshot = snapshot.path;
      const stopProblem = await old.stop();
      if (stopProblem) report.notes.push(`旧版本清理时有问题：${stopProblem}`);
      entry.instance = undefined;

      let candidate: Instance | undefined;
      try {
        candidate = this.#spawn(entry, next, absolute);
        await candidate.start(snapshot.record?.data, snapshot.record?.stateVersion);
        report.signals.push("ready");
        await candidate.probe();
        report.signals.push("healthy");

        entry.manifest = next;
        entry.dir = absolute;
        entry.instance = candidate;
        this.#replan();
        const confirmation = this.#confirm(entry, candidate);
        this.#resume(entry);
        report.signals.push(`confirmed:${await confirmation}`);
        entry.status = "running";
        return await finish("confirmed");
      } catch (error) {
        const reason = error instanceof LifecycleError
          ? `${error.stage} 阶段${error.timedOut ? "超时" : "失败"}：${error.message}`
          : (error as Error).message;
        this.#pause(entry);
        candidate?.kill("回滚");
        entry.instance = undefined;
        try {
          // 保存失败版本本身失败时不覆盖原件；整个回滚都在同一错误路径内。
          report.failedState = await this.#store.keepFailed(name, next.version);
          await this.#store.restore(name, snapshot.record);
          entry.record = snapshot.record;
          entry.manifest = oldManifest;
          entry.dir = oldDir;
          this.#replan();
          const restored = this.#spawn(entry, oldManifest, oldDir);
          await restored.start(snapshot.record?.data, snapshot.record?.stateVersion);
          entry.instance = restored;
          entry.status = "running";
          this.#resume(entry);
          return await finish("rolled_back", reason);
        } catch (rollbackError) {
          entry.status = "failed";
          this.#resume(entry);
          return await finish(
            "rollback_failed",
            `${reason}；旧版本也没能重新启动：${(rollbackError as Error).message}`,
          );
        }
      }
    } finally {
      this.#swapping = false;
      this.#resume(entry);
    }
  }
}
