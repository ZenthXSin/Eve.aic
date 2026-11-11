// 插件状态的持久化、快照与回执。
// 语义：写入原子（临时文件再改名）；读到损坏的数据时报错并保留原文件，绝不自动清空。

import { type Json, KernelError } from "./protocol.ts";

export interface StateRecord {
  schema: 1;
  plugin: string;
  /** 写下这份状态的插件版本。 */
  version: string;
  stateVersion: number;
  updatedAt: string;
  data: Json;
}

function isRecord(value: unknown): value is StateRecord {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  return record.schema === 1 && typeof record.plugin === "string" &&
    typeof record.version === "string" && typeof record.stateVersion === "number" &&
    "data" in record;
}

async function exists(path: string): Promise<boolean> {
  try {
    await Deno.stat(path);
    return true;
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) return false;
    throw error;
  }
}

async function writeAtomic(path: string, text: string): Promise<void> {
  const temp = `${path}.tmp-${crypto.randomUUID()}`;
  await Deno.writeTextFile(temp, text);
  await Deno.rename(temp, path);
}

export class Store {
  #queues = new Map<string, Promise<void>>();

  constructor(readonly dir: string) {}

  async init(): Promise<void> {
    for (const sub of ["state", "snapshots", "failed", "receipts"]) {
      await Deno.mkdir(`${this.dir}/${sub}`, { recursive: true });
    }
  }

  statePath(plugin: string): string {
    return `${this.dir}/state/${plugin}.json`;
  }

  /** 读取插件状态。没有文件返回 undefined；文件损坏时抛出 corrupt 错误，原文件不动。 */
  async load(plugin: string): Promise<StateRecord | undefined> {
    const path = this.statePath(plugin);
    let text: string;
    try {
      text = await Deno.readTextFile(path);
    } catch (error) {
      if (error instanceof Deno.errors.NotFound) return undefined;
      throw error;
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch (error) {
      throw new KernelError(
        "corrupt",
        `${plugin} 的状态文件损坏，已保留原样：${path}（${(error as Error).message}）`,
      );
    }
    if (!isRecord(parsed) || parsed.plugin !== plugin) {
      throw new KernelError("corrupt", `${plugin} 的状态文件格式不对，已保留原样：${path}`);
    }
    return parsed;
  }

  /** 保存插件状态；同一插件的写入按顺序排队。 */
  save(record: StateRecord): Promise<void> {
    const path = this.statePath(record.plugin);
    const previous = this.#queues.get(record.plugin) ?? Promise.resolve();
    const next = previous.catch(() => {}).then(() =>
      writeAtomic(path, JSON.stringify(record, null, 2))
    );
    this.#queues.set(record.plugin, next);
    return next;
  }

  /** 等这个插件排队中的写入全部落盘。 */
  async settle(plugin: string): Promise<void> {
    await (this.#queues.get(plugin) ?? Promise.resolve()).catch(() => {});
  }

  /** 换版前给当前状态拍快照，返回快照文件路径与内容。没有状态时内容为 undefined。 */
  async snapshot(plugin: string): Promise<{ path: string; record: StateRecord | undefined }> {
    await this.settle(plugin);
    const record = await this.load(plugin);
    const dir = `${this.dir}/snapshots/${plugin}`;
    await Deno.mkdir(dir, { recursive: true });
    const path = `${dir}/${Date.now()}-${crypto.randomUUID().slice(0, 8)}.json`;
    await writeAtomic(path, JSON.stringify({ record: record ?? null }, null, 2));
    return { path, record };
  }

  /** 把失败版本写下的状态另存一份供查看，不丢弃。返回保存的路径；没有状态则返回 undefined。 */
  async keepFailed(plugin: string, version: string): Promise<string | undefined> {
    await this.settle(plugin);
    const path = this.statePath(plugin);
    if (!(await exists(path))) return undefined;
    const dir = `${this.dir}/failed/${plugin}`;
    await Deno.mkdir(dir, { recursive: true });
    const target = `${dir}/${Date.now()}-${version}.json`;
    await Deno.copyFile(path, target);
    return target;
  }

  /** 回滚：把状态恢复成快照时的内容。调用前应先 keepFailed。 */
  async restore(plugin: string, record: StateRecord | undefined): Promise<void> {
    await this.settle(plugin);
    const path = this.statePath(plugin);
    if (record) {
      await writeAtomic(path, JSON.stringify(record, null, 2));
    } else if (await exists(path)) {
      await Deno.remove(path);
    }
  }

  /** 追加一条回执（每行一个 JSON）。 */
  async appendReceipt(name: string, receipt: unknown): Promise<void> {
    const path = `${this.dir}/receipts/${name}.jsonl`;
    await Deno.writeTextFile(path, JSON.stringify(receipt) + "\n", { append: true });
  }
}
