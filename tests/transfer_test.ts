import assert from "node:assert/strict";
import { exportSnapshot, importSnapshot } from "../supervisor/transfer.ts";
import { ScriptStore } from "../plugins/script-runtime/store.ts";

function input(lines: string[]) {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const line of lines) controller.enqueue(new TextEncoder().encode(line));
      controller.close();
    },
  });
}
async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-isolated-transfer-" });
  const configs = [];
  for (const name of ["old", "candidate"]) {
    const root = `${directory}/${name}`;
    await Deno.mkdir(`${root}/mind`, { recursive: true });
    await Deno.mkdir(`${root}/scripts`);
    const config = `${root}/config.json`;
    await Deno.writeTextFile(
      config,
      JSON.stringify({ mindDirectory: `${root}/mind`, scripts: { directory: `${root}/scripts` } }),
    );
    configs.push(config);
  }
  return { directory, oldConfig: configs[0], candidateConfig: configs[1] };
}

Deno.test("隔离交接协议：实际v2脚本原件、嵌套大值、旧迁移index和提醒保留，重复导入不重放", async () => {
  const { directory, oldConfig, candidateConfig } = await fixture();
  const source = `${directory}/old/scripts/ledger`;
  const target = `${directory}/candidate/scripts/ledger`;
  let store = new ScriptStore(source);
  try {
    await Deno.mkdir(source);
    const legacy = JSON.stringify({ schema: 1, jobs: [], completions: [] }) + "\n";
    await Deno.writeTextFile(`${source}/index.json`, legacy);
    await store.initialize();
    await store.change((state) => {
      state.jobs.push({
        jobId: "real",
        fingerprint: "original",
        request: { requestId: "real", code: "return 42" },
        status: "unknown",
        scriptStarted: true,
        startedAt: "2026-11-12T00:00:00Z",
        actions: [{
          seq: 1,
          actionId: "already_performed",
          namespace: "phone",
          method: "image",
          args: {},
          status: "completed",
          startedAt: "2026-11-12T00:00:00Z",
          value: { original: "raw".repeat(40_000), selectedMode: "original" },
        }],
      });
      state.completions.push({
        seq: 1,
        job: {
          jobId: "real",
          status: "unknown",
          startedAt: "2026-11-12T00:00:00Z",
          scriptStarted: true,
          actions: 1,
        },
      });
    });
    const expected = JSON.parse(JSON.stringify(store.state));
    await store.close();
    const mind = {
      schema: 1,
      events: [],
      alarms: [{ id: "old_alarm", text: "继续", status: "pending" }],
      facts: null,
    };
    await Deno.writeTextFile(`${directory}/old/mind/index.json`, JSON.stringify(mind));
    const lines: string[] = [];
    await exportSnapshot(oldConfig, "transfer_actual", (line) => {
      lines.push(line);
      return Promise.resolve();
    });
    const receipt = await importSnapshot(candidateConfig, "transfer_actual", input(lines));
    assert.deepEqual(
      await importSnapshot(candidateConfig, "transfer_actual", input(lines)),
      receipt,
    );
    assert.deepEqual(
      JSON.parse(await Deno.readTextFile(`${directory}/candidate/mind/index.json`)),
      mind,
    );
    assert.equal(await Deno.readTextFile(`${target}/legacy-index.json`), legacy);
    store = new ScriptStore(target);
    await store.initialize();
    assert.deepEqual(JSON.parse(JSON.stringify(store.state)), expected);
    assert.equal(store.state.jobs[0].actions[0].actionId, "already_performed");
  } finally {
    await store.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("隔离交接协议：拒绝活账本、符号链接、路径穿越、编号或摘要不符，截断不接入目标", async () => {
  const { directory, oldConfig, candidateConfig } = await fixture();
  const store = new ScriptStore(`${directory}/old/scripts/ledger`);
  try {
    await store.initialize();
    await assert.rejects(exportSnapshot(oldConfig, "locked", () => Promise.resolve()));
    await store.close();
    const lines: string[] = [];
    await exportSnapshot(oldConfig, "same", (line) => {
      lines.push(line);
      return Promise.resolve();
    });
    await assert.rejects(importSnapshot(candidateConfig, "different", input(lines)));
    const oversized = lines.map((line) =>
      line.includes('"kind":"file"')
        ? JSON.stringify({ ...JSON.parse(line), size: 256 * 1024 * 1024 + 1 }) + "\n"
        : line
    );
    await assert.rejects(importSnapshot(candidateConfig, "same", input(oversized)));
    await assert.rejects(importSnapshot(candidateConfig, "same", input(lines.slice(0, -1))));
    await assert.rejects(Deno.stat(`${directory}/candidate/mind/index.json`), Deno.errors.NotFound);
    const bad = lines.map((line) =>
      line.includes('"kind":"file"')
        ? JSON.stringify({ ...JSON.parse(line), path: "mind/../../private" }) + "\n"
        : line
    );
    await assert.rejects(importSnapshot(candidateConfig, "same", input(bad)));
    const broken = lines.map((line) =>
      line.includes('"kind":"file"')
        ? JSON.stringify({ ...JSON.parse(line), digest: "0".repeat(64) }) + "\n"
        : line
    );
    await assert.rejects(importSnapshot(candidateConfig, "same", input(broken)));
    // 相同编号的完整原件可在上次中断后收敛。
    await importSnapshot(candidateConfig, "same", input(lines));
    await Deno.symlink(`${directory}/old/mind/index.json`, `${directory}/old/mind/unsafe`);
    await assert.rejects(exportSnapshot(oldConfig, "linked", () => Promise.resolve()));
  } finally {
    await store.close();
    await Deno.remove(directory, { recursive: true });
  }
});
