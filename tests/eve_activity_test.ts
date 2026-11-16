import assert from "node:assert/strict";
import type { Message } from "../contracts/model.ts";
import type { Json } from "../kernel/protocol.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";

async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-activity-" });
  const source = new FactJournal(`${directory}/source`);
  await source.initialize();
  await Deno.writeTextFile(`${directory}/prompt`, "稳定身份");
  const requests: Message[][] = [], idles: boolean[] = [], commands: Json[] = [];
  let phoneSeq = 0, agentSeq = 0;
  const ctx = {
    async call(namespace: string, method: string, args: Record<string, Json>) {
      const address = `${namespace}.${method}`;
      if (address === "timeline.page") {
        const page = source.readPage(Number(args.afterSeq));
        return { items: page.events, throughSeq: page.nextSeq, hasMore: page.hasMore };
      }
      if (address === "timeline.append") return await source.append(String(args.kind), args.data);
      if (address === "model.budget") {
        return {
          dailyTokenBudget: 200_000_000,
          day: "2026-11-16",
          timeZone: "Asia/Tokyo",
          paused: false,
          safetyTotal: 0,
        };
      }
      if (address === "commands.completions") {
        return { items: commands.slice(Number(args.afterSeq)), throughSeq: commands.length };
      }
      if (address === "phone.notifications" || address === "agents.notifications") {
        const seq = namespace === "phone" ? phoneSeq : agentSeq;
        return {
          items: seq > Number(args.afterSeq) ? [{ seq, preview: `${namespace}新消息` }] : [],
          throughSeq: seq,
        };
      }
      if (address === "scripts.completions" || address === "mind.poll") {
        return { items: [], throughSeq: 0 };
      }
      if (address === "mind.update") return {};
      if (address === "tools.list") return [];
      if (address === "memory.recall") return { items: [] };
      if (address === "notes.self") {
        idles.push(Boolean(args.idle));
        return {
          aboutMe: null,
          ideas: args.idle ? [{ noteId: "curiosity", revision: 1, text: "研究工具回执展示" }] : [],
        };
      }
      if (address === "model.complete") {
        requests.push(args.messages as unknown as Message[]);
        return {
          content: "暂歇，随后继续",
          toolCalls: [{
            id: "provider",
            name: "rest",
            args: { reason: "本阶段完成", unfinished: [], resume: "继续研究回执展示" },
          }],
        };
      }
      throw new Error(address);
    },
  } as unknown as PluginContext;
  const config = {
    directory: `${directory}/unused`,
    promptFile: `${directory}/prompt`,
    sourceCommit: "a".repeat(40),
    modelTimeoutMs: 1000,
    maxOutputTokens: 100,
    pollMs: 60000,
    defaultSleepMs: 3600000,
    dailyTokenBudget: 200_000_000,
    remoteRecords: true,
    agentsEnabled: true,
    restReviewEnabled: true,
  };
  let eve = new EveRuntime(ctx, config);
  await eve.initialize();
  return {
    get eve() {
      return eve;
    },
    requests,
    idles,
    commands,
    wakePhone() {
      phoneSeq++;
    },
    wakeAgent() {
      agentSeq++;
    },
    async rest() {
      await eve.pulse();
      await eve.pulse();
      await eve.pulse();
    },
    async restart() {
      await eve.close();
      eve = new EveRuntime(ctx, config);
      await eve.initialize();
    },
    async close() {
      await eve.close();
      await source.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("自主活动：原生休息醒后仍浮现兴趣，后台任务不占用自己的活动窗口", async () => {
  const f = await fixture();
  try {
    const before = Date.now();
    await f.rest();
    assert.equal(f.requests.length, 1, "原生休息不增加检查模型轮次");
    const delay = f.eve.status().wakeAt - before;
    assert.ok(delay >= 600000 && delay < 605000, "未指定休息从一小时降为十分钟");
    await f.eve.journal.append("tool_result", {
      callId: "background",
      result: { output: JSON.stringify({ jobId: "waiting-helper", status: "queued" }) },
    });
    await f.eve.journal.append("rest", { wakeAt: Date.now() - 1, resume: "继续研究回执展示" });
    await f.restart();
    await f.eve.pulse();
    assert.equal(f.requests.length, 2);
    assert.equal(f.idles.at(-1), true);
    assert.ok(JSON.stringify(f.requests[1]).includes("研究工具回执展示"));
    assert.ok(JSON.stringify(f.requests[1]).includes("waiting-helper"));
    assert.ok(JSON.stringify(f.requests[1]).includes("只阻塞对应事项"));
  } finally {
    await f.close();
  }
});

Deno.test("通知分流：子代理命令不唤醒或污染主上下文，游标持久化，自己命令与最终回复仍唤醒", async () => {
  const f = await fixture();
  try {
    await f.rest();
    for (const state of ["completed", "unknown"]) {
      f.commands.push({
        requestId: `script_${"a".repeat(64)}_1_tool_${f.commands.length}_0`,
        state,
        exitCode: 1,
      });
    }
    await f.eve.pulse();
    assert.equal(f.requests.length, 1);
    assert.equal(f.eve.status().commandThroughSeq, 2);
    const notice = f.eve.journal.events.findLast((e) =>
      e.kind === "notice" && (e.data as { source?: string }).source === "commands"
    )!;
    assert.deepEqual((notice.data as { items: Json[] }).items, []);
    assert.equal((notice.data as { delegated: number }).delegated, 2);
    await f.restart();
    await f.eve.pulse();
    assert.equal(f.requests.length, 1, "重启不能重放已分流的唤醒");
    f.commands.push({ requestId: "script_own_0", state: "unknown" });
    await f.eve.pulse();
    assert.equal(f.requests.length, 2);
    assert.ok(JSON.stringify(f.requests[1]).includes("script_own_0"));
    assert.ok(!JSON.stringify(f.requests[1]).includes("_tool_0_0"));
    await f.eve.pulse();
    await f.eve.pulse();
    f.wakeAgent();
    await f.eve.pulse();
    assert.equal(f.requests.length, 3);
    assert.equal(f.idles.at(-1), false, "真实新回复优先处理，不塞随机兴趣");
    await f.eve.pulse();
    await f.eve.pulse();
    f.wakePhone();
    await f.eve.pulse();
    assert.equal(f.requests.length, 4);
    assert.equal(f.idles.at(-1), false);
  } finally {
    await f.close();
  }
});
