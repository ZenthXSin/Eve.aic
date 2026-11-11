// 独立适配器中的有界手机查询，不调用模型，不标已读，不认定主人。
import type {
  HistoryCursor,
  PhoneActionReceipt,
  PhoneMessage,
  PhoneSend,
} from "../../contracts/phone.ts";
import { historyScreen, messageScreen, messageTextPage } from "../../sdk/phone-view.ts";
import { QqSendActions, type SendStatus } from "./actions.ts";
import type { InboundRecord } from "./journal.ts";
import { QqTimeline, type TimelineSource } from "./timeline.ts";

const SCREEN = { pageSize: 8, previewCharacters: 160, attachmentPageSize: 4 };
const NOTICE = { ...SCREEN, previewCharacters: 80 };

export class QqPhoneError extends Error {
  constructor(readonly category: "request" | "not_found") {
    super(`QQ 手机请求未完成：${category}`);
    this.name = "QqPhoneError";
  }
}

function integer(value: unknown, min: number): asserts value is number {
  if (!Number.isSafeInteger(value) || (value as number) < min) throw new QqPhoneError("request");
}
function conversation(value: unknown): asserts value is string {
  if (typeof value !== "string" || !/^qq:(user|group):.{1,512}$/.test(value)) {
    throw new QqPhoneError("request");
  }
}
function cursor(value: HistoryCursor | undefined): void {
  if (value === undefined) return;
  integer(value.snapshotThroughSeq, 0);
  integer(value.beforeSeq, 1);
  if (value.beforeSeq > value.snapshotThroughSeq) throw new QqPhoneError("request");
}
export function phoneReceipt(status: SendStatus): PhoneActionReceipt {
  if (status.status === "running") return { actionId: status.actionId, state: "running" };
  if (status.status === "unknown") {
    return { actionId: status.actionId, state: "unknown", reason: "interrupted" };
  }
  const result = status.result;
  return result.status === "sent"
    ? { actionId: status.actionId, state: "sent", platformMessageId: result.messageId }
    : {
      actionId: status.actionId,
      state: result.status,
      attempted: result.attempted,
      error: result.error,
    };
}

export class QqPhone {
  constructor(
    readonly incoming: () => InboundRecord[],
    readonly actions: QqSendActions,
    readonly timeline: QqTimeline,
    readonly self: { id: string; displayName: string },
    readonly now: () => number = Date.now,
  ) {}

  async #snapshot() {
    const sources: TimelineSource[] = this.incoming().map((record) => ({
      key: `incoming:${record.seq}`,
      fingerprint: record.identity,
      observedAt: record.receivedAt,
      message: record.message.message,
      notify: record.message.notify,
    }));
    for (const record of this.actions.records()) {
      const delivery = phoneReceipt(this.actions.status(record.actionId)!);
      sources.push({
        key: `outgoing:${record.actionId}`,
        fingerprint: record.fingerprint,
        observedAt: record.startedAt,
        notify: false,
        message: {
          conversationId:
            `qq:${record.request.destination.kind}:${record.request.destination.openId}`,
          sender: { platformUserId: this.self.id, displayName: this.self.displayName },
          direction: "outgoing",
          text: record.request.text,
          delivery,
          ...(delivery.platformMessageId === undefined ? {} : {
            platformMessageId: delivery.platformMessageId,
          }),
          ...(record.result?.status === "sent" && record.result.timestamp !== undefined
            ? { sentAt: record.result.timestamp }
            : {}),
        },
      });
    }
    return await this.timeline.snapshot(sources);
  }

  async conversations(args: {
    cursor?: { snapshotThroughSeq: number; beforeLastSeq: number };
  } = {}) {
    const rows = await this.#snapshot();
    const through = args.cursor?.snapshotThroughSeq ?? rows.at(-1)?.message.seq ?? 0;
    if (through > (rows.at(-1)?.message.seq ?? 0)) throw new QqPhoneError("request");
    if (args.cursor) {
      integer(through, 0);
      integer(args.cursor.beforeLastSeq, 1);
      if (args.cursor.beforeLastSeq > through) throw new QqPhoneError("request");
    }
    const latest = new Map<string, PhoneMessage>();
    for (const { message } of rows) {
      if (message.seq <= through) latest.set(message.conversationId, message);
    }
    const eligible = [...latest.values()].filter((message) =>
      args.cursor === undefined || message.seq < args.cursor.beforeLastSeq
    ).sort((a, b) => b.seq - a.seq);
    const page = eligible.slice(0, SCREEN.pageSize);
    return {
      items: page.map((message) => ({
        conversationId: message.conversationId,
        kind: message.conversationId.startsWith("qq:user:") ? "user" : "group",
        latest: messageScreen(message, SCREEN, this.now()),
      })),
      snapshotThroughSeq: through,
      ...(eligible.length > page.length
        ? {
          older: { snapshotThroughSeq: through, beforeLastSeq: page.at(-1)!.seq },
        }
        : {}),
    };
  }

  async history(args: { conversationId: string; cursor?: HistoryCursor }) {
    conversation(args.conversationId);
    cursor(args.cursor);
    const rows = await this.#snapshot();
    if (args.cursor && args.cursor.snapshotThroughSeq > (rows.at(-1)?.message.seq ?? 0)) {
      throw new QqPhoneError("request");
    }
    return historyScreen(
      rows.map((row) => row.message),
      args.conversationId,
      SCREEN,
      this.now(),
      args.cursor,
    );
  }

  async #message(handle: { conversationId: string; seq: number }) {
    conversation(handle?.conversationId);
    integer(handle.seq, 1);
    const found = (await this.#snapshot()).find(({ message }) =>
      message.seq === handle.seq && message.conversationId === handle.conversationId
    );
    if (!found) throw new QqPhoneError("not_found");
    return found.message;
  }

  async message(args: {
    handle: { conversationId: string; seq: number };
    attachmentOffset?: number;
  }) {
    return messageScreen(
      await this.#message(args.handle),
      SCREEN,
      this.now(),
      args.attachmentOffset,
    );
  }

  async text(
    args: { handle: { conversationId: string; seq: number }; startChar: number; limit: number },
  ) {
    return messageTextPage(await this.#message(args.handle), args.startChar, args.limit);
  }

  async search(args: { query: string; conversationId?: string; cursor?: HistoryCursor }) {
    if (typeof args.query !== "string" || args.query.length === 0 || args.query.length > 256) {
      throw new QqPhoneError("request");
    }
    if (args.conversationId !== undefined) conversation(args.conversationId);
    cursor(args.cursor);
    const rows = await this.#snapshot();
    const through = args.cursor?.snapshotThroughSeq ?? rows.at(-1)?.message.seq ?? 0;
    if (through > (rows.at(-1)?.message.seq ?? 0)) throw new QqPhoneError("request");
    const eligible = rows.map((row) => row.message).filter((message) =>
      message.seq <= through && (!args.cursor || message.seq < args.cursor.beforeSeq) &&
      (args.conversationId === undefined || message.conversationId === args.conversationId) &&
      message.text.includes(args.query)
    );
    const page = eligible.slice(-SCREEN.pageSize);
    return {
      messages: page.map((message) => messageScreen(message, SCREEN, this.now())),
      snapshotThroughSeq: through,
      ...(eligible.length > page.length
        ? {
          older: { snapshotThroughSeq: through, beforeSeq: page[0].seq },
        }
        : {}),
    };
  }

  async notifications(args: { afterSeq?: number } = {}) {
    const after = args.afterSeq ?? 0;
    integer(after, 0);
    const rows = await this.#snapshot();
    if (after > (rows.at(-1)?.message.seq ?? 0)) throw new QqPhoneError("request");
    const eligible = rows.filter((row) => row.notify && row.message.seq > after);
    const page = eligible.slice(0, SCREEN.pageSize);
    return {
      items: page.map((row) => messageScreen(row.message, NOTICE, this.now())),
      // 队列尚有未展示通知时，只确认本页末尾；不因快照末尾有更多通知就跳过。
      throughSeq: eligible.length > page.length
        ? page.at(-1)!.message.seq
        : rows.at(-1)?.message.seq ?? after,
      hasMore: eligible.length > page.length,
    };
  }

  async send(args: PhoneSend) {
    conversation(args.conversationId);
    const match = /^qq:(user|group):(.+)$/.exec(args.conversationId)!;
    let reply: { messageId: string; sequence: number } | undefined;
    if (args.replyTo !== undefined) {
      integer(args.replyTo.seq, 1);
      integer(args.replyTo.sequence, 1);
      const message = await this.#message({
        conversationId: args.conversationId,
        seq: args.replyTo.seq,
      });
      if (message.direction !== "incoming" || !message.platformMessageId) {
        throw new QqPhoneError("request");
      }
      reply = { messageId: message.platformMessageId, sequence: args.replyTo.sequence };
    }
    const result = await this.actions.send(args.actionId, {
      destination: { kind: match[1] as "user" | "group", openId: match[2] },
      text: args.text,
      ...(reply ? { reply } : {}),
      ...(args.referenceIndex === undefined ? {} : { referenceIndex: args.referenceIndex }),
      ...(args.wakeup === undefined ? {} : { wakeup: args.wakeup }),
    });
    // 回执先由发送账本保存；时间线失败不能重发已执行动作。恢复会从原账本补齐引用。
    await this.#snapshot();
    return phoneReceipt(result);
  }

  action(args: { actionId: string }) {
    const result = this.actions.status(args.actionId);
    return result ? phoneReceipt(result) : null;
  }
}
