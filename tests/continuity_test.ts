import assert from "node:assert/strict";
import type { Message, ModelResponse } from "../contracts/model.ts";
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
async function fixture(responses: ModelResponse[]) {
  const directory = await Deno.makeTempDir({ prefix: "eve-continuity-" });
  const promptFile = `${directory}/prompt.md`;
  await Deno.writeTextFile(promptFile, "稳定身份");
  const messages: Message[][] = [];
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
  const f = await fixture([response("rest", decision), response("act")]);
  try {
    await f.eve.journal.append("tool_result", {
      callId: "earlier",
      result: { output: JSON.stringify({ jobId: "pending-job", status: "queued" }) },
    });
    await f.restart();
    await f.eve.pulse();
    await f.eve.pulse();
    await f.eve.pulse();
    assert.equal(f.messages.length, 1);
    assert.ok(JSON.stringify(f.messages[0]).includes("pending-job"));
    f.completeJob();
    await f.eve.pulse();
    assert.equal(f.messages.length, 2);
    assert.ok(JSON.stringify(f.messages[1]).includes(decision.resume));
  } finally {
    await f.close();
  }
});
