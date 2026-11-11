// 手机自己的通知水位和最近一次打开的屏幕；不改平台原件，也不声称对方收到已读回执。
import type { PhoneReadingState } from "../../contracts/phone.ts";
import { KernelError } from "../../kernel/protocol.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";

interface Entry extends PhoneReadingState {
  conversationId: string;
}
const LIMIT = 4_194_304;
function sequence(value: unknown, min = 0): value is number {
  return Number.isSafeInteger(value) && Number(value) >= min;
}
function valid(entry: Entry): boolean {
  return !!entry && typeof entry.conversationId === "string" &&
    /^qq:(user|group):.{1,512}$/.test(entry.conversationId) &&
    sequence(entry.notificationThroughSeq, 1) && !!entry.lastOpened &&
    sequence(entry.lastOpened.snapshotThroughSeq, entry.notificationThroughSeq) &&
    Array.isArray(entry.lastOpened.displayedSeqs) &&
    entry.lastOpened.displayedSeqs.length >= 1 && entry.lastOpened.displayedSeqs.length <= 8 &&
    entry.lastOpened.displayedSeqs.every((seq, index, all) =>
      sequence(seq, 1) && (index === 0 || seq > all[index - 1]) &&
      seq <= entry.notificationThroughSeq
    ) && entry.lastOpened.displayedSeqs.at(-1) === entry.notificationThroughSeq;
}

export class QqPhoneReading {
  #entries = new Map<string, Entry>();
  #lock?: Deno.FsFile;
  #initialized = false;
  #failed = false;
  #closing = false;
  #validated = false;
  #tail: Promise<unknown> = Promise.resolve();
  constructor(readonly directory: string) {}

  get available() {
    return !!this.#lock && !this.#failed && !this.#closing;
  }

  async initialize(): Promise<void> {
    if (this.#initialized) throw new KernelError("unavailable", "手机阅读状态已经启动或关闭");
    this.#initialized = true;
    let lock: Deno.FsFile | undefined;
    try {
      await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
      if ((await Deno.lstat(this.directory)).isSymlink) throw new Error();
      for await (const file of Deno.readDir(this.directory)) {
        if (!file.isFile || !["writer.lock", "index.json"].includes(file.name)) throw new Error();
      }
      lock = await Deno.open(`${this.directory}/writer.lock`, {
        create: true,
        read: true,
        write: true,
        mode: 0o600,
      });
      if (!(await lock.tryLock(true))) {
        throw new KernelError("unavailable", "手机阅读状态已有写入者");
      }
      const path = `${this.directory}/index.json`;
      try {
        const stat = await Deno.lstat(path);
        if (!stat.isFile || stat.isSymlink || stat.size > LIMIT) throw new Error();
        const state = JSON.parse(await Deno.readTextFile(path));
        if (state?.schema !== 1 || !Array.isArray(state.entries)) throw new Error();
        for (const entry of state.entries) {
          if (!valid(entry) || this.#entries.has(entry.conversationId)) throw new Error();
          this.#entries.set(entry.conversationId, entry);
        }
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      this.#lock = lock;
    } catch (error) {
      lock?.close();
      this.#failed = true;
      if (error instanceof KernelError) throw error;
      throw new KernelError("corrupt", "手机阅读状态异常，原件保留");
    }
  }

  get(conversationId: string): PhoneReadingState | null {
    if (!this.available) throw new KernelError("unavailable", "手机阅读状态不可用");
    const entry = this.#entries.get(conversationId);
    return entry
      ? structuredClone({
        notificationThroughSeq: entry.notificationThroughSeq,
        lastOpened: entry.lastOpened,
      })
      : null;
  }

  /** 与事实编号交叉检查，拒绝索引回退或将别的会话误作本屏。 */
  validateSnapshot(rows: readonly { message: { seq: number; conversationId: string } }[]): void {
    if (!this.available) throw new KernelError("unavailable", "手机阅读状态不可用");
    // 只核对启动恢复的数据；运行中的更新已由本机快照产生。并行查询可能持有更早的快照。
    if (this.#validated) return;
    const ids = new Map(rows.map(({ message }) => [message.seq, message.conversationId]));
    for (const entry of this.#entries.values()) {
      if (
        entry.lastOpened.snapshotThroughSeq > (rows.at(-1)?.message.seq ?? 0) ||
        entry.lastOpened.displayedSeqs.some((seq) => ids.get(seq) !== entry.conversationId)
      ) {
        this.#failed = true;
        throw new KernelError("corrupt", "手机阅读水位与事实不符，原件保留");
      }
    }
    this.#validated = true;
  }

  async opened(
    conversationId: string,
    snapshotThroughSeq: number,
    displayedSeqs: number[],
  ): Promise<PhoneReadingState> {
    if (!this.available) throw new KernelError("unavailable", "手机阅读状态不可用");
    const captured: Entry = {
      conversationId,
      notificationThroughSeq: displayedSeqs.at(-1) ?? 0,
      lastOpened: { snapshotThroughSeq, displayedSeqs: [...displayedSeqs] },
    };
    if (!valid(captured)) throw new KernelError("rejected", "手机屏幕水位无效");
    const operation = this.#tail.then(async () => {
      if (this.#failed) throw new KernelError("storage", "手机阅读状态无法提交");
      const previous = this.#entries.get(conversationId);
      // 迟到的旧快照不得覆盖已经打开的新屏幕或使通知水位倒退。
      if (
        previous && (previous.notificationThroughSeq > captured.notificationThroughSeq ||
          previous.lastOpened.snapshotThroughSeq > snapshotThroughSeq)
      ) return;
      if (previous && JSON.stringify(previous) === JSON.stringify(captured)) return;
      const entries = new Map(this.#entries);
      entries.set(conversationId, captured);
      const state = { schema: 1, entries: [...entries.values()] };
      try {
        if (new TextEncoder().encode(JSON.stringify(state) + "\n").length > LIMIT) {
          throw new Error();
        }
        await writeDurableJson(`${this.directory}/index.json`, state);
      } catch {
        this.#failed = true;
        throw new KernelError("storage", "手机阅读状态保存失败，原件保留");
      }
      this.#entries = entries;
    });
    this.#tail = operation.catch(() => {});
    await operation;
    // close 可能正在等待本轮提交，返回已完成的事实而不启动新的读取。
    const entry = this.#entries.get(conversationId)!;
    return structuredClone({
      notificationThroughSeq: entry.notificationThroughSeq,
      lastOpened: entry.lastOpened,
    });
  }

  async close(): Promise<void> {
    this.#closing = true;
    await this.#tail;
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failed) throw new KernelError("storage", "手机阅读状态停止失败，原件保留");
  }
}
