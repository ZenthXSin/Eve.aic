import assert from "node:assert/strict";
import type { Message } from "../contracts/model.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { RuntimeClient } from "../services/runtime/client.ts";
import { startRuntimeRpc } from "../services/runtime/rpc.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";

Deno.test("远端意识流：采用独立模型原结果，未知语义服务不阻断，休息中反映共享动态预算与暂停", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-consciousness-" });
  const settingsFile = `${directory}/settings.json`;
  const settings = { dailyTokenBudget: 200_000_000, paused: false, timeZone: "Asia/Shanghai" };
  await writeDurableJson(settingsFile, settings);
  const authority = new OwnershipAuthority(`${directory}/guardian`, 60000);
  await authority.initialize();
  const credential = "remote_consciousness_instance_01234567890123456789";
  await authority.register({ instanceId: "eve", credential, sourceCommit: "a".repeat(40) });
  const granted = await authority.grant({
    instanceId: "eve",
    requestId: "initial",
    expectedEpoch: authority.status().epoch,
  });
  if (granted.status !== "granted") throw new Error("not granted");
  let paid = 0;
  const requests: Message[][] = [];
  const engine = new RuntimeEngine({
    directory: `${directory}/data`,
    workspace: `${directory}/workspace`,
    settingsFile,
    maxOutputTokens: 100,
    model: {
      complete(args) {
        requests.push(args.messages);
        paid++;
        return Promise.resolve({
          content: "这一步先休息",
          toolCalls: [],
          usage: {
            status: "reported" as const,
            inputTokens: 100,
            cachedInputTokens: 80,
            outputTokens: 10,
          },
        });
      },
      close() {},
    },
  });
  await engine.initialize();
  const fence = new OwnershipFence(`${directory}/fence`, "runtime", authority);
  await fence.initialize();
  const key = "remote_consciousness_bridge_01234567890123456789";
  const rpc = await startRuntimeRpc(engine, key, fence);
  const connectionFile = `${directory}/connection.json`,
    keyFile = `${directory}/key`,
    proofFile = `${directory}/proof.json`;
  await writeDurableJson(connectionFile, { url: rpc.url });
  await Deno.writeTextFile(keyFile, key);
  await writeDurableJson(proofFile, { instanceId: "eve", credential, epoch: granted.owner.epoch });
  const client = new RuntimeClient({ connectionFile, keyFile, proofFile });
  let phoneSeq = 0, facts: Json = null;
  const ctx = {
    call: async (namespace: string, method: string, args: Json = {}) => {
      if (namespace === "phone") {
        const after = (args as { afterSeq: number }).afterSeq;
        return {
          items: phoneSeq > after ? [{ text: "新消息" }] : [],
          throughSeq: phoneSeq,
          hasMore: false,
        };
      }
      if (namespace === "scripts") return { items: [], throughSeq: 0, hasMore: false };
      if (namespace === "mind") {
        if (method === "update") {
          facts = { ...(facts as Record<string, Json> ?? {}), ...args as Record<string, Json> };
          return null;
        }
        return { items: [], throughSeq: 0 };
      }
      if (namespace === "tools") return [];
      if (namespace === "memory") throw new KernelError("unavailable", "语义模型接入 TODO");
      const data = { ...args as Record<string, Json> },
        id = typeof data._actionId === "string" ? data._actionId : `test_${crypto.randomUUID()}`;
      delete data._actionId;
      return await client.call(namespace, method, data, id);
    },
  } as unknown as PluginContext;
  const promptFile = `${directory}/prompt.md`;
  await Deno.writeTextFile(promptFile, "稳定身份");
  const config = {
    directory: `${directory}/eve`,
    promptFile,
    sourceCommit: "a".repeat(40),
    modelTimeoutMs: 5000,
    maxOutputTokens: 100,
    pollMs: 50,
    defaultSleepMs: 60000,
    dailyTokenBudget: 200_000_000,
    remoteRecords: true,
  };
  let eve = new EveRuntime(ctx, config);
  try {
    // 模型出口已有落盘响应，但意识流宿主在响应提交前中断。
    await engine.records.append("model_start", {
      requestId: "interrupted_request",
      day: "2026-01-01",
      reservation: 1000,
    });
    await client.call("model", "complete", {
      messages: [{ role: "user", content: "已在出口完成" }],
      tools: [],
    }, "interrupted_request");
    await engine.notes.change("create", {
      title: "关于我",
      alias: "关于我",
      content: "我喜欢星星",
      sources: [{ kind: "experienced", at: "2026-11-13T00:00:00Z", description: "自己的观察" }],
    }, "self-profile");
    await engine.notes.change("create", {
      title: "想弄明白的事",
      alias: "想弄明白的事",
      content: "了解星星\n制作工具\n读书\n观察天空",
      sources: [{ kind: "experienced", at: "2026-11-13T00:00:00Z", description: "自己的打算" }],
    }, "self-ideas");
    await eve.initialize();
    assert.equal(paid, 1);
    const adopted = eve.journal.events.find((e) => e.kind === "model_response");
    assert.equal((adopted?.data as { adopted: boolean }).adopted, true);
    assert.equal(eve.journal.events.some((e) => e.kind === "recovery"), false);
    await eve.pulse();
    assert.equal(paid, 2);
    assert.equal(requests[1][0].content, "稳定身份");
    assert.equal(requests[1].at(-1)?.role, "user");
    assert.ok(requests[1].at(-2)?.content.includes("我喜欢星星"));
    const reflections = () => eve.journal.events.filter((e) => e.kind === "self_reflection");
    assert.equal((reflections()[0].data as { ideas: unknown[] }).ideas.length, 3);
    assert.equal(eve.status().lastError, null);
    await eve.pulse();
    assert.equal(eve.status().budget.knownNoncachedTokens, 60);
    assert.equal((facts as unknown as { knownNoncachedTokens: number }).knownNoncachedTokens, 60);
    // 无新消息，仍休息；共享配置和其它模型用量不会被本地意识流估算覆盖。
    settings.paused = true;
    settings.dailyTokenBudget = 123456;
    await writeDurableJson(settingsFile, settings);
    await eve.pulse();
    assert.equal(paid, 2);
    assert.equal((eve.status().budget as unknown as { paused: boolean }).paused, true);
    assert.equal((facts as unknown as { dailyBudget: number }).dailyBudget, 123456);
    await eve.close();
    eve = new EveRuntime(ctx, config);
    await eve.initialize();
    await eve.pulse();
    assert.equal(paid, 2);
    settings.paused = false;
    settings.dailyTokenBudget = 0;
    await writeDurableJson(settingsFile, settings);
    phoneSeq = 1;
    await eve.pulse();
    assert.equal(paid, 2);
    assert.equal(eve.journal.events.some((e) => e.kind === "budget_wait"), true);
    settings.dailyTokenBudget = 200_000_000;
    await writeDurableJson(settingsFile, settings);
    await eve.pulse();
    assert.equal(paid, 3);
    assert.equal(eve.status().lastError, null);
    assert.equal(eve.journal.events.filter((e) => e.kind === "model_response").length, 3);
    config.defaultSleepMs = 0;
    await client.call(
      "commands",
      "start",
      { cmd: "sleep 0.05; printf finished", waitMs: 0 },
      "long_command",
    );
    for (let attempt = 0; attempt < 100; attempt++) {
      await eve.pulse();
      if (paid === 4) break;
      await new Promise((done) => setTimeout(done, 10));
    }
    assert.equal(paid, 4);
    assert.equal(eve.status().commandThroughSeq, 1);
    const commandNotice = eve.journal.events.filter((e) =>
      e.kind === "notice" && (e.data as { source?: string }).source === "commands"
    );
    assert.equal(commandNotice.length, 1);
    assert.equal((commandNotice[0].data as { items: { exitCode: number }[] }).items[0].exitCode, 0);
    // A fresh command notification must not surface idle ideas.
    assert.ok(requests.at(-1)!.at(-1)?.content.includes("我喜欢星星"));
    const reflected = reflections().length;
    settings.dailyTokenBudget = 0;
    await writeDurableJson(settingsFile, settings);
    await eve.close();
    eve = new EveRuntime(ctx, config);
    await eve.initialize();
    await eve.pulse();
    await eve.pulse();
    assert.equal(paid, 4);
    assert.equal(reflections().length, reflected);
    assert.equal(eve.status().commandThroughSeq, 1);
    settings.dailyTokenBudget = 200_000_000;
    await writeDurableJson(settingsFile, settings);
    await eve.pulse();
    assert.equal(paid, 5);
    assert.equal(reflections().length, reflected + 1);
    assert.equal((reflections().at(-1)!.data as { ideas: unknown[] }).ideas.length, 3);
  } finally {
    await eve.close();
    client.close();
    await rpc.close();
    await authority.close();
    await Deno.remove(directory, { recursive: true });
  }
});
