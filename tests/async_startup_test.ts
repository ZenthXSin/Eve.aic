import assert from "node:assert/strict";
import { FactJournal } from "../sdk/fact-journal.ts";
import { RuntimeAdminClient, startRuntimeAdmin } from "../services/runtime/admin.ts";
import type { RuntimeEngine } from "../services/runtime/engine.ts";
import { AdminControlClient } from "../services/admin/client.ts";

Deno.test("恢复：有界预读保持顺序、让出事件循环，篡改后不投影坏记录", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-async-recovery-" });
  let journal = new FactJournal(directory);
  try {
    await journal.initialize();
    for (let n = 0; n < 130; n++) await journal.append("sample", { n });
    await journal.close();
    journal = new FactJournal(directory);
    let responsive = false;
    const timer = setTimeout(() => responsive = true, 0);
    const seen: number[] = [];
    await journal.initialize((event) => seen.push(event.seq));
    clearTimeout(timer);
    assert.equal(responsive, true);
    assert.deepEqual(seen, Array.from({ length: 130 }, (_, i) => i + 1));
    assert.deepEqual(journal.recovery, { phase: "ready", read: 130, total: 130 });
    await journal.close();
    const path = `${directory}/events/0000000000000003.json`;
    const bad = JSON.parse(await Deno.readTextFile(path));
    bad.data.n = 999;
    await Deno.writeTextFile(path, JSON.stringify(bad));
    journal = new FactJournal(directory);
    const accepted: number[] = [];
    await assert.rejects(journal.initialize((event) => accepted.push(event.seq)));
    assert.deepEqual(accepted, [1, 2]);
    assert.equal(journal.available, false);
  } finally {
    await journal.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("异步启动：管理状态先可读，未就绪时禁止查询和修改", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-early-admin-" });
  const key = "fixture_early_startup_012345678901234567890";
  const engine = {
    available: false,
    recovery: { stage: "actions", journals: {} },
    budget: {
      status() {
        throw new Error("未初始化的预算不得被读取");
      },
    },
  } as unknown as RuntimeEngine;
  const server = startRuntimeAdmin(engine, key, () => Promise.resolve());
  try {
    await Deno.writeTextFile(`${root}/key`, key);
    await Deno.writeTextFile(`${root}/connection`, JSON.stringify({ url: server.url }));
    const client = new RuntimeAdminClient({
      connectionFile: `${root}/connection`,
      keyFile: `${root}/key`,
    });
    assert.deepEqual(await client.status(), { available: false, recovery: engine.recovery });
    await assert.rejects(client.query({ resource: "notes", operation: "page", args: {} }), /恢复/);
    await assert.rejects(client.call("configure", { dailyTokenBudget: 1 }), /恢复/);
  } finally {
    await server.close();
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("面板：恢复中回退到新鲜状态文件，控制请求不回退或重放", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-startup-panel-" });
  try {
    const path = `${root}/recovery.json`;
    const value = {
      service: "guardian",
      pid: Deno.pid,
      updatedAt: Date.now(),
      phase: "recovering",
      progress: { read: 4, total: 10 },
    };
    await Deno.writeTextFile(path, JSON.stringify(value));
    const client = new AdminControlClient(`${root}/missing`, `${root}/key`, [path]);
    assert.deepEqual(await client.status(), {
      runtime: { available: false },
      startup: { phase: "recovering", services: [value] },
    });
    await assert.rejects(client.control({ action: "resume", args: {} }));
    await Deno.writeTextFile(path, JSON.stringify({ ...value, updatedAt: Date.now() - 60_000 }));
    await assert.rejects(client.status());
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});
