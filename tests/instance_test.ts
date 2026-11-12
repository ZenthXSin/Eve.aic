import assert from "node:assert/strict";
import { startInstance } from "../runtime/instance.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { startOwnershipRpc } from "../guardian/rpc.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { startRuntimeRpc } from "../services/runtime/rpc.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import type { Json } from "../kernel/protocol.ts";
import type { ModelRequest, ModelResponse, ToolCall } from "../contracts/model.ts";

const keys = {
  control: "fixture_instance_control_012345678901234567890",
  admin: "fixture_instance_admin_01234567890123456789012",
  exit: "fixture_instance_exit_012345678901234567890123",
  actor: "fixture_instance_actor_01234567890123456789012",
  runtime: "fixture_instance_runtime_012345678901234567890",
  phone: "fixture_instance_phone_01234567890123456789012",
};
async function fixture(options: { leaseMs?: number; modelDelayMs?: number } = {}) {
  const directory = await Deno.makeTempDir({ prefix: "eve-instance-" });
  await writeDurableJson(`${directory}/settings.json`, {
    dailyTokenBudget: 200_000_000,
    paused: false,
    timeZone: "Asia/Shanghai",
  });
  const authority = new OwnershipAuthority(`${directory}/guardian`, options.leaseMs ?? 60_000);
  await authority.initialize();
  await authority.register({
    instanceId: "fixture",
    credential: keys.actor,
    sourceCommit: "a".repeat(40),
  });
  const grant = await authority.grant({
    instanceId: "fixture",
    requestId: "initial",
    expectedEpoch: authority.status().epoch,
  });
  if (grant.status !== "granted") throw new Error("not granted");
  const guardian = startOwnershipRpc(authority, keys.admin, keys.exit);
  let renewFailures = 0;
  const guardianProxy = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (request) => {
      const data = await request.clone().json();
      if (data.method === "renew" && renewFailures > 0) {
        renewFailures--;
        return new Response(null, { status: 503 });
      }
      return await fetch(guardian.url, {
        method: "POST",
        headers: request.headers,
        body: await request.text(),
      });
    },
  );
  await writeDurableJson(`${directory}/guardian.json`, {
    url: `http://127.0.0.1:${guardianProxy.addr.port}/ownership`,
  });
  const proof = { instanceId: "fixture", credential: keys.actor, epoch: grant.owner.epoch };
  await writeDurableJson(`${directory}/proof.json`, proof);
  for (const [name, value] of Object.entries(keys)) {
    await Deno.writeTextFile(`${directory}/${name}.key`, value);
  }
  const requests: ModelRequest[] = [];
  const engine = new RuntimeEngine({
    directory: `${directory}/runtime`,
    workspace: `${directory}/workspace`,
    settingsFile: `${directory}/settings.json`,
    maxOutputTokens: 100,
    model: {
      complete(request) {
        requests.push(request);
        const result = {
          content: "我已经看到了，现在先休息。",
          toolCalls: [],
          usage: {
            status: "reported" as const,
            inputTokens: 100,
            cachedInputTokens: 80,
            outputTokens: 10,
          },
        };
        return new Promise<ModelResponse>((resolve) =>
          setTimeout(() => resolve(result as ModelResponse), options.modelDelayMs ?? 0)
        );
      },
      close() {},
    },
  });
  await engine.initialize();
  const fence = new OwnershipFence(`${directory}/fence`, "runtime", authority);
  await fence.initialize();
  const runtime = await startRuntimeRpc(engine, keys.runtime, fence);
  await writeDurableJson(`${directory}/runtime.json`, { url: runtime.url });
  const phone = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const body = await request.json();
    assert.equal(request.headers.get("authorization"), `Bearer ${keys.phone}`);
    if (body.method === "health") return Response.json({ ok: true, value: { available: true } });
    await authority.actorStatus(body.ownership.proof);
    if (body.method === "notifications") {
      return Response.json({ ok: true, value: { items: [], throughSeq: 0, hasMore: false } });
    }
    return Response.json({ ok: false, error: { category: "request" } });
  });
  await Deno.mkdir(`${directory}/eve`);
  await Deno.writeTextFile(
    `${directory}/prompt.md`,
    "你是 Eve，这是固定身份与真实能力的测试契约。",
  );
  const config = {
    instance: {
      id: "fixture",
      sourceCommit: "a".repeat(40),
      directory: `${directory}/instance`,
      keyFile: `${directory}/control.key`,
      connectionFile: `${directory}/instance.json`,
      guardianConnectionFile: `${directory}/guardian.json`,
      credentialFile: `${directory}/actor.key`,
      leaseMs: 600,
    },
    runtime: {
      connectionFile: `${directory}/runtime.json`,
      keyFile: `${directory}/runtime.key`,
      proofFile: `${directory}/proof.json`,
    },
    phone: {
      adapterUrl: `http://127.0.0.1:${phone.addr.port}/rpc`,
      adapterKeyFile: `${directory}/phone.key`,
      ownershipProofFile: `${directory}/proof.json`,
    },
    eve: {
      directory: `${directory}/eve`,
      promptFile: `${directory}/prompt.md`,
      sourceCommit: "a".repeat(40),
      remoteRecords: true,
      modelTimeoutMs: 5000,
      maxOutputTokens: 100,
      pollMs: 50,
      defaultSleepMs: 3_600_000,
      dailyTokenBudget: 200_000_000,
    },
    scripts: {
      directory: `${directory}/scripts`,
      cacheDirectory: `${directory}/script-cache`,
      denoBinary: Deno.execPath(),
      defaultTimeoutMs: 5000,
    },
    kernelDataDirectory: `${directory}/kernel`,
    mindDirectory: `${directory}/mind`,
  };
  const configPath = `${directory}/config.json`;
  await writeDurableJson(configPath, config);
  let instance = await startInstance(configPath);
  const raw = (method: string, args: unknown = {}) =>
    fetch(instance.url, {
      method: "POST",
      headers: { authorization: `Bearer ${keys.control}`, "content-type": "application/json" },
      body: JSON.stringify({ method, args }),
    });
  const call = async (method: string, args: unknown = {}): Promise<Json> => {
    const result = await (await raw(method, args)).json();
    if (!result.ok) throw new Error(`${result.error.kind}: ${result.error.message}`);
    return result.value;
  };
  return {
    directory,
    configPath,
    engine,
    authority,
    requests,
    raw,
    call,
    failRenewals(count: number) {
      renewFailures = count;
    },
    url: () => instance.url,
    async restart() {
      await instance.close();
      instance = await startInstance(configPath);
    },
    async close() {
      await instance.close();
      await runtime.close();
      await phone.shutdown();
      await guardianProxy.shutdown();
      await guardian.close();
      await authority.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}
async function waitFor(check: () => boolean | Promise<boolean>) {
  for (let i = 0; i < 500; i++) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error("实例状态10秒内未达到预期");
}

Deno.test("固定版本宿主：真实插件在线运行、并发控制串行、暂停守护后自动失去租约", async () => {
  const f = await fixture();
  try {
    await Promise.all([f.call("activate"), f.call("activate")]);
    await waitFor(() => f.engine.records.events.some((event) => event.kind === "rest"));
    const status = await f.call("status") as Record<string, Json>;
    assert.equal(status.state, "active");
    assert.equal(status.sourceCommit, "a".repeat(40));
    assert.equal((status.plugins as Json[]).length, 5);
    assert.equal(f.requests.length, 1);
    await f.authority.pause({ requestId: "user-pause", expectedEpoch: f.authority.status().epoch });
    await waitFor(async () =>
      (await f.call("status") as Record<string, Json>).state === "lease_lost"
    );
    const denied = await (await f.raw("activate")).json();
    assert.equal(denied.ok, false);
    assert.equal(f.requests.length, 1);
    await Promise.all([f.call("deactivate"), f.call("deactivate")]);
  } finally {
    await f.close();
  }
});

Deno.test("固定版本宿主：观察步骤持久幂等、工具回执成对、原意识流保持原件", async () => {
  const f = await fixture();
  try {
    await f.engine.records.append("activation", { origin: "old-version" });
    assert.deepEqual(await f.call("prepareObserver", { stepId: "upgrade-1:prepare" }), {
      throughSeq: 1,
    });
    const requestArgs = { stepId: "upgrade-1:1", candidate: "b".repeat(40), health: "healthy" };
    const request = await f.call("observerRequest", requestArgs) as unknown as ModelRequest;
    assert.deepEqual(await f.call("observerRequest", requestArgs), request);
    assert.equal(request.tools.length, 3);
    const resultArgs = {
      stepId: "upgrade-1:1",
      response: {
        content: "先保留观察",
        toolCalls: [
          {
            id: "upstream-id",
            name: "save_observation_note",
            args: { title: "观察", content: "新版尚待确认" },
          },
          { id: "upstream-id", name: "trial_candidate", args: { text: "你看到了什么？" } },
        ],
      },
    };
    const calls = await f.call("observerResponse", resultArgs) as unknown as ToolCall[];
    assert.equal(new Set(calls.map((call) => call.id)).size, 2);
    await f.restart();
    assert.deepEqual(await f.call("observerResponse", resultArgs), calls);
    await assert.rejects(() => f.call("observerRequest", { stepId: "upgrade-1:2" }), /回执/);
    await assert.rejects(
      () => f.call("observerToolResults", { items: [{ callId: "fake", value: null }] }),
      /一一对应/,
    );
    await f.call("observerToolResults", {
      stepId: "upgrade-1:1",
      items: calls.map((call) => ({ callId: call.id, value: { status: "observed" } })),
    });
    const next = await f.call("observerRequest", {
      stepId: "upgrade-1:2",
      health: "still healthy",
    }) as unknown as ModelRequest;
    assert.equal(next.messages.filter((message) => message.role === "tool").length, 2);
    await assert.rejects(
      () => f.call("observerRequest", { ...requestArgs, health: "changed" }),
      /参数发生变化/,
    );
    assert.equal(f.engine.records.events.length, 1);
    assert.equal(f.requests.length, 0);
  } finally {
    await f.close();
  }
});

Deno.test("固定版本宿主：试聊只读独立覆盖层、无真实工具，控制拒绝跨站与过大请求", async () => {
  const f = await fixture();
  try {
    await f.engine.records.append("activation", { origin: "candidate" });
    const request = await f.call("trialRequest", {
      stepId: "trial-1",
      text: "说明你的观察",
    }) as unknown as ModelRequest;
    assert.deepEqual(request.tools, []);
    assert.match(request.messages.at(-1)!.content, /独立升级试聊/);
    await assert.rejects(
      () =>
        f.call("trialResponse", {
          response: {
            content: "调用工具",
            toolCalls: [{ id: "x", name: "execute_script", args: {} }],
          },
        }),
      /不允许执行工具/,
    );
    const responseArgs = {
      stepId: "trial-1",
      response: { content: "隔离试聊回复", toolCalls: [] },
    };
    await f.call("trialResponse", responseArgs);
    await f.restart();
    assert.equal(await f.call("trialResponse", responseArgs), null);
    assert.equal(f.engine.records.events.length, 1);
    const overlay = JSON.parse(
      await Deno.readTextFile(`${f.directory}/instance/trial-overlay.json`),
    );
    assert.equal(overlay.at(-1).content, "隔离试聊回复");
    const cross = await fetch(f.url(), {
      method: "POST",
      headers: { authorization: `Bearer ${keys.control}`, origin: "http://other" },
      body: "{}",
    });
    assert.equal(cross.status, 403);
    await cross.body?.cancel();
    const invalid = await (await f.raw("status", [])).json();
    assert.equal(invalid.error.kind, "rejected");
    const large = await fetch(f.url(), {
      method: "POST",
      headers: { authorization: `Bearer ${keys.control}` },
      body: JSON.stringify({ method: "status", args: { data: "中".repeat(5_600_000) } }),
    });
    assert.equal((await large.json()).error.kind, "rejected");
  } finally {
    await f.close();
  }
});

Deno.test("固定版本宿主：拒绝同目录双宿主与旧代际激活和读取", async () => {
  const f = await fixture();
  try {
    await assert.rejects(() => startInstance(f.configPath), /已有宿主/);
    await f.authority.pause({
      requestId: "pause-before-start",
      expectedEpoch: f.authority.status().epoch,
    });
    await assert.rejects(() => f.call("activate"), /unavailable/);
    assert.equal(f.requests.length, 0);
    assert.equal((await f.call("status") as Record<string, Json>).state, "standby");
    await f.call("prepareObserver", { stepId: "stale-prepare" }).then(
      () => assert.fail("old epoch must not read timeline"),
      () => {},
    );
  } finally {
    await f.close();
  }
});

Deno.test("固定版本宿主：600毫秒租约覆盖慢分页准备，准备返回后停止续租", async () => {
  const f = await fixture({ leaseMs: 600 });
  try {
    for (let i = 0; i < 3; i++) await f.engine.records.append("notice", { origin: `old-${i}` });
    const dispatch = f.engine.dispatch.bind(f.engine);
    f.engine.dispatch = async (...args: Parameters<typeof dispatch>) => {
      if (args[0] === "timeline" && args[1] === "page") {
        await new Promise((resolve) => setTimeout(resolve, 700));
      }
      const value = await dispatch(...args);
      if (args[0] === "timeline" && args[1] === "page") {
        const page = value as { items: { seq: number }[]; throughSeq: number; hasMore: boolean };
        const items = page.items.slice(0, 1),
          throughSeq = items.at(-1)?.seq ?? Number(args[2].afterSeq);
        return { items, throughSeq, hasMore: throughSeq < f.engine.records.events.length };
      }
      return value;
    };
    assert.deepEqual(await f.call("prepareObserver", { stepId: "slow-pages" }), { throughSeq: 3 });
    assert.equal((await f.call("status") as Record<string, Json>).state, "standby");
    const expiry = f.authority.status().owner!.expiresAt;
    await new Promise((resolve) => setTimeout(resolve, 800));
    assert.equal(f.authority.status().owner!.expiresAt, expiry);
    const proof = JSON.parse(await Deno.readTextFile(`${f.directory}/proof.json`));
    await assert.rejects(() => f.authority.renew(proof), /有效在线租约/);
  } finally {
    await f.close();
  }
});

Deno.test("固定版本宿主：优雅停止在途模型超过租期时仍保存真实回复", async () => {
  const f = await fixture({ leaseMs: 600, modelDelayMs: 1500 });
  try {
    await f.call("activate");
    await waitFor(() => f.requests.length === 1);
    await f.call("deactivate");
    assert.equal((await f.call("status") as Record<string, Json>).state, "standby");
    assert.equal(
      f.engine.records.events.filter((event) => event.kind === "model_response").length,
      1,
    );
    assert.equal(
      f.engine.records.events.filter((event) => event.kind === "model_failure").length,
      0,
    );
    assert.equal(f.requests.length, 1);
  } finally {
    await f.close();
  }
});

Deno.test("固定版本宿主：真实镜像写入失败阻止停止，status及重启保留完整诊断", async () => {
  const f = await fixture({ modelDelayMs: 300 });
  try {
    await f.call("activate");
    await waitFor(() => f.requests.length === 1);
    const events = `${f.directory}/eve/remote-mirror/events`;
    const originals = new Map<string, string>();
    for await (const entry of Deno.readDir(events)) {
      if (entry.isFile) {
        originals.set(entry.name, await Deno.readTextFile(`${events}/${entry.name}`));
      }
    }
    // Keep the original fixture records; block the next local mirror commit.
    await Deno.rename(events, `${f.directory}/original-mirror-events`);
    await Deno.writeTextFile(events, "fixture停止写入错误");
    await assert.rejects(f.call("deactivate"), /storage/);
    const status = await f.call("status") as Record<string, Json>;
    assert.equal(status.state, "failed");
    assert.equal(status.error, "storage");
    const report = status.stopFailure as {
      persisted: boolean;
      failures: { plugin: string; kind: string; stage: string; timedOut: boolean }[];
    };
    assert.equal(report.persisted, true);
    assert.equal(report.failures[0].plugin, "eve");
    assert.equal(report.failures[0].stage, "disable");
    assert.equal(report.failures[0].kind, "storage");
    assert.equal(report.failures[0].timedOut, false);
    for (const [name, original] of originals) {
      assert.equal(
        await Deno.readTextFile(`${f.directory}/original-mirror-events/${name}`),
        original,
      );
    }
    await f.restart();
    assert.deepEqual(
      (await f.call("status") as Record<string, Json>).stopFailure,
      status.stopFailure,
    );
  } finally {
    await f.close();
  }
});

Deno.test("固定版本宿主：3387条观察原件以27页读取，保持来源链和完整快照", async () => {
  const f = await fixture();
  try {
    for (let i = 0; i < 3387; i++) {
      await f.engine.records.append("notice", { origin: `history-${i}` });
    }
    let pages = 0;
    const dispatch = f.engine.dispatch.bind(f.engine);
    f.engine.dispatch = async (...args: Parameters<typeof dispatch>) => {
      if (args[0] === "timeline" && args[1] === "page") pages++;
      return await dispatch(...args);
    };
    assert.deepEqual(await f.call("prepareObserver", { stepId: "large-history" }), {
      throughSeq: 3387,
    });
    assert.equal(pages, 27);
    const events = JSON.parse(
      await Deno.readTextFile(`${f.directory}/instance/observer-snapshot.json`),
    );
    assert.equal(events.length, 3387);
    assert.equal(events.at(-1).digest, f.engine.records.events.at(-1)!.digest);
    assert.equal(f.requests.length, 0);
  } finally {
    await f.close();
  }
});

Deno.test("续租：通信未知在有效租约内重试，明确失效仍停用并留下具体诊断", async () => {
  const f = await fixture({ leaseMs: 3000 });
  try {
    await f.call("activate");
    const before = await f.call("status") as { lease: { lastRenewedAt: number } };
    f.failRenewals(1);
    const diagnosticPath = `${f.directory}/instance/lease-failure.json`;
    await waitFor(async () => {
      try {
        return JSON.parse(await Deno.readTextFile(diagnosticPath)).retry === true;
      } catch (error) {
        if (error instanceof Deno.errors.NotFound) return false;
        throw error;
      }
    });
    await waitFor(async () =>
      (await f.call("status") as { lease: { lastRenewedAt: number } }).lease.lastRenewedAt >
        before.lease.lastRenewedAt
    );
    assert.equal((await f.call("status") as { state: string }).state, "active");
    await f.authority.pause({ requestId: "real-pause", expectedEpoch: f.authority.status().epoch });
    await waitFor(async () => (await f.call("status") as { state: string }).state === "lease_lost");
    const diagnostic = JSON.parse(await Deno.readTextFile(diagnosticPath));
    assert.equal(diagnostic.retry, false);
    assert.equal(diagnostic.error.kind, "unavailable");
    assert.ok(diagnostic.lastRenewedAt > 0);
    assert.ok(diagnostic.leaseExpiresAt > diagnostic.lastRenewedAt);
  } finally {
    await f.close();
  }
});

Deno.test("续租：通信持续未知不得越过最后确认的租约期限继续运行", async () => {
  const f = await fixture({ leaseMs: 1500 });
  try {
    await f.call("activate");
    f.failRenewals(1000);
    await waitFor(async () => (await f.call("status") as { state: string }).state === "lease_lost");
    const diagnostic = JSON.parse(
      await Deno.readTextFile(`${f.directory}/instance/lease-failure.json`),
    );
    assert.equal(diagnostic.error.kind, "unknown");
    assert.equal(diagnostic.retry, false);
    assert.ok(Date.parse(diagnostic.at) >= diagnostic.leaseExpiresAt);
  } finally {
    await f.close();
  }
});
