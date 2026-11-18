import assert from "node:assert/strict";
import type { Message, ModelRequest, ToolSpec } from "../contracts/model.ts";
import type { Json } from "../kernel/protocol.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import { contextComposition } from "../plugins/eve/view.ts";
import type { PluginContext } from "../sdk/plugin.ts";

const score = (messages: Message[], tools: ToolSpec[]) =>
  messages.reduce((total, message) => total + message.content.length, tools.length);
const reservation = ({ messages, tools }: ModelRequest, output: number) =>
  new TextEncoder().encode(JSON.stringify({ messages, tools })).byteLength + messages.length * 32 +
  output;

async function fixture(count = score, provided: ToolSpec[] = []) {
  const directory = await Deno.makeTempDir({ prefix: "eve-round-measurement-" });
  const measurements: (ModelRequest & { estimatedTokens: number })[] = [];
  const requests: ModelRequest[] = [];
  const states: {
    contextEstimatedTokens: number;
    contextComposition: ReturnType<typeof contextComposition>;
  }[] = [];
  let duringUpdate: (() => Promise<unknown>) | undefined;
  const ctx = {
    async call(contract: string, method: string, args: Json) {
      switch (`${contract}.${method}`) {
        case "phone.notifications":
        case "scripts.completions":
        case "mind.poll":
          return { items: [], throughSeq: 0 };
        case "tools.list":
          return provided;
        case "mind.update": {
          states.push(args as unknown as typeof states[number]);
          const hook = duringUpdate;
          duringUpdate = undefined;
          await hook?.();
          return null;
        }
        case "model.complete":
          requests.push(args as unknown as ModelRequest);
          return {
            content: "已完成",
            toolCalls: [],
            usage: { status: "reported", inputTokens: 9, cachedInputTokens: 9, outputTokens: 1 },
          };
        default:
          throw new Error(`unexpected ${contract}.${method}`);
      }
    },
  } as unknown as PluginContext;
  const config = {
    directory: `${directory}/journal`,
    promptFile: `${directory}/prompt`,
    sourceCommit: "test",
    modelTimeoutMs: 1000,
    maxOutputTokens: 100,
    pollMs: 60_000,
    defaultSleepMs: 0,
    dailyTokenBudget: 1_000_000,
  };
  const runtime = new EveRuntime(ctx, config, {
    description: "fixture counter",
    count(messages, tools) {
      const estimatedTokens = count(messages, tools);
      measurements.push({ messages, tools, estimatedTokens });
      return estimatedTokens;
    },
  });
  const dispose = async () => {
    try {
      await runtime.close();
    } finally {
      await Deno.remove(directory, { recursive: true });
    }
  };
  try {
    await Deno.writeTextFile(config.promptFile, "稳定身份");
    await runtime.initialize();
  } catch (error) {
    await dispose();
    throw error;
  }
  return {
    runtime,
    requests,
    measurements,
    states,
    onNextUpdate(hook: () => Promise<unknown>) {
      duringUpdate = hook;
    },
    checkStart() {
      const request = requests.at(-1)!;
      const matched = measurements.filter(({ messages, tools }) =>
        messages === request.messages && tools === request.tools
      );
      assert.equal(matched.length, 1);
      const start = runtime.journal.events.findLast((event) => event.kind === "model_start")!
        .data as Record<string, Json>;
      assert.equal(start.estimatedTokens, matched[0].estimatedTokens);
      assert.equal(start.reservation, reservation(request, config.maxOutputTokens));
      assert.deepEqual(start.contextComposition, contextComposition(request.messages));
      assert.deepEqual(runtime.status().contextComposition, start.contextComposition);
    },
    [Symbol.asyncDispose]: dispose,
  };
}

Deno.test("轮次计量：预演提交后复用；新增感知重测，稳定轮只计量和预留一次", async () => {
  let schemaReads = 0;
  const tool = (name: string) => ({
    name,
    get description() {
      schemaReads++;
      return "外部工具";
    },
  });
  const provided = [tool("fixture_v1")];
  await using round = await fixture(score, provided);
  const { runtime, requests, measurements, states } = round;
  await runtime.pulse();
  assert.equal(measurements.length, 3, "原视图、投影预演、含感知的最终请求");
  assert.equal(schemaReads, 2, "未选用的原视图不预留字节");
  assert.equal(states[0].contextEstimatedTokens, measurements[1].estimatedTokens);
  assert.ok(states[0].contextEstimatedTokens < measurements[2].estimatedTokens);
  assert.ok(
    requests[0].messages.some((message) => message.content.includes('"kind":"perception"')),
  );
  round.checkStart();

  measurements.length = 0;
  schemaReads = 0;
  provided[0] = tool("fixture_v2");
  await runtime.journal.append("notice", { source: "phone", items: ["追加原文"] });
  await runtime.pulse();
  assert.equal(requests.length, 2);
  assert.equal(measurements.length, 1);
  assert.equal(schemaReads, 1);
  assert.equal(runtime.status().contextComposition, states[1].contextComposition);
  assert.ok(requests[1].messages.some((message) => message.content.includes("追加原文")));
  assert.equal(requests[0].tools[0].name, "fixture_v1");
  assert.equal(requests[1].tools[0].name, "fixture_v2");
  round.checkStart();
});

Deno.test("轮次计量：感知使最终预算越线时不派发、不记模型预留", async () => {
  await using calibration = await fixture();
  await calibration.runtime.pulse();
  const before = reservation(
    calibration.measurements[1],
    calibration.runtime.config.maxOutputTokens,
  );
  assert.ok(
    reservation(calibration.requests[0], calibration.runtime.config.maxOutputTokens) > before,
  );
  await using round = await fixture();
  round.runtime.config.dailyTokenBudget = before;
  await round.runtime.pulse();
  assert.equal(round.requests.length, 0);
  const events = round.runtime.journal.events;
  assert.ok(events.some((event) => event.kind === "perception"));
  assert.equal(events.at(-1)?.kind, "budget_wait");
  assert.ok(!events.some((event) => event.kind === "model_start"));
});

for (const fits of [true, false]) {
  Deno.test(`轮次计量：感知触发500k保护，压后${fits ? "按最终投影派发" : "仍超线则阻止"}`, async () => {
    await using round = await fixture((messages, tools) => {
      if (messages.some((message) => message.content.includes("系统机械压缩"))) {
        return fits ? score(messages, tools) : 500_000;
      }
      return messages.some((message) => message.content.includes('"kind":"perception"'))
        ? 500_000
        : score(messages, tools);
    });
    await round.runtime.pulse();
    const events = round.runtime.journal.events;
    const compression = events.find((event) => event.kind === "compression")!;
    assert.ok(compression.seq > events.find((event) => event.kind === "perception")!.seq);
    assert.ok(
      events.findLast((event) => event.kind === "context_projection")!.seq > compression.seq,
    );
    assert.equal(round.requests.length, fits ? 1 : 0);
    assert.equal(round.runtime.status().contextBlocked, !fits);
    if (fits) {
      assert.ok(
        round.requests[0].messages.some((message) => message.content.includes("系统机械压缩")),
      );
      round.checkStart();
    } else {
      assert.equal(events.at(-1)?.kind, "context_wait");
      assert.ok(!events.some((event) => event.kind === "model_start"));
    }
  });
}

Deno.test("轮次计量：投影提交跨过并发事实时不提升旧预演；准备期间改变日志则重新准备", async () => {
  await using round = await fixture();
  const { runtime, requests, states } = round;
  const append = runtime.journal.append.bind(runtime.journal);
  runtime.journal.append = async (kind, data) => {
    if (kind === "context_projection") {
      await append("notice", { source: "phone", items: ["投影提交时追加"] });
    }
    return await append(kind, data);
  };
  await runtime.pulse();
  assert.equal(states[0].contextComposition.parts["notice:phone"].messages, 1);
  round.checkStart();
  round.onNextUpdate(() => append("notice", { source: "scripts", items: ["状态更新时追加"] }));
  await runtime.pulse();
  assert.equal(requests.length, 1);
  await runtime.pulse();
  assert.equal(requests.length, 2);
  assert.ok(requests[1].messages.some((message) => message.content.includes("状态更新时追加")));
  round.checkStart();
});

Deno.test("轮次计量：model_start提交才发现前件变化时记录未派发，不发旧输入", async () => {
  await using round = await fixture();
  const { journal } = round.runtime;
  const append = journal.append.bind(journal);
  journal.append = async (kind, data) => {
    if (kind === "model_start") await append("notice", { source: "phone", items: ["新前件"] });
    return await append(kind, data);
  };
  await round.runtime.pulse();
  assert.equal(round.requests.length, 0);
  const failure = journal.events.at(-1)!;
  assert.equal(failure.kind, "model_failure");
  const data = failure.data as { requestId: string; error: { message: string } };
  assert.match(data.error.message, /未派发/);
  assert.equal(data.requestId, (journal.events.at(-2)!.data as { requestId: string }).requestId);
});
