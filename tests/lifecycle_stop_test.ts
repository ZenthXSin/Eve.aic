import assert from "node:assert/strict";
import { Kernel } from "../kernel/kernel.ts";
import { KernelError } from "../kernel/protocol.ts";
import { StopDiagnostics, stopErrorKind } from "../runtime/stop-diagnostics.ts";
import { eveStopDeadlineMs, instanceStopControlDeadlineMs } from "../sdk/lifecycle-timeouts.ts";

Deno.test("停止诊断：真实worker超时与真实storage保持分类，兼容字符串汇总", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-stop-diagnostic-" });
  try {
    for (
      const [name, body, expected] of [
        ["timeout", "await new Promise(() => {})", "timeout"],
        ["storage", 'throw new KernelError("storage", "fixture保存失败")', "storage"],
      ]
    ) {
      const plugin = `${directory}/${name}`;
      await Deno.mkdir(plugin);
      await Deno.writeTextFile(
        `${plugin}/manifest.json`,
        JSON.stringify({
          name,
          version: "1.0.0",
          deadlines: { disableMs: 100 },
        }),
      );
      await Deno.writeTextFile(
        `${plugin}/mod.ts`,
        `import {KernelError} from ${
          JSON.stringify(new URL("../kernel/protocol.ts", import.meta.url).href)
        };
export default {async disable() { ${body} }};`,
      );
      const kernel = new Kernel({ dataDir: `${directory}/data-${name}`, typecheck: false });
      try {
        await kernel.register(plugin);
        await kernel.start();
        const problems = await kernel.stop();
        assert.equal(problems.length, 1);
        assert.ok(problems[0].includes(name));
        assert.equal(kernel.stopFailures.length, 1);
        const failure = kernel.stopFailures[0];
        assert.equal(failure.plugin, name);
        assert.equal(failure.stage, "disable");
        assert.equal(failure.kind, expected);
        assert.equal(failure.timedOut, expected === "timeout");
        assert.equal(stopErrorKind(kernel.stopFailures), expected);
        assert.ok(kernel.status().every((plugin) => plugin.status === "stopped"));
        const store = new StopDiagnostics(`${directory}/reports-${name}`);
        await store.initialize();
        await store.record("fixture", "a".repeat(40), kernel.stopFailures);
        const reloaded = new StopDiagnostics(store.directory);
        await reloaded.initialize();
        assert.deepEqual(reloaded.last, store.last);
        assert.equal(reloaded.last!.persisted, true);
      } finally {
        await kernel.stop();
      }
    }
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("停止诊断：诊断写入失败仍报storage并保留原超时，原持久报告不覆盖", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-stop-persist-" });
  try {
    const store = new StopDiagnostics(`${directory}/reports`);
    await store.initialize();
    const failures = [{
      plugin: "eve",
      stage: "disable",
      kind: "timeout" as const,
      timedOut: true,
      message: "fixture",
    }];
    await store.record("fixture", "a".repeat(40), failures);
    await Deno.rename(store.directory, `${directory}/preserved`);
    await Deno.writeTextFile(store.directory, "fixture阻止目录写入");
    await assert.rejects(
      store.record("fixture", "a".repeat(40), failures),
      (error: unknown) => error instanceof KernelError && error.kind === "storage",
    );
    assert.equal(store.last!.persisted, false);
    assert.equal(store.last!.failures[0].kind, "timeout");
    const original = new StopDiagnostics(`${directory}/preserved`);
    await original.initialize();
    assert.equal(original.last!.persisted, true);
    assert.equal(original.last!.failures[0].kind, "timeout");
    assert.equal(
      stopErrorKind([...failures, {
        plugin: "mind",
        stage: "settle",
        kind: "storage",
        timedOut: false,
        message: "fixture",
      }]),
      "storage",
    );
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("停止期限：长模型期限保留完整收尾窗口，控制端不提前超时，拒绝计时器溢出", () => {
  assert.ok(eveStopDeadlineMs(120_000) > 120_000);
  assert.ok(eveStopDeadlineMs(240_000) > 240_000);
  assert.ok(instanceStopControlDeadlineMs(240_000) > eveStopDeadlineMs(240_000));
  assert.throws(() => eveStopDeadlineMs(2_147_483_647), KernelError);
});
