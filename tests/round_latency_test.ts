import assert from "node:assert/strict";
import { FactJournal } from "../sdk/fact-journal.ts";
import type { Json } from "../kernel/protocol.ts";
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

Deno.test("轮次准备：记忆与自我便签并行就绪后才组装模型上下文", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-round-prepare-" });
  const source = new FactJournal(`${directory}/source`);
  let release!: () => void, complete!: () => void;
  const gate = new Promise<void>((done) => release = done);
  const model = new Promise<void>((done) => complete = done);
  const entered = new Set<string>();
  const ctx = {
    async call(contract: string, method: string, args: Record<string, Json>) {
      const address = `${contract}.${method}`;
      if (address === "timeline.page") {
        const page = source.readPage(Number(args.afterSeq));
        return { items: page.events, throughSeq: page.nextSeq, hasMore: page.hasMore };
      }
      if (address === "timeline.append") return await source.append(String(args.kind), args.data);
      if (address === "model.budget") {
        return {
          dailyTokenBudget: 50000000,
          timeZone: "Asia/Tokyo",
          paused: false,
          day: "2026-11-14",
          knownNoncachedTokens: 0,
          unknownReservedTokens: 0,
          safetyTotal: 0,
          safeRemaining: 50000000,
        };
      }
      if (
        ["phone.notifications", "scripts.completions", "commands.completions", "mind.poll"]
          .includes(address)
      ) {
        return { items: [], throughSeq: 0 };
      }
      if (address === "mind.update") return {};
      if (address === "tools.list") return [];
      if (["notes.self", "memory.recall"].includes(address)) {
        entered.add(address);
        if (entered.size === 2) release();
        await gate;
        return address === "notes.self"
          ? {
            aboutMe: { id: "self", revision: 1, tokens: 1, encoding: "test", content: "self-note" },
            ideas: [],
          }
          : { items: [{ text: "recalled-memory" }] };
      }
      if (address === "model.complete") {
        assert.equal(entered.size, 2);
        const text = JSON.stringify(args.messages);
        assert.ok(text.includes("self-note"));
        assert.ok(text.includes("recalled-memory"));
        complete();
        return { content: "done", toolCalls: [] };
      }
      throw new Error(address);
    },
  } as unknown as PluginContext;
  const runtime = new EveRuntime(ctx, {
    directory: `${directory}/journal`,
    promptFile: `${directory}/prompt`,
    sourceCommit: "test",
    modelTimeoutMs: 5000,
    maxOutputTokens: 100,
    pollMs: 60000,
    defaultSleepMs: 3600000,
    dailyTokenBudget: 50000000,
    remoteRecords: true,
  });
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await source.initialize();
    await source.append("model_response", {
      response: { content: "需要召回的上一轮", toolCalls: [] },
    });
    await Deno.writeTextFile(`${directory}/prompt`, "test");
    await runtime.initialize();
    runtime.start();
    await Promise.race([
      model,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error("独立准备没有并行完成")), 5000);
      }),
    ]);
    const timing = source.events.findLast((event) => event.kind === "model_start")!.data as {
      runtimeTiming: { noticeReads: Record<string, number> };
    };
    assert.deepEqual(Object.keys(timing.runtimeTiming.noticeReads).sort(), [
      "budgetMs",
      "commandsMs",
      "mindMs",
      "phoneMs",
      "scriptsMs",
    ]);
  } finally {
    release();
    clearTimeout(timer);
    await runtime.close();
    await source.close();
    await Deno.remove(directory, { recursive: true });
  }
});
