import assert from "node:assert/strict";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { RuntimeClient } from "../services/runtime/client.ts";
import { startRuntimeRpc } from "../services/runtime/rpc.ts";
import { ChatModelClient } from "../plugins/model-chat/client.ts";
import { createRuntimeClientPlugin } from "../plugins/runtime-client/mod.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";
import type { Handler, PluginContext } from "../sdk/plugin.ts";
import type { ModelRequest } from "../contracts/model.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

Deno.test("模型目的隔离：在线插件实际强化，trial/observer/未标注讨论仅保存响应与共享费用，重启重试不重复", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-model-memory-isolation-" });
  let paid = 0, onlyToolArgs = false;
  const provider = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (request) => {
      const body = await request.json();
      assert.equal(Object.hasOwn(body, "_purpose"), false);
      paid++;
      return Response.json({
        choices: [{
          finish_reason: onlyToolArgs ? "tool_calls" : "stop",
          message: {
            role: "assistant",
            content: onlyToolArgs ? "我想继续处理。" : "我想到QQ上的这件事。",
            ...(onlyToolArgs
              ? {
                tool_calls: [{
                  id: "call_1",
                  type: "function",
                  function: {
                    name: "execute_script",
                    arguments: JSON.stringify({
                      code: 'await tools.phone.read({handle:{conversationId:"qq:user:QQ",seq:1}});',
                    }),
                  },
                }],
              }
              : {}),
          },
        }],
        usage: {
          prompt_tokens: 12,
          completion_tokens: 3,
          prompt_tokens_details: { cached_tokens: 8 },
        },
      });
    },
  );
  const authority = new OwnershipAuthority(`${directory}/guardian`, 60_000);
  await authority.initialize();
  const credential = "fixture_isolation_owner_012345678901234567890";
  await authority.register({ instanceId: "eve", credential, sourceCommit: "a".repeat(40) });
  const grant = await authority.grant({
    instanceId: "eve",
    requestId: "initial",
    expectedEpoch: authority.status().epoch,
  });
  assert.equal(grant.status, "granted");
  if (grant.status !== "granted") throw new Error("ownership unavailable");
  const settingsFile = `${directory}/settings.json`;
  await writeDurableJson(settingsFile, {
    dailyTokenBudget: 200_000_000,
    paused: false,
    timeZone: "Asia/Tokyo",
  });
  const makeEngine = () => {
    const model = new ChatModelClient({
      baseUrl: `http://127.0.0.1:${provider.addr.port}`,
      model: "fixture",
      apiKey: "fixture_only",
      sessionId: "memory-isolation",
      maxOutputTokens: 100,
      timeoutMs: 5000,
    });
    return new RuntimeEngine({
      directory: `${directory}/data`,
      workspace: `${directory}/workspace`,
      settingsFile,
      maxOutputTokens: 100,
      model: {
        complete(request: ModelRequest) {
          // 验证出口已剥离内部字段，而非依赖HTTP客户端恰好忽略它。
          assert.equal(Object.hasOwn(request, "_purpose"), false);
          return model.complete(request);
        },
        close: () => model.close(),
      },
    });
  };
  let engine = makeEngine();
  await engine.initialize();
  let fence = new OwnershipFence(`${directory}/fence`, "runtime", authority);
  await fence.initialize();
  const key = "fixture_isolation_bridge_012345678901234567890";
  let rpc = await startRuntimeRpc(engine, key, fence);
  const runtime = {
    connectionFile: `${directory}/connection.json`,
    keyFile: `${directory}/key`,
    proofFile: `${directory}/proof.json`,
  };
  await writeDurableJson(runtime.connectionFile, { url: rpc.url });
  await Deno.writeTextFile(runtime.keyFile, key);
  await writeDurableJson(runtime.proofFile, {
    instanceId: "eve",
    credential,
    epoch: grant.owner.epoch,
  });
  const configFile = `${directory}/config.json`;
  await writeDurableJson(configFile, { runtime });
  const methods: Record<string, Record<string, Handler>> = {};
  const closeHooks: (() => unknown)[] = [];
  const ctx = {
    provide(namespace: string, handlers: Record<string, Handler>) {
      methods[namespace] = handlers;
    },
    onDisable(close: () => unknown) {
      closeHooks.push(close);
    },
  } as unknown as PluginContext;
  const plugin = createRuntimeClientPlugin(configFile), client = new RuntimeClient(runtime);
  const request = { messages: [{ role: "user", content: "本地协议夹具" }], tools: [] };
  try {
    await plugin.load!(ctx);
    await plugin.enable!(ctx);
    const source = await engine.records.append("activation", { fixture: true });
    const memory = await engine.memory.remember({
      text: "我记得QQ的首次启动。",
      entities: ["QQ"],
      sources: [{ recordSeq: source.seq, digest: source.digest, kind: "experienced" }],
    }, "formation");
    assert.equal(memory.strength, 1);
    const online = await methods.model.complete({ ...request, _actionId: "model_1" });
    assert.equal(engine.memory.get({ id: memory.id }).strength, 3);
    for (
      const [id, purpose] of [["upgrade_trial", "isolated"], ["upgrade_observer", "isolated"], [
        "codex_discussion",
        undefined,
      ]]
    ) {
      const args = purpose ? { ...request, _purpose: purpose } : request;
      assert.deepEqual(await client.call("model", "complete", args, id), online);
      assert.equal(engine.memory.get({ id: memory.id }).strength, 3);
    }
    assert.equal(paid, 4);
    assert.equal(engine.modelResponses.events.length, 4);
    assert.equal((await engine.budget.status()).knownNoncachedTokens, 28);
    assert.equal(engine.memory.journal.events.filter((event) => event.kind === "used").length, 1);
    onlyToolArgs = true;
    const toolOnly = await methods.model.complete({
      ...request,
      tools: [{
        name: "execute_script",
        description: "fixture",
        parameters: { type: "object", properties: { code: { type: "string" } } },
      }],
      _actionId: "model_2",
    }) as unknown as { toolCalls: { args: Json }[] };
    assert.equal(toolOnly.toolCalls.length, 1);
    assert.ok(JSON.stringify(toolOnly.toolCalls[0].args).includes("QQ"));
    assert.equal(engine.memory.get({ id: memory.id }).strength, 3);
    assert.equal(paid, 5);
    assert.equal((await engine.budget.status()).knownNoncachedTokens, 35);
    assert.deepEqual(await methods.model.complete({ ...request, _actionId: "model_1" }), online);
    await assert.rejects(
      client.call("model", "complete", { ...request, _purpose: "isolated" }, "model_1"),
      KernelError,
    );
    await assert.rejects(
      client.call("model", "complete", { ...request, _purpose: "typo" }, "invalid_purpose"),
      KernelError,
    );
    assert.equal(paid, 5);
    await rpc.close();
    engine = makeEngine();
    await engine.initialize();
    fence = new OwnershipFence(`${directory}/fence`, "runtime", authority);
    await fence.initialize();
    rpc = await startRuntimeRpc(engine, key, fence);
    await writeDurableJson(runtime.connectionFile, { url: rpc.url });
    assert.deepEqual(await methods.model.complete({ ...request, _actionId: "model_1" }), online);
    assert.deepEqual(
      await client.call("model", "complete", { ...request, _purpose: "isolated" }, "upgrade_trial"),
      online,
    );
    assert.deepEqual(
      await client.call("model", "result", { requestId: "codex_discussion" }),
      online,
    );
    assert.equal(paid, 5);
    assert.equal(engine.memory.get({ id: memory.id }).strength, 3);
  } finally {
    client.close();
    for (const close of closeHooks.reverse()) await close();
    await rpc.close();
    await authority.close();
    await provider.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
