// 独立适配器中的有界手机查询，打开会话另存通知水位，不认定主人。
import type {
  HistoryCursor,
  OpenedConversation,
  PhoneActionReceipt,
  PhoneMessage,
  PhoneSend,
  PhoneSendFile,
} from "../../contracts/phone.ts";
import { historyScreen, messageScreen, messageTextPage } from "../../sdk/phone-view.ts";
import { QqSendActions, type SendStatus } from "./actions.ts";
import type { InboundRecord } from "./journal.ts";
import { QqTimeline, type TimelineSource } from "./timeline.ts";
import { QqPhoneReading } from "./reading.ts";
import { QqPhoneDevice } from "./device.ts";
import { QqPhoneDownloads } from "./downloads.ts";
import { KernelError } from "../../kernel/protocol.ts";
import { fingerprint } from "../../kernel/calls.ts";
import type { Json } from "../../kernel/protocol.ts";

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
  const attempts = status.attemptCount !== undefined && status.attemptCount > 1
    ? { attemptCount: status.attemptCount }
    : {};
  if (status.status === "running") {
    return { actionId: status.actionId, state: "running", ...attempts };
  }
  if (status.status === "blocked") {
    return {
      actionId: status.actionId,
      state: "blocked",
      blockedBy: status.blockedBy,
      ...attempts,
    };
  }
  if (status.status === "unknown") {
    return { actionId: status.actionId, state: "unknown", reason: "interrupted", ...attempts };
  }
  if (status.status === "cancelled") {
    const result = status.originalResult;
    return {
      actionId: status.actionId,
      state: "cancelled",
      ...attempts,
      resolution: {
        kind: status.resolution.kind,
        actor: status.resolution.actor,
        at: status.resolution.resolvedAt,
      },
      ...(result && result.status !== "sent"
        ? { attempted: result.attempted, error: result.error }
        : {}),
    };
  }
  const result = status.result;
  return result.status === "sent"
    ? { actionId: status.actionId, state: "sent", platformMessageId: result.messageId, ...attempts }
    : {
      actionId: status.actionId,
      state: result.status,
      ...attempts,
      attempted: result.attempted,
      error: result.error,
    };
}

/** 群全量推送也可明确单独 @ 本机器人；不能从正文、昵称或 @ 全体推断。 */
function directGroupMention(record: InboundRecord): boolean {
  if (record.raw.t !== "GROUP_MESSAGE_CREATE" || record.message.destination.kind !== "group") {
    return false;
  }
  const data = record.raw.d;
  if (!data || typeof data !== "object" || Array.isArray(data)) return false;
  const raw = data as Record<string, unknown>;
  // /model 等系统接管后的视图已明确抑制通知，不把其派生正文重新当原消息亮屏。
  if (raw.content !== record.message.message.text || !Array.isArray(raw.mentions)) return false;
  return raw.mentions.some((mention: unknown) =>
    !!mention && typeof mention === "object" && !Array.isArray(mention) &&
    (mention as Record<string, unknown>).scope === "single" &&
    (mention as Record<string, unknown>).is_you === true
  );
}

export class QqPhone {
  constructor(
    readonly incoming: () => InboundRecord[],
    readonly actions: QqSendActions,
    readonly timeline: QqTimeline,
    readonly self: { id: string; displayName: string },
    readonly reading: QqPhoneReading,
    readonly now: () => number = Date.now,
    readonly device?: QqPhoneDevice,
    readonly downloads?: QqPhoneDownloads,
  ) {}

  async #snapshot(options: { storedNotifications?: boolean } = {}) {
    const sources: TimelineSource[] = this.incoming().map((record) => ({
      key: `incoming:${record.seq}`,
      fingerprint: record.identity,
      observedAt: record.receivedAt,
      // 只增强可再生的手机视图；旧入站投影和原件的格式/指纹保持不变。
      message: {
        ...record.message.message,
        ...(record.message.deduplication.messageIndex === undefined ? {} : {
          referenceIndex: record.message.deduplication.messageIndex,
        }),
        ...(record.message.referenceIndex === undefined ? {} : {
          reference: { index: record.message.referenceIndex },
        }),
      },
      notify: record.message.notify || (!options.storedNotifications && directGroupMention(record)),
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
          ...(record.request.referenceIndex === undefined ? {} : {
            reference: { index: record.request.referenceIndex },
          }),
          ...(record.result?.status === "sent" && record.result.referenceIndex !== undefined
            ? { referenceIndex: record.result.referenceIndex }
            : {}),
          ...(record.request.attachment
            ? {
              attachments: [{
                kind: record.request.attachment.kind,
                name: record.request.attachment.path.split("/").at(-1),
              }],
            }
            : {}),
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
    const rows = await this.timeline.snapshot(sources);
    // 一次线性建表，不逐条扫描历史；索引仅在本会话唯一命中时才可解析。
    // 平台 msg_id 可能被并行消息共用，不能拿它或正文猜测引用对象。
    const targets = new Map<string, (typeof rows)[number] | null>();
    const indexKey = (conversationId: string, index: string) =>
      JSON.stringify([conversationId, index]);
    for (const row of rows) {
      if (row.message.referenceIndex === undefined) continue;
      const key = indexKey(row.message.conversationId, row.message.referenceIndex);
      targets.set(key, targets.has(key) ? null : row);
    }
    for (const row of rows) {
      const reference = row.message.reference;
      if (!reference) continue;
      const target = targets.get(indexKey(row.message.conversationId, reference.index));
      if (!target || target.key === row.key) continue;
      reference.target = {
        conversationId: target.message.conversationId,
        seq: target.message.seq,
      };
      if (target.message.platformMessageId !== undefined) {
        row.message.quote = { platformMessageId: target.message.platformMessageId };
      }
    }
    this.reading.validateSnapshot(rows);
    return rows;
  }

  async conversations(args: {
    cursor?: { snapshotThroughSeq: number; beforeLastSeq: number; beforePinned?: boolean };
  } = {}) {
    const rows = await this.#snapshot();
    const through = args.cursor?.snapshotThroughSeq ?? rows.at(-1)?.message.seq ?? 0;
    if (through > (rows.at(-1)?.message.seq ?? 0)) throw new QqPhoneError("request");
    if (args.cursor) {
      integer(through, 0);
      integer(args.cursor.beforeLastSeq, 1);
      if (args.cursor.beforePinned !== undefined && typeof args.cursor.beforePinned !== "boolean") {
        throw new QqPhoneError("request");
      }
      if (args.cursor.beforeLastSeq > through) throw new QqPhoneError("request");
    }
    const latest = new Map<string, PhoneMessage>();
    for (const { message } of rows) {
      if (message.seq <= through) latest.set(message.conversationId, message);
    }
    const pinned = (id: string) => this.device?.get(id).pinned ?? false;
    const eligible = [...latest.values()].filter((message) => {
      if (!args.cursor) return true;
      const wasPinned = args.cursor.beforePinned ?? false;
      const isPinned = pinned(message.conversationId);
      return wasPinned
        ? (!isPinned || message.seq < args.cursor.beforeLastSeq)
        : (!isPinned && message.seq < args.cursor.beforeLastSeq);
    }).sort((a, b) =>
      Number(pinned(b.conversationId)) - Number(pinned(a.conversationId)) || b.seq - a.seq
    );
    const page = eligible.slice(0, SCREEN.pageSize);
    const pending = new Map<string, number>();
    for (const row of rows) {
      const id = row.message.conversationId;
      if (
        row.message.seq <= through && row.notify &&
        row.message.seq > (this.reading.get(id)?.notificationThroughSeq ?? 0)
      ) {
        pending.set(id, (pending.get(id) ?? 0) + 1);
      }
    }
    return {
      items: page.map((message) => ({
        conversationId: message.conversationId,
        kind: message.conversationId.startsWith("qq:user:") ? "user" : "group",
        latest: messageScreen(message, SCREEN, this.now()),
        pendingNotifications: this.device?.get(message.conversationId).muted
          ? 0
          : pending.get(message.conversationId) ?? 0,
        reading: this.reading.get(message.conversationId),
        settings: {
          muted: this.device?.get(message.conversationId).muted ?? false,
          pinned: pinned(message.conversationId),
          hasDraft: !!this.device?.get(message.conversationId).draft,
        },
      })),
      snapshotThroughSeq: through,
      ...(eligible.length > page.length
        ? {
          older: {
            snapshotThroughSeq: through,
            beforeLastSeq: page.at(-1)!.seq,
            ...(pinned(page.at(-1)!.conversationId) ? { beforePinned: true } : {}),
          },
        }
        : {}),
    };
  }

  /** 仅供部署捕获事实边界，不输出正文、不移动阅读/通知水位、不开放为RPC。 */
  async captureNotificationBoundary(throughSeq?: number) {
    // 首次上线边界校验的是历史原件，不随可再生通知视图增强而改变摘要。
    const rows = await this.#snapshot({ storedNotifications: true });
    const through = throughSeq ?? rows.at(-1)?.message.seq ?? 0;
    integer(through, 0);
    if (through > (rows.at(-1)?.message.seq ?? 0)) {
      throw new KernelError("corrupt", "启动边界高于当前手机事实，原件保留");
    }
    const included = rows.filter((row) => row.message.seq <= through);
    const metadata = included.map((row) => ({
      seq: row.message.seq,
      key: row.key,
      conversationId: row.message.conversationId,
      notify: row.notify,
    }));
    return {
      throughSeq: through,
      count: included.length,
      notificationCount: included.filter((row) => row.notify).length,
      digest: await fingerprint(metadata as unknown as Json),
    };
  }

  async open(args: { conversationId: string }): Promise<OpenedConversation> {
    conversation(args.conversationId);
    // open 不接受历史游标，防止将向前翻页误作打开最新一屏。
    if (Object.hasOwn(args, "cursor")) throw new QqPhoneError("request");
    const rows = await this.#snapshot();
    const screen = historyScreen(
      rows.map((row) => row.message),
      args.conversationId,
      SCREEN,
      this.now(),
    );
    const displayedSeqs = screen.messages.map((message) => message.handle.seq);
    const reading = displayedSeqs.length
      ? await this.reading.opened(args.conversationId, screen.snapshotThroughSeq, displayedSeqs)
      : this.reading.get(args.conversationId);
    return {
      ...screen,
      notificationThroughSeq: reading?.notificationThroughSeq ?? 0,
      displayedSeqs,
      earlierMessagesNotShown:
        rows.filter((row) =>
          row.message.conversationId === args.conversationId &&
          row.message.seq < (displayedSeqs[0] ?? 0)
        ).length,
      notice:
        "通知水位仅用于消除本机会话提醒；本屏上方未展示的消息并非真的已读。当前只展示预览，长文和附件仍需主动打开。",
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
    const eligible = rows.filter((row) =>
      row.notify && row.message.seq > after &&
      !(this.device?.get(row.message.conversationId).muted ?? false) &&
      row.message.seq > (this.reading.get(row.message.conversationId)?.notificationThroughSeq ?? 0)
    );
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

  async send(
    args: PhoneSend & { attachment?: { path: string; kind: "image" | "file" } },
    options: { beforeAttempt?: () => Promise<void> } = {},
  ) {
    conversation(args.conversationId);
    const match = /^qq:(user|group):(.+)$/.exec(args.conversationId)!;
    let reply: { messageId: string; sequence?: number } | undefined;
    if (args.replyTo !== undefined) {
      integer(args.replyTo.seq, 1);
      if (args.replyTo.sequence !== undefined) integer(args.replyTo.sequence, 1);
      const message = await this.#message({
        conversationId: args.conversationId,
        seq: args.replyTo.seq,
      });
      if (message.direction !== "incoming" || !message.platformMessageId) {
        throw new QqPhoneError("request");
      }
      reply = {
        messageId: message.platformMessageId,
        ...(args.replyTo.sequence === undefined ? {} : { sequence: args.replyTo.sequence }),
      };
    }
    const result = await this.actions.send(args.actionId, {
      destination: { kind: match[1] as "user" | "group", openId: match[2] },
      text: args.text,
      ...(args.attachment === undefined ? {} : { attachment: args.attachment }),
      ...(reply ? { reply } : {}),
      ...(args.referenceIndex === undefined ? {} : { referenceIndex: args.referenceIndex }),
      ...(args.wakeup === undefined ? {} : { wakeup: args.wakeup }),
    }, options);
    // 回执先由发送账本保存；时间线失败不能重发已执行动作。恢复会从原账本补齐引用。
    await this.#snapshot();
    return phoneReceipt(result);
  }

  action(args: { actionId: string }) {
    const result = this.actions.status(args.actionId);
    return result ? phoneReceipt(result) : null;
  }
  async listSendQueue(
    args: { conversationId?: string; afterSequence?: number; limit?: number } = {},
  ) {
    if (
      Object.keys(args).some((key) => !["conversationId", "afterSequence", "limit"].includes(key))
    ) throw new QqPhoneError("request");
    const page = await this.actions.listSendQueue(args);
    return {
      ...page,
      items: page.items.map(({ status, ...item }) => ({ ...item, receipt: phoneReceipt(status) })),
    };
  }
  async cancelSend(
    args: import("./actions.ts").SendCancellation,
    options: { beforeAttempt?: () => Promise<void> } = {},
  ) {
    const result = await this.actions.cancelSend(args, {
      actor: "eve",
      checkpoint: options.beforeAttempt,
    });
    await this.#snapshot();
    return phoneReceipt(result);
  }

  async settings(
    args: { conversationId: string; muted?: boolean; pinned?: boolean; draft?: string },
  ) {
    conversation(args.conversationId);
    if (!this.device) throw new KernelError("unavailable", "手机设置存储未接入");
    if (
      Object.keys(args).some((key) =>
        !["conversationId", "muted", "pinned", "draft"].includes(key)
      ) ||
      (args.muted !== undefined && typeof args.muted !== "boolean") ||
      (args.pinned !== undefined && typeof args.pinned !== "boolean") ||
      (args.draft !== undefined &&
        (typeof args.draft !== "string" || new TextEncoder().encode(args.draft).length > 65_536))
    ) throw new QqPhoneError("request");
    const patch = {
      ...(args.muted === undefined ? {} : { muted: args.muted }),
      ...(args.pinned === undefined ? {} : { pinned: args.pinned }),
      ...(args.draft === undefined ? {} : { draft: args.draft }),
    };
    return Object.keys(patch).length
      ? await this.device.update(args.conversationId, patch)
      : this.device.get(args.conversationId);
  }

  async download(args: { handle: { conversationId: string; seq: number }; index: number }) {
    integer(args.index, 1);
    if (!this.downloads) throw new KernelError("unavailable", "附件下载存储未接入");
    const message = await this.#message(args.handle);
    const rows = await this.#snapshot();
    if (message.direction !== "incoming" || !message.attachments?.[args.index - 1]) {
      throw new QqPhoneError("not_found");
    }
    const row = rows.find((row) => row.message.seq === args.handle.seq)!;
    const incomingSeq = Number(row.key.slice("incoming:".length));
    const original = this.incoming().find((record) => record.seq === incomingSeq);
    const raw = original?.raw.d as { attachments?: { url?: unknown }[] } | undefined;
    const url = raw?.attachments?.[args.index - 1]?.url;
    if (typeof url !== "string") {
      throw new KernelError("unavailable", "平台没有提供附件下载地址，原始元信息仍保留");
    }
    return await this.downloads.download({
      kind: "phone_attachment",
      conversationId: message.conversationId,
      sourceSeq: message.seq,
      attachmentId: `${message.conversationId}:${message.seq}:${args.index}`,
      senderPlatformUserId: message.sender.platformUserId,
    }, url);
  }

  async image(args: { handle: { conversationId: string; seq: number }; index: number }) {
    const downloaded = await this.download(args);
    return await this.downloads!.image(downloaded);
  }
  async file(
    args: {
      handle: { conversationId: string; seq: number };
      index: number;
      offsetBytes?: number;
      limitBytes?: number;
      encoding?: "utf8" | "base64";
    },
  ) {
    const download = await this.download(args);
    return await this.downloads!.file(download, args);
  }

  async sendFile(args: PhoneSendFile, options: { beforeAttempt?: () => Promise<void> } = {}) {
    return await this.send({
      ...args,
      text: args.text ?? "",
      attachment: { path: args.path, kind: args.kind },
    }, options);
  }
}
