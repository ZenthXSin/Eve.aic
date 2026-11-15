import assert from "node:assert/strict";
import { readAgentModels } from "../services/runtime/agent-models.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { ChatModelClient, type ChatModelOptions } from "../plugins/model-chat/client.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

Deno.test("agent role profiles isolate credentials, inherit transport and preserve per-session choices", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agent-models-" });
  const calls: { key: string | null; model: string; reasoning: string; stream: boolean }[] = [];
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (req) => {
    assert.equal(new URL(req.url).pathname, "/v1/chat/completions");
    const body = await req.json();
    assert.ok(body.prompt_cache_key.length <= 64);
    assert.equal(req.headers.get("session_id"), body.prompt_cache_key);
    assert.equal(req.headers.get("X-Session-ID"), body.prompt_cache_key);
    calls.push({
      key: req.headers.get("authorization"),
      model: body.model,
      reasoning: body.reasoning_effort,
      stream: body.stream,
    });
    return Response.json({
      choices: [{ message: { role: "assistant", content: "done" }, finish_reason: "stop" }],
      usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110 },
    });
  });
  const inherited: ChatModelOptions = {
    baseUrl: `http://127.0.0.1:${server.addr.port}`,
    protocol: "chat",
    model: "shared-model",
    apiKey: "main-fixture-key",
    sessionId: "main",
    reasoningEffort: "high",
    maxOutputTokens: 8192,
    timeoutMs: 10000,
  };
  let engine: RuntimeEngine | undefined;
  try {
    await Deno.writeTextFile(`${directory}/coding-key`, "coding-fixture-key");
    const agentModels = await readAgentModels({
      coding: {
        model: "gpt-6-astra",
        apiKeyFile: `${directory}/coding-key`,
        reasoningEffort: "high",
      },
    }, inherited);
    await writeDurableJson(`${directory}/settings`, {
      dailyTokenBudget: 1000000,
      paused: false,
      timeZone: "UTC",
    });
    const client = new ChatModelClient(inherited);
    engine = new RuntimeEngine({
      directory: `${directory}/data`,
      workspace: `${directory}/workspace`,
      settingsFile: `${directory}/settings`,
      maxOutputTokens: 8192,
      model: client,
      modelName: inherited.model,
      modelSelection: {
        snapshot: () => ({
          client,
          modelName: inherited.model,
          maxOutputTokens: 8192,
          revision: 3,
        }),
        current: () => ({ model: inherited.model, revision: 3 }),
      },
      agentModels,
    });
    await engine.initialize();
    assert.equal(
      engine.agents.list().items.find((a) => a.agentId === "agent_coding")?.config.model,
      "gpt-6-astra",
    );
    await engine.dispatch(
      "agents",
      "send",
      { agentId: "agent_coding", text: "code" },
      "script_coding_task_1",
    );
    await engine.dispatch(
      "agents",
      "send",
      { agentId: "agent_review", text: "review" },
      "agent_review_test",
    );
    await engine.dispatch("model", "complete", {
      messages: [{ role: "user", content: "hello" }],
      tools: [],
    }, "model_1");
    const second = await engine.agents.create({ role: "coding" }, "second_coder");
    assert.equal(second.agentId.length, 70);
    assert.equal(second.config.model, "gpt-6-astra");
    await engine.agents.configure({
      agentId: second.agentId,
      model: "another-model",
      reasoningEffort: "medium",
    });
    await engine.dispatch(
      "agents",
      "send",
      { agentId: second.agentId, text: "code" },
      "script_second_coding_task_2",
    );
    assert.deepEqual(calls, [
      { key: "Bearer coding-fixture-key", model: "gpt-6-astra", reasoning: "high", stream: true },
      { key: "Bearer main-fixture-key", model: "shared-model", reasoning: "high", stream: true },
      { key: "Bearer main-fixture-key", model: "shared-model", reasoning: "high", stream: true },
      {
        key: "Bearer coding-fixture-key",
        model: "another-model",
        reasoning: "medium",
        stream: true,
      },
    ]);
    const saved = engine.modelResponses.events.map((e) => e.data as { modelName: string });
    assert.deepEqual(saved.map((e) => e.modelName), [
      "gpt-6-astra",
      "shared-model",
      "shared-model",
      "another-model",
    ]);
    assert.deepEqual(
      engine.modelResponses.events.map((e) =>
        (e.data as { credentialProfile: string }).credentialProfile
      ),
      ["agent:coding", "shared", "shared", "agent:coding"],
    );
    for (const journal of [engine.agents.journal, engine.transactions, engine.modelResponses]) {
      assert.equal(JSON.stringify(journal.events).includes("fixture-key"), false);
    }
  } finally {
    await engine?.close();
    await server.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
