import assert from "node:assert/strict";
import type { Message } from "../contracts/model.ts";
import type { Json } from "../kernel/protocol.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";

async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-projection-runtime-" });
  await Deno.writeTextFile(`${directory}/prompt`, "稳定身份");
  const requests: Message[][] = [];
  const ctx = {
    call(contract: string, method: string, args: Json) {
      if (contract === "tools") return Promise.resolve([]);
      if (method === "notifications" || method === "completions" || method === "poll") {
        return Promise.resolve({ items: [], throughSeq: 0 });
      }
      if (contract === "mind" && method === "update") return Promise.resolve({});
      if (contract === "model" && method === "complete") {
        requests.push((args as unknown as { messages: Message[] }).messages);
        return Promise.resolve({ content: "完成", toolCalls: [] });
      }
      throw new Error(`unexpected ${contract}.${method}`);
    },
  } as unknown as PluginContext;
  const config = {
    directory: `${directory}/journal`,
    promptFile: `${directory}/prompt`,
    sourceCommit: "test",
    modelTimeoutMs: 1000,
    maxOutputTokens: 10,
    pollMs: 1000,
    defaultSleepMs: 0,
    dailyTokenBudget: 200_000_000,
  };
  const counter = {
    description: "test character count",
    count: (messages: Message[]) => messages.reduce((n, m) => n + m.content.length, 0),
  };
  let runtime = new EveRuntime(ctx, config, counter);
  await runtime.initialize();
  return {
    get runtime() {
      return runtime;
    },
    requests,
    async restart() {
      await runtime.close();
      runtime = new EveRuntime(ctx, config, counter);
      await runtime.initialize();
    },
    async close() {
      await runtime.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

async function tool(runtime: EveRuntime, size: number) {
  const response = await runtime.journal.append("model_response", {
    requestId: `history_${runtime.journal.events.length}`,
    response: {
      content: "历史工具",
      toolCalls: [{ id: "prior", name: "execute_script", args: { code: "return 1" } }],
    },
  });
  return await runtime.journal.append("tool_result", {
    callId: `eve_${response.seq}_0`,
    result: { output: "原".repeat(size) },
  });
}
const epochs = (runtime: EveRuntime) =>
  runtime.journal.events.filter((event) => event.kind === "context_projection");

Deno.test("投影runtime：首个持久epoch先于500k机械保护，历史原件保留", async () => {
  const f = await fixture();
  try {
    for (let i = 0; i < 20; i++) await tool(f.runtime, 30_000);
    await f.runtime.pulse();
    assert.equal(f.requests.length, 1);
    assert.equal(epochs(f.runtime).length, 1);
    assert.equal(f.runtime.journal.events.filter((e) => e.kind === "compression").length, 0);
    assert.equal(
      f.requests[0].filter((m) => m.role === "tool" && m.content.length > 20_000).length,
      8,
    );
    assert.equal(
      f.runtime.journal.events.filter((e) => e.kind === "tool_result").length,
      20,
    );
    const start = f.runtime.journal.events.find((e) => e.kind === "model_start")!;
    assert.ok(epochs(f.runtime)[0].seq < start.seq);
  } finally {
    await f.close();
  }
});

Deno.test("投影runtime：普通追加与重启复用边界，模型已见前缀保持不变", async () => {
  const f = await fixture();
  try {
    for (let i = 0; i < 10; i++) await tool(f.runtime, 5_000);
    await f.runtime.pulse();
    const first = f.requests[0];
    await tool(f.runtime, 2_000);
    await f.runtime.pulse();
    assert.deepEqual(f.requests[1].slice(0, first.length), first);
    assert.equal(epochs(f.runtime).length, 1);
    const boundary = epochs(f.runtime)[0].data;
    await f.restart();
    await f.runtime.pulse();
    assert.equal(epochs(f.runtime).length, 1);
    assert.deepEqual(epochs(f.runtime)[0].data, boundary);
    assert.deepEqual(f.requests[2].slice(0, first.length), first);
  } finally {
    await f.close();
  }
});

Deno.test("投影runtime：120k高水位且节约8k才换epoch，预演不污染日志", async () => {
  const f = await fixture();
  try {
    for (let i = 0; i < 8; i++) await tool(f.runtime, 17_000);
    await f.runtime.pulse();
    await f.runtime.pulse();
    assert.equal(epochs(f.runtime).length, 1);
    await tool(f.runtime, 17_000);
    await f.runtime.pulse();
    assert.equal(epochs(f.runtime).length, 2);
    const data = epochs(f.runtime)[1].data as {
      throughSeq: number;
      estimatedBefore: number;
      estimatedAfter: number;
    };
    assert.ok(data.estimatedBefore >= 120_000);
    assert.ok(data.estimatedBefore - data.estimatedAfter >= 8_000);
    assert.ok(data.throughSeq < epochs(f.runtime)[1].seq);
    await f.runtime.pulse();
    assert.equal(epochs(f.runtime).length, 2);
    assert.ok(f.runtime.journal.events.every((e) => e.digest !== "preview"));
  } finally {
    await f.close();
  }
});

Deno.test("投影runtime：回滚移除未确认epoch，恢复旧边界而不采纳候选投影", async () => {
  const f = await fixture();
  try {
    for (let i = 0; i < 8; i++) await tool(f.runtime, 17_000);
    await f.runtime.pulse();
    const acceptedThrough = f.runtime.journal.events.at(-1)!.seq;
    await tool(f.runtime, 17_000);
    await f.runtime.pulse();
    assert.equal(epochs(f.runtime).length, 2);
    const rejectedThrough = f.runtime.journal.events.at(-1)!.seq;
    await f.runtime.journal.append("upgrade_rollback", {
      afterSeq: acceptedThrough,
      throughSeq: rejectedThrough,
    });
    await f.restart();
    await f.runtime.pulse();
    assert.equal(epochs(f.runtime).length, 2); // 原件仍完整，活动边界是第一个。
    assert.deepEqual(f.requests.at(-1)!.slice(0, f.requests[0].length), f.requests[0]);
  } finally {
    await f.close();
  }
});

Deno.test("投影runtime：可节约8k但未到高水位不换边界，显式raw保留跨epoch生效", async () => {
  const f = await fixture();
  try {
    for (let i = 0; i < 8; i++) await tool(f.runtime, 10_000);
    await f.runtime.pulse();
    await tool(f.runtime, 10_000);
    await f.runtime.pulse();
    assert.equal(epochs(f.runtime).length, 1);
    const firstResponse = f.runtime.journal.events.find((e) => e.kind === "model_response")!;
    await f.runtime.journal.append("retain", {
      responseSeq: firstResponse.seq,
      toolIndex: 0,
      mode: "raw",
    });
    for (let i = 0; i < 6; i++) await tool(f.runtime, 10_000);
    await f.runtime.pulse();
    assert.equal(epochs(f.runtime).length, 2);
    const retained = f.requests.at(-1)!.find((m) =>
      m.role === "tool" && m.toolCallId === `eve_${firstResponse.seq}_0`
    );
    assert.ok(retained?.content.includes("原".repeat(10_000)));
    await f.runtime.pulse();
    assert.equal(epochs(f.runtime).length, 2);
  } finally {
    await f.close();
  }
});
