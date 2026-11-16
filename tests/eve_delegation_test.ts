import assert from "node:assert/strict";
import type { Json } from "../kernel/protocol.ts";
import { KernelError } from "../kernel/protocol.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { DELEGATE_TOOL, delegateTask } from "../plugins/eve/delegation.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";

Deno.test("原生派工：默认通用、独立会话和追问用稳定原动作，不执行工作脚本", async () => {
  const admissions = new Map<string, Json>();
  const ctx = {
    // deno-lint-ignore require-await -- PluginContext mocks preserve its async contract.
    async call(namespace: string, method: string, args: Record<string, Json>) {
      assert.equal(namespace, "agents");
      const id = String(args._actionId);
      if (!admissions.has(id)) admissions.set(id, structuredClone(args));
      if (method === "create") return { agentId: `agent_${id}` };
      return { jobId: id, agentId: args.agentId, status: "queued", requestSeq: 1 };
    },
  } as unknown as PluginContext;
  const defaultJob = await delegateTask(ctx, { text: "巡检两台服务器并报告真实状态" }, "eve_1_0");
  assert.equal(defaultJob.agentId, "agent_general");
  const args = { text: "处理另一项独立工作", role: "general", newSession: true };
  const first = await delegateTask(ctx, args, "eve_2_0");
  const restored = await delegateTask(ctx, args, "eve_2_0");
  assert.deepEqual(first, restored);
  assert.equal(admissions.size, 3, "recovery reuses original create and task IDs");
  const follow = await delegateTask(ctx, {
    agentId: first.agentId,
    text: "补齐缺少的验收证据",
  }, "eve_3_0");
  assert.equal(follow.agentId, first.agentId);
  await assert.rejects(
    delegateTask(ctx, {
      agentId: first.agentId,
      newSession: true,
      text: "重复工作",
    }, "eve_4_0"),
    KernelError,
  );
  assert.equal(admissions.size, 4);
});

Deno.test("原生派工接线：准入回执直接进入接续状态，重启不重复派发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-delegation-" });
  await Deno.writeTextFile(`${directory}/prompt`, "单条意识流");
  let models = 0, admitted = 0;
  const ctx = {
    // deno-lint-ignore require-await -- PluginContext mocks preserve its async contract.
    async call(namespace: string, method: string, args: Record<string, Json>) {
      if (namespace === "model") {
        models++;
        assert.ok(
          (args.tools as unknown as { name: string }[]).some((t) => t.name === DELEGATE_TOOL.name),
        );
        if (models === 1) {
          return {
            content: "交给通用帮手，我继续交流和玩。",
            toolCalls: [{ id: "provider", name: "delegate_task", args: { text: "整理资料" } }],
          };
        }
        assert.ok(JSON.stringify(args.messages).includes("eve_"));
        assert.ok(JSON.stringify(args.messages).includes("queued"));
        return {
          content: "已派出",
          toolCalls: [{
            id: "rest",
            name: "rest",
            args: { reason: "暂歇", unfinished: ["通用帮手正在整理资料"], resume: "看交付" },
          }],
        };
      }
      if (namespace === "agents" && method === "send") {
        admitted++;
        return { agentId: args.agentId, jobId: args._actionId, status: "queued", requestSeq: 1 };
      }
      if (namespace === "tools") {
        assert.equal(method, "list", "delegation must not execute a script");
        return [];
      }
      return { items: [], throughSeq: 0 };
    },
  } as unknown as PluginContext;
  const config = {
    directory: `${directory}/events`,
    promptFile: `${directory}/prompt`,
    sourceCommit: "a".repeat(40),
    modelTimeoutMs: 1000,
    maxOutputTokens: 100,
    pollMs: 60000,
    defaultSleepMs: 60000,
    dailyTokenBudget: 200_000_000,
    agentsEnabled: true,
    restReviewEnabled: true,
  };
  let eve = new EveRuntime(ctx, config);
  try {
    await eve.initialize();
    await eve.pulse();
    await eve.pulse();
    assert.equal(admitted, 1);
    await eve.close();
    eve = new EveRuntime(ctx, config);
    await eve.initialize();
    await eve.pulse();
    assert.equal(admitted, 1);
    assert.equal(models, 2);
  } finally {
    await eve.close();
    await Deno.remove(directory, { recursive: true });
  }
});
