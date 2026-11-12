import { PHONE_METHODS } from "../../contracts/phone-rpc.ts";
import { RUNTIME_METHODS } from "../../contracts/runtime.ts";
import { UPGRADE_METHODS } from "../../contracts/upgrades.ts";
import type { ScriptRequest, ScriptSummary } from "../../contracts/script.ts";
import type { FromScript } from "../../contracts/script-wire.ts";
import { canonical, fingerprint } from "../../kernel/calls.ts";
import { type Json, KernelError, toWireError, type WireError } from "../../kernel/protocol.ts";
import type { PluginContext } from "../../sdk/plugin.ts";
import { imageTextView } from "../../sdk/image-text.ts";
import { type JobRecord, ScriptStore } from "./store.ts";

interface LiveJob {
  worker?: Worker;
  checker?: Deno.ChildProcess;
  timer?: ReturnType<typeof setTimeout>;
  done: Promise<void>;
  resolve(): void;
  invocations: Set<Promise<unknown>>;
  nextSeq: number;
  finishing: boolean;
  runner?: Promise<void>;
}
export interface ScriptConfig {
  startupTimeoutMs?: number;
  directory: string;
  denoBinary: string;
  cacheDirectory: string;
  defaultTimeoutMs: number;
}
const TERMINAL = new Set(["completed", "failed", "cancelled", "unknown"]);
const ERROR_KINDS = new Set([
  "remote",
  "timeout",
  "unavailable",
  "not_found",
  "rejected",
  "corrupt",
  "storage",
  "unknown",
]);
function validError(error: unknown): error is WireError {
  if (error === null || typeof error !== "object") return false;
  const value = error as WireError;
  return ERROR_KINDS.has(value.kind) && typeof value.message === "string";
}
function summary(job: JobRecord): ScriptSummary {
  return {
    jobId: job.jobId,
    status: job.status,
    scriptStarted: job.scriptStarted,
    actions: job.actions.length,
    startedAt: job.startedAt,
    ...(job.finishedAt ? { finishedAt: job.finishedAt } : {}),
    ...(job.error ? { error: job.error } : {}),
  };
}
function validate(request: ScriptRequest) {
  if (
    !request || typeof request.requestId !== "string" || !request.requestId ||
    request.requestId.length > 512 ||
    typeof request.code !== "string" || !request.code ||
    new TextEncoder().encode(request.code).length > 1_048_576 ||
    (request.mode !== undefined && !["async", "sync"].includes(request.mode)) ||
    (request.timeoutMs !== undefined &&
      (!Number.isSafeInteger(request.timeoutMs) || request.timeoutMs < 1))
  ) {
    throw new KernelError("rejected", "脚本需要真实调用编号、代码及有效等待/超时参数");
  }
}
export class ScriptRuntime {
  readonly store: ScriptStore;
  #live = new Map<string, LiveJob>();
  #starting = new Map<string, Promise<ScriptSummary>>();
  #closing = false;
  #retiring = new Set<Promise<void>>();
  constructor(readonly ctx: PluginContext, readonly config: ScriptConfig) {
    this.store = new ScriptStore(`${config.directory}/ledger`);
  }

  async initialize(): Promise<void> {
    await Deno.mkdir(this.config.directory, { recursive: true, mode: 0o700 });
    await Deno.mkdir(this.config.cacheDirectory, { recursive: true, mode: 0o700 });
    await this.store.initialize();
    try {
      const ids = new Set<string>();
      for (const job of this.store.state.jobs) {
        if (!job || typeof job !== "object") throw new KernelError("corrupt", "脚本任务结构损坏");
        validate(job.request);
        if (
          !/^[a-f0-9]{64}$/.test(job.jobId) || ids.has(job.jobId) ||
          job.jobId !== await fingerprint(["script-job-v1", job.request.requestId]) ||
          job.fingerprint !== await fingerprint(this.#identity(job.request)) ||
          !["queued", "checking", "running", ...TERMINAL].includes(job.status) ||
          typeof job.scriptStarted !== "boolean" || !Number.isFinite(Date.parse(job.startedAt)) ||
          !Array.isArray(job.actions) ||
          (TERMINAL.has(job.status) && !Number.isFinite(Date.parse(job.finishedAt ?? ""))) ||
          (job.error !== undefined && !validError(job.error)) ||
          (job.status === "completed" && !Object.hasOwn(job, "result"))
        ) throw new KernelError("corrupt", "脚本历史不能与原请求关联，原件保留");
        ids.add(job.jobId);
        if (Object.hasOwn(job, "result")) canonical(job.result!);
        for (const [index, action] of job.actions.entries()) {
          if (
            action.seq !== index + 1 ||
            action.actionId !== `script_${job.jobId}_${action.seq}` ||
            !["started", "completed", "failed", "unknown"].includes(action.status) ||
            typeof action.namespace !== "string" || typeof action.method !== "string" ||
            !Number.isFinite(Date.parse(action.startedAt)) ||
            (action.finishedAt !== undefined && !Number.isFinite(Date.parse(action.finishedAt))) ||
            (action.status === "completed" &&
              (!Object.hasOwn(action, "value") || action.error !== undefined)) ||
            ((action.status === "failed" || action.status === "unknown") &&
              !validError(action.error)) ||
            (action.status === "started" &&
              (action.error !== undefined || Object.hasOwn(action, "value")))
          ) {
            throw new KernelError("corrupt", "脚本动作记录损坏，原件保留");
          }
          canonical(action.args);
          if (Object.hasOwn(action, "value")) canonical(action.value!);
        }
      }
      let last = 0;
      const notified = new Set<string>();
      for (const event of this.store.state.completions) {
        if (
          event.seq !== ++last || !ids.has(event.job.jobId) || !TERMINAL.has(event.job.status) ||
          notified.has(event.job.jobId) ||
          canonical(event.job as unknown as Json) !==
            canonical(summary(this.store.job(event.job.jobId)) as unknown as Json)
        ) {
          throw new KernelError("corrupt", "脚本完成记录损坏，原件保留");
        }
        notified.add(event.job.jobId);
      }
      if (
        this.store.state.jobs.some((job) => TERMINAL.has(job.status) && !notified.has(job.jobId))
      ) {
        throw new KernelError("corrupt", "已结束脚本缺少真实完成记录，原件保留");
      }
      if (this.store.state.jobs.some((job) => !TERMINAL.has(job.status))) {
        await this.store.change((state) => {
          for (const job of state.jobs) {
            if (TERMINAL.has(job.status)) continue;
            const actual = job.scriptStarted || job.actions.some((a) => a.status === "started");
            job.status = actual ? "unknown" : "cancelled";
            job.finishedAt = new Date().toISOString();
            job.error = {
              kind: actual ? "unknown" : "unavailable",
              message: "宿主中断；原脚本不自动重跑，先查已有动作",
            };
            for (const action of job.actions) {
              if (action.status === "started") {
                action.status = "unknown";
                action.error = { kind: "unknown", message: "宿主中断，执行结果待核对" };
              }
            }
            state.completions.push({ seq: state.completions.length + 1, job: summary(job) });
          }
        });
      }
    } catch (error) {
      await this.store.close().catch(() => {});
      if (error instanceof KernelError && error.kind === "storage") throw error;
      throw new KernelError("corrupt", "脚本状态或原请求校验失败，原件保留");
    }
  }
  #identity(request: ScriptRequest): Json {
    return { code: request.code, timeoutMs: request.timeoutMs ?? this.config.defaultTimeoutMs };
  }
  #available() {
    if (this.#closing || !this.store.available) {
      throw new KernelError("unavailable", "脚本运行时不可用");
    }
  }

  async start(input: ScriptRequest): Promise<ScriptSummary> {
    this.#available();
    validate(input);
    const request = structuredClone({
      ...input,
      timeoutMs: input.timeoutMs ?? this.config.defaultTimeoutMs,
    });
    const jobId = await fingerprint(["script-job-v1", request.requestId]);
    const hash = await fingerprint(this.#identity(request));
    const starting = this.#starting.get(jobId);
    if (starting) {
      await starting;
      return this.start(request);
    }
    const operation = (async () => {
      const previous = this.store.findJob(jobId);
      if (previous) {
        if (previous.fingerprint !== hash) {
          throw new KernelError("rejected", "同一脚本调用编号不能换代码或执行参数");
        }
        if (request.mode === "sync") await this.#live.get(jobId)?.done;
        return summary(this.store.job(jobId));
      }
      await this.store.change((state) =>
        state.jobs.push({
          jobId,
          fingerprint: hash,
          request,
          status: "queued",
          scriptStarted: false,
          startedAt: new Date().toISOString(),
          actions: [],
        })
      );
      let resolve!: () => void;
      const done = new Promise<void>((finish) => {
        resolve = finish;
      });
      const live: LiveJob = { done, resolve, invocations: new Set(), nextSeq: 0, finishing: false };
      this.#live.set(jobId, live);
      const expire = () => {
        void this.#finish(jobId, "unknown", {
          kind: "timeout",
          message: "脚本执行期限到达；已开始的动作待核对，不重跑",
        }).catch(() => {});
      };
      const deadline = Date.now() + request.timeoutMs!;
      const schedule = () => {
        if (live.finishing) return;
        const remaining = deadline - Date.now();
        if (remaining <= 0) {
          expire();
          return;
        }
        live.timer = setTimeout(schedule, Math.min(remaining, 2_147_483_647));
      };
      schedule();
      live.runner = this.#run(jobId).catch((error) =>
        this.#finish(jobId, "failed", toWireError(error))
      )
        .catch(() => {});
      if (request.mode === "sync") await done;
      return summary(this.store.job(jobId));
    })();
    this.#starting.set(jobId, operation);
    try {
      return await operation;
    } finally {
      this.#starting.delete(jobId);
    }
  }

  async #run(jobId: string) {
    const live = this.#live.get(jobId)!;
    const record = this.store.job(jobId);
    await this.store.change((state) => {
      state.jobs[this.store.jobIndex(jobId)].status = "checking";
    });
    const apiUrl = new URL("../../contracts/script.ts", import.meta.url).href;
    const path = `${this.config.directory}/${jobId}.ts`;
    const code = `import type { ScriptApi } from ${
      JSON.stringify(apiUrl)
    };\nexport default async function run(tools: ScriptApi) {\n${record.request.code}\n}\n`;
    try {
      const stat = await Deno.lstat(path);
      if (!stat.isFile || stat.isSymlink || await Deno.readTextFile(path) !== code) {
        throw new KernelError("corrupt", "脚本原件与当前请求不匹配，保留已有源码");
      }
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
      await Deno.writeTextFile(path, code, { mode: 0o600, createNew: true });
    }
    if (live.finishing) return;
    const checker = new Deno.Command(this.config.denoBinary, {
      args: ["check", "--no-config", "--no-lock", "--no-remote", "--no-npm", path],
      clearEnv: true,
      env: { DENO_DIR: this.config.cacheDirectory },
      stdin: "null",
      stdout: "null",
      stderr: "piped",
    }).spawn();
    live.checker = checker;
    let diagnostic = "";
    const decoder = new TextDecoder();
    for await (const chunk of checker.stderr) {
      if (diagnostic.length < 16_384) {
        diagnostic += decoder.decode(chunk, { stream: true }).slice(0, 16_384 - diagnostic.length);
      }
    }
    const checked = await checker.status;
    live.checker = undefined;
    if (live.finishing) return;
    if (!checked.success) {
      await this.#finish(jobId, "failed", {
        kind: "rejected",
        message: `脚本类型检查未通过，未开始执行：${diagnostic}`,
      });
      return;
    }
    if (await Deno.readTextFile(path) !== code) {
      throw new KernelError("corrupt", "类型检查期间脚本原件发生变化，未执行");
    }
    await this.store.change((state) => {
      const job = state.jobs[this.store.jobIndex(jobId)];
      job.status = "running";
      job.scriptStarted = true;
    });
    if (live.finishing) return;
    const worker = new Worker(
      new URL("./worker.ts", import.meta.url),
      {
        type: "module",
        name: `eve-script-${jobId.slice(0, 12)}`,
        deno: {
          permissions: {
            read: [
              path,
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
      } as unknown as WorkerOptions,
    );
    live.worker = worker;
    worker.onmessage = (event: MessageEvent<FromScript>) => {
      const message = event.data;
      if (live.finishing) return;
      if (message.t === "finished") {
        void this.#finish(jobId, "completed", undefined, message.value).catch(() => {});
      } else if (message.t === "failed") {
        const unknown = message.error.kind === "unknown" || message.error.kind === "timeout";
        void this.#finish(jobId, unknown ? "unknown" : "failed", message.error).catch(() => {});
      } else if (message.t === "invoke") {
        const task = this.#invoke(jobId, message);
        live.invocations.add(task);
        task.finally(() => live.invocations.delete(task)).catch(() => {});
        task.then((value) => {
          if (!live.finishing) {
            worker.postMessage({ t: "reply", seq: message.seq, ok: true, value });
          }
        }, (error) => {
          if (!live.finishing) {
            worker.postMessage({
              t: "reply",
              seq: message.seq,
              ok: false,
              error: toWireError(error),
            });
          }
        });
      }
    };
    worker.onerror = (event) => {
      event.preventDefault();
      void this.#finish(jobId, "failed", {
        kind: "remote",
        message: `脚本worker失败：${event.message}`,
      }).catch(() => {});
    };
    // 执行请求保存的不可变源码，而不是再次读取可能被并发改写的同一路径。
    worker.postMessage({
      t: "start",
      moduleUrl: `data:application/typescript,${encodeURIComponent(code)}`,
    });
  }

  async #invoke(jobId: string, message: Extract<FromScript, { t: "invoke" }>): Promise<Json> {
    const live = this.#live.get(jobId)!;
    if (message.seq !== live.nextSeq + 1 || !Number.isSafeInteger(message.seq)) {
      throw new KernelError("rejected", "脚本动作编号必须连续");
    }
    if (
      !(message.namespace === "phone" && PHONE_METHODS.includes(message.method as never)) &&
      !(message.namespace === "upgrades" && UPGRADE_METHODS.includes(message.method as never)) &&
      !(["notes", "files", "commands", "memory", "records"].includes(message.namespace) &&
        (RUNTIME_METHODS[message.namespace as "notes"] as readonly string[]).includes(
          message.method,
        )) &&
      !(message.namespace === "scripts" &&
        ["status", "cancel", "result", "receipt"].includes(message.method)) &&
      !(message.namespace === "mind" &&
        ["rest", "remind", "watch", "cancelReminder", "compress", "retain", "status"].includes(
          message.method,
        ))
    ) throw new KernelError("rejected", "该能力不在当前契约中");
    canonical(message.args);
    const actionId = `script_${jobId}_${message.seq}`;
    const args = structuredClone(message.args);
    if (
      ["notes", "files", "commands", "memory", "records", "upgrades"].includes(message.namespace)
    ) {
      if (
        args === null || typeof args !== "object" || Array.isArray(args) ||
        Object.hasOwn(args, "_actionId")
      ) throw new KernelError("rejected", "工具动作编号由脚本宿主提供");
      args._actionId = actionId;
    }
    if (message.namespace === "phone" && ["send", "sendFile"].includes(message.method)) {
      if (
        args === null || typeof args !== "object" || Array.isArray(args) ||
        Object.hasOwn(args, "actionId")
      ) throw new KernelError("rejected", "手机发送编号由脚本宿主提供");
      args.actionId = actionId;
    }
    if (message.namespace === "mind" && message.method !== "status") {
      if (
        args === null || typeof args !== "object" || Array.isArray(args) ||
        Object.hasOwn(args, "requestId")
      ) throw new KernelError("rejected", "意识请求编号由宿主提供");
      args.requestId = actionId;
    }
    live.nextSeq++;
    await this.store.change((state) =>
      state.jobs[this.store.jobIndex(jobId)].actions.push({
        seq: message.seq,
        actionId,
        namespace: message.namespace,
        method: message.method,
        args,
        status: "started",
        startedAt: new Date().toISOString(),
      })
    );
    try {
      if (live.finishing) throw new KernelError("unavailable", "脚本已取消，此动作未开始转发");
      const value = message.namespace === "scripts"
        ? await this[message.method as "status" | "cancel" | "result" | "receipt"](
          args as never,
        ) as unknown as Json
        : await this.ctx.call<Json>(message.namespace, message.method, args, {
          idempotencyKey: actionId,
          timeoutMs: this.config.defaultTimeoutMs,
        });
      canonical(value);
      await this.store.change((state) => {
        const action = state.jobs[this.store.jobIndex(jobId)].actions[message.seq - 1];
        action.status = "completed";
        delete action.error;
        action.value = value;
        action.finishedAt = new Date().toISOString();
      });
      return value;
    } catch (error) {
      const wire = toWireError(error);
      await this.store.change((state) => {
        const action = state.jobs[this.store.jobIndex(jobId)].actions[message.seq - 1];
        action.status =
          wire.kind === "timeout" || wire.kind === "unknown" || wire.kind === "storage"
            ? "unknown"
            : "failed";
        action.error = wire;
        action.finishedAt = new Date().toISOString();
      });
      throw error;
    }
  }

  async #finish(jobId: string, status: JobRecord["status"], error?: WireError, result?: Json) {
    const live = this.#live.get(jobId);
    if (!live || live.finishing) return;
    live.finishing = true;
    clearTimeout(live.timer);
    live.worker?.terminate();
    try {
      live.checker?.kill("SIGKILL");
    } catch { /* 已停 */ }
    try {
      await this.store.change((state) => {
        const job = state.jobs[this.store.jobIndex(jobId)];
        const pending = job.actions.some((action) =>
          action.status === "started" || action.status === "unknown"
        );
        job.status = pending ? "unknown" : status;
        if (!job.scriptStarted && status === "unknown") job.status = "cancelled";
        job.finishedAt = new Date().toISOString();
        if (error) job.error = error;
        if (result !== undefined) job.result = result;
        for (const action of job.actions) {
          if (action.status === "started") {
            action.status = "unknown";
            action.error = { kind: "unknown", message: "脚本已结束等待，动作执行结果仍待核对" };
          }
        }
        state.completions.push({ seq: state.completions.length + 1, job: summary(job) });
      });
    } finally {
      live.resolve();
      const retiring = (async () => {
        await live.runner;
        await Promise.allSettled([...live.invocations]);
        if (this.#live.get(jobId) === live) this.#live.delete(jobId);
      })();
      this.#retiring.add(retiring);
      retiring.finally(() => this.#retiring.delete(retiring)).catch(() => {});
    }
  }
  status(args: { jobId: string }): ScriptSummary {
    return summary(this.store.job(args.jobId));
  }
  async cancel(args: { jobId: string }): Promise<ScriptSummary> {
    this.store.job(args.jobId);
    const live = this.#live.get(args.jobId);
    if (live?.finishing) {
      await live.done;
      return this.status(args);
    }
    await this.#finish(args.jobId, "cancelled", {
      kind: "unknown",
      message: "脚本取消，已开始的具体动作不重放",
    });
    return this.status(args);
  }
  result(args: { jobId: string; startChar?: number; limit?: number }) {
    const job = this.store.job(args.jobId);
    return this.#page(JSON.stringify(imageTextView(job.result ?? null)), args);
  }
  /** 宿主专用：只采用真实 phone.image 完成回执，一次最多返回一张。 */
  observations(args: { jobId: string; afterSeq?: number }) {
    const job = this.store.job(args.jobId);
    const after = args.afterSeq ?? 0;
    if (!Number.isSafeInteger(after) || after < 0 || after > job.actions.length) {
      throw new KernelError("rejected", "图像描述回执游标无效");
    }
    const action = job.actions.slice(after).find((a) =>
      a.namespace === "phone" &&
      a.method === "image" && a.status === "completed" && a.value !== null &&
      typeof a.value === "object" && !Array.isArray(a.value) &&
      typeof a.value.description === "string" && a.value.provenance
    );
    return {
      items: action ? [{ actionId: action.actionId, observation: action.value }] : [],
      throughSeq: action?.seq ?? job.actions.length,
      hasMore: action ? action.seq < job.actions.length : false,
    };
  }
  /** 只有脚本明确 mode:original，且手机真实回执确认该选择时才提供原图。 */
  images(args: { jobId: string; afterSeq?: number }) {
    const job = this.store.job(args.jobId), after = args.afterSeq ?? 0;
    if (!Number.isSafeInteger(after) || after < 0 || after > job.actions.length) {
      throw new KernelError("rejected", "原图回执游标无效");
    }
    const action = job.actions.slice(after).find((a) =>
      a.namespace === "phone" && a.method === "image" && a.status === "completed" &&
      a.value !== null && typeof a.value === "object" && !Array.isArray(a.value) &&
      a.value.selectedMode === "original" && a.value.image
    );
    return {
      items: action
        ? [{ actionId: action.actionId, image: (action.value as { image: Json }).image }]
        : [],
      throughSeq: action?.seq ?? job.actions.length,
      hasMore: action ? action.seq < job.actions.length : false,
    };
  }
  #page(text: string, args: { startChar?: number; limit?: number }) {
    const start = args.startChar ?? 0;
    const limit = args.limit ?? 1024;
    if (
      !Number.isSafeInteger(start) || start < 0 || !Number.isSafeInteger(limit) || limit < 1 ||
      limit > 8192
    ) throw new KernelError("rejected", "结果页参数无效");
    const chars = Array.from(text);
    const page = chars.slice(start, start + limit).join("");
    const next = start + Array.from(page).length;
    return {
      text: page,
      characters: chars.length,
      truncated: next < chars.length,
      ...(next < chars.length ? { nextChar: next } : {}),
    };
  }
  receipt(
    args: {
      jobId: string;
      seq: number;
      section: "args" | "result" | "error";
      startChar?: number;
      limit?: number;
    },
  ) {
    const job = this.store.job(args.jobId);
    if (
      !Number.isSafeInteger(args.seq) || args.seq < 1 || args.seq > job.actions.length ||
      !["args", "result", "error"].includes(args.section)
    ) {
      throw new KernelError("rejected", "动作结果页参数无效");
    }
    const action = job.actions[args.seq - 1];
    const value = args.section === "args"
      ? action.args
      : args.section === "result"
      ? action.value
      : action.error;
    return this.#page(JSON.stringify(imageTextView((value ?? null) as Json)), args);
  }
  completions(args: { afterSeq?: number } = {}) {
    const after = args.afterSeq ?? 0;
    if (!Number.isSafeInteger(after) || after < 0 || after > this.store.state.completions.length) {
      throw new KernelError("rejected", "完成通知游标无效");
    }
    const items = this.store.state.completions.slice(after, after + 8);
    return {
      items: structuredClone(items),
      throughSeq: items.at(-1)?.seq ?? after,
      hasMore: after + items.length < this.store.state.completions.length,
    };
  }
  receipts(args: { jobId: string; afterSeq?: number }) {
    const job = this.store.job(args.jobId);
    const after = args.afterSeq ?? 0;
    if (!Number.isSafeInteger(after) || after < 0 || after > job.actions.length) {
      throw new KernelError("rejected", "动作游标无效");
    }
    const items = job.actions.slice(after, after + 8).map((a) => ({
      seq: a.seq,
      actionId: a.actionId,
      namespace: a.namespace,
      method: a.method,
      status: a.status,
      ...(a.error ? { error: a.error } : {}),
    }));
    return {
      items,
      throughSeq: items.at(-1)?.seq ?? after,
      hasMore: after + items.length < job.actions.length,
    };
  }
  async close() {
    this.#closing = true;
    try {
      for (const [id, live] of this.#live) if (!live.finishing) await this.cancel({ jobId: id });
      await Promise.allSettled(
        [...this.#live.values()].flatMap((live) => [live.done, live.runner, ...live.invocations]),
      );
      await Promise.allSettled([...this.#retiring]);
      await this.store.settle();
    } finally {
      await this.store.close();
    }
  }
  get healthy() {
    return !this.#closing && this.store.available;
  }
}
