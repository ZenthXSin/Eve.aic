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
          typeof value.settings.pinned !== "boolean" || typeof value.settings.draft !== "string" ||
          value.settings.remark !== undefined &&
            (typeof value.settings.remark !== "string" ||
              Array.from(value.settings.remark).length > 128) ||
          value.settings.alias !== undefined && !conversationAlias(value.settings.alias)
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
  resolve(reference: string): string {
    if (!this.available) throw new KernelError("unavailable", "手机设置不可用");
    if (/^qq:(user|group):.{1,512}$/.test(reference)) return reference;
    if (!conversationAlias(reference)) throw new KernelError("rejected", "会话短名无效");
    const matches = [...this.#settings].filter(([, settings]) => settings.alias === reference);
    if (matches.length !== 1) {
      throw new KernelError("rejected", "会话短名不存在或不唯一，请使用原始会话编号");
    }
    return matches[0][0];
  }
  get(conversationId: string): PhoneConversationSettings {
    if (!this.available) throw new KernelError("unavailable", "手机设置不可用");
    return structuredClone(
      this.#settings.get(conversationId) ?? { muted: false, pinned: false, draft: "" },
    );
  }
  async update(
    conversationId: string,
    patch: Omit<Partial<PhoneConversationSettings>, "alias"> & { alias?: string | null },
  ) {
    const captured = structuredClone(patch);
    const operation = this.#tail.then(async () => {
      if (
        captured.alias !== undefined && captured.alias !== null &&
        (!conversationAlias(captured.alias) ||
          [...this.#settings].some(([id, s]) =>
            id !== conversationId && s.alias === captured.alias
          ))
      ) {
        throw new KernelError("rejected", "会话短名无效或已被占用");
      }
      const settings = { ...this.get(conversationId), ...captured } as PhoneConversationSettings;
      if (captured.alias === null) delete settings.alias;
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

export function conversationAlias(value: unknown): value is string {
  return typeof value === "string" && Array.from(value).length >= 1 &&
    Array.from(value).length <= 64 && !/[\s\p{Cc}\p{Cf}\\/:]/u.test(value);
}
