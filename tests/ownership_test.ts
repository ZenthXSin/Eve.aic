import assert from "node:assert/strict";
import type { OwnershipExit, OwnershipProof } from "../contracts/ownership.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { OwnershipExitClient, OwnershipRpcClient, startOwnershipRpc } from "../guardian/rpc.ts";
import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";

const oldKey = "o".repeat(48),
  newKey = "n".repeat(48),
  adminKey = "a".repeat(48),
  exitKey = "x".repeat(48);
const source = "7".repeat(40);
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
async function rejects(operation: () => Promise<unknown>, kind: string) {
  await assert.rejects(operation, (e: unknown) => e instanceof KernelError && e.kind === kind);
}
async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-ownership-test-" });
  let now = 1000;
  const authority = new OwnershipAuthority(`${directory}/guard`, 100, () => now);
  await authority.initialize();
  await authority.register({ instanceId: "old", sourceCommit: source, credential: oldKey });
  await authority.register({ instanceId: "new", sourceCommit: source, credential: newKey });
  const granted = await authority.grant({
    instanceId: "old",
    requestId: "initial",
    expectedEpoch: authority.status().epoch,
  });
  assert.equal(granted.status, "granted");
  if (granted.status !== "granted") throw new Error("initial grant absent");
  const proof: OwnershipProof = {
    instanceId: "old",
    epoch: granted.owner.epoch,
    credential: oldKey,
  };
  return {
    directory,
    authority,
    proof,
    advance: (ms: number) => {
      now += ms;
    },
    close: async () => {
      await authority.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}
const permitRequest = async (proof: OwnershipProof, actionId = "send-1") => ({
  permitId: await fingerprint(["test", actionId, crypto.randomUUID()]),
  proof,
  actionId,
  resource: "phone",
  requestHash: await fingerprint({ actionId }),
});

Deno.test("交接等待真实许可结束；旧租约过期也不能抹掉在途动作", async () => {
  const f = await fixture();
  try {
    const request = await permitRequest(f.proof);
    const permit = await f.authority.begin(request);
    assert.deepEqual(await f.authority.begin(request), permit);
    f.advance(101);
    const handover = { instanceId: "new", requestId: "handover", expectedEpoch: f.proof.epoch };
    assert.deepEqual(await f.authority.grant(handover), { status: "draining", activePermits: 1 });
    await rejects(() => f.authority.begin(permitRequestSync(f.proof)), "unavailable");
    await f.authority.finish(permit.id, "completed", await fingerprint({ delivered: true }));
    const result = await f.authority.grant(handover);
    assert.equal(result.status, "granted");
    if (result.status !== "granted") throw new Error("grant absent");
    assert.equal(result.owner.epoch, f.proof.epoch + 1);
    await rejects(() => f.authority.renew(f.proof), "unavailable");
    assert.deepEqual(await f.authority.grant(handover), result);
    await rejects(() => f.authority.grant({ ...handover, instanceId: "old" }), "rejected");
  } finally {
    await f.close();
  }
});
function permitRequestSync(proof: OwnershipProof) {
  return {
    permitId: "f".repeat(64),
    proof,
    actionId: "other",
    resource: "phone",
    requestHash: "a".repeat(64),
  };
}

Deno.test("取消原申请封住迟到 begin；已执行结果与动作参数不可覆盖", async () => {
  const f = await fixture();
  try {
    const request = await permitRequest(f.proof);
    const permit = {
      id: request.permitId,
      instanceId: "old",
      epoch: f.proof.epoch,
      resource: "phone",
      actionId: request.actionId,
    };
    await f.authority.cancel(permit, request.requestHash);
    await f.authority.cancel(permit, request.requestHash);
    await rejects(() => f.authority.begin(request), "rejected");
    const next = { ...request, permitId: "b".repeat(64) };
    const active = await f.authority.begin(next);
    await f.authority.finish(active.id, "completed", "c".repeat(64));
    await rejects(() => f.authority.cancel(active, next.requestHash), "rejected");
    await rejects(() => f.authority.finish(active.id, "unknown"), "rejected");
    await rejects(
      () => f.authority.begin({ ...next, permitId: "d".repeat(64), requestHash: "e".repeat(64) }),
      "rejected",
    );
  } finally {
    await f.close();
  }
});

Deno.test("守护重启号码上升、保留未结算许可与暂停，原交接回执只作历史", async () => {
  const f = await fixture();
  let reopened: OwnershipAuthority | undefined;
  try {
    const active = await f.authority.begin(await permitRequest(f.proof));
    const pause = await f.authority.pause({ requestId: "stop", expectedEpoch: f.proof.epoch });
    await f.authority.close();
    reopened = new OwnershipAuthority(`${f.directory}/guard`, 100, () => 2000);
    await reopened.initialize();
    assert.equal(reopened.status().epoch, pause.epoch + 1);
    assert.equal(reopened.status().mode, "paused");
    assert.equal(reopened.status().activePermits, 1);
    await rejects(
      () =>
        reopened!.grant({
          instanceId: "new",
          requestId: "auto",
          expectedEpoch: reopened!.status().epoch,
        }),
      "unavailable",
    );
    const args = {
      instanceId: "old",
      requestId: "owner-resume",
      expectedEpoch: reopened.status().epoch,
      resumePaused: true,
    };
    assert.equal((await reopened.grant(args)).status, "draining");
    await reopened.finish(active.id, "unknown", "d".repeat(64));
    const granted = await reopened.grant(args);
    assert.equal(granted.status, "granted");
    await rejects(() => reopened!.renew(f.proof), "unavailable");
    assert.deepEqual(
      await reopened.pause({ requestId: "stop", expectedEpoch: f.proof.epoch }),
      pause,
    );
    assert.equal(reopened.status().mode, "active");
  } finally {
    await reopened?.close();
    await f.close();
  }
});

Deno.test("出口共用在途调用；调用方超时和交接都不会提前释放真实动作", async () => {
  const f = await fixture(), gate = deferred<Json>(), entered = deferred<void>();
  const fence = new OwnershipFence(`${f.directory}/fence`, "phone", f.authority);
  await fence.initialize();
  let calls = 0;
  try {
    const operation = () => {
      calls++;
      entered.resolve();
      return gate.promise;
    };
    const first = fence.run(f.proof, "one-send", { content: "test" }, operation);
    await entered.promise;
    const second = fence.run(f.proof, "one-send", { content: "test" }, operation);
    await rejects(
      () => fence.run(f.proof, "one-send", { content: "changed" }, operation),
      "rejected",
    );
    const observed = await Promise.race([
      first.then(() => "finished"),
      new Promise<string>((done) => setTimeout(() => done("caller-timed-out"), 10)),
    ]);
    assert.equal(observed, "caller-timed-out");
    const args = { instanceId: "new", requestId: "swap", expectedEpoch: f.proof.epoch };
    assert.equal((await f.authority.grant(args)).status, "draining");
    assert.equal(f.authority.status().activePermits, 1);
    gate.resolve({ platformMessageId: "actual" });
    assert.deepEqual(await first, await second);
    assert.equal(calls, 1);
    assert.equal((await f.authority.grant(args)).status, "granted");
    await rejects(() => fence.run(f.proof, "stale", null, operation), "unavailable");
    assert.deepEqual(fence.pending(), []);
  } finally {
    gate.resolve(null);
    await fence.close();
    await f.close();
  }
});

Deno.test("begin 回执丢失并取消失败：重启取消原编号，迟到请求不会执行", async () => {
  const f = await fixture();
  let original: Parameters<OwnershipExit["begin"]>[0] | undefined;
  const lost: OwnershipExit = {
    begin: async (request) => {
      original = request;
      await f.authority.begin(request);
      throw new KernelError("unknown", "lost");
    },
    cancel: () => Promise.reject(new KernelError("unknown", "cancel not observed")),
    finish: (id, outcome, hash) => f.authority.finish(id, outcome, hash),
  };
  const fence = new OwnershipFence(`${f.directory}/fence`, "phone", lost);
  let reopened: OwnershipFence | undefined;
  await fence.initialize();
  let operations = 0;
  try {
    await rejects(() =>
      fence.run(f.proof, "original", null, () => {
        operations++;
        return Promise.resolve(null);
      }), "unknown");
    assert.equal(operations, 0);
    assert.equal(fence.pending()[0].phase, "requested");
    assert.equal(f.authority.status().activePermits, 1);
    await fence.close();
    reopened = new OwnershipFence(`${f.directory}/fence`, "phone", f.authority);
    await reopened.initialize();
    assert.equal(reopened.available, false);
    assert.deepEqual(await reopened.reconcile(), { remaining: 0 });
    await rejects(() => f.authority.begin(original!), "rejected");
    assert.equal(f.authority.status().activePermits, 0);
    assert.equal(operations, 0);
  } finally {
    await reopened?.close();
    await fence.close();
    await f.close();
  }
});

Deno.test("执行回执已存但 finish 回执未知：重启只结算，不重新执行", async () => {
  const f = await fixture();
  const lost: OwnershipExit = {
    begin: (r) => f.authority.begin(r),
    cancel: (p, h) => f.authority.cancel(p, h),
    finish: async (id, outcome, hash) => {
      await f.authority.finish(id, outcome, hash);
      throw new KernelError("unknown", "finish reply lost");
    },
  };
  const fence = new OwnershipFence(`${f.directory}/fence`, "phone", lost);
  let reopened: OwnershipFence | undefined, operations = 0;
  await fence.initialize();
  try {
    await rejects(() =>
      fence.run(f.proof, "sent", null, () => {
        operations++;
        return Promise.resolve({ delivery: "sent" });
      }), "unknown");
    assert.equal(fence.pending()[0].phase, "settled");
    assert.equal(fence.available, false);
    await fence.close();
    reopened = new OwnershipFence(`${f.directory}/fence`, "phone", f.authority);
    await reopened.initialize();
    assert.deepEqual(await reopened.reconcile(), { remaining: 0 });
    assert.equal(operations, 1);
    assert.equal(reopened.available, true);
    assert.equal(f.authority.status().activePermits, 0);
  } finally {
    await reopened?.close();
    await fence.close();
    await f.close();
  }
});

Deno.test("守护 RPC 实际区分控制、出口与实例凭据，正文不入仲裁存储", async () => {
  const f = await fixture();
  const rpc = startOwnershipRpc(f.authority, adminKey, exitKey);
  const actor = new OwnershipRpcClient(rpc.url, oldKey),
    admin = new OwnershipRpcClient(rpc.url, adminKey),
    outlet = new OwnershipRpcClient(rpc.url, exitKey);
  try {
    await rejects(
      () =>
        actor.call("grant", {
          instanceId: "old",
          requestId: "steal",
          expectedEpoch: f.proof.epoch,
        }),
      "rejected",
    );
    await rejects(() => actor.call("begin", permitRequestSync(f.proof)), "rejected");
    await rejects(
      () =>
        admin.call("register", { instanceId: "bad", sourceCommit: source, credential: adminKey }),
      "rejected",
    );
    assert.equal((await actor.call<{ epoch: number }>("renew", f.proof)).epoch, f.proof.epoch);
    await rejects(
      () => outlet.call("pause", { requestId: "bad", expectedEpoch: f.proof.epoch }),
      "rejected",
    );
    const fence = new OwnershipFence(
      `${f.directory}/rpc-fence`,
      "phone",
      new OwnershipExitClient(outlet),
    );
    await fence.initialize();
    try {
      assert.equal(
        await fence.run(
          f.proof,
          "private-action",
          { content: "private-needle-do-not-store" },
          () => Promise.resolve("private-result-do-not-store"),
        ),
        "private-result-do-not-store",
      );
    } finally {
      await fence.close();
    }
    const stored = await Deno.readTextFile(`${f.directory}/guard/index.json`);
    for (
      const secret of [
        oldKey,
        newKey,
        adminKey,
        exitKey,
        "private-needle-do-not-store",
        "private-result-do-not-store",
      ]
    ) assert.equal(stored.includes(secret), false);
  } finally {
    actor.close();
    admin.close();
    outlet.close();
    await rpc.close();
    await f.close();
  }
});

Deno.test("守护损坏/号码上限和出口回退号码不被静默接受，原件保留", async () => {
  const f = await fixture();
  try {
    const legacy = JSON.stringify(f.authority.store.state);
    await f.authority.close();
    await Deno.remove(`${f.directory}/guard/changes`, { recursive: true });
    await Deno.writeTextFile(`${f.directory}/guard/index.json`, legacy);
    const index = `${f.directory}/guard/index.json`,
      valid = await Deno.readTextFile(index),
      state = JSON.parse(valid);
    state.requests[0].result = {
      status: "granted",
      owner: { instanceId: "ghost", epoch: 1, expiresAt: 100 },
    };
    await Deno.writeTextFile(index, JSON.stringify(state));
    const damaged = await Deno.readTextFile(index);
    await rejects(
      () => new OwnershipAuthority(`${f.directory}/guard`, 100).initialize(),
      "corrupt",
    );
    assert.equal(await Deno.readTextFile(index), damaged);
    const max = JSON.parse(valid);
    max.epoch = Number.MAX_SAFE_INTEGER;
    max.owner.epoch = max.epoch;
    await Deno.writeTextFile(index, JSON.stringify(max));
    const before = await Deno.readTextFile(index);
    await rejects(
      () => new OwnershipAuthority(`${f.directory}/guard`, 100).initialize(),
      "storage",
    );
    assert.equal(await Deno.readTextFile(index), before);
    const directory = `${f.directory}/fence`;
    await Deno.mkdir(directory);
    await Deno.writeTextFile(
      `${directory}/index.json`,
      JSON.stringify({ schema: 1, maxEpoch: 99, pending: [] }),
    );
    const fence = new OwnershipFence(directory, "phone", f.authority);
    await fence.initialize();
    try {
      await rejects(() =>
        fence.run(f.proof, "stale", null, () => {
          throw new Error("must not execute");
        }), "unavailable");
    } finally {
      await fence.close();
    }
  } finally {
    await f.close();
  }
});

Deno.test("长重试链复核原许可：不新增许可，暂停或交接后禁止下一次真实尝试", async () => {
  for (const mode of ["pause", "drain"] as const) {
    const f = await fixture();
    const fence = new OwnershipFence(`${f.directory}/retry-fence`, "phone", f.authority);
    await fence.initialize();
    let attempts = 0;
    try {
      const operation = fence.run(f.proof, "retry-chain", { text: "one" }, async (checkpoint) => {
        await checkpoint();
        attempts++;
        await checkpoint();
        assert.equal(f.authority.status().activePermits, 1);
        if (mode === "pause") {
          await f.authority.pause({ requestId: "pause-chain", expectedEpoch: f.proof.epoch });
        } else {
          const result = await f.authority.grant({
            instanceId: "new",
            requestId: "drain-chain",
            expectedEpoch: f.proof.epoch,
          });
          assert.equal(result.status, "draining");
        }
        await checkpoint();
        attempts++;
        return null;
      });
      await rejects(() => operation, "unavailable");
      assert.equal(attempts, 1);
      assert.equal(f.authority.status().activePermits, 0);
      assert.deepEqual(fence.pending(), []);
    } finally {
      await fence.close();
      await f.close();
    }
  }
});

Deno.test("守护增量：检查点固定，恢复完整许可与取消历史，变更检查点或丢失增量拒绝启动", async () => {
  const f = await fixture();
  let reopened: OwnershipAuthority | undefined;
  try {
    const path = `${f.directory}/guard/index.json`;
    const checkpoint = await Deno.readTextFile(path);
    const request = await permitRequest(f.proof, "ledger-retained");
    const permit = await f.authority.begin(request);
    await f.authority.finish(permit.id, "completed", "d".repeat(64));
    const cancelled = await permitRequest(f.proof, "cancel-retained");
    await f.authority.cancel({
      id: cancelled.permitId,
      instanceId: f.proof.instanceId,
      epoch: f.proof.epoch,
      resource: cancelled.resource,
      actionId: cancelled.actionId,
    }, cancelled.requestHash);
    const before = structuredClone(f.authority.store.state);
    assert.equal(await Deno.readTextFile(path), checkpoint);
    await f.authority.close();
    reopened = new OwnershipAuthority(`${f.directory}/guard`, 100, () => 2000);
    await reopened.initialize();
    assert.deepEqual(reopened.store.state.permits, before.permits);
    assert.deepEqual(reopened.store.state.cancellations, before.cancellations);
    assert.deepEqual(reopened.store.state.requests, before.requests);
    await rejects(() => reopened!.begin(request), "rejected");
    await rejects(() => reopened!.begin(cancelled), "rejected");
    await rejects(() => reopened!.finish(permit.id, "unknown"), "rejected");
    await reopened.close();
    const altered = JSON.parse(checkpoint);
    altered.epoch++;
    await Deno.writeTextFile(path, JSON.stringify(altered));
    await rejects(
      () => new OwnershipAuthority(`${f.directory}/guard`, 100).initialize(),
      "corrupt",
    );
    await Deno.writeTextFile(path, checkpoint);
    await Deno.remove(`${f.directory}/guard/changes/events/0000000000000002.json`);
    await rejects(
      () => new OwnershipAuthority(`${f.directory}/guard`, 100).initialize(),
      "corrupt",
    );
  } finally {
    await reopened?.close();
    await f.close();
  }
});
