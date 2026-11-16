import assert from "node:assert/strict";
import type { AgentJob, AgentRole } from "../contracts/agents.ts";
import type { Message, ModelResponse } from "../contracts/model.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { delegateTask } from "../plugins/eve/delegation.ts";
import { ChatModelClient } from "../plugins/model-chat/client.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { type AgentHooks, AgentService } from "../services/runtime/agents.ts";
import { readAgentModels } from "../services/runtime/agent-models.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => resolve = r);
  return { promise, resolve };
}
const final = (content = "verified"): ModelResponse => ({ content, toolCalls: [] });
const dispatch = (role = "coding"): ModelResponse => ({
  content: "delegate",
  toolCalls: [{ id: "dispatch", name: "delegate_task", args: { role, text: "child" } }],
});
async function prefix(source: string, dest: string, through: number) {
  await Deno.mkdir(`${dest}/events`, { recursive: true });
  await Deno.mkdir(`${dest}/bodies`);
  for await (const file of Deno.readDir(`${source}/bodies`)) {
    await Deno.copyFile(`${source}/bodies/${file.name}`, `${dest}/bodies/${file.name}`);
  }
  for (let seq = 1; seq <= through; seq++) {
    const name = `${String(seq).padStart(16, "0")}.json`;
    await Deno.copyFile(`${source}/events/${name}`, `${dest}/events/${name}`);
  }
}

Deno.test("native Eve IDs reach real Chat HTTP for all four roles; general joins three parallel specialists", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-native-children-" });
  const roles: AgentRole[] = ["general", "coding", "review", "investigation"];
  const started = deferred();
  let children = 0, generalStep = 0;
  const calls: { key: string | null; model: string; effort: string; general: boolean }[] = [];
  const fullText = "完整交付。".repeat(3000);
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (req) => {
    assert.equal(new URL(req.url).pathname, "/v1/chat/completions");
    const body = await req.json();
    assert.equal(body.stream, true);
    const general = body.tools.some((t: { function: { name: string } }) =>
      t.function.name === "delegate_task"
    );
    calls.push({
      key: req.headers.get("authorization"),
      model: body.model,
      effort: body.reasoning_effort,
      general,
    });
    let message: unknown = { role: "assistant", content: "done" };
    if (
      general && body.messages.some((m: Message) => m.role === "user" && m.content === "nested")
    ) {
      generalStep++;
      if (generalStep === 1) {
        message = {
          role: "assistant",
          content: "parallel delegation",
          tool_calls: roles.slice(1).map((role) => ({
            id: role,
            type: "function",
            function: {
              name: "delegate_task",
              arguments: JSON.stringify({ role, text: `child-${role}`, newSession: true }),
            },
          })),
        };
      } else if (generalStep === 2) {
        message = {
          role: "assistant",
          content: "wait",
          tool_calls: [{
            id: "wait",
            type: "function",
            function: { name: "wait_for_agents", arguments: "{}" },
          }],
        };
      } else {
        const receipt = JSON.parse(body.messages.at(-1).content);
        assert.equal(receipt.items.length, 3);
        assert.ok(
          receipt.items.every((x: { text: string; status: string }) =>
            x.text === fullText && x.status === "completed"
          ),
        );
        message = { role: "assistant", content: "verified three deliveries" };
      }
    } else if (
      body.messages.some((m: Message) => m.role === "user" && m.content.startsWith("child-"))
    ) {
      assert.equal(general, false, "specialists receive no delegation or waiting capability");
      assert.deepEqual(body.tools.map((t: { function: { name: string } }) => t.function.name), [
        "use_tool",
      ]);
      if (++children === 3) started.resolve();
      await started.promise;
      message = { role: "assistant", content: fullText };
    }
    return Response.json({
      choices: [{ message, finish_reason: "stop" }],
      usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110 },
    });
  });
  const base = {
    baseUrl: `http://127.0.0.1:${server.addr.port}`,
    protocol: "chat" as const,
    model: "eve-model",
    apiKey: "eve-fixture-key",
    reasoningEffort: "high" as const,
    maxOutputTokens: 8192,
    timeoutMs: 10000,
    sessionId: "eve",
  };
  let engine: RuntimeEngine | undefined;
  try {
    await Deno.writeTextFile(`${directory}/specialist-key`, "specialist-fixture-key");
    const profile = {
      model: "gpt-6.1-sol",
      apiKeyFile: `${directory}/specialist-key`,
      reasoningEffort: "high" as const,
    };
    await writeDurableJson(`${directory}/settings`, {
      dailyTokenBudget: 1000000,
      paused: false,
      timeZone: "UTC",
    });
    engine = new RuntimeEngine({
      directory: `${directory}/data`,
      workspace: `${directory}/workspace`,
      settingsFile: `${directory}/settings`,
      maxOutputTokens: 8192,
      model: new ChatModelClient(base),
      modelName: base.model,
      agentModels: await readAgentModels({
        coding: profile,
        review: profile,
        investigation: profile,
      }, base),
    });
    await engine.initialize();
    const ctx = {
      call(namespace: string, method: string, value: Record<string, Json>) {
        const { _actionId, ...args } = value;
        return engine!.dispatch(namespace, method, args, String(_actionId));
      },
    } as unknown as PluginContext;
    for (const [i, role] of roles.entries()) {
      const job = await delegateTask(
        ctx,
        { role, text: `direct-${role}`, newSession: true },
        `eve_51039_${i}`,
      );
      assert.equal(job.status, "completed");
      assert.equal(job.jobId, `eve_51039_${i}_task`);
    }
    assert.deepEqual(calls.slice(0, 4).map((c) => [c.key, c.model, c.effort]), [
      ["Bearer eve-fixture-key", "eve-model", "high"],
      ...roles.slice(1).map(() => ["Bearer specialist-fixture-key", "gpt-6.1-sol", "high"]),
    ]);
    const before = calls.length;
    const config = engine.agents.list().items.find((a) => a.agentId === "agent_coding")!.config;
    for (const id of ["eve_fake_task_model_1", "agent_fake_model_1", "model_1"]) {
      await assert.rejects(
        engine.dispatch("model", "complete", {
          messages: [{ role: "user", content: "forge" }],
          tools: [],
          _agentConfig: { ...config, sessionId: "agent_coding" },
        } as unknown as Record<string, Json>, id),
        KernelError,
      );
    }
    assert.equal(calls.length, before, "forged requests never reach the model endpoint");
    const root = await delegateTask(ctx, { text: "nested", newSession: true }, "eve_51274_0");
    assert.equal(root.status, "completed");
    assert.equal(root.childJobIds?.length, 3);
    for (const id of root.childJobIds!) {
      const child = engine.agents.status({ jobId: id });
      assert.equal(child.parentJobId, root.jobId);
      assert.equal(child.rootJobId, root.jobId);
      assert.equal((await engine.agents.message({ seq: child.replySeq! })).text, fullText);
    }
    assert.equal(
      engine.agents.notifications().items.length,
      5,
      "only Eve's direct agents notify her",
    );
    assert.equal(
      (await engine.agents.message({ seq: root.replySeq! })).text,
      "verified three deliveries",
    );
  } finally {
    started.resolve();
    await engine?.close();
    await server.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("general recovery reuses child admission and waits for unconsumed delivery before finalizing", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-child-recovery-" });
  const hooks: AgentHooks = {
    complete(_request, options, id) {
      return Promise.resolve(
        (options as { role?: string }).role !== "general"
          ? final("child result")
          : id.endsWith("_model_1")
          ? dispatch()
          : id.endsWith("_model_2")
          ? final("too early")
          : final(),
      );
    },
    invoke: () => Promise.resolve(null),
  };
  const source = new AgentService(`${directory}/source`, hooks);
  try {
    await source.initialize();
    const job = await source.send({ agentId: "agent_general", text: "root" }, "eve_51039_1_task");
    await source.close();
    const child = source.status({ jobId: job.childJobIds![0] });
    const boundaries = new Set([child.requestSeq]);
    for (const event of source.journal.events) {
      if (event.kind !== "step" || (event.data as { jobId: string }).jobId !== job.jobId) continue;
      const hash = (event.data as { body: { hash: string } }).body.hash;
      const message: Message = JSON.parse(
        await Deno.readTextFile(`${source.directory}/bodies/${hash}.json`),
      );
      if (message.role === "user" || message.role === "assistant" && !message.toolCalls?.length) {
        boundaries.add(event.seq);
      }
    }
    for (const through of boundaries) {
      const dest = `${directory}/prefix-${through}`;
      await prefix(source.directory, dest, through);
      const restored = new AgentService(dest, hooks);
      try {
        await restored.initialize();
        const result = await restored.send({ agentId: job.agentId, text: "root" }, job.jobId);
        assert.equal(result.status, "completed");
        assert.deepEqual(result.childJobIds, job.childJobIds);
        assert.equal((await restored.message({ seq: result.replySeq! })).text, "verified");
        assert.equal(restored.journal.events.filter((e) => e.kind === "accepted").length, 2);
        assert.equal(restored.notifications().items.length, 1);
      } finally {
        await restored.close();
      }
    }
  } finally {
    await source.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("all three specialists reject delegation even when the model invents a tool call", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-child-depth-" });
  const service = new AgentService(directory, {
    complete(request, _options, id) {
      if (id.endsWith("_model_1")) {
        assert.deepEqual(request.tools.map((t) => t.name), ["use_tool"]);
        return Promise.resolve(dispatch());
      }
      assert.match(request.messages.at(-1)!.content, /仅通用子代理可以派工/);
      return Promise.resolve(final());
    },
    invoke() {
      throw new Error("no external delegation allowed");
    },
  });
  try {
    await service.initialize();
    for (const role of ["coding", "review", "investigation"]) {
      assert.equal(
        (await service.send({ agentId: `agent_${role}`, text: "task" }, `eve_${role}_task`)).status,
        "completed",
      );
    }
    assert.equal(service.list().items.length, 4);
    assert.equal(service.journal.events.filter((e) => e.kind === "accepted").length, 3);
  } finally {
    await service.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("cancelling a general coordinator releases its wait and cancels children without losing receipts", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-child-cancel-" });
  const began = deferred(), gate = deferred();
  const service = new AgentService(directory, {
    async complete(_request, options) {
      if ((options as { role?: string }).role === "general") {
        const message = dispatch();
        message.toolCalls.push({ id: "wait", name: "wait_for_agents", args: {} });
        return message;
      }
      began.resolve();
      await gate.promise;
      return final("actual result");
    },
    invoke: () => Promise.resolve(null),
  });
  let work: Promise<AgentJob> | undefined;
  try {
    await service.initialize();
    work = service.send({ agentId: "agent_general", text: "root" }, "eve_cancel_task");
    await began.promise;
    const cancelled = await service.cancel({ jobId: "eve_cancel_task" });
    assert.equal((await work).status, "cancelled");
    gate.resolve();
    await service.close();
    const child = service.status({ jobId: cancelled.childJobIds![0] });
    assert.equal(child.status, "cancelled");
    assert.ok(child.replySeq);
    assert.equal(service.journal.events.filter((e) => e.kind === "cancelled").length, 2);
  } finally {
    gate.resolve();
    await work?.catch(() => {});
    await service.close();
    await Deno.remove(directory, { recursive: true });
  }
});
