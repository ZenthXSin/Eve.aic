import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { startPhoneRpc } from "../adapters/qq/rpc.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import { composePhonePlugin } from "../composition/phone.ts";
import { composeScriptPlugin } from "../composition/scripts.ts";
import type { OwnershipProof } from "../contracts/ownership.ts";
import type { PhoneActionReceipt, PhoneApi } from "../contracts/phone.ts";
import type { ScriptSummary } from "../contracts/script.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { Kernel } from "../kernel/kernel.ts";
import type { Json } from "../kernel/protocol.ts";
import { PhoneRpcClient, PhoneRpcError } from "../plugins/phone/client.ts";
import { ScriptStore } from "../plugins/script-runtime/store.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";

const bridgeKey = "b".repeat(48), oldKey = "o".repeat(48), newKey = "n".repeat(48);
type Queue = Awaited<ReturnType<PhoneApi["listSendQueue"]>>;
function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
function cancellation(item: Queue["items"][number]) {
  assert.ok(item.receiptDigest);
  return {
    actionId: item.actionId,
    expectedFingerprint: item.fingerprint,
    expectedReceiptDigest: item.receiptDigest,
  };
}
async function fixture(options: { pauseAfterCancelAdmission?: boolean } = {}) {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-cancel-phone-" });
  const authority = new OwnershipAuthority(`${directory}/authority`, 30_000);
  await authority.initialize();
  await authority.register({ instanceId: "old", sourceCommit: "7".repeat(40), credential: oldKey });
  await authority.register({ instanceId: "new", sourceCommit: "8".repeat(40), credential: newKey });
  const initial = await authority.grant({
    instanceId: "old",
    requestId: "initial",
    expectedEpoch: authority.status().epoch,
  });
  assert.equal(initial.status, "granted");
  if (initial.status !== "granted") throw new Error("ownership not granted");
  const proof: OwnershipProof = {
    instanceId: "old",
    epoch: initial.owner.epoch,
    credential: oldKey,
  };
  const entered = deferred(), release = deferred();
  let sends = 0;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (request) => {
      if (new URL(request.url).pathname === "/app/getAppAccessToken") {
        return Response.json({ access_token: "fixture", expires_in: 7200 });
      }
      const body = await request.json();
      sends++;
      if ((body.markdown?.content ?? body.content) === "hold") {
        entered.resolve();
        await release.promise;
        return Response.json({ id: "actually-sent" });
      }
      return (body.markdown?.content ?? body.content) === "unknown"
        ? Response.json({ code: 304023 }, { status: 202 })
        : Response.json({ code: 40010000, message: "explicit rejection" }, { status: 400 });
    },
  );
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    }),
    `${directory}/actions`,
  );
  await actions.initialize();
  const timeline = new QqTimeline(`${directory}/timeline`);
  await timeline.initialize();
  const reading = new QqPhoneReading(`${directory}/reading`);
  await reading.initialize();
  const phone = new QqPhone(
    () => [],
    actions,
    timeline,
    { id: "fixture", displayName: "Eve" },
    reading,
  );
  let pauseAfterAdmission = options.pauseAfterCancelAdmission === true;
  const fence = new OwnershipFence(`${directory}/fence`, "phone", {
    async begin(request) {
      const permit = await authority.begin(request);
      if (pauseAfterAdmission && request.actionId.startsWith("cancel_")) {
        pauseAfterAdmission = false;
        await authority.pause({
          requestId: "pause-before-cancel-commit",
          expectedEpoch: proof.epoch,
        });
      }
      return permit;
    },
    cancel: (permit, hash) => authority.cancel(permit, hash),
    finish: (id, outcome, hash) => authority.finish(id, outcome, hash),
  });
  await fence.initialize();
  const rpc = startPhoneRpc(phone, bridgeKey, () => ({ available: true }), fence);
  const client = new PhoneRpcClient(rpc.url, bridgeKey, 10_000, () => Promise.resolve(proof));
  return {
    directory,
    authority,
    proof,
    actions,
    phone,
    rpc,
    client,
    entered,
    release,
    sends: () => sends,
    queue: async () => await client.call("listSendQueue") as unknown as Queue,
    seed: async (actionId = "failed-head", text = "fail") => {
      const receipt = await client.call("send", {
        actionId,
        conversationId: `qq:user:${actionId}`,
        text,
      }) as unknown as PhoneActionReceipt;
      return receipt;
    },
    close: async () => {
      release.resolve();
      client.close();
      await rpc.close();
      await fence.close();
      await actions.close();
      await timeline.close();
      await reading.close();
      await platform.shutdown();
      await authority.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("手机取消：当前所有权可结束失败项，旧 proof 和陈旧摘要拒绝，原失败原件保留", async () => {
  const f = await fixture();
  let current: PhoneRpcClient | undefined;
  try {
    assert.equal((await f.seed()).state, "failed");
    const item = (await f.queue()).items[0];
    assert.equal(item.canCancel, true);
    assert.equal(item.receipt.state, "failed");
    const args = cancellation(item);
    const recordPath = `${f.directory}/actions/records/${item.actionId}.json`;
    const original = await Deno.readTextFile(recordPath);
    await assert.rejects(() =>
      f.client.call("cancelSend", {
        ...args,
        expectedReceiptDigest: "0".repeat(64),
      }), (error) => error instanceof PhoneRpcError && error.category === "conflict");
    const changed = await f.authority.grant({
      instanceId: "new",
      requestId: "replace",
      expectedEpoch: f.proof.epoch,
    });
    assert.equal(changed.status, "granted");
    if (changed.status !== "granted") throw new Error("ownership not granted");
    await assert.rejects(
      () => f.client.call("cancelSend", args),
      (error) => error instanceof PhoneRpcError && error.category === "unavailable",
    );
    assert.equal(f.actions.status(item.actionId)?.status, "finished");
    current = new PhoneRpcClient(f.rpc.url, bridgeKey, 10_000, () =>
      Promise.resolve({
        instanceId: "new",
        epoch: changed.owner.epoch,
        credential: newKey,
      }));
    const receipt = await current.call("cancelSend", args) as unknown as PhoneActionReceipt;
    assert.equal(receipt.state, "cancelled");
    assert.equal(receipt.resolution?.kind, "failed_retained");
    assert.equal(receipt.resolution?.actor, "eve");
    assert.equal(receipt.error?.category, "platform");
    assert.deepEqual(await current.call("cancelSend", args), receipt);
    assert.deepEqual(await current.call("action", { actionId: item.actionId }), receipt);
    assert.equal(await Deno.readTextFile(recordPath), original);
    assert.equal(f.sends(), 1);
    assert.deepEqual((await current.call("listSendQueue") as unknown as Queue).items, []);
  } finally {
    current?.close();
    await f.close();
  }
});

Deno.test("手机取消：无 ownership fence 的兼容 RPC 不能取消", async () => {
  const f = await fixture();
  const plainRpc = startPhoneRpc(f.phone, bridgeKey);
  const plain = new PhoneRpcClient(plainRpc.url, bridgeKey);
  try {
    await f.seed();
    const args = cancellation((await f.queue()).items[0]);
    await assert.rejects(
      () => plain.call("cancelSend", args),
      (error) => error instanceof PhoneRpcError && error.category === "unavailable",
    );
    assert.equal(f.actions.status(args.actionId)?.status, "finished");
    assert.equal(f.sends(), 1);
  } finally {
    plain.close();
    await plainRpc.close();
    await f.close();
  }
});

Deno.test("手机取消：已取得许可后暂停，存盘前再次复核所有权并拒绝取消", async () => {
  const f = await fixture({ pauseAfterCancelAdmission: true });
  try {
    await f.seed();
    const args = cancellation((await f.queue()).items[0]);
    const recordPath = `${f.directory}/actions/records/${args.actionId}.json`;
    const original = await Deno.readTextFile(recordPath);
    await assert.rejects(
      () => f.client.call("cancelSend", args),
      (error) => error instanceof PhoneRpcError && error.category === "unavailable",
    );
    assert.equal(f.authority.status().mode, "paused");
    assert.equal(f.authority.status().activePermits, 0);
    assert.equal(f.actions.status(args.actionId)?.status, "finished");
    assert.equal(await Deno.readTextFile(recordPath), original);
    assert.deepEqual(await Array.fromAsync(Deno.readDir(`${f.directory}/actions/resolutions`)), []);
    assert.equal(f.sends(), 1);
  } finally {
    await f.close();
  }
});

Deno.test("手机取消：unknown可取消等待且保留未知，实际发送中的项目不可取消", async () => {
  const f = await fixture();
  let active: Promise<Json> | undefined;
  try {
    assert.equal((await f.seed("uncertain", "unknown")).state, "unknown");
    const unknown = (await f.queue()).items[0];
    assert.equal(unknown.canCancel, true);
    const cancelled = await f.client.call(
      "cancelSend",
      cancellation(unknown),
    ) as unknown as PhoneActionReceipt;
    assert.equal(cancelled.state, "cancelled");
    assert.equal(cancelled.resolution?.kind, "unknown_retained");
    assert.equal(cancelled.error?.category, "pending");
    active = f.seed("active", "hold") as unknown as Promise<Json>;
    await f.entered.promise;
    const item = (await f.queue()).items.find((item) => item.actionId === "active")!;
    assert.equal(item.canCancel, false);
    assert.equal(item.cancelBlockedReason, "running");
    assert.equal(item.receiptDigest, undefined);
    await assert.rejects(() =>
      f.client.call("cancelSend", {
        actionId: item.actionId,
        expectedFingerprint: item.fingerprint,
        expectedReceiptDigest: "0".repeat(64),
      })
    );
    f.release.resolve();
    assert.equal((await active as unknown as PhoneActionReceipt).state, "sent");
    assert.equal(f.actions.status("uncertain")?.status, "cancelled");
    assert.equal(f.sends(), 2);
  } finally {
    f.release.resolve();
    await active;
    await f.close();
  }
});

Deno.test("手机取消：响应丢失分类为 unknown，查询真实回执确认已取消而不重发", async () => {
  const f = await fixture();
  const committed = deferred(), release = deferred();
  const proxy = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const response = await fetch(f.rpc.url, {
      method: "POST",
      headers: request.headers,
      body: await request.text(),
    });
    const bytes = await response.arrayBuffer();
    committed.resolve();
    await release.promise;
    return new Response(bytes, { status: response.status, headers: response.headers });
  });
  const client = new PhoneRpcClient(
    `http://127.0.0.1:${proxy.addr.port}/rpc`,
    bridgeKey,
    500,
    () => Promise.resolve(f.proof),
  );
  try {
    await f.seed();
    const args = cancellation((await f.queue()).items[0]);
    const pending = assert.rejects(
      () => client.call("cancelSend", args),
      (error) => error instanceof PhoneRpcError && error.category === "unknown",
    );
    await committed.promise;
    await pending;
    assert.equal(
      (await f.client.call("action", { actionId: args.actionId }) as unknown as PhoneActionReceipt)
        .state,
      "cancelled",
    );
    assert.equal(f.sends(), 1);
  } finally {
    release.resolve();
    client.close();
    await proxy.shutdown();
    await f.close();
  }
});

Deno.test("脚本手机取消：真实 worker 类型接口和持久账本包含 listSendQueue/cancelSend", async () => {
  const f = await fixture();
  let kernel: Kernel | undefined;
  try {
    await f.seed();
    const original = await Deno.readTextFile(`${f.directory}/actions/records/failed-head.json`);
    await Deno.writeTextFile(`${f.directory}/key`, bridgeKey, { mode: 0o600 });
    await Deno.writeTextFile(`${f.directory}/proof.json`, JSON.stringify(f.proof), { mode: 0o600 });
    const configPath = `${f.directory}/config.json`;
    await Deno.writeTextFile(
      configPath,
      JSON.stringify({
        phone: {
          adapterUrl: f.rpc.url,
          adapterKeyFile: `${f.directory}/key`,
          ownershipProofFile: `${f.directory}/proof.json`,
        },
        scripts: {
          directory: `${f.directory}/scripts`,
          denoBinary: Deno.execPath(),
          cacheDirectory: `${f.directory}/cache`,
          defaultTimeoutMs: 30_000,
        },
      }),
    );
    await composePhonePlugin(`${f.directory}/phone-plugin`, configPath);
    await composeScriptPlugin(`${f.directory}/script-plugin`, configPath);
    const start = async () => {
      kernel = new Kernel({ dataDir: `${f.directory}/kernel`, callTimeoutMs: 40_000 });
      await kernel.register(`${f.directory}/phone-plugin`);
      await kernel.register(`${f.directory}/script-plugin`);
      await kernel.start();
    };
    await start();
    const request = {
      requestId: "cancel-failed-via-worker",
      mode: "sync",
      code: `const page = await tools.phone.listSendQueue({limit: 8});
const item = page.items.find(item => item.actionId === "failed-head");
if (!item || !item.canCancel || !item.receiptDigest) throw new Error("not cancellable");
return await tools.phone.cancelSend({actionId:item.actionId,expectedFingerprint:item.fingerprint,expectedReceiptDigest:item.receiptDigest});`,
    };
    const job = await kernel!.call<ScriptSummary & Json>("scripts", "start", request);
    assert.equal(job.status, "completed", JSON.stringify(job));
    assert.equal(job.scriptStarted, true);
    assert.equal(job.actions, 2);
    const result = await kernel!.call<{ text: string }>("scripts", "result", { jobId: job.jobId });
    assert.equal(JSON.parse(result.text).state, "cancelled");
    assert.equal(
      await Deno.readTextFile(`${f.directory}/actions/records/failed-head.json`),
      original,
    );
    assert.deepEqual(await kernel!.stop(), []);
    kernel = undefined;
    const store = new ScriptStore(`${f.directory}/scripts/ledger`);
    await store.initialize();
    try {
      const saved = store.job(job.jobId);
      assert.deepEqual(saved.actions.map((item) => item.method), ["listSendQueue", "cancelSend"]);
      assert.equal(saved.actions.every((item) => item.status === "completed"), true);
      assert.deepEqual(saved.actions[0].args, { limit: 8 });
      const args = saved.actions[1].args as Record<string, Json>;
      assert.equal(args.actionId, "failed-head");
      assert.match(String(args.expectedReceiptDigest), /^[a-f0-9]{64}$/);
      assert.equal((saved.actions[1].value as Record<string, Json>).state, "cancelled");
    } finally {
      await store.close();
    }
    await start();
    assert.deepEqual(await kernel!.call("scripts", "start", request), job);
    assert.equal(f.sends(), 1);
  } finally {
    await kernel?.stop();
    await f.close();
  }
});
