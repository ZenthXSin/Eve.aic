import assert from "node:assert/strict";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { RuntimeAdminClient, startRuntimeAdmin } from "../services/runtime/admin.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { KernelError } from "../kernel/protocol.ts";

Deno.test("独立管理出口分页、动态预算、幂等回退标记且不接受实例/浏览器凭据", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-runtime-admin-" });
  const settingsFile = `${root}/settings.json`;
  await writeDurableJson(settingsFile, {
    dailyTokenBudget: 200_000_000,
    paused: false,
    timeZone: "Asia/Tokyo",
  });
  const engine = new RuntimeEngine({
    directory: `${root}/data`,
    workspace: `${root}/workspace`,
    settingsFile,
    maxOutputTokens: 100,
    model: {
      complete() {
        throw new Error("管理不调用模型");
      },
      close() {},
    },
  });
  await engine.initialize();
  let drains = 0;
  const key = "fixture_runtime_admin_012345678901234567890";
  const server = startRuntimeAdmin(engine, key, () => {
    drains++;
    return Promise.resolve();
  });
  await Deno.writeTextFile(`${root}/key`, key);
  await writeDurableJson(`${root}/connection.json`, { url: server.url });
  const client = new RuntimeAdminClient({
    connectionFile: `${root}/connection.json`,
    keyFile: `${root}/key`,
  });
  try {
    for (let n = 0; n < 12; n++) {
      await engine.records.append("fixture", { n, sourceCommit: "a".repeat(40) });
    }
    const page = await client.query({ resource: "timeline", operation: "page", args: {} }) as {
      items: { seq: number }[];
      next: { afterSeq: number };
    };
    assert.deepEqual(page.items.map((x) => x.seq), [1, 2, 3, 4, 5, 6, 7, 8]);
    const next = await client.query({
      resource: "timeline",
      operation: "page",
      args: page.next,
    }) as { items: { seq: number }[]; next?: unknown };
    assert.deepEqual(next.items.map((x) => x.seq), [9, 10, 11, 12]);
    assert.equal(next.next, undefined);
    const found = await client.query({
      resource: "timeline",
      operation: "search",
      args: { query: '"n":10' },
    }) as { items: { seq: number }[] };
    assert.equal(found.items[0].seq, 11);
    const detail = await client.query({
      resource: "timeline",
      operation: "get",
      args: { seq: 11, limit: 20 },
    }) as { text: string; nextChar: number };
    assert.equal(detail.text.length, 20);
    assert.equal(detail.nextChar, 20);
    await client.settings({ dailyTokenBudget: 1234, paused: true });
    assert.equal((await engine.budget.status()).dailyTokenBudget, 1234);
    assert.equal((await engine.budget.status()).paused, true);
    await assert.rejects(() => client.call("settings", { timeZone: "UTC" }), KernelError);
    const marker = {
      upgradeId: "upgrade_fixture",
      sourceCommit: "a".repeat(40),
      afterSeq: 8,
      throughSeq: 12,
      reason: "候选未确认",
    };
    const candidateNote = await engine.notes.change("create", {
      title: "候选版本便签",
      content: "保留供主人查验的未确认原件",
      sources: [{ kind: "experienced", at: new Date().toISOString(), description: "候选现场" }],
    }, "candidate-note");
    await client.rollback(marker);
    await client.rollback(marker);
    assert.equal(await client.watermark(), 13);
    assert.throws(() => engine.notes.get({ id: candidateNote.id }), KernelError);
    const noteDetail = await client.query({
      resource: "notes",
      operation: "get",
      args: { id: candidateNote.id },
    }) as { text: string; note: { quarantined: boolean } };
    assert.equal(noteDetail.text, "保留供主人查验的未确认原件");
    assert.equal(noteDetail.note.quarantined, true);
    const notePage = await client.query({
      resource: "notes",
      operation: "page",
      args: {},
    }) as { items: { id: string; quarantined: boolean }[] };
    assert.equal(notePage.items[0].id, candidateNote.id);
    assert.equal(notePage.items[0].quarantined, true);
    await assert.rejects(() => client.rollback({ ...marker, reason: "改写结果" }), KernelError);
    await client.quiesce();
    assert.equal(drains, 1);
    const originalText = "我" + "a".repeat(16300);
    const memory = await engine.memory.remember({
      text: originalText,
      sources: [{ recordSeq: 1, digest: engine.records.events[0].digest, kind: "experienced" }],
    }, "long-memory");
    const memories = await client.query({ resource: "memory", operation: "page", args: {} }) as {
      items: { id: string; truncated: boolean }[];
    };
    assert.equal(memories.items[0].id, memory.id);
    assert.equal(memories.items[0].truncated, true);
    let memoryOriginal = "", startChar = 0;
    while (true) {
      const part = await client.query({
        resource: "memory",
        operation: "get",
        args: { id: memory.id, startChar },
      }) as { text: string; nextChar?: number };
      memoryOriginal += part.text;
      if (part.nextChar === undefined) break;
      startChar = part.nextChar;
    }
    assert.equal(JSON.parse(memoryOriginal).data.text, originalText);
    const denied = await fetch(server.url, {
      method: "POST",
      headers: { authorization: "Bearer fixture_instance_key_01234567890123456789" },
      body: "{}",
    });
    assert.equal(denied.status, 403);
    await denied.body?.cancel();
    const browser = await fetch(server.url, {
      method: "POST",
      headers: { authorization: `Bearer ${key}`, origin: "http://evil.example" },
      body: "{}",
    });
    assert.equal(browser.status, 403);
    await browser.body?.cancel();
    const result = await fetch(server.url, {
      method: "POST",
      headers: { authorization: `Bearer ${key}` },
      body: "x".repeat(65537),
    });
    assert.equal((await result.json()).ok, false);
  } finally {
    await server.close();
    await engine.close();
    await Deno.remove(root, { recursive: true });
  }
});
