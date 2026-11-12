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

Deno.test("回退交接：保留旧账本，接回新增完成与未知动作；截断和重复调用不丢原件", async () => {
  const { directory, oldConfig, candidateConfig } = await fixture();
  let store = new ScriptStore(`${directory}/old/scripts/ledger`);
  try {
    await store.initialize();
    await store.change((state) => {
      state.completions.push({
        seq: 1,
        job: {
          jobId: "old",
          status: "completed",
          scriptStarted: true,
          actions: 0,
          startedAt: "2026-11-12T00:00:00Z",
        },
      });
    });
    await store.close();
    const mind = { schema: 1, events: [], alarms: [], facts: null };
    await Deno.writeTextFile(`${directory}/old/mind/index.json`, JSON.stringify(mind));
    const forward: string[] = [];
    await exportSnapshot(oldConfig, "forward", (line) => {
      forward.push(line);
      return Promise.resolve();
    });
    await importSnapshot(candidateConfig, "forward", input(forward));
    store = new ScriptStore(`${directory}/candidate/scripts/ledger`);
    await store.initialize();
    await store.change((state) => {
      state.jobs.push({
        jobId: "unknown",
        fingerprint: "unknown",
        request: { requestId: "unknown", code: "return 1" },
        status: "unknown",
        scriptStarted: true,
        startedAt: "2026-11-12T00:00:00Z",
        actions: [{
          seq: 1,
          actionId: "already-sent",
          namespace: "phone",
          method: "send",
          args: {},
          status: "unknown",
          startedAt: "2026-11-12T00:00:00Z",
        }],
      });
      state.completions.push({
        seq: 2,
        job: {
          jobId: "unknown",
          status: "unknown",
          scriptStarted: true,
          actions: 1,
          startedAt: "2026-11-12T00:00:00Z",
        },
      });
    });
    const expected = JSON.parse(JSON.stringify(store.state));
    await store.close();
    const latestMind = {
      ...mind,
      events: [{
        seq: 1,
        kind: "rest",
        requestId: "new-rest",
        args: {},
        at: "2026-11-12T00:00:00Z",
      }],
    };
    await Deno.writeTextFile(`${directory}/candidate/mind/index.json`, JSON.stringify(latestMind));
    const reverse: string[] = [];
    await exportSnapshot(candidateConfig, "rollback", (line) => {
      reverse.push(line);
      return Promise.resolve();
    });
    await assert.rejects(importSnapshot(oldConfig, "rollback", input(reverse.slice(0, -1)), true));
    assert.deepEqual(JSON.parse(await Deno.readTextFile(`${directory}/old/mind/index.json`)), mind);
    // 持有任一目标账本锁时，两份原件都不允许开始替换。
    store = new ScriptStore(`${directory}/old/scripts/ledger`);
    await store.initialize();
    await assert.rejects(importSnapshot(oldConfig, "rollback", input(reverse), true));
    assert.deepEqual(JSON.parse(await Deno.readTextFile(`${directory}/old/mind/index.json`)), mind);
    await store.close();
    const receipt = await importSnapshot(oldConfig, "rollback", input(reverse), true);
    assert.deepEqual(await importSnapshot(oldConfig, "rollback", input(reverse), true), receipt);
    assert.deepEqual(
      JSON.parse(await Deno.readTextFile(`${directory}/old/mind/index.json`)),
      latestMind,
    );
    const archives = [...Deno.readDirSync(`${directory}/old`)].filter((e) =>
      e.isDirectory && e.name.startsWith("mind.before-rollback-")
    );
    assert.equal(archives.length, 1);
    assert.deepEqual(
      JSON.parse(await Deno.readTextFile(`${directory}/old/${archives[0].name}/index.json`)),
      mind,
    );
    store = new ScriptStore(`${directory}/old/scripts/ledger`);
    await store.initialize();
    assert.deepEqual(JSON.parse(JSON.stringify(store.state)), expected);
    assert.equal(store.state.jobs[0].actions[0].status, "unknown");
  } finally {
    await store.close();
    await Deno.remove(directory, { recursive: true });
  }
});
