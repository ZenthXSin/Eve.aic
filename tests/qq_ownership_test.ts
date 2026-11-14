import assert from "node:assert/strict";
import { QQ_THIRD_PARTY_SDK } from "../adapters/qq/satori/api.ts";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { startPhoneRpc } from "../adapters/qq/rpc.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import type { OwnershipProof } from "../contracts/ownership.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { PhoneRpcClient, PhoneRpcError } from "../plugins/phone/client.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";
import { startQqPhoneAdapter } from "../adapters/qq/service.ts";

const oldKey = "o".repeat(48), newKey = "n".repeat(48), bridgeKey = "b".repeat(48);
function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { resolve, promise };
}
async function waitFor(check: () => boolean) {
  for (let i = 0; i < 1000; i++) {
    if (check()) return;
    await new Promise((done) => setTimeout(done, 5));
  }
  throw new Error("QQ 实际链路没有在5秒内完成");
}
async function setup(hold = false) {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-ownership-" });
  const guard = new OwnershipAuthority(`${directory}/guard`, 30_000);
  await guard.initialize();
  await guard.register({ instanceId: "old", sourceCommit: "7".repeat(40), credential: oldKey });
  await guard.register({ instanceId: "new", sourceCommit: "8".repeat(40), credential: newKey });
  const initial = await guard.grant({
    instanceId: "old",
    requestId: "initial",
    expectedEpoch: guard.status().epoch,
  });
  if (initial.status !== "granted") throw new Error("not granted");
  const proof: OwnershipProof = {
    instanceId: "old",
    epoch: initial.owner.epoch,
    credential: oldKey,
  };
  const gate = deferred();
  if (!hold) gate.resolve();
  let sends = 0;
  const platform = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture", expires_in: 7200 });
    }
    await r.json();
    sends++;
    await gate.promise;
    return Response.json({ id: "actual-offline-platform-message" });
  });
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
      sendScope: "private",
    }),
    `${directory}/actions`,
  );
  await actions.initialize();
  const timeline = new QqTimeline(`${directory}/timeline`);
  await timeline.initialize();
  const reading = new QqPhoneReading(`${directory}/phone-reading`);
  await reading.initialize();
  const incoming = new QqInboundJournal(`${directory}/incoming`);
  await incoming.initialize();
  const raw: QqDispatch = {
    op: 0,
    s: 1,
    t: "C2C_MESSAGE_CREATE",
    d: { id: "incoming-1", content: "私聊例子", author: { user_openid: "owner-fixture" } },
  };
  await incoming.append(qqMessage(raw)!, raw);
  const phone = new QqPhone(() => incoming.records(), actions, timeline, {
    id: "bot-fixture",
    displayName: "Eve",
  }, reading);
  const fence = new OwnershipFence(`${directory}/fence`, "phone", guard);
  await fence.initialize();
  const rpc = startPhoneRpc(phone, bridgeKey, () => ({ available: true }), fence);
  return {
    directory,
    guard,
    proof,
    gate,
    sends: () => sends,
    actions,
    reading,
    fence,
    rpc,
    close: async () => {
      gate.resolve();
      await rpc.close();
      await fence.close();
      await actions.close();
      await timeline.close();
      await reading.close();
      await platform.shutdown();
      await guard.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}
const sendArgs = {
  actionId: "same-logical-send",
  conversationId: "qq:user:owner-fixture",
  text: "离线真实发送链路",
};

Deno.test("QQ fenced RPC：无令牌拒绝读取/发送，换版采用原送达回执，旧实例不能消费入站", async () => {
  const f = await setup();
  const plain = new PhoneRpcClient(f.rpc.url, bridgeKey);
  const old = new PhoneRpcClient(f.rpc.url, bridgeKey, 1000, () => Promise.resolve(f.proof));
  let next: PhoneRpcClient | undefined;
  try {
    assert.deepEqual(await plain.call("health"), { available: true, ownershipFenced: true });
    await assert.rejects(
      () => plain.call("notifications"),
      (e: unknown) => e instanceof PhoneRpcError && e.category === "request",
    );
    await assert.rejects(() => plain.call("send", sendArgs));
    await assert.rejects(() => plain.call("open", { conversationId: "qq:user:owner-fixture" }));
    assert.equal(f.reading.get("qq:user:owner-fixture"), null);
    assert.equal(f.sends(), 0);
    const page = await old.call("history", { conversationId: "qq:user:owner-fixture" });
    assert.ok(page);
    const receipt = await old.call("send", sendArgs);
    assert.deepEqual(receipt, {
      actionId: sendArgs.actionId,
      state: "sent",
      platformMessageId: "actual-offline-platform-message",
    });
    const grant = await f.guard.grant({
      instanceId: "new",
      requestId: "upgrade",
      expectedEpoch: f.proof.epoch,
    });
    if (grant.status !== "granted") throw new Error("not granted");
    const proof = { instanceId: "new", epoch: grant.owner.epoch, credential: newKey };
    next = new PhoneRpcClient(f.rpc.url, bridgeKey, 1000, () => Promise.resolve(proof));
    assert.deepEqual(await next.call("send", sendArgs), receipt);
    assert.equal(f.sends(), 1);
    await assert.rejects(
      () => old.call("notifications"),
      (e: unknown) => e instanceof PhoneRpcError && e.category === "unavailable",
    );
    await assert.rejects(() => old.call("history", { conversationId: "qq:user:owner-fixture" }));
    await assert.rejects(() => old.call("open", { conversationId: "qq:user:owner-fixture" }));
    assert.equal(f.reading.get("qq:user:owner-fixture"), null);
    await next.call("open", { conversationId: "qq:user:owner-fixture" });
    assert.equal(f.reading.get("qq:user:owner-fixture")!.notificationThroughSeq, 2);
    await assert.rejects(() => next!.call("send", { ...sendArgs, text: "changed" }));
    assert.equal(f.sends(), 1);
    assert.deepEqual(await next.call("action", { actionId: sendArgs.actionId }), receipt);
  } finally {
    plain.close();
    old.close();
    next?.close();
    await f.close();
  }
});

Deno.test("QQ 调用方 HTTP 超时后实际发送继续持许可，查回执而不重发", async () => {
  const f = await setup(true);
  const old = new PhoneRpcClient(f.rpc.url, bridgeKey, 30, () => Promise.resolve(f.proof));
  let next: PhoneRpcClient | undefined;
  try {
    await assert.rejects(
      () => old.call("send", sendArgs),
      (e: unknown) => e instanceof PhoneRpcError && e.category === "unknown",
    );
    await waitFor(() => f.sends() === 1);
    assert.equal(f.guard.status().activePermits, 1);
    const swap = { instanceId: "new", requestId: "switch", expectedEpoch: f.proof.epoch };
    assert.deepEqual(await f.guard.grant(swap), { status: "draining", activePermits: 1 });
    f.gate.resolve();
    await waitFor(() => f.guard.status().activePermits === 0);
    const grant = await f.guard.grant(swap);
    if (grant.status !== "granted") throw new Error("not granted");
    next = new PhoneRpcClient(
      f.rpc.url,
      bridgeKey,
      1000,
      () => Promise.resolve({ instanceId: "new", epoch: grant.owner.epoch, credential: newKey }),
    );
    const result = await next.call("action", { actionId: sendArgs.actionId });
    assert.deepEqual(result, {
      actionId: sendArgs.actionId,
      state: "sent",
      platformMessageId: "actual-offline-platform-message",
    });
    assert.equal(f.sends(), 1);
  } finally {
    old.close();
    next?.close();
    await f.close();
  }
});

Deno.test("QQ 错误凭据/虚构未来号码不阻塞真实主人实例，也不能绕过私信范围", async () => {
  const f = await setup();
  const forged = new PhoneRpcClient(
    f.rpc.url,
    bridgeKey,
    1000,
    () =>
      Promise.resolve({
        instanceId: "unknown",
        epoch: Number.MAX_SAFE_INTEGER,
        credential: "z".repeat(48),
      }),
  );
  const good = new PhoneRpcClient(f.rpc.url, bridgeKey, 1000, () => Promise.resolve(f.proof));
  try {
    await assert.rejects(() => forged.call("notifications"));
    assert.equal(f.fence.available, true);
    assert.deepEqual(f.fence.pending(), []);
    assert.ok(await good.call("notifications"));
    const result = await good.call("send", {
      ...sendArgs,
      conversationId: "qq:group:fixture-group",
    });
    assert.ok(
      result && typeof result === "object" && !Array.isArray(result) && result.state === "failed",
    );
    assert.equal(f.sends(), 0);
    assert.equal(f.guard.status().activePermits, 0);
  } finally {
    forged.close();
    good.close();
    await f.close();
  }
});

Deno.test("QQ 正式服务组合传递 fence；真实 worker 只能携实例凭据，旧身份读不到新消息", async () => {
  const { composePhonePlugin } = await import("../composition/phone.ts");
  const { Kernel } = await import("../kernel/kernel.ts");
  const { startQqPhoneAdapter } = await import("../adapters/qq/service.ts");
  const { readQqOwnershipFence } = await import("../composition/ownership.ts");
  const { startOwnershipRpc } = await import("../guardian/rpc.ts");
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-owned-service-" });
  const authority = new OwnershipAuthority(`${directory}/guardian`, 30_000);
  await authority.initialize();
  await authority.register({ instanceId: "old", sourceCommit: "7".repeat(40), credential: oldKey });
  await authority.register({ instanceId: "new", sourceCommit: "8".repeat(40), credential: newKey });
  const grant = await authority.grant({
    instanceId: "old",
    requestId: "first",
    expectedEpoch: authority.status().epoch,
  });
  if (grant.status !== "granted") throw new Error("no owner");
  const guardian = startOwnershipRpc(authority, "a".repeat(48), "x".repeat(48));
  const sockets: WebSocket[] = [];
  let sends = 0;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (r): Promise<Response> => {
      const path = new URL(r.url).pathname;
      if (path === "/app/getAppAccessToken") {
        return Response.json({ access_token: "fixture", expires_in: 7200 });
      }
      if (path === "/users/@me") return Response.json({ id: "actual-bot", username: "Eve" });
      if (path === "/gateway") {
        return Response.json({
          url: `ws://127.0.0.1:${platform.addr.port}/websocket`,
          session_start_limit: { remaining: 100 },
        });
      }
      if (path.startsWith("/v2/users/")) {
        await r.json();
        sends++;
        return Response.json({ id: "actual-service-send" });
      }
      const { socket, response } = Deno.upgradeWebSocket(r);
      sockets.push(socket);
      // Exercise business flow independently of subsecond scheduling; watchdog tests cover expiry.
      socket.onopen = () =>
        socket.send(JSON.stringify({ op: 10, d: { heartbeat_interval: 30_000 } }));
      socket.onmessage = (event) => {
        const data = JSON.parse(event.data);
        if (data.op === 1) socket.send(JSON.stringify({ op: 11 }));
        if (data.op === 2) {
          socket.send(
            JSON.stringify({ op: 0, s: 0, t: "READY", d: { session_id: "real-offline-owned" } }),
          );
          socket.send(
            JSON.stringify({
              op: 0,
              s: 1,
              t: "C2C_MESSAGE_CREATE",
              d: {
                id: "incoming-1",
                content: "主人实际私信",
                author: { user_openid: "owner-fixture" },
              },
            }),
          );
        }
      };
      return response;
    },
  );
  let adapter: Awaited<ReturnType<typeof startQqPhoneAdapter>> | undefined,
    kernel: InstanceType<typeof Kernel> | undefined;
  try {
    await Deno.writeTextFile(
      `${directory}/guard-connection.json`,
      JSON.stringify({ url: guardian.url }),
    );
    await Deno.writeTextFile(`${directory}/guard-exit-key`, "x".repeat(48));
    await Deno.writeTextFile(`${directory}/bridge-key`, bridgeKey);
    const proofFile = `${directory}/instance-proof.json`;
    await Deno.writeTextFile(
      proofFile,
      JSON.stringify({ instanceId: "old", epoch: grant.owner.epoch, credential: oldKey }),
    );
    const config = `${directory}/config.json`;
    await Deno.writeTextFile(
      config,
      JSON.stringify({
        qqOwnership: {
          guardianConnectionFile: `${directory}/guard-connection.json`,
          exitKeyFile: `${directory}/guard-exit-key`,
        },
      }),
    );
    const fence = await readQqOwnershipFence(config, `${directory}/qq`);
    assert.ok(fence);
    let ready = false;
    adapter = await startQqPhoneAdapter(
      new QqClient({ appId: "fixture", appSecret: "fixture" }, {
        baseUrl: `http://127.0.0.1:${platform.addr.port}`,
        sendScope: "private",
      }),
      `${directory}/qq`,
      bridgeKey,
      (s) => {
        if (s.status === "ready") ready = true;
      },
      fence,
    );
    await waitFor(() => ready);
    await Deno.writeTextFile(
      config,
      JSON.stringify({
        phone: {
          adapterUrl: adapter.url,
          adapterKeyFile: `${directory}/bridge-key`,
          ownershipProofFile: proofFile,
          ownerPlatformUserId: "owner-fixture",
        },
      }),
    );
    await composePhonePlugin(`${directory}/phone`, config);
    kernel = new Kernel({ dataDir: `${directory}/kernel` });
    await kernel.register(`${directory}/phone`);
    await kernel.start();
    const page = await kernel.call<{ messages: { sender: { relationship: string } }[] }>(
      "phone",
      "history",
      { conversationId: "qq:user:owner-fixture" },
    );
    assert.equal(page.messages[0].sender.relationship, "owner");
    assert.deepEqual(await kernel.call("phone", "send", sendArgs), {
      actionId: sendArgs.actionId,
      state: "sent",
      platformMessageId: "actual-service-send",
    });
    assert.equal(sends, 1);
    const next = await authority.grant({
      instanceId: "new",
      requestId: "handover",
      expectedEpoch: grant.owner.epoch,
    });
    assert.equal(next.status, "granted");
    await assert.rejects(() => kernel!.call("phone", "notifications"));
    assert.equal(sends, 1);
    // Worker 的清单仅授予实例证明，未授予守护管理/出口凭据。
    const manifest = JSON.parse(await Deno.readTextFile(`${directory}/phone/manifest.json`));
    assert.ok(manifest.permissions.read.includes(proofFile));
    assert.equal(manifest.permissions.read.includes(`${directory}/guard-exit-key`), false);
    const keyless = new PhoneRpcClient(adapter.url, bridgeKey);
    try {
      assert.deepEqual(await keyless.call("health"), {
        available: true,
        gateway: { status: "ready" },
        sdk: QQ_THIRD_PARTY_SDK,
        replyScope: "private",
        ownershipFenced: true,
      });
    } finally {
      keyless.close();
    }
  } finally {
    await kernel?.stop();
    await adapter?.close();
    for (const socket of sockets) if (socket.readyState === WebSocket.OPEN) socket.close();
    await platform.shutdown();
    await guardian.close();
    await authority.close();
    await Deno.remove(directory, { recursive: true });
  }
});

async function bounded<T>(operation: Promise<T>, timeoutMs = 5_000): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => reject(new Error("QQ 集成操作没有在期限内排空")), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function until(check: () => Promise<boolean>): Promise<void> {
  for (let n = 0; n < 1_000; n++) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error("QQ 尝试事实没有在5秒内出现");
}

/** 真实 HTTP 明确限流失败；仅在平台确认未发送时安全续接队首。 */
async function retryFixture(options: {
  failures?: number;
  retryDelaysMs?: readonly number[];
  formalService?: boolean;
} = {}) {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-retry-ownership-" });
  const guard = new OwnershipAuthority(`${directory}/guardian`, 30_000);
  await guard.initialize();
  await guard.register({ instanceId: "old", sourceCommit: "7".repeat(40), credential: oldKey });
  await guard.register({ instanceId: "new", sourceCommit: "8".repeat(40), credential: newKey });
  const granted = await guard.grant({
    instanceId: "old",
    requestId: "initial",
    expectedEpoch: guard.status().epoch,
  });
  if (granted.status !== "granted") throw new Error("no retry owner");
  const proof: OwnershipProof = {
    instanceId: "old",
    epoch: granted.owner.epoch,
    credential: oldKey,
  };
  const requests: (Record<string, unknown> & { markdown?: { content: string } })[] = [];
  const sockets: WebSocket[] = [];
  let headRequests = 0;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (request): Promise<Response> => {
      const path = new URL(request.url).pathname;
      if (path === "/app/getAppAccessToken") {
        return Response.json({ access_token: "fixture", expires_in: 7200 });
      }
      if (path === "/users/@me") return Response.json({ id: "retry-bot", username: "Eve" });
      if (path === "/gateway") {
        return Response.json({
          url: `ws://127.0.0.1:${platform.addr.port}/websocket`,
          session_start_limit: { remaining: 100 },
        });
      }
      if (path === "/websocket") {
        const { socket, response } = Deno.upgradeWebSocket(request);
        sockets.push(socket);
        socket.onopen = () =>
          socket.send(JSON.stringify({ op: 10, d: { heartbeat_interval: 30_000 } }));
        socket.onmessage = (event) => {
          const data = JSON.parse(event.data);
          if (data.op === 1) socket.send(JSON.stringify({ op: 11 }));
          if (data.op === 2) {
            socket.send(JSON.stringify({
              op: 0,
              s: 0,
              t: "READY",
              d: { session_id: "retry-fixture" },
            }));
          }
        };
        return response;
      }
      assert.match(path, /^\/v2\/users\/owner-fixture\/messages$/);
      const body = await request.json();
      requests.push(body);
      if (
        (body.markdown?.content ?? body.content) === "head" &&
        ++headRequests <= (options.failures ?? 4)
      ) {
        return Response.json({ err_code: 100001 }, { status: 429 });
      }
      return Response.json({
        id: (body.markdown?.content ?? body.content) === "head" ? "head-sent" : "second-sent",
      });
    },
  );
  const transport = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
    baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    sendScope: "private",
    timeoutMs: 150,
  });
  await transport.authenticate();
  const incoming = new QqInboundJournal(`${directory}/inbound`);
  await incoming.initialize();
  const raw: QqDispatch = {
    op: 0,
    s: 1,
    t: "C2C_MESSAGE_CREATE",
    d: { id: "inbound-1", content: "hello", author: { user_openid: "owner-fixture" } },
  };
  await incoming.append(qqMessage(raw)!, raw);
  const fence = new OwnershipFence(`${directory}/fence`, "phone", guard);
  await fence.initialize();
  let actions: QqSendActions | undefined;
  let timeline: QqTimeline | undefined;
  let reading: QqPhoneReading | undefined;
  let rpc: ReturnType<typeof startPhoneRpc> | undefined;
  let adapter: Awaited<ReturnType<typeof startQqPhoneAdapter>> | undefined;
  if (options.formalService) {
    adapter = await startQqPhoneAdapter(transport, directory, bridgeKey, undefined, fence);
  } else {
    actions = new QqSendActions(transport, `${directory}/outbox`, "private", {
      retryDelaysMs: options.retryDelaysMs ?? [100],
    });
    timeline = new QqTimeline(`${directory}/timeline`);
    reading = new QqPhoneReading(`${directory}/reading`);
    await actions.initialize();
    await timeline.initialize();
    await reading.initialize();
    rpc = startPhoneRpc(
      new QqPhone(() => incoming.records(), actions, timeline, {
        id: "retry-bot",
        displayName: "Eve",
      }, reading),
      bridgeKey,
      () => ({ available: true }),
      fence,
    );
  }
  return {
    directory,
    guard,
    fence,
    proof,
    actions,
    requests,
    url: adapter?.url ?? rpc!.url,
    closeAdapter: () => adapter!.close(),
    async firstAttemptSaved() {
      try {
        const fact = JSON.parse(
          await Deno.readTextFile(
            `${directory}/outbox/attempts/retry-head/1.result.json`,
          ),
        );
        return fact.result.status === "failed" && fact.result.error.category === "rate_limit";
      } catch (error) {
        // The fixture polls while the attempt writer still owns this file. Creation can
        // become visible before its JSON body; wait for a complete fact before closing.
        if (error instanceof Deno.errors.NotFound || error instanceof SyntaxError) return false;
        throw error;
      }
    },
    async close() {
      actions?.requestStop();
      transport.close();
      if (adapter) await adapter.close();
      else {
        await rpc!.close();
        await fence.close();
        await actions!.close();
        await timeline!.close();
        await reading!.close();
      }
      for (const socket of sockets) if (socket.readyState === WebSocket.OPEN) socket.close();
      await platform.shutdown();
      await guard.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

const headSend = {
  actionId: "retry-head",
  conversationId: "qq:user:owner-fixture",
  text: "head",
  replyTo: { seq: 1, sequence: 1 },
};
const secondSend = {
  ...headSend,
  actionId: "retry-second",
  text: "second",
  replyTo: { seq: 1, sequence: 2 },
};

Deno.test("QQ 无限顺序重试：真实 RPC 先超时，后台四次明确限流后队首送达才发送第二条", async () => {
  const f = await retryFixture();
  const short = new PhoneRpcClient(f.url, bridgeKey, 60, () => Promise.resolve(f.proof));
  const patient = new PhoneRpcClient(f.url, bridgeKey, 5_000, () => Promise.resolve(f.proof));
  try {
    await assert.rejects(
      short.call("send", headSend),
      (error) => error instanceof PhoneRpcError && error.category === "unknown",
    );
    assert.equal(f.guard.status().activePermits, 1);
    assert.equal(f.actions!.status(headSend.actionId)?.status, "running");
    const second = await bounded(patient.call("send", secondSend));
    assert.ok(second && typeof second === "object" && !Array.isArray(second));
    assert.equal(second.state, "sent");
    const head = await patient.call("action", { actionId: headSend.actionId });
    assert.ok(head && typeof head === "object" && !Array.isArray(head));
    assert.equal(head.state, "sent");
    assert.equal(head.attemptCount, 5);
    assert.deepEqual(f.requests.map((body) => body.markdown?.content ?? body.content), [
      "head",
      "head",
      "head",
      "head",
      "head",
      "second",
    ]);
    for (const body of f.requests.slice(0, 5)) {
      assert.equal(body.msg_id, undefined);
      assert.equal(body.msg_seq, undefined);
      assert.equal(body.event_id, undefined);
    }
    await waitFor(() => f.guard.status().activePermits === 0);
  } finally {
    short.close();
    patient.close();
    await f.close();
  }
});

for (const mode of ["pause", "draining"] as const) {
  Deno.test(`QQ 无限顺序重试：${mode} 在退避间撤销续接，许可排空且第二条保持阻塞`, async () => {
    const f = await retryFixture({ failures: Infinity, retryDelaysMs: [250] });
    const client = new PhoneRpcClient(f.url, bridgeKey, 5_000, () => Promise.resolve(f.proof));
    const first = client.call("send", headSend);
    // 两条都及时挂上 rejection handler，维护中断不能制造未处理 Promise。
    const firstSettled = Promise.allSettled([first]);
    let secondSettled: Promise<PromiseSettledResult<unknown>[]> | undefined;
    try {
      await until(() => f.firstAttemptSaved());
      const second = client.call("send", secondSend);
      secondSettled = Promise.allSettled([second]);
      await waitFor(() => f.guard.status().activePermits === 2);
      if (mode === "pause") {
        await f.guard.pause({ requestId: "maintenance", expectedEpoch: f.proof.epoch });
      } else {
        assert.deepEqual(
          await f.guard.grant({
            instanceId: "new",
            requestId: "upgrade-drain",
            expectedEpoch: f.proof.epoch,
          }),
          { status: "draining", activePermits: 2 },
        );
      }
      await bounded(Promise.all([firstSettled, secondSettled]));
      await waitFor(() => f.guard.status().activePermits === 0);
      assert.deepEqual(f.requests.map((body) => body.markdown?.content ?? body.content), ["head"]);
      assert.equal(f.actions!.status(headSend.actionId)?.status, "unknown");
      const blocked = f.actions!.status(secondSend.actionId);
      assert.ok(blocked?.status === "blocked");
      assert.equal(blocked.blockedBy, headSend.actionId);
      const saved = JSON.parse(
        await Deno.readTextFile(
          `${f.directory}/outbox/records/retry-head.json`,
        ),
      );
      assert.equal(saved.status, "started");
      assert.equal(saved.result, undefined);
      assert.equal(saved.policy.kind, "until_sent_v1");
    } finally {
      client.close();
      await f.close();
      await firstSettled;
      await secondSettled;
    }
  });
}

Deno.test("QQ 无限顺序重试：正式 service.close 在明确限流退避期间停止并排空", async () => {
  const f = await retryFixture({ failures: Infinity, formalService: true });
  const client = new PhoneRpcClient(f.url, bridgeKey, 5_000, () => Promise.resolve(f.proof));
  const result = Promise.allSettled([client.call("send", headSend)]);
  try {
    await until(() => f.firstAttemptSaved());
    await bounded(f.closeAdapter(), 2_000);
    await bounded(result);
    assert.equal(f.guard.status().activePermits, 0);
    assert.deepEqual(f.requests.map((body) => body.markdown?.content ?? body.content), ["head"]);
    const restored = new QqSendActions(
      new QqClient({ appId: "fixture", appSecret: "fixture" }),
      `${f.directory}/outbox`,
      "private",
    );
    await restored.initialize();
    try {
      assert.equal(restored.status(headSend.actionId)?.status, "unknown");
    } finally {
      await restored.close();
    }
  } finally {
    client.close();
    await f.close();
    await result;
  }
});
