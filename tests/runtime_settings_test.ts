import assert from "node:assert/strict";
import { SharedBudget } from "../services/runtime/budget.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { RuntimeAdminClient, startRuntimeAdmin } from "../services/runtime/admin.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

Deno.test("运行设置：旧配置只导入一次，发布覆盖和快照缺失不重置预算、暂停、时区或用量", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-settings-restart-" });
  const path = `${root}/settings.json`;
  await writeDurableJson(path, {
    dailyTokenBudget: 50000000,
    paused: false,
    timeZone: "Asia/Tokyo",
  });
  let budget = new SharedBudget(`${root}/budget`, path);
  await budget.initialize();
  try {
    await budget.reserve("before-upgrade", 1000);
    await budget.response("before-upgrade", {
      content: "",
      toolCalls: [],
      usage: { status: "reported", inputTokens: 200, cachedInputTokens: 100, outputTokens: 10 },
    });
    await Promise.all([
      budget.updateSettings({ dailyTokenBudget: 12345678 }),
      budget.updateSettings({ paused: true }),
    ]);
    const expected = await budget.status();
    assert.equal(expected.dailyTokenBudget, 12345678);
    assert.equal(expected.paused, true);
    assert.equal(expected.knownNoncachedTokens, 110);
    const revisions = budget.settingsJournal.events.length;
    await budget.updateSettings({ paused: true });
    assert.equal(
      budget.settingsJournal.events.length,
      revisions,
      "unchanged settings create no extra facts",
    );
    await budget.close();
    await writeDurableJson(path, { dailyTokenBudget: 200000000, paused: false, timeZone: "UTC" });
    budget = new SharedBudget(`${root}/budget`, path);
    await budget.initialize();
    assert.deepEqual(await budget.status(), expected);
    assert.deepEqual(JSON.parse(await Deno.readTextFile(path)), {
      dailyTokenBudget: 12345678,
      paused: true,
      timeZone: "Asia/Tokyo",
    });
    await budget.close();
    await Deno.remove(path);
    budget = new SharedBudget(`${root}/budget`, path);
    await budget.initialize();
    assert.deepEqual(await budget.status(), expected);
    assert.equal(budget.settingsJournal.events.length, revisions);
  } finally {
    await budget.close();
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("运行设置：管理回执后重建完整执行服务仍采用预算，落盘和快照之间中断也不丢提交", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-settings-admin-" });
  const path = `${root}/settings.json`, key = "fixture_settings_admin_012345678901234567890";
  await writeDurableJson(path, {
    dailyTokenBudget: 50000000,
    paused: false,
    timeZone: "Asia/Tokyo",
  });
  const options = {
    directory: `${root}/backend`,
    workspace: `${root}/workspace`,
    settingsFile: path,
    maxOutputTokens: 100,
    model: {
      complete() {
        throw new Error("settings never call a model");
      },
      close() {},
    },
  };
  let engine = new RuntimeEngine(options);
  await engine.initialize();
  let server = startRuntimeAdmin(engine, key, () => Promise.resolve());
  await Deno.writeTextFile(`${root}/key`, key);
  await writeDurableJson(`${root}/connection.json`, { url: server.url });
  const client = new RuntimeAdminClient({
    connectionFile: `${root}/connection.json`,
    keyFile: `${root}/key`,
  });
  try {
    await client.settings({ dailyTokenBudget: 9876543, paused: true });
    await server.close();
    await engine.close();
    await writeDurableJson(path, { dailyTokenBudget: 200000000, paused: false, timeZone: "UTC" });
    engine = new RuntimeEngine(options);
    await engine.initialize();
    server = startRuntimeAdmin(engine, key, () => Promise.resolve());
    await writeDurableJson(`${root}/connection.json`, { url: server.url });
    const status = await client.status() as {
      budget: { dailyTokenBudget: number; paused: boolean };
    };
    assert.equal(status.budget.dailyTokenBudget, 9876543);
    assert.equal(status.budget.paused, true);
    await Deno.remove(path);
    await Deno.writeTextFile(`${root}/untouched`, "original");
    await Deno.symlink(`${root}/untouched`, path);
    await assert.rejects(() => client.settings({ dailyTokenBudget: 4567890 }));
    assert.equal(await Deno.readTextFile(`${root}/untouched`), "original");
    await server.close();
    await engine.close();
    await Deno.remove(path);
    engine = new RuntimeEngine(options);
    await engine.initialize();
    server = startRuntimeAdmin(engine, key, () => Promise.resolve());
    assert.equal((await engine.budget.status()).dailyTokenBudget, 4567890);
    assert.equal((await engine.budget.status()).paused, true);
  } finally {
    await server.close();
    await engine.close();
    await Deno.remove(root, { recursive: true });
  }
});
