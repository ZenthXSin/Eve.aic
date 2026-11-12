import type { InboundRecord } from "./journal.ts";

/** 入站先持久化，再串行下载；网络失败不阻塞网关/心跳，也不发送聊天消息。 */
export class QqAttachmentPrefetch {
  #pending = new Map<number, InboundRecord>();
  #seen = new Set<number>();
  #active?: Promise<void>;
  #started = false;
  #closed = false;
  constructor(readonly download: (record: InboundRecord) => Promise<void>) {}

  recover(records: InboundRecord[], now = Date.now()) {
    // 启动恢复近期未完成项；不扫取全部历史的过期平台链接。
    for (const record of records.filter((r) => Date.parse(r.receivedAt) >= now - 86_400_000)) {
      this.offer(record);
    }
  }
  offer(record: InboundRecord) {
    if (this.#closed || this.#seen.has(record.seq) || !record.message.message.attachments?.length) {
      return;
    }
    this.#seen.add(record.seq);
    this.#pending.set(record.seq, record);
    this.#drain();
  }
  start() {
    this.#started = true;
    this.#drain();
  }
  #drain() {
    if (!this.#started || this.#closed || this.#active) return;
    this.#active = (async () => {
      while (!this.#closed && this.#pending.size) {
        const [key, record] = this.#pending.entries().next().value!;
        this.#pending.delete(key);
        await this.download(record).catch(() => {});
      }
    })().finally(() => {
      this.#active = undefined;
      if (!this.#closed && this.#pending.size) this.#drain();
    });
  }
  async close() {
    this.#closed = true;
    this.#pending.clear();
    await this.#active;
  }
}
