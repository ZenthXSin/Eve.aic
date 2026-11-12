import assert from "node:assert/strict";
import type { Json } from "../kernel/protocol.ts";
import type { ConsciousEvent } from "../plugins/eve/journal.ts";
import { contextComposition, estimate, render } from "../plugins/eve/view.ts";

function event(seq: number, kind: string, data: Json): ConsciousEvent {
  return { seq, kind, data, at: "2026-11-12T00:00:00Z", digest: "fixture" } as ConsciousEvent;
}
function batches(count: number): ConsciousEvent[] {
  const events: ConsciousEvent[] = [];
  for (let i = 0; i < count; i++) {
    const seq = events.length + 1;
    events.push(event(seq, "model_response", {
      response: {
        content: `决定${i}`,
        toolCalls: [
          { id: "provider", name: "execute_script", args: { code: `return ${i}` } },
          { id: "provider2", name: "execute_script", args: { code: "return 2" } },
        ],
      },
    }));
    for (let tool = 0; tool < 2; tool++) {
      events.push(event(events.length + 1, "tool_result", {
        callId: `eve_${seq}_${tool}`,
        result: {
          status: "completed",
          output: JSON.stringify({
            jobId: `job_${i}_${tool}`,
            status: "completed",
            result: `HEAD_${i}_${tool}` + "x".repeat(6000) + `TAIL_${i}_${tool}`,
          }),
        },
      }));
    }
  }
  return events;
}

Deno.test("上下文投影：旧大回执可定位原件，最新八个完整工具批次不截断且调用成对", () => {
  const events = batches(10), before = JSON.stringify(events);
  const messages = render(events, "固定身份");
  assert.equal(messages.filter((m) => m.role === "tool").length, 20);
  for (let i = 0; i < 10; i++) {
    const assistant = messages[1 + i * 3];
    const first = messages[2 + i * 3], second = messages[3 + i * 3];
    assert.equal(first.toolCallId, assistant.toolCalls![0].id);
    assert.equal(second.toolCallId, assistant.toolCalls![1].id);
    if (i < 2) {
      assert.ok(first.content.includes(`jobId=job_${i}_0`));
      assert.ok(first.content.includes(`record=${i * 3 + 2}`));
      assert.ok(first.content.includes(`callId=eve_${i * 3 + 1}_0`));
      assert.ok(first.content.includes(`tools.records.text({seq:${i * 3 + 2}`));
      assert.ok(first.content.includes(`HEAD_${i}_0`));
      assert.ok(first.content.includes(`TAIL_${i}_0`));
      assert.ok(first.content.length < 1600);
    } else {
      assert.ok(first.content.includes("x".repeat(6000)));
      assert.ok(second.content.includes("x".repeat(6000)));
    }
  }
  assert.equal(JSON.stringify(events), before);
});

Deno.test("上下文投影：显式raw恢复完整旧回执，summary与omit继续由Eve决定", () => {
  const events = batches(10);
  events.push(event(events.length + 1, "retain", { responseSeq: 1, toolIndex: 0, mode: "raw" }));
  events.push(event(events.length + 1, "retain", {
    responseSeq: 1,
    toolIndex: 1,
    mode: "summary",
    summary: "我保留的待办与结果",
  }));
  const messages = render(events, "身份");
  assert.ok(messages[2].content.includes("x".repeat(6000)));
  assert.ok(messages[3].content.includes("我保留的待办与结果"));
  events.push(event(events.length + 1, "retain", { responseSeq: 1, toolIndex: 0, mode: "omit" }));
  const omitted = render(events, "身份");
  assert.equal(omitted[1].toolCalls?.length, 1);
  assert.equal(omitted[2].toolCallId, omitted[1].toolCalls![0].id);
});

Deno.test("上下文投影：相同记忆只留最新来源与强度，正文更新及非标准项保留，空游标不入模型", () => {
  const events = [
    event(1, "notice", {
      source: "memory",
      items: [{ id: "m", text: "未完成任务", strength: 1, sources: [1] }],
    }),
    event(2, "notice", { source: "phone", items: [], throughSeq: 100 }),
    event(3, "notice", { source: "scripts", items: [], throughSeq: 9, alreadyObserved: 1 }),
    event(4, "notice", { source: "commands", items: [], throughSeq: 2 }),
    event(5, "notice", {
      source: "memory",
      items: [{ id: "m", text: "未完成任务", strength: 2, sources: [1, 2] }],
    }),
    event(6, "notice", {
      source: "memory",
      items: [{ id: "m", text: "任务更新" }, { text: "无法去重的项" }],
    }),
    event(7, "notice", { source: "phone", items: [{ text: "新消息" }], throughSeq: 101 }),
  ];
  const before = JSON.stringify(events), messages = render(events, "身份");
  assert.equal(messages.length, 4);
  assert.equal(JSON.parse(messages[1].content).record, 5);
  assert.equal(JSON.parse(messages[1].content).data.items[0].strength, 2);
  assert.deepEqual(JSON.parse(messages[1].content).data.items[0].sources, [1, 2]);
  assert.equal(JSON.parse(messages[2].content).data.items.length, 2);
  assert.equal(JSON.parse(messages[3].content).data.items[0].text, "新消息");
  assert.equal(JSON.stringify(events), before);
});

Deno.test("上下文投影：已压缩记忆不会使当前版本消失，回滚版本不覆盖现役召回", () => {
  const events = [
    event(1, "notice", { source: "memory", items: [{ id: "m", text: "真实任务" }] }),
    event(2, "compression", { throughSeq: 1, summary: "Eve自己的时间线" }),
    event(3, "notice", { source: "memory", items: [{ id: "m", text: "真实任务" }] }),
    event(4, "notice", { source: "memory", items: [{ id: "m", text: "候选错误记忆" }] }),
    event(5, "upgrade_rollback", { afterSeq: 3, throughSeq: 4 }),
  ];
  const messages = render(events, "身份");
  assert.ok(messages[1].content.includes("Eve自己的时间线"));
  assert.ok(messages[2].content.includes("真实任务"));
  assert.ok(!JSON.stringify(messages).includes("候选错误记忆"));
});

Deno.test("上下文组成：仅返回分类计数和明确估算，不泄露正文或平台身份", () => {
  const messages = render([
    event(1, "notice", { source: "phone", items: [{ text: "秘密正文", sender: "private-id" }] }),
  ], "私有身份原文");
  const composition = contextComposition(messages);
  assert.equal(composition.messages, 2);
  assert.equal(composition.estimatedMessageTokens, estimate(messages));
  assert.equal(composition.parts.identityAndContracts.messages, 1);
  assert.equal(composition.parts["notice:phone"].messages, 1);
  assert.ok(!JSON.stringify(composition).includes("秘密正文"));
  assert.ok(!JSON.stringify(composition).includes("private-id"));
});
