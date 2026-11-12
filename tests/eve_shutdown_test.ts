import assert from "node:assert/strict";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { pendingTools } from "../plugins/eve/view.ts";

function deferred() {
  let resolve!: (value?: unknown) => void;
  const promise = new Promise((done) => resolve = done);
  return { promise, resolve };
}

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
