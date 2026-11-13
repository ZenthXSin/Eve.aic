import assert from "node:assert/strict";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import { pendingTools } from "../plugins/eve/view.ts";
import type { ConsciousEvent } from "../plugins/eve/journal.ts";
import type { PluginContext } from "../sdk/plugin.ts";

Deno.test("轮次：未完成工具只增量索引，回滚后重建；执行前重新读取原决定", () => {
  const originals: ConsciousEvent[] = [];
  let reads = 0;
  const events = new Proxy(originals, {
    get(target, property, receiver) {
      if (typeof property === "string" && /^\d+$/.test(property)) reads++;
      return Reflect.get(target, property, receiver);
    },
  });
  const append = (kind: string, data: ConsciousEvent["data"]) =>
    originals.push({
      schema: 1,
      seq: originals.length + 1,
      at: new Date().toISOString(),
      kind,
      data,
      previous: "0".repeat(64),
      digest: "1".repeat(64),
    });
  for (let i = 0; i < 100; i++) {
    const seq = originals.length + 1;
    append("model_response", {
      response: { content: "", toolCalls: [{ id: "call", name: "fixture", args: {} }] },
    });
    append("tool_result", { callId: `eve_${seq}_0`, result: {} });
  }
  assert.equal(pendingTools(events).length, 0);
  reads = 0;
  assert.equal(pendingTools(events).length, 0);
  assert.equal(reads, 0);
  append("model_response", {
    response: { content: "", toolCalls: [{ id: "call", name: "fixture", args: {} }] },
  });
  assert.equal(pendingTools(events)[0].call.id, "eve_201_0");
  reads = 0;
  assert.equal(pendingTools(events)[0].call.id, "eve_201_0");
  assert.equal(reads, 1);
  append("tool_result", { callId: "eve_201_0", result: {} });
  assert.equal(pendingTools(events).length, 0);
  append("upgrade_rollback", { afterSeq: 201, throughSeq: 202 });
  assert.equal(pendingTools(events)[0].call.id, "eve_201_0");
});

Deno.test("轮次：独立通知并行读取，模型决定和工具回执后立即续接，不等待poll", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-round-latency-" });
  let resolve!: () => void;
  const second = new Promise<void>((done) => resolve = done);
  let modelCalls = 0, executed = 0, reading = 0, maxReading = 0;
  const ctx = {
    async call(contract: string, method: string) {
      const address = `${contract}.${method}`;
      if (["phone.notifications", "scripts.completions", "mind.poll"].includes(address)) {
        reading++;
        maxReading = Math.max(maxReading, reading);
        await new Promise((done) => setTimeout(done, 20));
        reading--;
        return { items: [], throughSeq: 0 };
      }
      if (address === "mind.update") return {};
      if (address === "tools.list") {
        return [{ name: "fixture", description: "test", parameters: {} }];
      }
      if (address === "tools.execute") {
        executed++;
        return { status: "completed", output: "done" };
      }
      if (address === "model.complete") {
        modelCalls++;
        if (modelCalls === 2) resolve();
        return {
          content: "done",
          toolCalls: modelCalls === 1 ? [{ id: "call", name: "fixture", args: {} }] : [],
        };
      }
      throw new Error(address);
    },
  } as unknown as PluginContext;
  await Deno.writeTextFile(`${directory}/prompt`, "test");
  const runtime = new EveRuntime(ctx, {
    directory: `${directory}/journal`,
    promptFile: `${directory}/prompt`,
    sourceCommit: "test",
    modelTimeoutMs: 5000,
    maxOutputTokens: 100,
    pollMs: 60000,
    defaultSleepMs: 3600000,
    dailyTokenBudget: 200000000,
  });
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await runtime.initialize();
    runtime.start();
    await Promise.race([
      second,
      new Promise<never>((_, reject) =>
        timer = setTimeout(() => reject(new Error("工具续接仍在等待poll")), 5000)
      ),
    ]);
    assert.equal(executed, 1);
    assert.equal(maxReading, 3);
  } finally {
    clearTimeout(timer);
    await runtime.close();
    await Deno.remove(directory, { recursive: true });
  }
});
