import assert from "node:assert/strict";
import { Kernel, type KernelEvent } from "../kernel/kernel.ts";
import { KernelError } from "../kernel/protocol.ts";

const ROOT = new URL("..", import.meta.url).pathname;
const fixture = (name: string) => `${ROOT}tests/fixtures/${name}`;

async function boot(dirs: string[], dataDir?: string) {
  dataDir ??= await Deno.makeTempDir({ prefix: "eve-reliability-" });
  const events: KernelEvent[] = [];
  const kernel = new Kernel({ dataDir, typecheck: false, log: (e) => events.push(e) });
  for (const dir of dirs) await kernel.register(dir);
  await kernel.start();
  return { kernel, dataDir, events };
}

async function errorKind(operation: Promise<unknown>, kind: string) {
  await assert.rejects(operation, (e) => e instanceof KernelError && e.kind === kind);
}

Deno.test("超时不重复动作；迟到结果、重启查询与参数绑定保留", async () => {
  const dir = await Deno.makeTempDir({ prefix: "eve-late-plugin-" });
  const data = await Deno.makeTempDir({ prefix: "eve-late-data-" });
  await Deno.writeTextFile(
    `${dir}/manifest.json`,
    JSON.stringify({
      name: "late",
      version: "1.0.0",
      provides: { late: "1.0.0" },
    }),
  );
  await Deno.writeTextFile(
    `${dir}/mod.ts`,
    `import {definePlugin} from "${ROOT}sdk/plugin.ts";
export default definePlugin({load(ctx){ctx.provide("late",{
act:async()=>{const n=(ctx.getState()?.n??0)+1;await ctx.setState({n});
await new Promise(r=>setTimeout(r,100));return n;},get:()=>ctx.getState()?.n??0,
timeout:()=>"timeout"});}});`,
  );
  let k: Kernel | undefined;
  try {
    k = (await boot([dir], data)).kernel;
    await errorKind(
      k.call("late", "act", null, { timeoutMs: 15, idempotencyKey: "one" }),
      "timeout",
    );
    await errorKind(
      k.call("late", "act", null, { timeoutMs: 15, idempotencyKey: "one" }),
      "timeout",
    );
    assert.equal(await k.call("late", "get"), 1);
    assert.equal(await k.call("late", "act", null, { idempotencyKey: "one" }), 1);
    assert.equal((await k.callStatus("late", "act", "one"))?.status, "completed");
    await errorKind(
      k.call("late", "act", { changed: true }, { idempotencyKey: "one" }),
      "rejected",
    );
    assert.equal(await k.call("late", "timeout"), "timeout");
    await k.stop();
    k = (await boot([dir], data)).kernel;
    assert.equal(await k.call("late", "act", null, { idempotencyKey: "one" }), 1);
    assert.equal(await k.call("late", "get"), 1);
  } finally {
    await k?.stop();
    await Deno.remove(dir, { recursive: true });
    await Deno.remove(data, { recursive: true });
  }
});

Deno.test("执行中停止后重启，原动作未知且不会重放", async () => {
  const dir = await Deno.makeTempDir({ prefix: "eve-pending-plugin-" });
  const data = await Deno.makeTempDir({ prefix: "eve-pending-data-" });
  await Deno.writeTextFile(
    `${dir}/manifest.json`,
    JSON.stringify({
      name: "pending",
      version: "1.0.0",
      provides: { pending: "1.0.0" },
    }),
  );
  await Deno.writeTextFile(
    `${dir}/mod.ts`,
    `import {definePlugin} from "${ROOT}sdk/plugin.ts";
export default definePlugin({load(ctx){ctx.provide("pending",{
act:async()=>{await ctx.setState({n:(ctx.getState()?.n??0)+1});return await new Promise(()=>{});},
get:()=>ctx.getState()?.n??0});}});`,
  );
  let k: Kernel | undefined;
  try {
    k = (await boot([dir], data)).kernel;
    await errorKind(
      k.call("pending", "act", null, { timeoutMs: 30, idempotencyKey: "one" }),
      "timeout",
    );
    assert.equal(await k.call("pending", "get"), 1);
    await k.stop();
    k = (await boot([dir], data)).kernel;
    await errorKind(k.call("pending", "act", null, { idempotencyKey: "one" }), "unknown");
    assert.equal((await k.callStatus("pending", "act", "one"))?.status, "unknown");
    assert.equal(await k.call("pending", "get"), 1);
  } finally {
    await k?.stop();
    await Deno.remove(dir, { recursive: true });
    await Deno.remove(data, { recursive: true });
  }
});

Deno.test("保存失败阻止成功回执，停止明确报错", async () => {
  const { kernel, dataDir } = await boot([fixture("counter-v1")]);
  try {
    await Deno.mkdir(`${dataDir}/state/counter.json`);
    await errorKind(kernel.call("counter", "add", { n: 5 }), "storage");
    assert.ok((await kernel.stop()).some((e) => e.includes("状态保存失败")));
  } finally {
    await kernel.stop();
    await Deno.remove(dataDir, { recursive: true });
  }
});

Deno.test("并发输入沿同一意识流排队，两轮完整保存", async () => {
  const { kernel, dataDir } = await boot(
    ["model-mock", "tools-echo", "turn-loop"].map((p) => `${ROOT}plugins/${p}`),
  );
  try {
    const replies = await Promise.all([
      kernel.call("turn", "run", { input: "first" }),
      kernel.call("turn", "run", { input: "second" }),
    ]);
    assert.match((replies[1] as { reply: string }).reply, /第 2 轮/);
    const history = await kernel.call<{ role: string; content: string }[]>("turn", "history");
    assert.deepEqual(history.filter((m) => m.role === "user").map((m) => m.content), [
      "first",
      "second",
    ]);
  } finally {
    await kernel.stop();
    await Deno.remove(dataDir, { recursive: true });
  }
});

Deno.test("回滚保存失败释放门控并标记失败，原状态不被覆盖", async () => {
  const { kernel, dataDir, events } = await boot([fixture("counter-v1")]);
  try {
    await kernel.call("counter", "add", { n: 8 });
    await Deno.writeTextFile(`${dataDir}/failed/counter`, "故障注入");
    const swapping = kernel.swap("counter", fixture("counter-v2-badcall"));
    while (!events.some((e) => e.event === "swap_paused")) {
      await new Promise((r) => setTimeout(r, 1));
    }
    await errorKind(kernel.call("counter", "add", { n: 1 }), "remote");
    const report = await swapping;
    assert.equal(report.outcome, "rollback_failed");
    assert.equal(kernel.status()[0].status, "failed");
    await assert.rejects(
      kernel.call("counter", "get", null, { timeoutMs: 100 }),
      (e) =>
        e instanceof KernelError && e.kind === "unavailable" && !e.message.includes("正在换版"),
    );
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${dataDir}/state/counter.json`)).data.total,
      -999,
    );
    assert.equal(JSON.parse(await Deno.readTextFile(report.snapshot!)).record.data.total, 8);
  } finally {
    await kernel.stop();
    await Deno.remove(dataDir, { recursive: true });
  }
});
