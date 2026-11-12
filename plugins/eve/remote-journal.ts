import { type FactEvent, FactJournal } from "../../sdk/fact-journal.ts";
import type { PluginContext } from "../../sdk/plugin.ts";
import { canonical, fingerprint } from "../../kernel/calls.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";

/** 稳定出口持有原件；本实例仅保存经校验的镜像，正文按需从磁盘读取。 */
export class RemoteJournal {
  #mirror: FactJournal;
  #available = false;
  #tail: Promise<unknown> = Promise.resolve();
  #closing = false;
  #initializing = false;
  #initializationFinished?: Promise<void>;
  constructor(readonly ctx: PluginContext, directory: string) {
    this.#mirror = new FactJournal(`${directory}/remote-mirror`);
  }
  get events() {
    return this.#mirror.events;
  }
  get available() {
    return this.#available && this.#mirror.available;
  }
  async initialize() {
    if (this.#initializing || this.#closing || this.#available) {
      throw new KernelError("unavailable", "远程意识流已经初始化或关闭");
    }
    this.#initializing = true;
    let finished!: () => void;
    this.#initializationFinished = new Promise<void>((resolve) => {
      finished = resolve;
    });
    try {
      await this.#mirror.initialize();
      const cachedThrough = this.#mirror.count;
      // 重读镜像尾锚，防止远端截断、落后或历史分叉；其余只补缺失的增量。
      let afterSeq = Math.max(0, cachedThrough - 1);
      do {
        const page = await this.ctx.call("timeline", "page", { afterSeq }) as unknown as {
          items: FactEvent[];
          throughSeq: number;
          hasMore: boolean;
        };
        if (
          !page || !Array.isArray(page.items) || page.items.length > 128 ||
          typeof page.hasMore !== "boolean" || !Number.isSafeInteger(page.throughSeq)
        ) {
          throw new KernelError("corrupt", "远程意识记录分页无效");
        }
        for (const event of page.items) {
          if (event.seq !== afterSeq + 1) {
            throw new KernelError("corrupt", "远程意识记录序号不连续");
          }
          await this.#mirror.importOriginal(event);
          afterSeq = event.seq;
        }
        if (page.throughSeq !== afterSeq || afterSeq < cachedThrough) {
          throw new KernelError("corrupt", "远程事实水位落后或与本地镜像不符");
        }
        if (!page.hasMore) break;
        if (!page.items.length) throw new KernelError("corrupt", "远程记录游标没有推进");
      } while (true);
      this.#available = true;
    } catch (error) {
      this.#available = false;
      await this.#mirror.close().catch(() => {});
      throw error;
    } finally {
      this.#initializing = false;
      finished();
    }
  }
  async append(kind: string, data: Json): Promise<FactEvent> {
    if (this.#closing) throw new KernelError("storage", "远程意识流已关闭");
    const captured = structuredClone(data);
    const work = this.#tail.then(async () => {
      if (!this.available) throw new KernelError("storage", "远程意识流不可提交");
      const previous = this.events.at(-1)?.digest ?? "0".repeat(64),
        expectedSeq = this.events.length;
      const _actionId = await fingerprint(["conscious-event-v1", previous, kind, captured]);
      try {
        const event = await this.ctx.call("timeline", "append", {
          expectedSeq,
          expectedPrevious: previous,
          kind,
          data: captured,
          _actionId,
        }) as unknown as FactEvent;
        if (
          !event || event.seq !== expectedSeq + 1 || event.previous !== previous ||
          event.kind !== kind || canonical(event.data) !== canonical(captured)
        ) {
          throw new KernelError("corrupt", "意识流提交回执不符");
        }
        // 只有远端提交确实成功后才导入镜像；失败和未知不能冒充已保存。
        return await this.#mirror.importOriginal(event);
      } catch (error) {
        this.#available = false;
        throw error;
      }
    });
    this.#tail = work.catch(() => {});
    return await work;
  }
  async close() {
    this.#closing = true;
    await this.#initializationFinished;
    await this.#tail;
    this.#available = false;
    await this.#mirror.close();
  }
}
