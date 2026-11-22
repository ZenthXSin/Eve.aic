import type { ToolSpec } from "../../contracts/model.ts";
import type { HistoryScreen } from "../../contracts/phone.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { FocusItem } from "./continuity.ts";
import type { ConsciousEvent } from "./journal.ts";
import { lastActiveEvent } from "./view.ts";

export interface ConversationState {
  conversationId: string;
  throughSeq: number;
  mode: "continue" | "linger" | "close";
  summary: string;
  nextOpening: string;
  pending: string[];
  at: string;
}
export interface OutreachEntry {
  actionId: string;
  conversationId: string;
  focusId: string;
  angle: string;
  throughSeq: number;
  at: string;
}
const string = (v: unknown, max: number, empty = false): v is string =>
  typeof v === "string" && (empty || !!v.trim()) && v.length <= max;
function object(v: Json, keys: string[]): Record<string, Json> {
  if (
    !v || typeof v !== "object" || Array.isArray(v) ||
    Object.keys(v).some((key) => !keys.includes(key))
  ) {
    throw new KernelError("rejected", "参数需要契约规定的对象字段");
  }
  return v;
}
const sequence = (v: Json | undefined, min = 0) => Number.isSafeInteger(v) && Number(v) >= min;
const conversationProperty = { type: "string", minLength: 1, maxLength: 256 };

export const CONVERSATION_TOOL: ToolSpec = {
  name: "set_conversation",
  description:
    "记录一段聊天的继续、等待或自然收尾及下次接话线索，跨压缩与重启保留；不发告别消息、不关闭通知、不需要额外观察轮次。无变化不调用。expectedSeq用conversations.record，首次0；throughSeq用刚读到的本会话最高消息编号。close须没有待回答事项且确有sent回复或对方只有简短道别/附和，陈旧水位和未回答的实质消息会拒绝。对方再找你仍照常唤醒。",
  parameters: {
    type: "object",
    properties: {
      expectedSeq: { type: "integer", minimum: 0 },
      conversationId: conversationProperty,
      throughSeq: { type: "integer", minimum: 1 },
      mode: { type: "string", enum: ["continue", "linger", "close"] },
      summary: { type: "string", minLength: 1, maxLength: 600 },
      nextOpening: { type: "string", maxLength: 300 },
      pending: {
        type: "array",
        maxItems: 5,
        items: { type: "string", minLength: 1, maxLength: 300 },
      },
    },
    required: [
      "expectedSeq",
      "conversationId",
      "throughSeq",
      "mode",
      "summary",
      "nextOpening",
      "pending",
    ],
    additionalProperties: false,
  },
};
export function conversationArgs(raw: Json) {
  const a = object(raw, [
    "expectedSeq",
    "conversationId",
    "throughSeq",
    "mode",
    "summary",
    "nextOpening",
    "pending",
  ]);
  if (
    !sequence(a.expectedSeq) || !string(a.conversationId, 256) || !sequence(a.throughSeq, 1) ||
    !["continue", "linger", "close"].includes(String(a.mode)) || !string(a.summary, 600) ||
    !string(a.nextOpening, 300, true) || !Array.isArray(a.pending) || a.pending.length > 5 ||
    !a.pending.every((v) => string(v, 300))
  ) {
    throw new KernelError(
      "rejected",
      "聊天接续需要版本、真实会话水位、状态、摘要、下次线索和待回应事项",
    );
  }
  return a as unknown as Omit<ConversationState, "at"> & { expectedSeq: number };
}
export function conversationStates(events: ConsciousEvent[]) {
  const event = lastActiveEvent(events, "conversation_state");
  return {
    record: event?.seq ?? 0,
    items: (event?.data as unknown as { items: ConversationState[] })?.items ?? [],
  };
}
function latest(history: HistoryScreen, conversationId: string, throughSeq: number) {
  const messages = history.messages;
  if (
    !Array.isArray(messages) || !messages.length ||
    messages.some((m) => m.handle.conversationId !== conversationId) ||
    Math.max(...messages.map((m) => m.handle.seq)) !== throughSeq
  ) {
    throw new KernelError("rejected", "会话有新消息或水位不匹配，先读最新一屏再决定");
  }
  return messages;
}
function isAcknowledgement(message: HistoryScreen["messages"][number]) {
  return !message.body.truncated && !message.attachmentCount &&
    /^(?:好|好的|好呀|好哒|嗯|嗯嗯|哦|噢|ok|okay|谢谢|谢啦|晚安|拜拜|再见|不用回了|不用回复了)[。！!～~\s]*$/iu
      .test(message.body.text.trim());
}
export function updateConversation(
  events: ConsciousEvent[],
  args: ReturnType<typeof conversationArgs>,
  history: HistoryScreen,
  now = Date.now(),
) {
  const state = conversationStates(events);
  if (args.expectedSeq !== state.record) {
    throw new KernelError("rejected", "聊天接续版本已变化，请读当前conversations.record");
  }
  const messages = latest(history, args.conversationId, args.throughSeq);
  if (args.mode === "close") {
    if (args.pending.length) {
      throw new KernelError("rejected", "还有待回答事项，先回答或保留等待状态");
    }
    const incoming = messages.filter((m) =>
      m.direction === "incoming"
    ).sort((a, b) => b.handle.seq - a.handle.seq)[0];
    const sent = messages.filter((m) =>
      m.direction === "outgoing" && m.delivery?.state === "sent"
    ).sort((a, b) =>
      b.handle.seq - a.handle.seq
    )[0];
    // Only a small complete acknowledgement can finish without another reply.
    // Never classify arbitrary later text as feedback, an answer or a goodbye.
    const acknowledgement = incoming && isAcknowledgement(incoming);
    if (
      messages.some((m) =>
        m.direction === "outgoing" && m.handle.seq > (sent?.handle.seq ?? 0) &&
        m.delivery?.state !== "sent"
      )
    ) {
      throw new KernelError("rejected", "还有未确认送达的消息，先核对真实回执再收尾");
    }
    if (incoming && (!sent || sent.handle.seq < incoming.handle.seq) && !acknowledgement) {
      throw new KernelError(
        "rejected",
        "最新实质消息尚无sent回复，不能用收尾跳过；先回应或保留等待状态",
      );
    }
  }
  const { expectedSeq: _expectedSeq, ...item } = args;
  return [...state.items.filter((s) => s.conversationId !== args.conversationId), {
    ...item,
    at: new Date(now).toISOString(),
  }].slice(-12);
}

export const OUTREACH_TOOL: ToolSpec = {
  name: "start_conversation",
  description:
    "从真实近期重心主动搭话，直接用现有QQ主动发送，不分段。需要先读目标会话最新一屏，focusId来自active重心，angle简短写新角度，text是自然短消息。无自然话题就安静，不为用工具而找话。内置夜间安静、半小时无新消息、全局3小时间隔、每日最多4次、收尾冷却、相同角度不重复及无人回应最多2次；明确拒绝或过期重心不能发。deferred不是失败，按nextEligibleAt或现有休息等下次机会，别绕过用phone.send主动追问。回复新消息、任务结果通知仍用普通phone.send，不受此搭话节流。以receipt.state=sent为准，unknown检查原actionId，不换号重发。",
  parameters: {
    type: "object",
    properties: {
      conversationId: conversationProperty,
      throughSeq: { type: "integer", minimum: 1 },
      focusId: { type: "string", minLength: 1, maxLength: 64 },
      angle: { type: "string", minLength: 1, maxLength: 160 },
      text: { type: "string", minLength: 1, maxLength: 1000 },
    },
    required: ["conversationId", "throughSeq", "focusId", "angle", "text"],
    additionalProperties: false,
  },
};
export function outreachArgs(raw: Json) {
  const a = object(raw, ["conversationId", "throughSeq", "focusId", "angle", "text"]);
  if (
    !string(a.conversationId, 256) || !sequence(a.throughSeq, 1) || !string(a.focusId, 64) ||
    !string(a.angle, 160) || !string(a.text, 1000)
  ) throw new KernelError("rejected", "主动搭话需要真实会话水位、重心、新角度和正文");
  return a as unknown as {
    conversationId: string;
    throughSeq: number;
    focusId: string;
    angle: string;
    text: string;
  };
}
export function outreachEntries(events: ConsciousEvent[]): OutreachEntry[] {
  return (lastActiveEvent(events, "outreach_start")?.data as unknown as { items: OutreachEntry[] })
    ?.items ?? [];
}
function localTime(now: number, timeZone: string) {
  const parts = Object.fromEntries(
    new Intl.DateTimeFormat("en-CA", {
      timeZone,
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      hourCycle: "h23",
    }).formatToParts(now).map((p) => [p.type, p.value]),
  );
  return {
    day: `${parts.year}-${parts.month}-${parts.day}`,
    minutes: Number(parts.hour) * 60 + Number(parts.minute),
  };
}
export function assessOutreach(
  events: ConsciousEvent[],
  args: ReturnType<typeof outreachArgs>,
  history: HistoryScreen,
  now = Date.now(),
  timeZone = "Asia/Hong_Kong",
) {
  const deferred = (reason: string, next?: number) => ({
    status: "deferred" as const,
    reason,
    ...(next !== undefined ? { nextEligibleAt: new Date(next).toISOString() } : {}),
  });
  const messages = latest(history, args.conversationId, args.throughSeq);
  const incoming =
    messages.filter((m) => m.direction === "incoming").sort((a, b) =>
      b.handle.seq - a.handle.seq
    )[0];
  const sent =
    messages.filter((m) => m.direction === "outgoing" && m.delivery?.state === "sent").sort((
      a,
      b,
    ) => b.handle.seq - a.handle.seq)[0];
  if (
    messages.some((m) =>
      m.direction === "outgoing" && m.handle.seq > (sent?.handle.seq ?? 0) &&
      m.delivery?.state !== "sent"
    )
  ) return deferred("有未确认送达的消息，先核对原动作");
  if (
    incoming && (!sent || incoming.handle.seq > sent.handle.seq) && !isAcknowledgement(incoming)
  ) return deferred("最后的实质消息还没回应，先回答而不是另开话题");
  const focus = (lastActiveEvent(events, "focus")?.data as unknown as { items: FocusItem[] })?.items
    .find((f) => f.id === args.focusId);
  if (
    !focus || focus.state !== "active" ||
    (focus.feedback?.message.conversationId === args.conversationId &&
      ["declined", "redirected"].includes(focus.feedback.kind)) ||
    focus.expiresAt && Date.parse(focus.expiresAt) <= now
  ) {
    return deferred("重心不存在、已搁置、过期或被明确拒绝/转向；先尊重现状");
  }
  const clock = localTime(now, timeZone);
  if (clock.minutes >= 23 * 60 + 30 || clock.minutes < 8 * 60 + 30) {
    return deferred("这会儿是夜间安静时间");
  }
  const entries = outreachEntries(events);
  if (entries.filter((e) => localTime(Date.parse(e.at), timeZone).day === clock.day).length >= 4) {
    return deferred("今天已主动搭话4次，留到另一天");
  }
  const last = entries.at(-1);
  if (last && Date.parse(last.at) + 3 * 3600000 > now) {
    return deferred("离上次主动搭话还太近", Date.parse(last.at) + 3 * 3600000);
  }
  const conversation = conversationStates(events).items.find((s) =>
    s.conversationId === args.conversationId
  );
  if (conversation?.pending.length || conversation?.mode === "continue") {
    return deferred("这段还有待回应的内容，先接当前聊天");
  }
  if (conversation?.mode === "close" && Date.parse(conversation.at) + 20 * 60000 > now) {
    return deferred("刚自然收尾，先让话题停一会儿", Date.parse(conversation.at) + 20 * 60000);
  }
  const times = messages.map((m) => m.time.status === "known" ? Date.parse(m.time.at ?? "") : NaN);
  if (times.some((t) => !Number.isFinite(t))) {
    return deferred("当前一屏时间不完整，不能把未知当作无人说话");
  }
  const lastAt = Math.max(...times);
  if (lastAt + 30 * 60000 > now) {
    return deferred("最近还有消息，先接现场或等它自然安静", lastAt + 30 * 60000);
  }
  const incomingSeq = Math.max(
    0,
    ...messages.filter((m) => m.direction === "incoming").map((m) => m.handle.seq),
  );
  const same = entries.filter((e) =>
    e.conversationId === args.conversationId && e.focusId === args.focusId
  );
  if (
    same.some((e) => e.angle.trim().toLocaleLowerCase() === args.angle.trim().toLocaleLowerCase())
  ) return deferred("这个角度已经主动提过，有新发现再说");
  if (same.filter((e) => e.throughSeq >= incomingSeq).length >= 2) {
    return deferred("这个话题已主动提两次没人接，先放下，不再追问");
  }
  return { status: "ready" as const };
}
export function socialContext(events: ConsciousEvent[]) {
  const conversations = conversationStates(events), outreach = outreachEntries(events);
  return {
    ...(conversations.record
      ? {
        conversations: {
          ...conversations,
          guidance: "聊天接续：close只结束那一段，新消息照常回应；不补收尾句，无变化不改写。",
        },
      }
      : {}),
    ...(outreach.length
      ? {
        outreach: {
          recent: outreach.slice(-4),
          guidance: "主动搭话尝试，不证明送达或赞同；真实反馈才调整对应重心，沉默先放下。",
        },
      }
      : {}),
  };
}
