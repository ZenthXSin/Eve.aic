import assert from "node:assert/strict";
import type { HistoryScreen, MessageScreen } from "../contracts/phone.ts";
import type { Json } from "../kernel/protocol.ts";
import { focusArgs, validateFocusEvidence } from "../plugins/eve/continuity.ts";
import type { ConsciousEvent } from "../plugins/eve/journal.ts";
import {
  assessOutreach,
  conversationArgs,
  socialContext,
  updateConversation,
} from "../plugins/eve/social.ts";

const now = Date.parse("2026-11-16T05:00:00Z");
const conversationId = "qq:user:friend";
function append(events: ConsciousEvent[], kind: string, data: unknown) {
  const event: ConsciousEvent = {
    schema: 1,
    seq: events.length + 1,
    at: new Date(now).toISOString(),
    kind,
    data: data as Json,
    previous: "0".repeat(64),
    digest: "a".repeat(64),
  };
  events.push(event);
  return event;
}
const focus = {
  id: "stars",
  title: "星星",
  state: "active",
  reason: "想接着弄明白",
  next: "核对新资料",
  origin: "self",
  confidence: 0.5,
  evidence: [1],
};
function events() {
  const result: ConsciousEvent[] = [];
  append(result, "thought", { text: "昨天讨论星星" });
  append(result, "focus", { items: [focus] });
  return result;
}
function message(
  seq: number,
  direction: "incoming" | "outgoing",
  text: string,
  state: "sent" | "unknown" = "sent",
  at = now - 3600000,
): MessageScreen {
  return {
    handle: { conversationId, seq },
    direction,
    sender: {
      platformUserId: "friend",
      displayName: { text: "朋友", characters: 2, truncated: false },
    },
    time: { status: "known", at: new Date(at).toISOString() },
    body: { text, characters: text.length, truncated: false },
    attachments: [],
    attachmentCount: 0,
    ...(direction === "outgoing" ? { delivery: { actionId: `sent_${seq}`, state } } : {}),
  };
}
function history(...messages: MessageScreen[]): HistoryScreen {
  return {
    messages,
    snapshotThroughSeq: Math.max(...messages.map((m) => m.handle.seq)),
    readThroughSeq: Math.max(...messages.map((m) => m.handle.seq)),
  };
}
const closed = {
  expectedSeq: 0,
  conversationId,
  throughSeq: 20,
  mode: "close",
  summary: "昨天的问题已聊完",
  nextOpening: "有新发现再接",
  pending: [],
};
const outreach = {
  conversationId,
  throughSeq: 20,
  focusId: "stars",
  angle: "新的观测结果",
  text: "昨天那个有新发现了",
};
const answered = () =>
  history(message(19, "incoming", "昨天的问题呢"), message(20, "outgoing", "找到了"));

Deno.test("重心反馈：旧记录不能重复增加把握，新证据只影响所选项目", () => {
  const e = events();
  assert.throws(
    () =>
      validateFocusEvidence(
        focusArgs({ expectedSeq: 2, items: [{ ...focus, confidence: 0.8 }] }).items,
        e,
      ),
    /重复旧依据/,
  );
  append(e, "thought", { text: "新资料有了真实进展" });
  validateFocusEvidence(
    focusArgs({ expectedSeq: 2, items: [{ ...focus, confidence: 0.8, evidence: [1, 3] }] }).items,
    e,
  );
  assert.throws(
    () =>
      validateFocusEvidence(
        focusArgs({ expectedSeq: 2, items: [{ ...focus, evidence: [99] }] }).items,
        e,
      ),
    /真实意识记录/,
  );
  assert.throws(
    () =>
      focusArgs({
        expectedSeq: 2,
        items: [{
          ...focus,
          feedback: { kind: "welcome", note: "好", message: { conversationId, seq: 0 } },
        }],
      }),
    /真实消息句柄/,
  );
  append(e, "upgrade_rollback", { afterSeq: 2, throughSeq: 3 });
  assert.throws(
    () =>
      validateFocusEvidence(
        focusArgs({ expectedSeq: 2, items: [{ ...focus, evidence: [3] }] }).items,
        e,
      ),
    /真实意识记录/,
  );
});

Deno.test("自然收尾：真实sent、未回答事项、新水位及简短道别", () => {
  const e = events(), args = conversationArgs(closed);
  assert.throws(
    () => updateConversation(e, args, history(message(20, "incoming", "为什么还是不行")), now),
    /尚无sent回复/,
  );
  assert.throws(
    () => updateConversation(e, args, history(message(20, "incoming", "好？")), now),
    /尚无sent回复/,
  );
  assert.throws(
    () => updateConversation(e, args, history(message(20, "incoming", "谢谢，顺便再查一下")), now),
    /尚无sent回复/,
  );
  assert.throws(
    () =>
      updateConversation(
        e,
        args,
        history(message(19, "incoming", "帮我查"), message(20, "outgoing", "完成", "unknown")),
        now,
      ),
    /未确认送达/,
  );
  assert.throws(
    () => updateConversation(e, { ...args, pending: ["还有问题没回答"] }, answered(), now),
    /待回答/,
  );
  assert.throws(
    () => updateConversation(e, args, history(message(21, "incoming", "等下")), now),
    /水位不匹配/,
  );
  assert.equal(updateConversation(e, args, answered(), now)[0].mode, "close");
  assert.equal(
    updateConversation(e, args, history(message(20, "incoming", "晚安")), now)[0].nextOpening,
    args.nextOpening,
  );
});

Deno.test("聊天接续：有界快照与版本冲突，压缩保留，回滚不泄漏未确认想法", () => {
  const e = events();
  const items = updateConversation(e, conversationArgs(closed), answered(), now);
  const saved = append(e, "conversation_state", { items });
  append(e, "compression", { throughSeq: saved.seq, summary: "旧经历" });
  assert.ok(JSON.stringify(socialContext(e)).includes("有新发现再接"));
  assert.throws(
    () => updateConversation(e, conversationArgs(closed), answered(), now),
    /版本已变化/,
  );
  const many = Array.from(
    { length: 12 },
    (_, i) => ({ ...items[0], conversationId: `other_${i}` }),
  );
  const newer = append(e, "conversation_state", { items: many });
  assert.equal(
    updateConversation(e, { ...conversationArgs(closed), expectedSeq: newer.seq }, answered(), now)
      .length,
    12,
  );
  append(e, "upgrade_rollback", { afterSeq: 2, throughSeq: newer.seq });
  assert.deepEqual(socialContext(e), {});
});

Deno.test("主动搭话：只用真实重心，夜间、冷却、频率与未答消息延后", () => {
  const e = events();
  assert.equal(assessOutreach(e, outreach, answered(), now).status, "ready");
  assert.equal(
    assessOutreach(e, outreach, answered(), Date.parse("2026-11-16T16:00:00Z")).status,
    "deferred",
  );
  assert.equal(
    assessOutreach(e, { ...outreach, focusId: "invented" }, answered(), now).status,
    "deferred",
  );
  assert.equal(
    assessOutreach(e, outreach, history(message(20, "incoming", "还有个问题")), now).status,
    "deferred",
  );
  assert.equal(
    assessOutreach(e, outreach, history(message(20, "outgoing", "发完", "sent", now - 60000)), now)
      .status,
    "deferred",
  );
  append(e, "conversation_state", {
    items: updateConversation(e, conversationArgs(closed), answered(), now - 60000),
  });
  assert.equal(assessOutreach(e, outreach, answered(), now).status, "deferred");
  const pending = [{
    ...conversationArgs(closed),
    mode: "linger",
    pending: ["等他补充"],
    at: new Date(now - 3600000).toISOString(),
  }];
  append(e, "conversation_state", { items: pending });
  assert.equal(assessOutreach(e, outreach, answered(), now).status, "deferred");
});

Deno.test("主动搭话：无人回应不超过两次，拒绝只作用于对应对象", () => {
  const e = events();
  const entry = (angle: string, hours: number) => ({
    actionId: angle,
    conversationId,
    focusId: "stars",
    angle,
    throughSeq: 20,
    at: new Date(now - hours * 3600000).toISOString(),
  });
  append(e, "outreach_start", { items: [entry("昨天的话题", 8), entry("同一个问题", 4)] });
  assert.equal(assessOutreach(e, outreach, answered(), now).status, "deferred");
  const replied = history(message(21, "incoming", "好呀"), message(22, "outgoing", "嗯"));
  assert.equal(assessOutreach(e, { ...outreach, throughSeq: 22 }, replied, now).status, "ready");
  assert.equal(
    assessOutreach(e, { ...outreach, throughSeq: 22, angle: "同一个问题" }, replied, now).status,
    "deferred",
  );
  append(e, "focus", {
    items: [{
      ...focus,
      feedback: {
        kind: "declined",
        note: "对方不想继续这个话题",
        message: { conversationId, seq: 21 },
      },
    }],
  });
  assert.equal(assessOutreach(e, { ...outreach, throughSeq: 22 }, replied, now).status, "deferred");
  const other = history(
    ...replied.messages.map((m) => ({ ...m, handle: { ...m.handle, conversationId: "other" } })),
  );
  assert.equal(
    assessOutreach(e, { ...outreach, conversationId: "other", throughSeq: 22 }, other, now).status,
    "ready",
  );
});

Deno.test("主动搭话：全局间隔和每日上限持久计算，未知时间不能视为安静", () => {
  const e = events();
  const entry = (hours: number) => ({
    actionId: String(hours),
    conversationId: "other",
    focusId: "stars",
    angle: String(hours),
    throughSeq: 1,
    at: new Date(now - hours * 3600000).toISOString(),
  });
  append(e, "outreach_start", { items: [entry(1)] });
  const gate = assessOutreach(e, outreach, answered(), now);
  assert.equal(gate.status, "deferred");
  assert.ok("nextEligibleAt" in gate);
  append(e, "outreach_start", { items: [entry(12), entry(9), entry(6), entry(3)] });
  assert.equal(assessOutreach(e, outreach, answered(), now).status, "deferred");
  const unknown = answered();
  unknown.messages[0].time = { status: "missing" };
  assert.equal(assessOutreach(events(), outreach, unknown, now).status, "deferred");
});
