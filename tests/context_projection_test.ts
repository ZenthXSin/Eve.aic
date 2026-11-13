import assert from "node:assert/strict";
import { type Json, KernelError } from "../kernel/protocol.ts";
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
  const events = batches(10);
  events.push(event(31, "context_projection", { throughSeq: 30 }));
  const before = JSON.stringify(events);
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
  events.push(event(31, "context_projection", { throughSeq: 30 }));
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
  events.push(event(8, "context_projection", { throughSeq: 7 }));
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

function appendBatch(events: ConsciousEvent[]) {
  const offset = events.length;
  for (const item of batches(1)) {
    const data = item.kind === "tool_result"
      ? { ...item.data as Record<string, Json>, callId: `eve_${offset + 1}_${item.seq - 2}` }
      : item.data;
    events.push(event(offset + item.seq, item.kind, data));
  }
}

Deno.test("上下文缓存：固定epoch连续追加超过八批与重启不改已有消息前缀", () => {
  const events = batches(10);
  events.push(event(31, "context_projection", { throughSeq: 30 }));
  let previous = render(events, "身份");
  const originalEstimate = estimate(previous);
  for (let i = 0; i < 12; i++) {
    appendBatch(events);
    const next = render(events, "身份");
    assert.deepEqual(next.slice(0, previous.length), previous);
    assert.ok(next.at(-1)!.content.includes("x".repeat(6000)));
    previous = next;
  }
  assert.deepEqual(render(JSON.parse(JSON.stringify(events)), "身份"), previous);
  // 只有持久化新epoch才会节选旧尾部；当时最近八批仍完整。
  const beforeEpochEstimate = estimate(previous);
  events.push(event(events.length + 1, "context_projection", { throughSeq: events.length }));
  const renewed = render(events, "身份");
  assert.ok(estimate(renewed) < beforeEpochEstimate);
  assert.ok(estimate(renewed) >= originalEstimate);
  assert.equal(
    renewed.filter((m) => m.role === "tool" && m.content.includes("x".repeat(6000))).length,
    16,
  );
});

Deno.test("上下文缓存：无epoch追加工具和重复召回也保持前缀，基线最新来源不被新召回替换", () => {
  const events = batches(10);
  const raw = render(events, "身份");
  assert.equal(
    raw.filter((m) => m.role === "tool" && m.content.includes("x".repeat(6000))).length,
    20,
  );
  appendBatch(events);
  assert.deepEqual(render(events, "身份").slice(0, raw.length), raw);
  events.push(event(events.length + 1, "notice", {
    source: "memory",
    items: [{ id: "m", text: "任务", strength: 1 }],
  }));
  const firstRecall = render(events, "身份");
  events.push(event(events.length + 1, "notice", {
    source: "memory",
    items: [{ id: "m", text: "任务", strength: 2 }],
  }));
  assert.deepEqual(render(events, "身份"), firstRecall);
  events.push(event(events.length + 1, "context_projection", { throughSeq: events.length }));
  const baseline = render(events, "身份");
  assert.equal(JSON.parse(baseline.at(-1)!.content).data.items[0].strength, 2);
  for (let i = 0; i < 10; i++) {
    events.push(event(events.length + 1, "notice", {
      source: "memory",
      items: [{ id: "m", text: "任务", strength: 3 + i }],
    }));
    assert.deepEqual(render(events, "身份"), baseline);
  }
  events.push(event(events.length + 1, "notice", {
    source: "memory",
    items: [{ id: "m", text: "任务更新", strength: 20 }],
  }));
  const updated = render(events, "身份");
  assert.deepEqual(updated.slice(0, baseline.length), baseline);
  assert.equal(JSON.parse(updated.at(-1)!.content).data.items[0].text, "任务更新");
});

Deno.test("上下文缓存：回滚epoch恢复原投影且无效边界拒绝", () => {
  const events = batches(10);
  const raw = render(events, "身份");
  events.push(event(31, "context_projection", { throughSeq: 30 }));
  assert.notDeepEqual(render(events, "身份"), raw);
  events.push(event(32, "upgrade_rollback", { afterSeq: 30, throughSeq: 31 }));
  const restored = render(events, "身份");
  assert.deepEqual(restored.slice(0, raw.length), raw);
  for (const throughSeq of [-1, 0.5, 31, 32, Number.MAX_SAFE_INTEGER + 1]) {
    const invalid = [...batches(10), event(31, "context_projection", { throughSeq })];
    assert.throws(
      () => render(invalid, "身份"),
      (error) => error instanceof KernelError && error.kind === "corrupt",
    );
  }
});

Deno.test("轮次准备：预建索引后长压缩历史不再逐条读取，新增回执仍完整配对", () => {
  const source = Array.from(
    { length: 18_000 },
    (_, i) => event(i + 1, "notice", { source: "phone", items: [{ text: `旧消息${i}` }] }),
  );
  source.push(event(source.length + 1, "context_projection", { throughSeq: source.length }));
  source.push(event(source.length + 1, "compression", { throughSeq: 18_000, summary: "旧经历" }));
  let reads = 0;
  const events = new Proxy(source, {
    get(target, key, receiver) {
      if (typeof key === "string" && /^\d+$/.test(key)) reads++;
      return Reflect.get(target, key, receiver);
    },
  });
  const first = render(events, "身份");
  reads = 0;
  assert.deepEqual(render(events, "身份"), first);
  assert.ok(reads < 30, `稳定轮次不应重新读取18000条旧经历，实际读取${reads}次`);
  const responseSeq = source.length + 1;
  source.push(event(responseSeq, "model_response", {
    response: {
      content: "新决定",
      toolCalls: [{ id: "provider", name: "execute_script", args: { code: "return 1" } }],
    },
  }));
  source.push(event(source.length + 1, "notice", { source: "phone", items: [{ text: "新消息" }] }));
  assert.deepEqual(render(events, "身份"), first);
  source.push(event(source.length + 1, "tool_result", {
    callId: `eve_${responseSeq}_0`,
    result: { output: "新回执" },
  }));
  reads = 0;
  const next = render(events, "身份");
  assert.ok(reads < 60, `增量轮次不应读取旧历史，实际读取${reads}次`);
  assert.deepEqual(next.slice(0, first.length), first);
  assert.deepEqual(next.slice(-3).map((message) => message.role), ["assistant", "tool", "user"]);
  assert.ok(next.at(-1)!.content.includes("新消息"));
  assert.deepEqual(next, render(structuredClone(source), "身份"));
});

Deno.test("轮次准备：热索引正确处理保留策略、连续压缩及回滚并与冷启动一致", () => {
  const events = batches(10);
  const check = () =>
    assert.deepEqual(render(events, "身份"), render(structuredClone(events), "身份"));
  check();
  const accepted = events.length;
  events.push(event(events.length + 1, "context_projection", { throughSeq: events.length }));
  events.push(event(events.length + 1, "retain", {
    responseSeq: 1,
    toolIndex: 0,
    mode: "summary",
    summary: "保留摘要",
  }));
  check();
  events.push(event(events.length + 1, "compression", { throughSeq: 3, summary: "第一批" }));
  check();
  events.push(event(events.length + 1, "compression", { throughSeq: 9, summary: "前三批" }));
  check();
  events.push(event(events.length + 1, "upgrade_rollback", {
    afterSeq: accepted,
    throughSeq: events.length,
  }));
  check();
  events.push(event(events.length + 1, "context_projection", { throughSeq: events.length }));
  check();
  events.push(event(events.length + 1, "compression", { throughSeq: 6, summary: "确认的时间线" }));
  check();
});
