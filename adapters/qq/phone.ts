// 独立适配器中的有界手机查询，打开会话另存通知水位，不认定主人。
import { qqDestination } from "../../contracts/qq-conversation.ts";
import type {
  HistoryCursor,
  OpenedConversation,
  PhoneActionReceipt,
  PhoneBurstReceipt,
  PhoneBurstStatus,
  PhoneMessage,
  PhoneMessagesWaitResult,
  PhoneSend,
  PhoneSendBurst,
  PhoneSendFile,
  PhoneWaitForMessages,
} from "../../contracts/phone.ts";
import { historyScreen, messageScreen, messageTextPage } from "../../sdk/phone-view.ts";
import { discoverPlatform, type phonePlatformTools } from "./platform-bridge.ts";
import type {
  PhonePlatformCapabilitiesArgs,
  PhonePlatformEventsArgs,
  PhonePlatformReadArgs,
} from "../../contracts/phone-platform.ts";
import { QqSendActions, type SendRecord, type SendStatus } from "./actions.ts";
import type { InboundRecord, QqInboundJournal } from "./journal.ts";
import { QqTimeline, type TimelineFact } from "./timeline.ts";
import { qqModelCommand, type QqModelCommands } from "./model-command.ts";
import { QqPhoneReading } from "./reading.ts";
import { QqPhoneDevice } from "./device.ts";
import { QqPhoneDownloads } from "./downloads.ts";
import { KernelError } from "../../kernel/protocol.ts";
import { fingerprint } from "../../kernel/calls.ts";
import type { Json } from "../../kernel/protocol.ts";

import { admitBurst, readBurst, waitForQuiet } from "./phone-social.ts";
import { contactIndex, messageMentions, QqGroupNames } from "./contacts.ts";

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
  if (!qqDestination(value)) {
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

interface FactRow {
  key: string;
  message: { seq: number; conversationId: string; text: string };
  incomingSeq?: number;
  command?: boolean;
  hasEnvelopeLabel?: boolean;
  storedNotify: boolean;
  notify: boolean;
  referenceIndex?: string;
  platformMessageId?: string;
}
interface PhoneIndex {
  rows: FactRow[];
  contacts: ReturnType<typeof contactIndex>;
  incomingTargets: Map<string, FactRow | null>;
  outgoingSeqs: Map<string, number>;
  read: (seq: number) => InboundRecord;
}
function outgoingMessage(record: SendRecord, delivery: PhoneActionReceipt, self: QqPhone["self"]) {
  const { destination, text, referenceIndex, attachment } = record.request;
  const sent = record.result?.status === "sent" ? record.result : undefined;
  return {
    conversationId: `qq:${destination.kind}:${destination.openId}`,
    sender: { platformUserId: self.id, displayName: self.displayName },
    direction: "outgoing" as const,
    text,
    ...(referenceIndex === undefined ? {} : { reference: { index: referenceIndex } }),
    ...(sent?.referenceIndex === undefined ? {} : { referenceIndex: sent.referenceIndex }),
    ...(attachment
      ? { attachments: [{ kind: attachment.kind, name: attachment.path.split("/").at(-1) }] }
      : {}),
    delivery,
    ...(delivery.platformMessageId === undefined
      ? {}
      : { platformMessageId: delivery.platformMessageId }),
    ...(sent?.timestamp === undefined ? {} : { sentAt: sent.timestamp }),
  };
}

export class QqPhone {
  #burstTail: Promise<unknown> = Promise.resolve();
  #journal?: QqInboundJournal;
  #cache?: { incoming: number; outgoing: number; index: Promise<PhoneIndex> };
  #attachmentFailures = new Set<string>();
  #attachmentPending = new Set<string>();
  readonly incoming: () => InboundRecord[];
  constructor(
    incoming: (() => InboundRecord[]) | QqInboundJournal,
    readonly actions: QqSendActions,
    readonly timeline: QqTimeline,
    readonly self: { id: string; displayName: string },
    readonly reading: QqPhoneReading,
    readonly now: () => number = Date.now,
    readonly device?: QqPhoneDevice,
    readonly downloads?: QqPhoneDownloads,
    readonly autoDownloadAttachments = false,
    readonly groupNames?: QqGroupNames,
    private readonly commands?: QqModelCommands,
    private readonly platform?: ReturnType<typeof phonePlatformTools>,
  ) {
    if (typeof incoming === "function") this.incoming = incoming;
    else {
      this.#journal = incoming;
      this.incoming = () => incoming.records().map((record) => commands?.project(record) ?? record);
    }
  }

  platformCapabilities(args: PhonePlatformCapabilitiesArgs = {}) {
    return discoverPlatform(args);
  }

  async platformEvents(args: PhonePlatformEventsArgs = {}) {
    if (!this.platform) throw new KernelError("unavailable", "QQ 平台事件未配置");
    return await this.platform.events(args);
  }

  async platformRead(args: PhonePlatformReadArgs) {
    if (!this.platform) throw new KernelError("unavailable", "QQ 平台读取未配置");
    return await this.platform.read(args);
  }

  async #snapshot(): Promise<PhoneIndex> {
    const previous = this.#cache;
    const before = (previous?.incoming ?? 0) + (previous?.outgoing ?? 0);
    const incoming = this.#journal
      ? this.#journal.records(previous?.incoming ?? 0)
      : structuredClone(this.incoming());
    const outgoing = this.actions.newRecords(previous?.outgoing ?? 0);
    let index: PhoneIndex;
    if (previous && !incoming.length && !outgoing.length) {
      if (!this.timeline.current(before)) await this.timeline.sync(before, []);
      index = await previous.index;
    } else {
      const pending = this.#extend(previous?.index, before, incoming, outgoing);
      if (this.#journal) {
        this.#cache = {
          incoming: (previous?.incoming ?? 0) + incoming.length,
          outgoing: (previous?.outgoing ?? 0) + outgoing.length,
          index: pending,
        };
      }
      index = await pending;
    }
    this.reading.validateSnapshot(index.rows);
    if (index.rows.length && this.device && !this.device.available) {
      throw new KernelError("unavailable", "手机设置不可用");
    }
    return index;
  }

  async #extend(
    previous: Promise<PhoneIndex> | undefined,
    before: number,
    incoming: InboundRecord[],
    outgoing: SendRecord[],
  ): Promise<PhoneIndex> {
    const facts: TimelineFact[] = [];
    const fresh = new Map<string, FactRow>();
    for (const record of incoming) {
      const key = `incoming:${record.seq}`;
      const { conversationId, text, platformMessageId } = record.message.message;
      facts.push({ key, fingerprint: record.identity, observedAt: record.receivedAt });
      fresh.set(key, {
        key,
        message: { seq: 0, conversationId, text },
        incomingSeq: record.seq,
        command: !!this.commands && qqModelCommand(record) !== undefined,
        hasEnvelopeLabel: record.message.message.conversation !== undefined,
        storedNotify: record.message.notify,
        notify: record.message.notify || directGroupMention(record),
        referenceIndex: record.message.deduplication.messageIndex ??
          record.message.message.referenceIndex,
        platformMessageId,
      });
    }
    for (const record of outgoing) {
      const key = `outgoing:${record.actionId}`;
      const { destination, text } = record.request;
      facts.push({ key, fingerprint: record.fingerprint, observedAt: record.startedAt });
      fresh.set(key, {
        key,
        message: { seq: 0, conversationId: `qq:${destination.kind}:${destination.openId}`, text },
        storedNotify: false,
        notify: false,
      });
    }
    const [keys, prior] = await Promise.all([this.timeline.sync(before, facts), previous]);
    const contacts = incoming.length && prior
      ? {
        names: new Map(prior.contacts.names),
        conversations: new Map(prior.contacts.conversations),
      }
      : prior?.contacts;
    const added = keys.map((key, index) => {
      const row = fresh.get(key)!;
      row.message.seq = before + index + 1;
      return row;
    });
    let targets = prior?.incomingTargets ?? new Map<string, FactRow | null>();
    let outgoingSeqs = prior?.outgoingSeqs ?? new Map<string, number>();
    for (const row of added) {
      if (row.incomingSeq === undefined) {
        if (outgoingSeqs === prior?.outgoingSeqs) outgoingSeqs = new Map(outgoingSeqs);
        outgoingSeqs.set(row.key.slice("outgoing:".length), row.message.seq);
      } else if (row.referenceIndex !== undefined) {
        if (targets === prior?.incomingTargets) targets = new Map(targets);
        const key = JSON.stringify([row.message.conversationId, row.referenceIndex]);
        targets.set(key, targets.has(key) ? null : row);
      }
    }
    const journal = this.#journal;
    const originals = journal ? undefined : new Map(incoming.map((record) => [record.seq, record]));
    return {
      rows: prior ? prior.rows.concat(added) : added,
      contacts: contactIndex(incoming, contacts),
      incomingTargets: targets,
      outgoingSeqs,
      read: journal ? (seq) => journal.record(seq)! : (seq) => originals!.get(seq)!,
    };
  }

  #read(index: PhoneIndex, row: FactRow) {
    const record = index.read(row.incomingSeq!);
    return row.command ? this.commands!.project(record) : record;
  }
  #notify(index: PhoneIndex, row: FactRow, stored = false) {
    if (!row.command) return stored ? row.storedNotify : row.notify;
    const record = this.#read(index, row);
    return record.message.notify || !stored && directGroupMention(record);
  }
  #conversation(index: PhoneIndex, id: string) {
    const settings = this.device?.get(id);
    const platformName = index.contacts.conversations.get(id) ??
      (id.startsWith("qq:group:") ? this.groupNames?.peek(id.slice(9)) : undefined);
    return platformName || settings?.remark || settings?.alias
      ? {
        ...(platformName ? { platformName } : {}),
        ...(settings?.remark ? { remark: settings.remark } : {}),
        ...(settings?.alias ? { alias: settings.alias } : {}),
      }
      : undefined;
  }
  #messageFact(index: PhoneIndex, row: FactRow, through = index.rows.length): PhoneMessage {
    let message: PhoneMessage;
    const { seq, conversationId } = row.message;
    if (row.incomingSeq !== undefined) {
      const record = this.#read(index, row);
      const mentions = messageMentions(record, index.contacts.names, this.self);
      message = {
        ...record.message.message,
        seq,
        ...(mentions.length ? { mentions } : {}),
        ...(row.referenceIndex === undefined ? {} : { referenceIndex: row.referenceIndex }),
        ...(record.message.referenceIndex === undefined
          ? {}
          : { reference: { index: record.message.referenceIndex } }),
      };
    } else {
      const id = row.key.slice("outgoing:".length);
      message = {
        ...outgoingMessage(
          this.actions.record(id)!,
          phoneReceipt(this.actions.status(id)!),
          this.self,
        ),
        seq,
      };
    }
    const label = this.#conversation(index, conversationId);
    if (label) message.conversation = label;
    if (message.reference) {
      const ref = message.reference.index;
      const candidate = index.incomingTargets.get(JSON.stringify([conversationId, ref]));
      const incoming = candidate && candidate.message.seq <= through ? candidate : undefined;
      const outgoing = this.actions.sentReferences(conversationId, ref).filter((id) =>
        (index.outgoingSeqs.get(id) ?? Infinity) <= through
      );
      let target = incoming?.message.seq, platformMessageId = incoming?.platformMessageId;
      if (incoming === undefined && outgoing.length === 1) {
        target = index.outgoingSeqs.get(outgoing[0]);
        const result = this.actions.record(outgoing[0])!.result;
        platformMessageId = result?.status === "sent" ? result.messageId : undefined;
      } else if (outgoing.length) target = undefined;
      if (target !== undefined && target !== seq) {
        message.reference = { index: ref, target: { conversationId, seq: target } };
        if (platformMessageId !== undefined) message.quote = { platformMessageId };
      }
    }
    return message;
  }
  async #present(index: PhoneIndex, rows: FactRow[], through = index.rows.length) {
    const messages = rows.map((row) => this.#messageFact(index, row, through));
    if (this.autoDownloadAttachments && this.downloads) {
      for (const message of messages) {
        if (message.direction !== "incoming") continue;
        for (const [index, attachment] of (message.attachments ?? []).entries()) {
          const id = `${message.conversationId}:${message.seq}:${index + 1}`;
          let local;
          try {
            local = await this.downloads.local(id);
          } catch {
            attachment.download = { state: "failed" };
            continue;
          }
          if (local) {
            if (
              this.downloads.options.temporaryDirectory &&
              local.path.startsWith(`${this.downloads.options.temporaryDirectory}/`)
            ) {
              attachment.download = { state: "ready", path: local.path };
            }
          } else if (this.downloads.hasReceipt(id)) attachment.download = { state: "cleared" };
          else if (this.#attachmentFailures.has(id)) attachment.download = { state: "failed" };
          else if (this.#attachmentPending.has(id)) attachment.download = { state: "pending" };
        }
      }
    }
    await this.#platformNames(messages, index.contacts);
    return messages;
  }

  /** Only fetch for groups on the requested page/screen, not every historical group. */
  async #platformNames(messages: PhoneMessage[], contacts: PhoneIndex["contacts"]) {
    if (!this.groupNames) return;
    const groups = new Map<string, PhoneMessage[]>();
    for (const message of messages) {
      if (!message.conversationId.startsWith("qq:group:")) continue;
      const openId = message.conversationId.slice(9);
      const group = groups.get(openId) ?? [];
      group.push(message);
      groups.set(openId, group);
    }
    await Promise.all([...groups].map(async ([openId, group]) => {
      // Envelope names remain platform facts; never overwrite local alias/remark.
      if (contacts.conversations.has(`qq:group:${openId}`)) return;
      const name = await this.groupNames!.get(openId);
      for (const message of group) {
        const { platformName: _oldName, ...local } = message.conversation ?? {};
        message.conversation = {
          ...local,
          ...(name === undefined ? {} : { platformName: name }),
        };
        if (!Object.keys(message.conversation).length) delete message.conversation;
      }
    }));
  }

  /** 下载只取平台真实附件，复用同一个显式下载通道与并发去重。 */
  async prefetchAttachments(record: InboundRecord) {
    if (!this.downloads) return;
    const index = await this.#snapshot();
    const row = index.rows.find((row) => row.incomingSeq === record.seq);
    if (!row) return;
    const attachments = index.read(record.seq).message.message.attachments;
    for (const [index] of (attachments ?? []).entries()) {
      const id = `${row.message.conversationId}:${row.message.seq}:${index + 1}`;
      // 已被 Eve 清理的临时文件不在后台重新下载；显式请求仍可重取。
      if (this.downloads.hasReceipt(id)) continue;
      this.#attachmentPending.add(id);
      try {
        await this.download({
          handle: { conversationId: row.message.conversationId, seq: row.message.seq },
          index: index + 1,
        });
        this.#attachmentFailures.delete(id);
      } catch {
        this.#attachmentFailures.add(id);
      } finally {
        this.#attachmentPending.delete(id);
      }
    }
  }

  async conversations(args: {
    cursor?: { snapshotThroughSeq: number; beforeLastSeq: number; beforePinned?: boolean };
  } = {}) {
    const index = await this.#snapshot();
    const rows = index.rows;
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
    const latest = new Map<string, FactRow>();
    for (const row of rows) {
      if (row.message.seq > through) break;
      latest.set(row.message.conversationId, row);
    }
    const pinned = (id: string) => this.device?.get(id).pinned ?? false;
    const eligible = [...latest.values()].map((row) => ({
      row,
      isPinned: pinned(row.message.conversationId),
    })).filter(({ row, isPinned }) => {
      if (!args.cursor) return true;
      const wasPinned = args.cursor.beforePinned ?? false;
      return wasPinned
        ? (!isPinned || row.message.seq < args.cursor.beforeLastSeq)
        : (!isPinned && row.message.seq < args.cursor.beforeLastSeq);
    }).sort((left, right) =>
      Number(right.isPinned) - Number(left.isPinned) || right.row.message.seq - left.row.message.seq
    );
    const page = await this.#present(
      index,
      eligible.slice(0, SCREEN.pageSize).map(({ row }) => row),
      through,
    );
    const watermarks = new Map(
      page.map(({ conversationId: id }) => [id, this.reading.get(id)?.notificationThroughSeq ?? 0]),
    );
    const pending = new Map<string, number>();
    for (const row of rows) {
      const id = row.message.conversationId;
      if (
        row.message.seq <= through && row.message.seq > (watermarks.get(id) ?? Infinity) &&
        this.#notify(index, row)
      ) {
        pending.set(id, (pending.get(id) ?? 0) + 1);
      }
    }
    return {
      items: page.map((message) => ({
        conversationId: message.conversationId,
        kind: qqDestination(message.conversationId)!.kind,
        ...message.conversation,
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
    const index = await this.#snapshot();
    const rows = index.rows;
    const through = throughSeq ?? rows.at(-1)?.message.seq ?? 0;
    integer(through, 0);
    if (through > (rows.at(-1)?.message.seq ?? 0)) {
      throw new KernelError("corrupt", "启动边界高于当前手机事实，原件保留");
    }
    const metadata = rows.slice(0, through).map((row) => ({
      seq: row.message.seq,
      key: row.key,
      conversationId: row.message.conversationId,
      notify: this.#notify(index, row, true),
    }));
    return {
      throughSeq: through,
      count: metadata.length,
      notificationCount: metadata.filter((row) => row.notify).length,
      digest: await fingerprint(metadata as unknown as Json),
    };
  }

  async open(args: { conversationId: string }): Promise<OpenedConversation> {
    args = {
      ...args,
      conversationId: this.device?.resolve(args.conversationId) ?? args.conversationId,
    };
    conversation(args.conversationId);
    // open 不接受历史游标，防止将向前翻页误作打开最新一屏。
    if (Object.hasOwn(args, "cursor")) throw new QqPhoneError("request");
    const { screen, earlier } = await this.#historyPage(args.conversationId, undefined, true);
    const displayedSeqs = screen.messages.map((message) => message.handle.seq);
    const reading = displayedSeqs.length
      ? await this.reading.opened(args.conversationId, screen.snapshotThroughSeq, displayedSeqs)
      : this.reading.get(args.conversationId);
    return {
      ...screen,
      notificationThroughSeq: reading?.notificationThroughSeq ?? 0,
      displayedSeqs,
      earlierMessagesNotShown: earlier,
      notice:
        "通知水位仅用于消除本机会话提醒；本屏上方未展示的消息并非真的已读。当前只展示预览，长文和附件仍需主动打开。",
    };
  }

  async #historyPage(id: string, cursor?: HistoryCursor, countEarlier = false) {
    const index = await this.#snapshot();
    const through = cursor?.snapshotThroughSeq ?? index.rows.length;
    if (through > index.rows.length) throw new QqPhoneError("request");
    const page: FactRow[] = [];
    let earlier = 0;
    for (let seq = cursor ? cursor.beforeSeq - 1 : through; seq > 0; seq--) {
      const row = index.rows[seq - 1];
      if (row.message.conversationId !== id) continue;
      if (page.length < SCREEN.pageSize) page.push(row);
      else {
        earlier++;
        if (!countEarlier) break;
      }
    }
    const messages = await this.#present(index, page.reverse(), through);
    const screen = {
      ...historyScreen(messages, id, SCREEN, this.now(), cursor),
      snapshotThroughSeq: through,
      ...(earlier
        ? { older: { snapshotThroughSeq: through, beforeSeq: page[0].message.seq } }
        : {}),
    };
    return { screen, earlier };
  }

  async history(args: { conversationId: string; cursor?: HistoryCursor }) {
    const id = this.device?.resolve(args.conversationId) ?? args.conversationId;
    conversation(id);
    const page = args.cursor === undefined ? undefined : { ...args.cursor };
    cursor(page);
    return (await this.#historyPage(id, page)).screen;
  }

  async #lookup(handle: { conversationId: string; seq: number }) {
    const id = this.device?.resolve(handle?.conversationId) ?? handle?.conversationId;
    conversation(id);
    const seq = handle.seq;
    integer(seq, 1);
    const index = await this.#snapshot();
    const row = index.rows[seq - 1];
    if (row?.message.conversationId !== id) throw new QqPhoneError("not_found");
    return { index, row };
  }

  async #message(handle: { conversationId: string; seq: number }, display = false) {
    const { index, row } = await this.#lookup(handle);
    return display ? (await this.#present(index, [row]))[0] : this.#messageFact(index, row);
  }

  async message(args: {
    handle: { conversationId: string; seq: number };
    attachmentOffset?: number;
  }) {
    return messageScreen(
      await this.#message(args.handle, true),
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
    if (args.conversationId !== undefined) {
      args = {
        ...args,
        conversationId: this.device?.resolve(args.conversationId) ?? args.conversationId,
      };
      conversation(args.conversationId);
    }
    const query = args.query,
      pageCursor = args.cursor === undefined ? undefined : { ...args.cursor };
    cursor(pageCursor);
    const index = await this.#snapshot();
    const through = pageCursor?.snapshotThroughSeq ?? index.rows.length;
    if (through > index.rows.length) throw new QqPhoneError("request");
    const matches = new Map<string, boolean | undefined>();
    const labelMatches = (label: PhoneMessage["conversation"]) =>
      label?.remark?.includes(query) || label?.alias?.includes(query) ||
      label?.platformName?.includes(query);
    const selected: FactRow[] = [];
    let more = false;
    for (let seq = pageCursor ? pageCursor.beforeSeq - 1 : through; seq > 0; seq--) {
      const row = index.rows[seq - 1], id = row.message.conversationId;
      if (args.conversationId !== undefined && args.conversationId !== id) continue;
      if (!matches.has(id)) {
        const label = this.#conversation(index, id);
        matches.set(id, label && !!labelMatches(label));
      }
      const match = matches.get(id);
      const projected = row.command || row.hasEnvelopeLabel && match === undefined
        ? this.#read(index, row).message.message
        : undefined;
      if (
        !(match ?? labelMatches(projected?.conversation)) &&
        !(projected?.text ?? row.message.text).includes(query)
      ) continue;
      if (selected.length === SCREEN.pageSize) {
        more = true;
        break;
      }
      selected.push(row);
    }
    const page = await this.#present(index, selected.reverse(), through);
    return {
      messages: page.map((message) => messageScreen(message, SCREEN, this.now())),
      snapshotThroughSeq: through,
      ...(more
        ? {
          older: { snapshotThroughSeq: through, beforeSeq: page[0].seq },
        }
        : {}),
    };
  }

  async notifications(args: { afterSeq?: number } = {}) {
    const after = args.afterSeq ?? 0;
    integer(after, 0);
    const index = await this.#snapshot();
    if (after > index.rows.length) throw new QqPhoneError("request");
    const watermarks = new Map<string, number>();
    const selected: FactRow[] = [];
    let hasMore = false;
    for (let position = after; position < index.rows.length; position++) {
      const row = index.rows[position], id = row.message.conversationId;
      if (!this.#notify(index, row)) continue;
      if (!watermarks.has(id)) {
        watermarks.set(
          id,
          this.device?.get(id).muted ? Infinity : this.reading.get(id)?.notificationThroughSeq ?? 0,
        );
      }
      if (row.message.seq <= watermarks.get(id)!) continue;
      if (selected.length === SCREEN.pageSize) {
        hasMore = true;
        break;
      }
      selected.push(row);
    }
    const page = await this.#present(index, selected);
    return {
      items: page.map((message) => messageScreen(message, NOTICE, this.now())),
      // 队列尚有未展示通知时，只确认本页末尾；不因快照末尾有更多通知就跳过。
      throughSeq: hasMore ? page.at(-1)!.seq : index.rows.length,
      hasMore,
    };
  }

  async send(
    args: PhoneSend & {
      attachment?: { path: string; kind: "image" | "file" };
      /** 旧调用兼容；不在对外契约里展示。 */
      replyTo?: { seq: number; sequence?: number };
    },
    options: { beforeAttempt?: () => Promise<void> } = {},
  ) {
    args = {
      ...args,
      conversationId: this.device?.resolve(args.conversationId) ?? args.conversationId,
    };
    conversation(args.conversationId);
    const destination = qqDestination(args.conversationId)!;
    if (
      (destination.kind === "channel" || destination.kind === "guild-dm") &&
      (Object.keys(args).some((key) =>
        !["actionId", "conversationId", "text", "attachment", "referenceIndex"].includes(key)
      ) ||
        (args.attachment !== undefined &&
          (args.attachment.kind !== "image" || args.referenceIndex !== undefined)))
    ) {
      throw new QqPhoneError("request");
    }
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
      // 旧调用只验证本会话入站句柄；所有新发送统一主动，不再携带被动锚点或序号。
    }
    const result = await this.actions.send(args.actionId, {
      destination,
      text: args.text,
      ...(args.attachment === undefined ? {} : { attachment: args.attachment }),
      ...(args.referenceIndex === undefined ? {} : { referenceIndex: args.referenceIndex }),
      ...(args.wakeup === undefined ? {} : { wakeup: args.wakeup }),
    }, options);
    // 回执先由发送账本保存；时间线失败不能重发已执行动作。恢复会从原账本补齐引用。
    await this.#snapshot();
    return phoneReceipt(result);
  }

  async sendBurst(
    args: PhoneSendBurst,
    options: { beforeAttempt?: () => Promise<void> } = {},
  ): Promise<PhoneBurstReceipt> {
    // 冻结调用者输入；同一手机内父动作准入串行，发送本身仍交给现有会话FIFO。
    const captured = structuredClone({
      ...args,
      conversationId: this.device?.resolve(args.conversationId) ?? args.conversationId,
    });
    const admission = this.#burstTail.then(async () => {
      const manifest = await admitBurst(this.actions.directory, captured);
      const destination = qqDestination(manifest.conversationId)!;
      // 此处没有await：在任何其它调用有机会准入前，按顺序预留所有子动作。
      const sends = manifest.segments.map((text, index) =>
        this.actions.send(
          manifest.childActionIds[index],
          { destination, text },
          options,
        )
      );
      return { manifest, sends };
    });
    this.#burstTail = admission.then(() => {}, () => {});
    const { manifest, sends } = await admission;
    const statuses = await Promise.all(sends);
    await this.#snapshot();
    const items = statuses.map((status, index) => ({
      index: index + 1,
      actionId: manifest.childActionIds[index],
      receipt: phoneReceipt(status),
    }));
    return {
      actionId: manifest.actionId,
      conversationId: manifest.conversationId,
      items,
      complete: items.every((item) => item.receipt.state === "sent"),
    };
  }

  async burstStatus(args: { actionId: string }): Promise<PhoneBurstStatus | null> {
    if (!args || Object.keys(args).some((key) => key !== "actionId")) {
      throw new QqPhoneError("request");
    }
    const manifest = await readBurst(this.actions.directory, args.actionId);
    if (!manifest) return null;
    const items = manifest.childActionIds.map((actionId, index) => {
      const status = this.actions.status(actionId);
      return { index: index + 1, actionId, receipt: status ? phoneReceipt(status) : null };
    });
    return {
      actionId: manifest.actionId,
      conversationId: manifest.conversationId,
      items,
      complete: items.every((item) => item.receipt?.state === "sent"),
    };
  }

  async waitForMessages(
    args: PhoneWaitForMessages,
    options: { signal?: AbortSignal; beforeCheck?: () => Promise<void> } = {},
  ): Promise<PhoneMessagesWaitResult> {
    args = {
      ...args,
      conversationId: this.device?.resolve(args.conversationId) ?? args.conversationId,
    };
    const result = await waitForQuiet(args, () => {
      if (this.#journal) return this.#journal.latest(args.conversationId);
      const latest = this.incoming().findLast((record) =>
        record.message.message.conversationId === args.conversationId
      );
      return latest ? { seq: latest.seq, receivedAt: latest.receivedAt } : undefined;
    }, { signal: options.signal, checkpoint: options.beforeCheck });
    const { rows } = await this.#snapshot();
    const lastIncoming = result.lastSourceSeq === undefined
      ? undefined
      : rows.find((row) => row.key === `incoming:${result.lastSourceSeq}`);
    return {
      conversationId: args.conversationId,
      reason: result.reason,
      waitedMs: result.waitedMs,
      throughSeq: rows.at(-1)?.message.seq ?? 0,
      ...(lastIncoming ? { lastIncomingSeq: lastIncoming.message.seq } : {}),
    };
  }

  action(args: { actionId: string }) {
    const result = this.actions.status(args.actionId);
    return result ? phoneReceipt(result) : null;
  }
  async listSendQueue(
    args: { conversationId?: string; afterSequence?: number; limit?: number } = {},
  ) {
    if (args.conversationId !== undefined) {
      args = {
        ...args,
        conversationId: this.device?.resolve(args.conversationId) ?? args.conversationId,
      };
    }
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
    args: {
      conversationId: string;
      muted?: boolean;
      pinned?: boolean;
      draft?: string;
      remark?: string;
      alias?: string | null;
    },
  ) {
    args = {
      ...args,
      conversationId: this.device?.resolve(args.conversationId) ?? args.conversationId,
    };
    conversation(args.conversationId);
    if (!this.device) throw new KernelError("unavailable", "手机设置存储未接入");
    if (
      Object.keys(args).some((key) =>
        !["conversationId", "muted", "pinned", "draft", "remark", "alias"].includes(key)
      ) ||
      (args.remark !== undefined &&
        (typeof args.remark !== "string" || Array.from(args.remark).length > 128 ||
          /[\p{Cc}\p{Cf}]/u.test(args.remark))) ||
      (args.muted !== undefined && typeof args.muted !== "boolean") ||
      (args.pinned !== undefined && typeof args.pinned !== "boolean") ||
      (args.draft !== undefined &&
        (typeof args.draft !== "string" || new TextEncoder().encode(args.draft).length > 65_536))
    ) throw new QqPhoneError("request");
    const patch = {
      ...(args.remark === undefined ? {} : { remark: args.remark }),
      ...(args.alias === undefined ? {} : { alias: args.alias }),
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
    const { index, row } = await this.#lookup(args.handle);
    const original = row.incomingSeq === undefined ? undefined : index.read(row.incomingSeq);
    const message = original?.message.message;
    if (!message || message.direction !== "incoming" || !message.attachments?.[args.index - 1]) {
      throw new QqPhoneError("not_found");
    }
    const raw = original?.raw.d as { attachments?: { url?: unknown }[] } | undefined;
    const url = raw?.attachments?.[args.index - 1]?.url;
    if (typeof url !== "string") {
      throw new KernelError("unavailable", "平台没有提供附件下载地址，原始元信息仍保留");
    }
    return await this.downloads.download({
      kind: "phone_attachment",
      conversationId: message.conversationId,
      sourceSeq: row.message.seq,
      attachmentId: `${message.conversationId}:${row.message.seq}:${args.index}`,
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

  async sendFile(
    args: PhoneSendFile & { replyTo?: { seq: number; sequence?: number } },
    options: { beforeAttempt?: () => Promise<void> } = {},
  ) {
    const { path, kind, ...argsWithoutFile } = args;
    return await this.send({
      ...argsWithoutFile,
      text: args.text ?? "",
      attachment: { path, kind },
    }, options);
  }
}
