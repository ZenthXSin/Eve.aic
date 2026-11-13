import assert from "node:assert/strict";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import type { Json } from "../kernel/protocol.ts";

Deno.test("agent reply wakes resting Eve like a private message and persists its notification cursor", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agent-wake-" });
  await Deno.writeTextFile(`${directory}/prompt`, "test");
  let replySeq = 0, modelCalls = 0, modelMessages: Json = null;
  let rest = true;
  const ctx = {
    call(contract: string, method: string, args: { afterSeq?: number }) {
      const address = `${contract}.${method}`;
      if (address === "agents.notifications") {
        return {
          items: replySeq > Number(args.afterSeq ?? 0)
            ? [{
              seq: replySeq,
              agentId: "agent_review",
              direction: "incoming",
              preview: "审查完成",
              characters: 90000,
              truncated: true,
            }]
            : [],
          throughSeq: replySeq,
        };
      }
      if (["phone.notifications", "scripts.completions"].includes(address)) {
        return { items: [], throughSeq: 0 };
      }
      if (address === "mind.poll") {
        return {
          items: rest
            ? [{
              seq: 1,
              requestId: "rest-fixture",
              kind: "rest",
              args: { wakeAt: new Date(Date.now() + 3600000).toISOString() },
            }]
            : [],
          throughSeq: rest ? 1 : 0,
        };
      }
      if (["mind.update", "mind.applied", "mind.ack"].includes(address)) {
        rest = false;
        return {};
      }
      if (address === "tools.list") return [];
      if (address === "model.complete") {
        modelCalls++;
        modelMessages = args as Json;
        return { content: "看到子代理回复了", toolCalls: [] };
      }
      throw new Error(address);
    },
  } as unknown as PluginContext;
  const config = {
    directory: `${directory}/journal`,
    promptFile: `${directory}/prompt`,
    sourceCommit: "a".repeat(40),
    modelTimeoutMs: 5000,
    maxOutputTokens: 100,
    pollMs: 60000,
    defaultSleepMs: 3600000,
    dailyTokenBudget: 2000000,
    agentsEnabled: true,
  };
  let runtime = new EveRuntime(ctx, config);
  try {
    await runtime.initialize();
    await runtime.pulse();
    assert.equal(modelCalls, 0);
    replySeq = 9;
    await runtime.pulse();
    assert.equal(modelCalls, 1);
    assert.match(JSON.stringify(modelMessages), /审查完成/);
    assert.ok(!JSON.stringify(modelMessages).includes("全文"));
    assert.equal(runtime.status().agentThroughSeq, 9);
    await runtime.close();
    runtime = new EveRuntime(ctx, config);
    await runtime.initialize();
    assert.equal(runtime.status().agentThroughSeq, 9);
    const notices = runtime.journal.events.filter((e) =>
      e.kind === "notice" && (e.data as { source?: string }).source === "agents"
    );
    assert.equal(notices.length, 1);
  } finally {
    await runtime.close();
    await Deno.remove(directory, { recursive: true });
  }
});
