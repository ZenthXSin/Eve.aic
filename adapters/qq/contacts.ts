import type { PhoneMessage } from "../../contracts/phone.ts";
import type { InboundRecord } from "./journal.ts";

/** Derived only from platform envelopes and known senders, never from nicknames' claimed roles. */
export function contactIndex(
  records: InboundRecord[],
  index = { names: new Map<string, string>(), conversations: new Map<string, string>() },
) {
  const { names, conversations } = index;
  for (const record of records) {
    const m = record.message.message, raw = record.raw.d as Record<string, unknown>;
    if (m.sender.displayName) {
      names.set(JSON.stringify([m.conversationId, m.sender.platformUserId]), m.sender.displayName);
    }
    const group = raw?.group as Record<string, unknown> | undefined;
    const name =
      record.message.destination.kind === "user" || record.message.destination.kind === "guild-dm"
        ? m.sender.displayName
        : record.message.destination.kind === "group"
        ? raw?.group_name ?? group?.name
        : undefined;
    if (typeof name === "string" && name && name.length <= 512) {
      conversations.set(m.conversationId, name);
    }
    for (const entry of Array.isArray(raw?.mentions) ? raw.mentions : []) {
      const id = mentionId(entry),
        display = entry?.member?.nick ?? entry?.username ?? entry?.nickname ?? entry?.nick ??
          entry?.name;
      if (id && typeof display === "string" && display && display.length <= 512) {
        names.set(JSON.stringify([m.conversationId, id]), display);
      }
    }
  }
  return { names, conversations };
}
function mentionId(entry: Record<string, unknown> | null | undefined) {
  const value = entry?.user_openid ?? entry?.member_openid ?? entry?.user_id ?? entry?.openid ??
    entry?.id;
  return typeof value === "string" && value.length <= 512 && value ? value : undefined;
}
export function messageMentions(
  record: InboundRecord,
  names: Map<string, string>,
  self: { id: string; displayName: string },
): NonNullable<PhoneMessage["mentions"]> {
  const raw = record.raw.d as { mentions?: Record<string, unknown>[] };
  const entries = new Map<string, { isSelf?: boolean }>();
  for (const entry of Array.isArray(raw?.mentions) ? raw.mentions : []) {
    const id = mentionId(entry);
    if (id) {
      entries.set(id, {
        ...(entry.scope === "single" && entry.is_you === true ? { isSelf: true } : {}),
      });
    }
  }
  for (const match of record.message.message.text.matchAll(/<@!?(\w[\w-]{0,511})>/g)) {
    if (!entries.has(match[1])) entries.set(match[1], {});
  }
  return [...entries].map(([platformUserId, metadata]) => {
    const isSelf = platformUserId === self.id || metadata.isSelf === true;
    const displayName = isSelf
      ? self.displayName
      : names.get(JSON.stringify([record.message.message.conversationId, platformUserId]));
    return {
      platformUserId,
      ...(displayName ? { displayName } : {}),
      resolved: !!displayName,
      ...(isSelf ? { isSelf: true } : {}),
    };
  });
}

/** Process-local display metadata. Failures are missing names, not synthetic labels. */
export class QqGroupNames {
  #cache = new Map<string, { name?: string; expiresAt: number }>();
  #inflight = new Map<string, Promise<string | undefined>>();
  #active = 0;
  #waiting: (() => void)[] = [];

  constructor(
    readonly query: (
      openId: string,
      signal: AbortSignal,
    ) => Promise<{ groupOpenId: string; groupName?: string }>,
    readonly options: {
      now?: () => number;
      ttlMs?: number;
      failureTtlMs?: number;
      timeoutMs?: number;
      concurrency?: number;
      maxEntries?: number;
    } = {},
  ) {
    for (
      const value of [
        options.ttlMs ?? 1_200_000,
        options.failureTtlMs ?? 60_000,
        options.timeoutMs ?? 2_000,
        options.concurrency ?? 3,
        options.maxEntries ?? 512,
      ]
    ) {
      if (!Number.isSafeInteger(value) || value < 1 || value > 2_147_483_647) {
        throw new Error("Invalid QQ group-name cache options");
      }
    }
  }

  /** No network from background snapshots, send receipts or local alias resolution. */
  peek(openId: string): string | undefined {
    const cached = this.#cache.get(openId);
    return cached && this.#now() < cached.expiresAt ? cached.name : undefined;
  }

  get(openId: string): Promise<string | undefined> {
    if (!openId.trim() || openId.length > 512) return Promise.resolve(undefined);
    const cached = this.#cache.get(openId);
    if (cached && this.#now() < cached.expiresAt) return Promise.resolve(cached.name);
    const pending = this.#inflight.get(openId);
    if (pending) return pending;
    const result = this.#lookup(openId);
    this.#inflight.set(openId, result);
    return result;
  }

  #now() {
    return (this.options.now ?? Date.now)();
  }

  #acquire(signal: AbortSignal): Promise<boolean> {
    if (signal.aborted) return Promise.resolve(false);
    if (this.#active < (this.options.concurrency ?? 3)) {
      this.#active++;
      return Promise.resolve(true);
    }
    return new Promise((resolve) => {
      const enter = () => {
        signal.removeEventListener("abort", cancel);
        resolve(true);
      };
      const cancel = () => {
        this.#waiting.splice(this.#waiting.indexOf(enter), 1);
        resolve(false);
      };
      this.#waiting.push(enter);
      signal.addEventListener("abort", cancel, { once: true });
    });
  }

  #release() {
    const next = this.#waiting.shift();
    if (next) next();
    else this.#active--;
  }

  async #lookup(openId: string): Promise<string | undefined> {
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    let name: string | undefined;
    let query: Promise<{ groupOpenId: string; groupName?: string } | undefined> | undefined;
    try {
      // The deadline includes queueing and shared token refresh, not just the GET.
      const timeout = new Promise<undefined>((resolve) => {
        timer = setTimeout(() => {
          controller.abort();
          resolve(undefined);
        }, this.options.timeoutMs ?? 2_000);
      });
      query = (async () => {
        if (!await this.#acquire(controller.signal)) return undefined;
        try {
          controller.signal.throwIfAborted();
          return await this.query(openId, controller.signal);
        } finally {
          // Keep the slot until the actual query settles. Even a transport that
          // ignores abort must not allow more than the configured concurrency.
          this.#release();
        }
      })();
      const info = await Promise.race([query, timeout]);
      if (
        !controller.signal.aborted && info?.groupOpenId === openId &&
        typeof info.groupName === "string" && info.groupName.trim() && info.groupName.length <= 512
      ) name = info.groupName;
    } catch {
      // HTTP/auth/validation failure cannot make a whole phone screen unavailable.
    } finally {
      clearTimeout(timer);
      controller.abort();
    }
    this.#cache.delete(openId);
    if (this.#cache.size >= (this.options.maxEntries ?? 512)) {
      this.#cache.delete(this.#cache.keys().next().value!);
    }
    this.#cache.set(openId, {
      ...(name === undefined ? {} : { name }),
      expiresAt: this.#now() +
        (name === undefined
          ? this.options.failureTtlMs ?? 60_000
          : this.options.ttlMs ?? 1_200_000),
    });
    // A timed-out render may finish before the transport. Keep same-openid
    // deduplication until that original query settles, even after negative TTL.
    const clear = () => this.#inflight.delete(openId);
    void query?.then(clear, clear);
    return name;
  }
}
