// 手机设备自己的设置，保存在独立设备目录；不改变平台身份、入站日志或发送回执。
import type { PhoneConversationSettings } from "../../contracts/phone.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";

export class QqPhoneDevice {
  #settings = new Map<string, PhoneConversationSettings>();
  #tail: Promise<unknown> = Promise.resolve();
  readonly journal: FactJournal;
  constructor(directory: string) {
    this.journal = new FactJournal(directory);
  }
  get available() {
    return this.journal.available;
  }
  async initialize() {
    await this.journal.initialize();
    try {
      for (const event of this.journal.events) {
        const value = event.data as unknown as {
          conversationId: string;
          settings: PhoneConversationSettings;
        };
        if (
          event.kind !== "settings" || !value ||
          !/^qq:(user|group):.{1,512}$/.test(value.conversationId) ||
          typeof value.settings?.muted !== "boolean" ||
          typeof value.settings.pinned !== "boolean" || typeof value.settings.draft !== "string"
        ) {
          throw new KernelError("corrupt", "手机设置异常，原件保留");
        }
        this.#settings.set(value.conversationId, value.settings);
      }
    } catch (error) {
      await this.journal.close();
      throw error;
    }
  }
  get(conversationId: string): PhoneConversationSettings {
    if (!this.available) throw new KernelError("unavailable", "手机设置不可用");
    return structuredClone(
      this.#settings.get(conversationId) ?? { muted: false, pinned: false, draft: "" },
    );
  }
  async update(conversationId: string, patch: Partial<PhoneConversationSettings>) {
    const captured = structuredClone(patch);
    const operation = this.#tail.then(async () => {
      const settings = { ...this.get(conversationId), ...captured };
      await this.journal.append("settings", { conversationId, settings } as unknown as Json);
      this.#settings.set(conversationId, settings);
      return structuredClone(settings);
    });
    this.#tail = operation.catch(() => {});
    return await operation;
  }
  async close() {
    await this.#tail;
    await this.journal.close();
  }
}
