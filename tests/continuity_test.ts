import assert from "node:assert/strict";
import type { Message, ModelResponse } from "../contracts/model.ts";
import type { HistoryScreen, PhoneActionReceipt, PhoneSend } from "../contracts/phone.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import {
  continuityMessage,
  focusArgs,
  pendingRestReview,
  restArgs,
} from "../plugins/eve/continuity.ts";

const focus = {
  id: "learn",
  title: "研究星星",
  state: "active",
  reason: "自己感兴趣",
  next: "读完资料再决定",
};
const decision = { reason: "先歇一会儿", unfinished: ["资料尚未读完"], resume: "先看资料的新发现" };
const response = (name?: string, args: Json = {}): ModelResponse => ({
  content: name ? "" : "今天先放着，醒来再看资料",
  toolCalls: name ? [{ id: crypto.randomUUID(), name, args }] : [],
});
const socialHistory: HistoryScreen = {
  snapshotThroughSeq: 20,
  readThroughSeq: 20,
  messages: [{
    handle: { conversationId: "qq:user:friend", seq: 20 },
    direction: "incoming",
    sender: {
      platformUserId: "friend",
      displayName: { text: "朋友", characters: 2, truncated: false },
    },
    time: { status: "known", at: "2026-11-16T00:00:00Z" },
    body: { text: "晚安", characters: 2, truncated: false },
    attachments: [],
    attachmentCount: 0,
  }],
};
async function fixture(
  responses: ModelResponse[],
  phone: { history?: HistoryScreen; receipt?: PhoneActionReceipt } = {},
) {
  const directory = await Deno.makeTempDir({ prefix: "eve-continuity-" });
  const promptFile = `${directory}/prompt.md`;
  await Deno.writeTextFile(promptFile, "稳定身份");
  const messages: Message[][] = [];
  const sends: PhoneSend[] = [];
  let phoneSeq = 0;
  const mindEvents: Json[] = [];
  let completed = false;
  const ctx = {
    call: (namespace: string, method: string, args: Json = {}) => {
      if (namespace === "model") {
        messages.push((args as unknown as { messages: Message[] }).messages);
        const next = responses.shift();
        assert.ok(next, "不应空转或无限追加睡前检查");
        return Promise.resolve(next);
      }
      if (namespace === "tools") {
        return Promise.resolve(
          method === "list"
            ? [{ name: "act", description: "真实动作", parameters: { type: "object" } }]
            : { status: "completed", output: "done" },
        );
      }
      if (namespace === "mind" && method === "update") return Promise.resolve(null);
      if (namespace === "mind" && method === "poll") {
        return Promise.resolve({
          items: mindEvents.filter((e) =>
            Number((e as { seq: number }).seq) > Number((args as { afterSeq: number }).afterSeq)
          ),
          throughSeq: mindEvents.length,
        });
      }
      if (namespace === "scripts" && method === "status") {
        return Promise.resolve({ jobId: "old-job", status: "completed", actions: 1 });
      }
      if (namespace === "scripts" && method === "result") {
        return Promise.resolve({
          text: '{"message":"原任务真实结果"}',
          characters: 24,
          truncated: false,
        });
      }
      if (namespace === "scripts" && method === "completions") {
        return Promise.resolve({
          items: completed && Number((args as { afterSeq: number }).afterSeq) < 1
            ? [{ job: { jobId: "pending-job", status: "completed" } }]
            : [],
          throughSeq: completed ? 1 : 0,
          hasMore: false,
        });
      }
      if (namespace === "phone") {
        if (method === "history") return Promise.resolve(phone.history);
        if (method === "action") return Promise.resolve(phone.receipt ?? null);
        if (method === "send") {
          sends.push(args as unknown as PhoneSend);
          return Promise.resolve({
            actionId: (args as { actionId: string }).actionId,
            state: "sent",
          });
        }
        return Promise.resolve({
          items: phoneSeq > Number((args as { afterSeq: number }).afterSeq)
            ? [{ text: "新的私信" }]
            : [],
          throughSeq: phoneSeq,
          hasMore: false,
        });
      }
      return Promise.resolve({ items: [], throughSeq: 0, hasMore: false });
    },
  } as unknown as PluginContext;
  const config = {
    directory: `${directory}/events`,
    promptFile,
    sourceCommit: "a".repeat(40),
    modelTimeoutMs: 5000,
    maxOutputTokens: 100,
    pollMs: 1000,
    defaultSleepMs: 60000,
    dailyTokenBudget: 200_000_000,
    restReviewEnabled: true,
  };
  let eve = new EveRuntime(ctx, config);
  await eve.initialize();
  return {
    get eve() {
      return eve;
    },
    messages,
    sends,
    requestRest(wakeAt: string) {
      mindEvents.push({
        seq: mindEvents.length + 1,
        requestId: "legacy-rest",
        kind: "rest",
        args: { wakeAt },
      });
    },
    completeJob() {
      completed = true;
    },
    wake() {
      phoneSeq++;
    },
    async restart() {
      await eve.close();
      eve = new EveRuntime(ctx, config);
      await eve.initialize();
    },
    async close() {
      await eve.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}
Deno.test("生活接续：重心跨工具、休息、重启与压缩保留，版本冲突拒绝覆盖", async () => {
  const f = await fixture([
    response("set_focus", { expectedSeq: 0, items: [focus] }),
    response("rest", decision),
  ]);
  try {
    await f.eve.pulse();
    await f.eve.pulse();
    const event = f.eve.journal.events.find((e) => e.kind === "focus")!;
    assert.ok(event);
    assert.equal(f.eve.status().continuity?.focusRecordSeq, event.seq);
    await f.eve.pulse();
    assert.ok(JSON.stringify(f.messages[1]).includes("研究星星"));
    await f.eve.pulse();
    await f.eve.pulse();
    assert.equal(f.messages.length, 2, "原生rest不额外请求模型");
    await f.restart();
    const msg = continuityMessage(f.eve.journal.events, new Map());
    assert.ok(msg.content.includes("研究星星"));
    assert.ok(msg.content.includes(decision.resume));
    await f.eve.journal.append("compression", { throughSeq: event.seq, summary: "早期经历" });
    assert.ok(continuityMessage(f.eve.journal.events, new Map()).content.includes("研究星星"));
    await f.eve.journal.append("upgrade_rollback", {
      afterSeq: event.seq - 1,
      throughSeq: event.seq,
    });
    assert.equal(
      JSON.parse(continuityMessage(f.eve.journal.events, new Map()).content.split("\n")[1]).focus
        .record,
      0,
    );
  } finally {
    await f.close();
  }
  const g = await fixture([response("set_focus", { expectedSeq: 99, items: [focus] })]);
  try {
    await g.eve.pulse();
    await g.eve.pulse();
    assert.equal(g.eve.journal.events.filter((e) => e.kind === "focus").length, 0);
    assert.equal(
      (g.eve.journal.events.at(-1)!.data as { result: { status: string } }).result.status,
      "failed",
    );
  } finally {
    await g.close();
  }
});
Deno.test("休息收尾：无工具结尾只补一次核对，重启后仍核对，醒后带着原话接续", async () => {
  const f = await fixture([
    response(),
    response(),
    response("rest", { ...decision, reason: "醒来仍想再歇一会儿" }),
  ]);
  try {
    await f.eve.pulse();
    assert.ok(pendingRestReview(f.eve.journal.events));
    await f.restart();
    await f.eve.pulse();
    assert.ok(JSON.stringify(f.messages[1]).includes("beforeRest"));
    assert.equal(pendingRestReview(f.eve.journal.events), undefined);
    await f.eve.pulse();
    assert.equal(f.messages.length, 2);
    f.wake();
    await f.eve.pulse();
    assert.equal(f.messages.length, 3);
    assert.ok(JSON.stringify(f.messages[2]).includes("醒来再看资料"));
  } finally {
    await f.close();
  }
});
Deno.test("休息收尾：原生决定持久化后中断，恢复只补回执与休息，不再请求模型", async () => {
  const f = await fixture([response("rest", decision)]);
  try {
    await f.eve.pulse();
    // Simulate a durable decision whose worker stopped before the tool receipt.
    const e = f.eve.journal.events.find((e) => e.kind === "model_response")!;
    const callId = `eve_${e.seq}_0`;
    await f.eve.journal.append("tool_start", {
      responseSeq: e.seq,
      index: 0,
      callId,
      call: { id: callId, name: "rest", args: decision },
    });
    await f.eve.journal.append("rest_decision", { callId, ...decision });
    await f.restart();
    await f.eve.pulse();
    await f.eve.pulse();
    assert.equal(f.messages.length, 1);
    assert.equal(f.eve.journal.events.filter((e) => e.kind === "rest_decision").length, 1);
    assert.equal(f.eve.journal.events.filter((e) => e.kind === "rest").length, 1);
  } finally {
    await f.close();
  }
});
Deno.test("休息收尾：同批其他动作不能被rest跳过，新通知可立即唤醒", async () => {
  const r = response("rest", decision);
  r.toolCalls.push({ id: "other", name: "act", args: {} });
  const f = await fixture([r, response("rest", decision), response("act")]);
  try {
    await f.eve.pulse();
    await f.eve.pulse();
    assert.equal(f.eve.journal.events.some((e) => e.kind === "rest_decision"), false);
    assert.equal(f.eve.journal.events.filter((e) => e.kind === "tool_result").length, 2);
    await f.eve.pulse();
    await f.eve.pulse();
    f.wake();
    await f.eve.pulse();
    assert.equal(f.messages.length, 3);
  } finally {
    await f.close();
  }
});
Deno.test("生活接续：输入校验与未知任务语义不生成任务、不重放", () => {
  assert.deepEqual(focusArgs({ expectedSeq: 0, items: [] }), { expectedSeq: 0, items: [] });
  assert.throws(() => focusArgs({ expectedSeq: 0, items: [focus, focus] }), KernelError);
  assert.throws(() => restArgs({ ...decision, wakeAt: "昨天" }), KernelError);
  assert.throws(() => restArgs({ ...decision, resume: " " }), KernelError);
  const msg = continuityMessage(
    [],
    new Map([["unknown-job", { status: "unknown" }], ["queued-job", { status: "queued" }]]),
  );
  const v = JSON.parse(msg.content.split("\n")[1]);
  assert.equal(v.unknownJobCount, 1);
  assert.equal(v.pendingJobs[0].jobId, "queued-job");
  assert.deepEqual(v.focus.items, []);
});

Deno.test("休息收尾：旧mind.rest先核对，保留请求唤醒时间，不强制继续", async () => {
  const f = await fixture([response()]);
  try {
    const wakeAt = new Date(Date.now() + 120_000).toISOString();
    f.requestRest(wakeAt);
    await f.eve.pulse();
    assert.equal(f.messages.length, 1);
    assert.ok(JSON.stringify(f.messages[0]).includes("beforeRest"));
    const rest = f.eve.journal.events.find((e) => e.kind === "rest")!;
    assert.equal((rest.data as { wakeAt: number }).wakeAt, Date.parse(wakeAt));
    await f.eve.pulse();
    assert.equal(f.messages.length, 1);
  } finally {
    await f.close();
  }
});
Deno.test("休息收尾：后台任务可带着休息，完成通知仍能唤醒并进入接续", async () => {
  const f = await fixture([
    response("rest", decision),
    response("rest", decision),
    response("act"),
  ]);
  const pending = (messages: Message[]) =>
    JSON.parse(
      messages.findLast((m) => m.content.startsWith("当前生活接续"))!.content.split("\n")[1],
    )
      .pendingJobs;
  try {
    await f.eve.journal.append("tool_result", {
      callId: "earlier",
      result: {
        status: "completed",
        output: JSON.stringify({ jobId: "pending-job", status: "queued" }),
      },
    });
    await f.restart();
    await f.eve.pulse();
    await f.eve.pulse();
    await f.eve.pulse();
    assert.equal(f.messages.length, 1);
    assert.deepEqual(
      pending(f.messages[0]).map(({ jobId, status }: { jobId: string; status: string }) => ({
        jobId,
        status,
      })),
      [{ jobId: "pending-job", status: "queued" }],
    );
    f.completeJob();
    await f.eve.pulse();
    assert.equal(f.messages.length, 2);
    assert.ok(JSON.stringify(f.messages[1]).includes(decision.resume));
    assert.deepEqual(pending(f.messages[1]), [], "完成通知应清除待办");
    await f.eve.pulse();
    // A receipt written after the completion can still describe an earlier queued state.
    await f.eve.journal.append("tool_result", {
      callId: "late",
      result: {
        status: "completed",
        output: JSON.stringify({ jobId: "pending-job", status: "queued" }),
      },
    });
    await f.restart();
    f.wake();
    await f.eve.pulse();
    assert.deepEqual(pending(f.messages[2]), [], "重启和迟到的旧回执不能复活已完成任务");
  } finally {
    await f.close();
  }
});

Deno.test("完整轮次缓存：生活状态只在变化时追加，跨工具、重启与唤醒保留输入前缀", async () => {
  const f = await fixture([
    response("set_focus", { expectedSeq: 0, items: [focus] }),
    response("act"),
    response("rest", decision),
    response("act"),
  ]);
  try {
    await f.eve.pulse();
    await f.eve.pulse();
    await f.eve.pulse();
    assert.deepEqual(f.messages[1].slice(0, f.messages[0].length), f.messages[0]);
    const states = f.eve.journal.events.filter((e) => e.kind === "context_state").length;
    await f.eve.pulse();
    await f.restart();
    await f.eve.pulse();
    assert.deepEqual(f.messages[2].slice(0, f.messages[1].length), f.messages[1]);
    assert.equal(f.eve.journal.events.filter((e) => e.kind === "context_state").length, states);
    await f.eve.pulse();
    f.wake();
    await f.eve.pulse();
    assert.deepEqual(f.messages[3].slice(0, f.messages[2].length), f.messages[2]);
    assert.ok(JSON.stringify(f.messages[3]).includes(decision.resume));
  } finally {
    await f.close();
  }
});

Deno.test("休息收尾：后续工具失败和新私信使旧审查失效，重启后重新核对再休息", async () => {
  const f = await fixture([response(), response("act"), response(), response(), response()]);
  try {
    await f.eve.pulse();
    const original = pendingRestReview(f.eve.journal.events)!;
    await f.eve.pulse();
    assert.equal(pendingRestReview(f.eve.journal.events), undefined);
    await f.eve.pulse();
    const callId = (f.eve.journal.events.at(-1)!.data as { callId: string }).callId;
    await f.eve.journal.append("tool_result", {
      callId,
      result: {
        status: "completed",
        output: JSON.stringify({
          jobId: "failed-job",
          status: "failed",
          actions: 1,
          error: { kind: "rejected", message: "limit范围1..32" },
        }),
      },
    });
    await f.restart();
    await f.eve.pulse();
    const renewed = pendingRestReview(f.eve.journal.events)!;
    assert.ok(renewed.seq > original.seq);
    assert.equal(f.eve.journal.events.some((e) => e.kind === "rest"), false);
    assert.ok(JSON.stringify(f.messages[2]).includes("failed-job"));
    assert.ok(JSON.stringify(f.messages[2]).includes("limit范围1..32"));
    f.wake();
    await f.eve.pulse();
    const afterMessage = pendingRestReview(f.eve.journal.events)!;
    assert.ok(afterMessage.seq > renewed.seq);
    await f.eve.pulse();
    const rest = f.eve.journal.events.findLast((e) => e.kind === "rest")!;
    assert.equal((rest.data as { reviewSeq: number }).reviewSeq, afterMessage.seq);
    assert.equal(f.messages.length, 5, "有效审查只多走一轮，不无限检查或重放动作");
  } finally {
    await f.close();
  }
});

Deno.test("原生脚本读取接线：工具表可用，结果直接进入下一轮结构化上下文，重启不重放", async () => {
  const f = await fixture([
    response("read_script", { jobId: "old-job" }),
    response("rest", decision),
  ]);
  try {
    await f.eve.pulse();
    await f.eve.pulse();
    const result = f.eve.journal.events.find((e) => e.kind === "tool_result");
    assert.ok(result);
    assert.equal((result.data as { result: { status: string } }).result.status, "completed");
    await f.eve.pulse();
    const tool = f.messages[1].find((m) => m.role === "tool");
    assert.ok(tool);
    const structured = JSON.parse(tool.content);
    assert.equal(structured.tool, "read_script");
    assert.equal(structured.task.jobId, "old-job");
    assert.equal(structured.receipt.output.result.value.message, "原任务真实结果");
    await f.eve.pulse();
    await f.eve.pulse();
    const count = f.eve.journal.events.filter((e) => e.kind === "tool_result").length;
    await f.restart();
    await f.eve.pulse();
    assert.equal(f.eve.journal.events.filter((e) => e.kind === "tool_result").length, count);
  } finally {
    await f.close();
  }
});

Deno.test("自然收尾接线：不额外请求模型或发告别，跨重启压缩接续，新私信照常唤醒", async () => {
  const f = await fixture([
    response("set_conversation", {
      expectedSeq: 0,
      conversationId: "qq:user:friend",
      throughSeq: 20,
      mode: "close",
      summary: "聊完星星",
      nextOpening: "有新观测再接",
      pending: [],
    }),
    response("rest", decision),
    response("act"),
  ], { history: socialHistory });
  try {
    await f.eve.pulse();
    await f.eve.pulse();
    const saved = f.eve.journal.events.find((e) => e.kind === "conversation_state")!;
    assert.ok(saved);
    await f.eve.pulse();
    assert.ok(JSON.stringify(f.messages[1]).includes("有新观测再接"));
    await f.eve.pulse();
    await f.eve.pulse();
    assert.equal(f.messages.length, 2);
    assert.equal(f.sends.length, 0);
    await f.eve.journal.append("compression", { throughSeq: saved.seq, summary: "旧经历" });
    await f.restart();
    f.wake();
    await f.eve.pulse();
    assert.equal(f.messages.length, 3, "close不能屏蔽新私信");
    assert.ok(JSON.stringify(f.messages[2]).includes("有新观测再接"));
  } finally {
    await f.close();
  }
});

Deno.test("主动搭话恢复：中断后的原actionId已有unknown，不重新发送，不重复占机会", async () => {
  const args = {
    conversationId: "qq:user:friend",
    throughSeq: 20,
    focusId: "learn",
    angle: "新观测",
    text: "昨天那个有结果了",
  };
  const phone: { history: HistoryScreen; receipt?: PhoneActionReceipt } = {
    history: socialHistory,
  };
  const f = await fixture(
    [response("start_conversation", args), response("rest", decision)],
    phone,
  );
  try {
    await f.eve.pulse();
    const e = f.eve.journal.events.find((event) => event.kind === "model_response")!;
    const callId = `eve_${e.seq}_0`, actionId = `${callId}_outreach`;
    const entry = {
      actionId,
      conversationId: args.conversationId,
      throughSeq: 20,
      focusId: "learn",
      angle: args.angle,
      at: new Date().toISOString(),
    };
    await f.eve.journal.append("outreach_start", { callId, entry, items: [entry] });
    // Original platform action already exists, even though the Eve receipt was lost.
    phone.receipt = { actionId, state: "unknown" };
    await f.restart();
    await f.eve.pulse();
    assert.equal(f.sends.length, 0);
    const result = f.eve.journal.events.find((event) => event.kind === "outreach_result")!;
    assert.equal(
      (result.data as unknown as { receipt: { receipt: PhoneActionReceipt } }).receipt.receipt
        .state,
      "unknown",
    );
    assert.equal(f.eve.journal.events.filter((event) => event.kind === "outreach_start").length, 1);
    await f.restart();
    await f.eve.pulse();
    assert.equal(f.sends.length, 0, "持久回执后不重发");
  } finally {
    await f.close();
  }
});
