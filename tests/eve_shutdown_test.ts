import assert from "node:assert/strict";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { pendingTools } from "../plugins/eve/view.ts";
import { fingerprint } from "../kernel/calls.ts";

function deferred() {
  let resolve!: (value?: unknown) => void;
  const promise = new Promise((done) => resolve = done);
  return { promise, resolve };
}

Deno.test("脚本回执不返回也能继续下一轮，保留原任务编号，不重复提交", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-handoff-" });
  const late = deferred();
  let executed = 0, rounds = 0;
  const context = {
    async call(contract: string, method: string) {
      switch (`${contract}.${method}`) {
        case "tools.list":
          return [{
            name: "execute_script",
            description: "fixture",
            parameters: { type: "object" },
          }];
        case "model.complete":
          return rounds++ === 0
            ? {
              content: "查一下",
              toolCalls: [{ id: "fixture", name: "execute_script", args: { code: "return 1" } }],
            }
            : { content: "暂时没结果", toolCalls: [] };
        case "tools.execute":
          executed++;
          return await late.promise;
        case "mind.update":
          return null;
        case "phone.notifications":
        case "scripts.completions":
        case "mind.poll":
          return { items: [], throughSeq: 0 };
        default:
          throw new Error(`Unexpected fixture call: ${contract}.${method}`);
      }
    },
  } as unknown as PluginContext;
  const runtime = new EveRuntime(context, {
    directory: `${directory}/journal`,
    promptFile: `${directory}/prompt.md`,
    sourceCommit: "fixture",
    modelTimeoutMs: 50,
    maxOutputTokens: 100,
    pollMs: 60_000,
    defaultSleepMs: 60_000,
    dailyTokenBudget: 1_000_000,
  });
  try {
    await Deno.writeTextFile(`${directory}/prompt.md`, "fixture");
    await runtime.initialize();
    await runtime.pulse();
    const [original] = pendingTools(runtime.journal.events);
    assert.ok(original);
    await runtime.pulse();
    const receipt = runtime.journal.events.findLast((event) => event.kind === "tool_result")
      ?.data as {
        result: { status: string; jobId: string };
      };
    assert.equal(receipt.result.status, "unknown");
    assert.equal(receipt.result.jobId, await fingerprint(["script-job-v1", original.call.id]));
    assert.equal(pendingTools(runtime.journal.events).length, 0);
    await runtime.pulse();
    assert.equal(rounds, 2);
    assert.equal(executed, 1);
  } finally {
    late.resolve({ status: "completed" });
    await runtime.close();
    await Deno.remove(directory, { recursive: true });
  }
});

for (const stage of ["tools.list", "model.complete"]) {
  Deno.test(`意识流停止：${stage}在途时不派发新工作，已收到响应和工具决定保留`, async () => {
    const directory = await Deno.makeTempDir({ prefix: "eve-shutdown-" });
    const entered = deferred(), release = deferred();
    let modelCalls = 0, executed = 0, blocked = false;
    const ctx = {
      async call(contract: string, method: string) {
        const address = `${contract}.${method}`;
        if (address === stage && !blocked) {
          blocked = true;
          entered.resolve();
          await release.promise;
        }
        if (address === "tools.list") {
          return [{ name: "fixture", description: "fixture", parameters: { type: "object" } }];
        }
        if (address === "model.complete") {
          modelCalls++;
          return { content: "fixture", toolCalls: [{ id: "fixture", name: "fixture", args: {} }] };
        }
        if (address === "tools.execute") {
          executed++;
          return { status: "completed", output: "fixture" };
        }
        if (address === "mind.update") return null;
        if (["phone.notifications", "scripts.completions", "mind.poll"].includes(address)) {
          return { items: [], throughSeq: 0, hasMore: false };
        }
        throw new Error(`Unexpected fixture call: ${address}`);
      },
    } as unknown as PluginContext;
    const config = {
      directory: `${directory}/journal`,
      promptFile: `${directory}/prompt.md`,
      sourceCommit: "a".repeat(40),
      modelTimeoutMs: 1000,
      maxOutputTokens: 100,
      pollMs: 10,
      defaultSleepMs: 86_400_000,
      dailyTokenBudget: 200_000_000,
    };
    await Deno.writeTextFile(config.promptFile, "fixture");
    let runtime = new EveRuntime(ctx, config);
    try {
      await runtime.initialize();
      const pulse = runtime.pulse();
      await entered.promise;
      const close = runtime.close();
      release.resolve();
      await Promise.all([pulse, close]);
      assert.equal(executed, 0);
      assert.equal(modelCalls, stage === "model.complete" ? 1 : 0);
      runtime = new EveRuntime(ctx, config);
      await runtime.initialize();
      assert.equal(
        runtime.journal.events.filter((event) => event.kind === "model_response").length,
        stage === "model.complete" ? 1 : 0,
      );
      if (stage === "model.complete") {
        assert.equal(pendingTools(runtime.journal.events).length, 1);
        await runtime.pulse();
        assert.equal(executed, 1);
        assert.equal(modelCalls, 1);
        assert.equal(pendingTools(runtime.journal.events).length, 0);
      }
    } finally {
      release.resolve();
      await runtime.close();
      await Deno.remove(directory, { recursive: true });
    }
  });
}
