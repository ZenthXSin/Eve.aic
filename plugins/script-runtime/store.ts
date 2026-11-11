// 独立的事实账本，不参与插件快照回滚；每个脚本与嵌套动作独立持久提交。
import type { Json, WireError } from "../../kernel/protocol.ts";
import { KernelError } from "../../kernel/protocol.ts";
import type { ScriptCompletion, ScriptRequest, ScriptStatus } from "../../contracts/script.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";

export interface ActionRecord {
  seq: number;
  actionId: string;
  namespace: string;
  method: string;
  args: Json;
  status: "started" | "completed" | "failed" | "unknown";
  startedAt: string;
  finishedAt?: string;
  value?: Json;
  error?: WireError;
}
export interface JobRecord {
  jobId: string;
  fingerprint: string;
  request: ScriptRequest;
  status: ScriptStatus;
  scriptStarted: boolean;
  startedAt: string;
  finishedAt?: string;
  error?: WireError;
  result?: Json;
  actions: ActionRecord[];
}
export interface ScriptState {
  schema: 1;
  jobs: JobRecord[];
  completions: ScriptCompletion[];
}

export class ScriptStore {
  state: ScriptState = { schema: 1, jobs: [], completions: [] };
  #failed = false;
  #tail: Promise<unknown> = Promise.resolve();
  #lock?: Deno.FsFile;
  #closing = false;
  constructor(readonly directory: string) {}
  async initialize(): Promise<void> {
    if (this.#lock || this.#failed || this.#closing) {
      throw new KernelError("unavailable", "脚本账本不可重复持有");
    }
    let lock: Deno.FsFile | undefined;
    try {
      await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
      lock = await Deno.open(`${this.directory}/writer.lock`, {
        create: true,
        read: true,
        write: true,
        mode: 0o600,
      });
      if (!(await lock.tryLock(true))) {
        throw new KernelError("unavailable", "已有脚本运行时持有账本");
      }
      for await (const file of Deno.readDir(this.directory)) {
        if (!file.isFile || !["writer.lock", "index.json"].includes(file.name)) {
          throw new KernelError("corrupt", "脚本账本有未完成或未知文件，原件保留");
        }
      }
      let saved: unknown;
      try {
        const path = `${this.directory}/index.json`;
        const stat = await Deno.lstat(path);
        if (!stat.isFile || stat.isSymlink || stat.size > 134_217_728) {
          throw new KernelError("corrupt", "脚本账本原件损坏或过大，原件保留");
        }
        saved = JSON.parse(await Deno.readTextFile(path));
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      if (saved !== undefined) {
        const value = saved as ScriptState;
        if (
          !saved || typeof saved !== "object" || Array.isArray(saved) || value.schema !== 1 ||
          !Array.isArray(value.jobs) || !Array.isArray(value.completions)
        ) {
          throw new KernelError("corrupt", "脚本状态结构损坏，原件保留");
        }
        this.state = structuredClone(saved) as unknown as ScriptState;
      }
      this.#lock = lock;
    } catch (error) {
      lock?.close();
      this.#failed = true;
      if (error instanceof KernelError) throw error;
      throw new KernelError("corrupt", "脚本账本读取失败，原件保留");
    }
  }
  get available() {
    return this.#lock !== undefined && !this.#failed && !this.#closing;
  }
  async change(edit: (state: ScriptState) => void): Promise<void> {
    const operation = this.#tail.then(async () => {
      if (!this.available) throw new KernelError("storage", "脚本状态未保存，原件保留");
      const next = structuredClone(this.state);
      edit(next);
      try {
        await writeDurableJson(`${this.directory}/index.json`, next);
      } catch {
        this.#failed = true;
        throw new KernelError("storage", "脚本状态未保存，原件保留");
      }
      this.state = next;
    });
    this.#tail = operation.catch(() => {});
    await operation;
  }
  async settle() {
    await this.#tail;
    if (this.#failed) throw new KernelError("storage", "脚本状态提交失败");
  }
  async close() {
    await this.#tail;
    this.#closing = true;
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failed) throw new KernelError("storage", "脚本账本关闭时有存盘失败");
  }
  job(jobId: string): JobRecord {
    if (!this.available) throw new KernelError("storage", "脚本状态不可用");
    const result = this.state.jobs.find((j) => j.jobId === jobId);
    if (!result) throw new KernelError("not_found", "没有该脚本任务");
    return result;
  }
}
