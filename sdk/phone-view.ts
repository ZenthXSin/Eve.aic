// 纯屏幕投影：不改入站原文、不标已读、不发消息、不触发模型。
import type {
  HistoryCursor,
  HistoryScreen,
  MessageScreen,
  PhoneMessage,
  ScreenPolicy,
  TextPreview,
} from "../contracts/phone.ts";

function integer(value: number, min: number, max: number, label: string): void {
  if (!Number.isSafeInteger(value) || value < min || value > max) {
    throw new TypeError(`${label} 超出范围`);
  }
}

function id(value: string, label: string): void {
  if (typeof value !== "string" || value.length === 0 || value.length > 512) {
    throw new TypeError(`${label} 必须是非空短标识`);
  }
}

function policyValid(policy: ScreenPolicy): void {
  integer(policy.pageSize, 1, 20, "pageSize");
  integer(policy.previewCharacters, 1, 1024, "previewCharacters");
  integer(policy.attachmentPageSize, 1, 8, "attachmentPageSize");
}

/** 截取原文字片段并计数，只为输出片段分配内存。偏移也以 Unicode 码点为单位。 */
function excerpt(text: string, start: number, limit: number): TextPreview {
  if (typeof text !== "string") throw new TypeError("消息正文必须是字符串");
  const parts: string[] = [];
  let count = 0;
  for (const character of text) {
    if (count >= start && count - start < limit) parts.push(character);
    count++;
  }
  return { text: parts.join(""), characters: count, truncated: start + parts.length < count };
}

function timeView(sentAt: string | undefined, now: number): MessageScreen["time"] {
  if (sentAt === undefined) return { status: "missing" };
  // 必须带时区，不能把运行机器的本地时区当成平台事实。
  if (typeof sentAt !== "string") return { status: "invalid" };
  const fields = /^(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)(?:\.\d{1,3})?(?:Z|[+-]\d\d:\d\d)$/
    .exec(sentAt);
  if (!fields) return { status: "invalid" };
  const [year, month, day, hour, minute, second] = fields.slice(1).map(Number);
  const calendar = new Date(0);
  calendar.setUTCFullYear(year, month - 1, day);
  calendar.setUTCHours(hour, minute, second, 0);
  if (
    calendar.getUTCFullYear() !== year || calendar.getUTCMonth() + 1 !== month ||
    calendar.getUTCDate() !== day || calendar.getUTCHours() !== hour ||
    calendar.getUTCMinutes() !== minute || calendar.getUTCSeconds() !== second
  ) return { status: "invalid" };
  const milliseconds = Date.parse(sentAt);
  if (!Number.isFinite(milliseconds)) return { status: "invalid" };
  const at = new Date(milliseconds).toISOString();
  if (milliseconds > now) return { status: "future", at };
  const seconds = Math.floor((now - milliseconds) / 1000);
  const age = seconds < 60
    ? `${seconds}s`
    : seconds < 3600
    ? `${Math.floor(seconds / 60)}m`
    : seconds < 86400
    ? `${Math.floor(seconds / 3600)}h`
    : `${Math.floor(seconds / 86400)}d`;
  return { status: "known", at, age };
}

function messageValid(message: PhoneMessage): void {
  integer(message.seq, 1, Number.MAX_SAFE_INTEGER, "消息 seq");
  id(message.conversationId, "conversationId");
  id(message.sender.platformUserId, "platformUserId");
  if (message.platformMessageId !== undefined) id(message.platformMessageId, "platformMessageId");
  if (message.quote) id(message.quote.platformMessageId, "引用编号");
  if (message.referenceIndex !== undefined) id(message.referenceIndex, "平台引用索引");
  if (message.reference) {
    id(message.reference.index, "被引用索引");
    if (message.reference.target) {
      if (message.reference.target.conversationId !== message.conversationId) {
        throw new TypeError("引用对象必须属于本会话");
      }
      integer(message.reference.target.seq, 1, Number.MAX_SAFE_INTEGER, "引用目标 seq");
    }
  }
  if (message.direction !== "incoming" && message.direction !== "outgoing") {
    throw new TypeError("direction 不合法");
  }
}

export function messageScreen(
  message: PhoneMessage,
  policy: ScreenPolicy,
  now: number,
  attachmentOffset = 0,
): MessageScreen {
  policyValid(policy);
  integer(now, 0, 8_640_000_000_000_000, "now");
  integer(attachmentOffset, 0, Number.MAX_SAFE_INTEGER, "attachmentOffset");
  messageValid(message);
  const resources = message.attachments ?? [];
  const attachments = resources.slice(
    attachmentOffset,
    attachmentOffset + policy.attachmentPageSize,
  ).map((resource, position) => {
    if (!["image", "file", "audio", "video", "other"].includes(resource.kind)) {
      throw new TypeError("附件类型不合法");
    }
    if (resource.sizeBytes !== undefined) {
      integer(resource.sizeBytes, 0, Number.MAX_SAFE_INTEGER, "附件大小");
    }
    if (resource.durationMs !== undefined) {
      integer(resource.durationMs, 0, Number.MAX_SAFE_INTEGER, "附件时长");
    }
    return {
      index: attachmentOffset + position + 1,
      kind: resource.kind,
      ...(resource.name === undefined ? {} : { name: excerpt(resource.name, 0, 100) }),
      ...(resource.sizeBytes === undefined ? {} : { sizeBytes: resource.sizeBytes }),
      ...(resource.durationMs === undefined ? {} : { durationMs: resource.durationMs }),
      ...(resource.download === undefined ? {} : { download: structuredClone(resource.download) }),
    };
  });
  // 显式构造输出，不把任意原始字段、资源 URL 或伪造的 isOwner 传给模型。
  return {
    handle: { conversationId: message.conversationId, seq: message.seq },
    ...(message.platformMessageId === undefined ? {} : {
      platformMessageId: message.platformMessageId,
    }),
    sender: {
      platformUserId: message.sender.platformUserId,
      displayName: excerpt(message.sender.displayName, 0, 100),
    },
    direction: message.direction,
    time: timeView(message.sentAt, now),
    body: excerpt(message.text, 0, policy.previewCharacters),
    ...(message.quote ? { quote: { platformMessageId: message.quote.platformMessageId } } : {}),
    ...(message.referenceIndex === undefined ? {} : { referenceIndex: message.referenceIndex }),
    ...(message.reference
      ? {
        reference: {
          index: message.reference.index,
          ...(message.reference.target ? { target: { ...message.reference.target } } : {}),
        },
      }
      : {}),
    attachments,
    attachmentCount: resources.length,
    ...(message.delivery ? { delivery: structuredClone(message.delivery) } : {}),
  };
}

/** 最新一屏与向前翻页；输入是适配器查询所得记录，不是全量日志存储实现。 */
export function historyScreen(
  records: readonly PhoneMessage[],
  conversationId: string,
  policy: ScreenPolicy,
  now: number,
  cursor?: HistoryCursor,
): HistoryScreen {
  policyValid(policy);
  id(conversationId, "conversationId");
  integer(now, 0, 8_640_000_000_000_000, "now");
  let previous = 0;
  for (const record of records) {
    integer(record.seq, 1, Number.MAX_SAFE_INTEGER, "消息 seq");
    if (record.seq <= previous) throw new TypeError("日志编号必须唯一并按升序提供");
    previous = record.seq;
  }
  if (cursor) {
    integer(cursor.snapshotThroughSeq, 0, Number.MAX_SAFE_INTEGER, "快照水位");
    integer(cursor.beforeSeq, 1, Number.MAX_SAFE_INTEGER, "翻页水位");
    if (cursor.beforeSeq > cursor.snapshotThroughSeq) throw new TypeError("翻页水位不在快照中");
  }
  const snapshotThroughSeq = cursor?.snapshotThroughSeq ?? previous;
  const eligible = records.filter((record) =>
    record.conversationId === conversationId && record.seq <= snapshotThroughSeq &&
    (cursor === undefined || record.seq < cursor.beforeSeq)
  );
  const page = eligible.slice(-policy.pageSize);
  const last = page.at(-1);
  return {
    messages: page.map((record) => messageScreen(record, policy, now)),
    snapshotThroughSeq,
    ...(last ? { readThroughSeq: last.seq } : {}),
    ...(eligible.length > page.length
      ? {
        older: { snapshotThroughSeq, beforeSeq: page[0].seq },
      }
      : {}),
  };
}

/** 点开长消息后的有界文本页；引用/附件仍通过原消息句柄定位。 */
export function messageTextPage(message: PhoneMessage, startChar: number, limit: number) {
  messageValid(message);
  integer(startChar, 0, Number.MAX_SAFE_INTEGER, "文本偏移");
  integer(limit, 1, 1024, "文本页大小");
  const body = excerpt(message.text, startChar, limit);
  const next = startChar + Array.from(body.text).length;
  return {
    handle: { conversationId: message.conversationId, seq: message.seq },
    startChar,
    ...body,
    ...(body.truncated ? { nextChar: next } : {}),
  };
}
