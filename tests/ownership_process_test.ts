import assert from "node:assert/strict";
import type { OwnershipProof } from "../contracts/ownership.ts";
import { OwnershipExitClient, OwnershipRpcClient } from "../guardian/rpc.ts";
import { fingerprint } from "../kernel/calls.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";

const adminKey = "a".repeat(48),
  exitKey = "x".repeat(48),
  oldKey = "o".repeat(48),
  newKey = "n".repeat(48);
async function waitFor(check: () => boolean | Promise<boolean>) {
  const end = performance.now() + 10_000;
  while (performance.now() < end) {
    if (await check()) return;
    await new Promise((done) => setTimeout(done, 10));
  }
  throw new Error("实际子进程未在10秒内到达指定边界");
}
function process(entry: string, config: string) {
  const child = new Deno.Command(Deno.execPath(), {
    args: ["run", "--allow-read", "--allow-write", "--allow-net=127.0.0.1", entry, config],
    stdout: "piped",
    stderr: "piped",
    stdin: "null",
  }).spawn();
  let stdout = "", stderr = "";
  const output = (async () => {
    for await (const text of child.stdout.pipeThrough(new TextDecoderStream())) stdout += text;
  })();
  const errors = (async () => {
    for await (const text of child.stderr.pipeThrough(new TextDecoderStream())) stderr += text;
  })();
  let terminal = false;
  const status = child.status.then((result) => {
    terminal = true;
    return result;
  });
  return {
    child,
    status,
    ready: async (marker: string) => {
      await waitFor(() => stdout.includes(`"${marker}"`));
    },
    end: async (signal: "SIGTERM" | "SIGKILL" = "SIGKILL") => {
      try {
        if (!terminal) child.kill(signal);
      } catch (e) {
        if (
          !(e instanceof Deno.errors.NotFound) &&
          !(e instanceof TypeError && e.message === "Child process has already terminated")
        ) throw e;
      }
      const result = await status;
      await output;
      await errors;
      return { ...result, stdout, stderr };
    },
    completed: async () => {
      const result = await status;
      await output;
      await errors;
      return { ...result, stdout, stderr };
    },
  };
}
async function setup() {
  const directory = await Deno.makeTempDir({ prefix: "eve-ownership-process-" });
  await Deno.mkdir(`${directory}/business`);
  await Deno.writeTextFile(`${directory}/admin-key`, adminKey);
  await Deno.writeTextFile(`${directory}/exit-key`, exitKey);
  const config = `${directory}/config.json`;
  await Deno.writeTextFile(
    config,
    JSON.stringify({
      ownership: {
        directory: `${directory}/guard`,
        adminKeyFile: `${directory}/admin-key`,
        exitKeyFile: `${directory}/exit-key`,
        leaseMs: 30_000,
      },
    }),
  );
  return { directory, config };
}
async function start(f: Awaited<ReturnType<typeof setup>>) {
  const service = process(new URL("../guardian/main.ts", import.meta.url).pathname, f.config);
  await service.ready("ready");
  const { url } = JSON.parse(await Deno.readTextFile(`${f.directory}/guard/connection.json`));
  const admin = new OwnershipRpcClient(url, adminKey),
    outlet = new OwnershipRpcClient(url, exitKey);
  return { service, admin, outlet, url };
}
async function enroll(f: Awaited<ReturnType<typeof start>>) {
  await f.admin.call("register", {
    instanceId: "old",
    sourceCommit: "7".repeat(40),
    credential: oldKey,
  });
  await f.admin.call("register", {
    instanceId: "new",
    sourceCommit: "8".repeat(40),
    credential: newKey,
  });
  const status = await f.admin.call<{ epoch: number }>("status");
  const initial = await f.admin.call<{ status: "granted"; owner: { epoch: number } }>("grant", {
    instanceId: "old",
    requestId: "initial",
    expectedEpoch: status.epoch,
  });
  return {
    instanceId: "old",
    epoch: initial.owner.epoch,
    credential: oldKey,
  } satisfies OwnershipProof;
}
async function actor(
  f: Awaited<ReturnType<typeof setup>>,
  service: Awaited<ReturnType<typeof start>>,
  proof: OwnershipProof,
  waitAfterReceipt: boolean,
) {
  const config = `${f.directory}/actor.json`;
  await Deno.writeTextFile(
    config,
    JSON.stringify({
      url: service.url,
      exitKey,
      proof,
      fence: `${f.directory}/fence`,
      business: `${f.directory}/business`,
      waitAfterReceipt,
    }),
  );
  const child = process(
    new URL("./fixtures/ownership-exit/worker.ts", import.meta.url).pathname,
    config,
  );
  await child.ready("operation_started");
  return child;
}

Deno.test("SIGKILL 守护后实际在途出口不释放；恢复只结算既有回执再交接", async () => {
  const f = await setup();
  let current: Awaited<ReturnType<typeof start>> | undefined,
    running: ReturnType<typeof process> | undefined;
  try {
    current = await start(f);
    const proof = await enroll(current);
    running = await actor(f, current, proof, false);
    const pending = await current.admin.call<{ activePermits: number }>("status");
    assert.equal(pending.activePermits, 1);
    await current.service.end("SIGKILL");
    current.admin.close();
    current.outlet.close();
    current = await start(f);
    const status = await current.admin.call<{ epoch: number; activePermits: number }>("status");
    assert.ok(status.epoch > proof.epoch);
    assert.equal(status.activePermits, 1);
    const swap = { instanceId: "new", requestId: "recover", expectedEpoch: status.epoch };
    assert.deepEqual(await current.admin.call("grant", swap), {
      status: "draining",
      activePermits: 1,
    });
    await Deno.writeTextFile(`${f.directory}/business/release`, "release");
    await running.ready("settlement_unknown");
    const stopped = await running.completed();
    assert.equal(stopped.success, true, stopped.stderr);
    running = undefined;
    const receipt = await Deno.readTextFile(`${f.directory}/business/actual-receipt.json`);
    const fence = new OwnershipFence(
      `${f.directory}/fence`,
      "phone",
      new OwnershipExitClient(current.outlet),
    );
    await fence.initialize();
    try {
      assert.equal(fence.pending()[0].phase, "settled");
      assert.deepEqual(await fence.reconcile(), { remaining: 0 });
    } finally {
      await fence.close();
    }
    const final = await current.admin.call<{ status: string; owner: { epoch: number } }>(
      "grant",
      swap,
    );
    assert.equal(final.status, "granted");
    assert.ok(final.owner.epoch > status.epoch);
    assert.equal(await Deno.readTextFile(`${f.directory}/business/actual-receipt.json`), receipt);
  } finally {
    await running?.end();
    current?.admin.close();
    current?.outlet.close();
    await current?.service.end("SIGTERM");
    await Deno.remove(f.directory, { recursive: true });
  }
});

Deno.test("SIGKILL 出口后不能凭租约超时猜完成；查实际回执，不重放平台动作", async () => {
  const f = await setup();
  let current: Awaited<ReturnType<typeof start>> | undefined,
    running: ReturnType<typeof process> | undefined;
  try {
    current = await start(f);
    const proof = await enroll(current);
    running = await actor(f, current, proof, true);
    await Deno.writeTextFile(`${f.directory}/business/release`, "release");
    await running.ready("receipt_saved");
    await running.end("SIGKILL");
    running = undefined;
    const before = await Deno.readTextFile(`${f.directory}/business/actual-receipt.json`);
    const swap = { instanceId: "new", requestId: "handover", expectedEpoch: proof.epoch };
    assert.deepEqual(await current.admin.call("grant", swap), {
      status: "draining",
      activePermits: 1,
    });
    const fence = new OwnershipFence(
      `${f.directory}/fence`,
      "phone",
      new OwnershipExitClient(current.outlet),
    );
    await fence.initialize();
    try {
      assert.equal(fence.pending()[0].phase, "admitted");
      assert.deepEqual(await fence.reconcile(), { remaining: 1 });
      assert.equal(fence.available, false);
      await assert.rejects(() =>
        fence.run(proof, "replay", null, () => {
          throw new Error("must not replay");
        })
      );
      assert.deepEqual(
        await fence.reconcile(async () => ({
          outcome: "completed",
          resultHash: await fingerprint(JSON.parse(before)),
        })),
        { remaining: 0 },
      );
    } finally {
      await fence.close();
    }
    assert.equal((await current.admin.call<{ status: string }>("grant", swap)).status, "granted");
    assert.equal(await Deno.readTextFile(`${f.directory}/business/actual-receipt.json`), before);
  } finally {
    await running?.end();
    current?.admin.close();
    current?.outlet.close();
    await current?.service.end("SIGTERM");
    await Deno.remove(f.directory, { recursive: true });
  }
});

Deno.test("守护真实重启换端口，独立出口重新读连接文件并结算原动作", async () => {
  const f = await setup();
  let current: Awaited<ReturnType<typeof start>> | undefined, fence: OwnershipFence | undefined;
  let release!: () => void;
  const gate = new Promise<void>((done) => {
    release = done;
  });
  let entered = false;
  try {
    current = await start(f);
    const proof = await enroll(current);
    const { FileOwnershipExit } = await import("../composition/ownership.ts");
    fence = new OwnershipFence(
      `${f.directory}/dynamic-fence`,
      "phone",
      new FileOwnershipExit(`${f.directory}/guard/connection.json`, `${f.directory}/exit-key`),
    );
    await fence.initialize();
    const action = fence.run(proof, "in-flight", null, async () => {
      entered = true;
      await gate;
      return { actual: true };
    });
    await waitFor(() => entered);
    await current.service.end("SIGKILL");
    current.admin.close();
    current.outlet.close();
    current = await start(f);
    const s = await current.admin.call<{ epoch: number; activePermits: number }>("status");
    assert.equal(s.activePermits, 1);
    const grant = { instanceId: "new", requestId: "switch", expectedEpoch: s.epoch };
    assert.equal((await current.admin.call<{ status: string }>("grant", grant)).status, "draining");
    release();
    assert.deepEqual(await action, { actual: true });
    assert.equal((await current.admin.call<{ status: string }>("grant", grant)).status, "granted");
    assert.deepEqual(fence.pending(), []);
  } finally {
    release();
    await fence?.close();
    current?.admin.close();
    current?.outlet.close();
    await current?.service.end("SIGTERM");
    await Deno.remove(f.directory, { recursive: true });
  }
});

Deno.test("两份守护实际进程不能并行仲裁，拒绝第二份时不改原所有权", async () => {
  const f = await setup();
  let current: Awaited<ReturnType<typeof start>> | undefined,
    duplicate: ReturnType<typeof process> | undefined;
  try {
    current = await start(f);
    const proof = await enroll(current);
    const original = await Deno.readTextFile(`${f.directory}/guard/index.json`);
    duplicate = process(new URL("../guardian/main.ts", import.meta.url).pathname, f.config);
    const result = await duplicate.completed();
    assert.equal(result.success, false);
    assert.ok(result.stderr.includes("已有守护仲裁实例"));
    assert.equal(await Deno.readTextFile(`${f.directory}/guard/index.json`), original);
    assert.equal((await current.admin.call<{ epoch: number }>("status")).epoch, proof.epoch);
  } finally {
    await duplicate?.end();
    current?.admin.close();
    current?.outlet.close();
    await current?.service.end("SIGTERM");
    await Deno.remove(f.directory, { recursive: true });
  }
});
