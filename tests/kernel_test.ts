// 内核的端到端测试：真实起 worker、真实换版、真实回滚，不用桩。

import assert from "node:assert/strict";
import { plan } from "../kernel/graph.ts";
import { Kernel, type KernelEvent } from "../kernel/kernel.ts";
import { parseManifest } from "../kernel/manifest.ts";
import { KernelError } from "../kernel/protocol.ts";
import { satisfies } from "../kernel/semver.ts";
import { Store } from "../kernel/store.ts";

const ROOT = new URL("..", import.meta.url).pathname;
const fixture = (name: string) => `${ROOT}tests/fixtures/${name}`;
const pluginDir = (name: string) => `${ROOT}plugins/${name}`;

interface Harness {
  kernel: Kernel;
  dataDir: string;
  events: KernelEvent[];
}

async function boot(dirs: string[], options: { typecheck?: boolean; dataDir?: string } = {}) {
  const dataDir = options.dataDir ?? await Deno.makeTempDir({ prefix: "eve-test-" });
  const events: KernelEvent[] = [];
  const kernel = new Kernel({
    dataDir,
    typecheck: options.typecheck ?? false,
    drainMs: 1000,
    log: (event) => events.push(event),
  });
  for (const dir of dirs) await kernel.register(dir);
  await kernel.start();
  return { kernel, dataDir, events } satisfies Harness;
}

/** 等到内核发出某个事件，用来在换版暂停调用之后再发起调用。 */
async function until(events: KernelEvent[], name: string): Promise<void> {
  const deadline = Date.now() + 5000;
  while (!events.some((event) => event.event === name)) {
    if (Date.now() > deadline) throw new Error(`没有等到事件 ${name}`);
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}

async function rejects(promise: Promise<unknown>, kind: string, pattern?: RegExp) {
  try {
    await promise;
  } catch (error) {
    assert.ok(error instanceof KernelError, `应当是 KernelError，实际是 ${error}`);
    assert.equal(error.kind, kind);
    if (pattern) assert.match(error.message, pattern);
    return;
  }
  assert.fail("应当失败，却成功了");
}

Deno.test("版本范围", () => {
  assert.ok(satisfies("1.2.3", "^1.0.0"));
  assert.ok(!satisfies("2.0.0", "^1.0.0"));
  assert.ok(!satisfies("0.3.0", "^0.2.1"));
  assert.ok(satisfies("0.2.5", "^0.2.1"));
  assert.ok(satisfies("1.2.9", "~1.2.3"));
  assert.ok(!satisfies("1.3.0", "~1.2.3"));
  assert.ok(satisfies("3.0.0", ">=1.2.3"));
  assert.ok(satisfies("9.9.9", "*"));
  assert.ok(!satisfies("1.0.1", "1.0.0"));
});

Deno.test("依赖图：顺序、缺依赖、版本不满足、有环、重复提供", () => {
  const make = (raw: Record<string, unknown>) =>
    parseManifest({ version: "1.0.0", ...raw }, "测试");
  const a = make({ name: "a", provides: { a: "1.0.0" }, requires: { b: "^1.0.0" } });
  const b = make({ name: "b", provides: { b: "1.2.0" }, requires: { c: "^1.0.0" } });
  const c = make({ name: "c", provides: { c: "1.0.0" } });
  assert.deepEqual(plan([a, b, c]).order, ["c", "b", "a"]);
  assert.throws(() => plan([a, b]), /没有提供者/);
  const oldB = make({ name: "b", provides: { b: "2.0.0" }, requires: { c: "^1.0.0" } });
  assert.throws(() => plan([a, oldB, c]), /需要 b@\^1\.0\.0/);
  const loopC = make({ name: "c", provides: { c: "1.0.0" }, requires: { a: "^1.0.0" } });
  assert.throws(() => plan([a, b, loopC]), /依赖有环/);
  const twin = make({ name: "twin", provides: { c: "1.0.0" } });
  assert.throws(() => plan([a, b, c, twin]), /同时提供/);
});

Deno.test("状态文件损坏时报错并保留原样", async () => {
  const dir = await Deno.makeTempDir({ prefix: "eve-store-" });
  const store = new Store(dir);
  await store.init();
  await Deno.writeTextFile(store.statePath("demo"), "{ 这不是 JSON");
  await rejects(store.load("demo"), "corrupt", /已保留原样/);
  assert.equal(await Deno.readTextFile(store.statePath("demo")), "{ 这不是 JSON");
});

Deno.test("生命周期：加载、启用、停止与逆序清理", async () => {
  const { kernel, events } = await boot([fixture("lifecycle")]);
  assert.equal(await kernel.call("lifecycle", "ping"), "pong");
  assert.deepEqual(await kernel.stop(), []);
  const logs = events.filter((event) => event.event === "plugin_log").map((event) => event.detail);
  assert.deepEqual(logs, ["load", "enable", "disable", "cleanup-2", "cleanup-1"]);
});

Deno.test("按依赖顺序启动，逆序停止", async () => {
  const { kernel, events } = await boot([
    pluginDir("turn-loop"),
    pluginDir("tools-echo"),
    pluginDir("model-mock"),
  ]);
  const started = events.filter((e) => e.event === "plugin_started").map((e) => e.plugin);
  assert.equal(started.at(-1), "turn-loop");
  await kernel.stop();
  const stopped = events.filter((e) => e.event === "plugin_stopped").map((e) => e.plugin);
  assert.equal(stopped[0], "turn-loop");
  assert.deepEqual(stopped, [...started].reverse());
});

Deno.test("轮次循环：多轮、工具调用、重启后历史延续", async () => {
  const dirs = [pluginDir("model-mock"), pluginDir("tools-echo"), pluginDir("turn-loop")];
  const first = await boot(dirs);
  const plain = await first.kernel.call<{ reply: string; toolCalls: number }>("turn", "run", {
    input: "你好",
  });
  assert.equal(plain.reply, "第 1 轮，你说：你好");
  assert.equal(plain.toolCalls, 0);
  const tool = await first.kernel.call<{ reply: string; toolCalls: number; modelRequests: number }>(
    "turn",
    "run",
    { input: "回声 海风" },
  );
  assert.equal(tool.reply, "工具说：海风");
  assert.equal(tool.toolCalls, 1);
  assert.equal(tool.modelRequests, 2);
  await first.kernel.stop();

  const second = await boot(dirs, { dataDir: first.dataDir });
  const history = await second.kernel.call<{ role: string }[]>("turn", "history");
  assert.deepEqual(history.map((m) => m.role), [
    "user",
    "assistant",
    "user",
    "assistant",
    "tool",
    "assistant",
  ]);
  const third = await second.kernel.call<{ reply: string }>("turn", "run", { input: "还在吗" });
  assert.equal(third.reply, "第 3 轮，你说：还在吗");
  await second.kernel.stop();
});

Deno.test("换版成功：三段信号、状态延续、新能力可用", async () => {
  const { kernel, dataDir, events } = await boot([fixture("counter-v1")]);
  const candidate = await Deno.makeTempDir({ prefix: "eve-counter-business-confirm-" });
  try {
    // This checks durable business calls; deadline/probe behavior has a separate test.
    const manifest = JSON.parse(await Deno.readTextFile(`${fixture("counter-v2")}/manifest.json`));
    manifest.confirm.deadlineMs = 10_000;
    await Deno.writeTextFile(`${candidate}/manifest.json`, JSON.stringify(manifest));
    const module = await Deno.readTextFile(`${fixture("counter-v2")}/mod.ts`);
    await Deno.writeTextFile(
      `${candidate}/mod.ts`,
      module.replace(
        '"../../../sdk/plugin.ts"',
        JSON.stringify(new URL("../sdk/plugin.ts", import.meta.url).href),
      ),
    );
    assert.equal(await kernel.call("counter", "add", { n: 5 }), 5);
    const swapping = kernel.swap("counter", candidate);
    await until(events, "swap_paused");
    const during = kernel.call("counter", "add", { n: 2 });
    const report = await swapping;
    assert.equal(report.outcome, "confirmed");
    assert.deepEqual(report.signals, ["ready", "healthy", "confirmed:real_calls"]);
    assert.equal(await during, 7);
    assert.equal(await kernel.call("counter", "version"), "1.1.0");
    assert.equal(await kernel.call("counter", "double"), 14);
    const receipts = (await Deno.readTextFile(`${dataDir}/receipts/swaps.jsonl`)).trim().split(
      "\n",
    );
    assert.equal(JSON.parse(receipts[0]).outcome, "confirmed");
  } finally {
    await kernel.stop();
    await Deno.remove(candidate, { recursive: true });
  }
});

Deno.test("没有真实流量时由内核探测确认，不误判", async () => {
  const { kernel } = await boot([fixture("counter-v1")]);
  const report = await kernel.swap("counter", fixture("counter-v2"));
  assert.equal(report.outcome, "confirmed");
  assert.equal(report.signals.at(-1), "confirmed:probe_only");
  await kernel.stop();
});

for (
  const [name, signalsBefore, pattern] of [
    ["counter-v2-throw", [], /enable 阶段失败.*启用时故意失败/],
    ["counter-v2-unhealthy", ["ready"], /自检不通过/],
    ["counter-v2-hang", [], /enable 阶段超时/],
    ["counter-v2-nomigrate", [], /load 阶段失败.*没有提供 migrate/],
  ] as const
) {
  Deno.test(`换版失败回滚：${name}`, async () => {
    const { kernel } = await boot([fixture("counter-v1")]);
    assert.equal(await kernel.call("counter", "add", { n: 3 }), 3);
    const report = await kernel.swap("counter", fixture(name));
    assert.equal(report.outcome, "rolled_back");
    assert.deepEqual(report.signals, signalsBefore);
    assert.match(report.reason ?? "", pattern);
    assert.equal(await kernel.call("counter", "version"), "1.0.0");
    assert.equal(await kernel.call("counter", "add", { n: 1 }), 4);
    await kernel.stop();
  });
}

Deno.test("首批真实调用失败：回滚、恢复快照、保留失败版本写下的状态", async () => {
  const { kernel, events } = await boot([fixture("counter-v1")]);
  assert.equal(await kernel.call("counter", "add", { n: 8 }), 8);
  const swapping = kernel.swap("counter", fixture("counter-v2-badcall"));
  await until(events, "swap_paused");
  const during = kernel.call("counter", "add", { n: 1 });
  await rejects(during, "remote", /新版本的 add 坏了/);
  const report = await swapping;
  assert.equal(report.outcome, "rolled_back");
  assert.deepEqual(report.signals, ["ready", "healthy"]);
  assert.match(report.reason ?? "", /首批真实调用失败/);
  // 状态回到快照时的 8，而不是坏版本写下的 -999。
  assert.equal(await kernel.call("counter", "get"), 8);
  assert.equal(await kernel.call("counter", "version"), "1.0.0");
  // 坏版本写下的东西没有被丢弃。
  const kept = JSON.parse(await Deno.readTextFile(report.failedState!));
  assert.equal(kept.data.total, -999);
  assert.equal(kept.version, "1.1.0");
  const snapshot = JSON.parse(await Deno.readTextFile(report.snapshot!));
  assert.equal(snapshot.record.data.total, 8);
  await kernel.stop();
});

Deno.test("类型检查不通过：换版被拒绝，旧版本不中断", async () => {
  const { kernel } = await boot([fixture("counter-v1")], { typecheck: true });
  assert.equal(await kernel.call("counter", "add", { n: 1 }), 1);
  const report = await kernel.swap("counter", fixture("counter-v2-typeerror"));
  assert.equal(report.outcome, "rejected");
  assert.match(report.reason ?? "", /类型检查不通过/);
  assert.equal(report.snapshot, undefined);
  assert.equal(await kernel.call("counter", "version"), "1.0.0");
  const good = await kernel.swap("counter", fixture("counter-v2"));
  assert.equal(good.outcome, "confirmed");
  await kernel.stop();
});

Deno.test("契约大版本变化且依赖方不满足：换版被拒绝", async () => {
  const { kernel } = await boot([fixture("counter-v1"), fixture("consumer")]);
  assert.equal(await kernel.call("consumer", "bump"), 1);
  const report = await kernel.swap("counter", fixture("counter-v3-major"));
  assert.equal(report.outcome, "rejected");
  assert.match(report.reason ?? "", /consumer 需要 counter@\^1\.0\.0/);
  assert.equal(await kernel.call("consumer", "bump"), 2);
  await kernel.stop();
});

Deno.test("依赖方的调用在换版后由新版本处理", async () => {
  const { kernel, events } = await boot([fixture("counter-v1"), fixture("consumer")]);
  assert.equal(await kernel.call("consumer", "bump"), 1);
  const swapping = kernel.swap("counter", fixture("counter-v2"));
  await until(events, "swap_paused");
  const during = kernel.call("consumer", "bump");
  const report = await swapping;
  assert.equal(report.outcome, "confirmed");
  assert.equal(report.signals.at(-1), "confirmed:real_calls");
  assert.equal(await during, 2);
  await kernel.stop();
});

Deno.test("状态格式升级：有 migrate 的新版本接手旧状态", async () => {
  const { kernel } = await boot([fixture("counter-v1")]);
  assert.equal(await kernel.call("counter", "add", { n: 6 }), 6);
  const report = await kernel.swap("counter", fixture("counter-v2-migrate"));
  assert.equal(report.outcome, "confirmed");
  assert.equal(await kernel.call("counter", "get"), 6);
  assert.equal(await kernel.call("counter", "add", { n: 1 }), 7);
  await kernel.stop();
});

Deno.test("权限：越权读文件和读环境变量被拒，未声明的依赖被拒", async () => {
  const { kernel } = await boot([fixture("snoop"), fixture("counter-v1")]);
  const own = await kernel.call<string>("snoop", "read", {
    path: `${fixture("snoop")}/manifest.json`,
  });
  assert.match(own, /"snoop"/);
  await rejects(
    kernel.call("snoop", "read", { path: "/etc/hostname" }),
    "remote",
    /NotCapable|PermissionDenied/,
  );
  await rejects(kernel.call("snoop", "env"), "remote", /NotCapable|PermissionDenied/);
  await rejects(kernel.call("snoop", "sneak"), "rejected", /没有在清单里声明/);
  await kernel.stop();
});

Deno.test("启动时状态损坏：拒绝启动，文件原样保留", async () => {
  const first = await boot([fixture("counter-v1")]);
  await first.kernel.call("counter", "add", { n: 2 });
  await first.kernel.stop();
  const path = `${first.dataDir}/state/counter.json`;
  await Deno.writeTextFile(path, "坏掉的内容");
  await rejects(boot([fixture("counter-v1")], { dataDir: first.dataDir }), "corrupt");
  assert.equal(await Deno.readTextFile(path), "坏掉的内容");
});

Deno.test("幂等键：同一个键只执行一次", async () => {
  const { kernel } = await boot([fixture("counter-v1")]);
  const a = await kernel.call("counter", "add", { n: 4 }, { idempotencyKey: "k1" });
  const b = await kernel.call("counter", "add", { n: 4 }, { idempotencyKey: "k1" });
  assert.equal(a, 4);
  assert.equal(b, 4);
  assert.equal(await kernel.call("counter", "get"), 4);
  await kernel.stop();
});

Deno.test("没有提供者的契约、没有的方法", async () => {
  const { kernel } = await boot([fixture("counter-v1")]);
  await rejects(kernel.call("nothing", "x"), "not_found");
  await rejects(kernel.call("counter", "missing"), "not_found");
  await kernel.stop();
});
