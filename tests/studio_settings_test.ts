import assert from "node:assert/strict";
import { KernelError } from "../kernel/protocol.ts";
import {
  studioSettingsFailure,
  type StudioSnapshot,
  validateStudioUpdate,
} from "../services/admin/studio-settings.ts";
import { SharedBudget } from "../services/runtime/budget.ts";
import { createStudioSettings, type StudioSettingsOptions } from "../supervisor/studio-settings.ts";
import type { StudioState } from "../supervisor/studio-settings-catalog.ts";

const sentinel = "never_echo_fixture_secret";
const commit = "ab".repeat(20);
const initial = "eve-fixture";
async function json(path: string, value: unknown, mode = 0o600) {
  await Deno.writeTextFile(path, JSON.stringify(value) + "\n", { mode });
}
function item(snapshot: StudioSnapshot, key: string) {
  const result = snapshot.items.find((entry) => entry.key === key);
  assert.ok(result, key);
  return result;
}
function failure(status: number) {
  return (error: unknown) => {
    assert.ok(error instanceof KernelError);
    const wire = studioSettingsFailure(new KernelError(error.kind, error.message));
    assert.equal(wire.status, status);
    assert.ok(!JSON.stringify(wire).includes(sentinel));
    return true;
  };
}
async function fixture(run: (fixture: Awaited<ReturnType<typeof setup>>) => Promise<void>) {
  const root = await Deno.makeTempDir({ prefix: "eve-studio-settings-" });
  let files: Awaited<ReturnType<typeof setup>> | undefined;
  try {
    files = await setup(root);
    await run(files);
  } finally {
    try {
      await files?.budget.close();
    } finally {
      await Deno.remove(root, { recursive: true });
    }
  }
}
async function setup(root: string) {
  const configPath = `${root}/supervisor.json`, settingsFile = `${root}/runtime-settings.json`;
  const key = `${root}/private-${sentinel}`;
  await Deno.writeTextFile(key, sentinel, { mode: 0o600 });
  const configFile = `${root}/instances/${initial}/config.json`;
  await Deno.mkdir(`${root}/instances/${initial}`, { recursive: true, mode: 0o700 });
  await json(configFile, {
    instance: { id: initial, sourceCommit: commit, credentialFile: key, leaseMs: 30_000 },
    eve: {
      sourceCommit: commit,
      pollMs: 1000,
      defaultSleepMs: 60000,
      modelTimeoutMs: 120000,
      maxOutputTokens: 8192,
      remoteRecords: true,
    },
    scripts: { defaultTimeoutMs: 60000, denoBinary: `${root}/private-${sentinel}` },
    phone: { ownerPlatformUserId: sentinel, adapterUrl: `http://example.invalid/${sentinel}` },
    nightlySelfCheck: { timeZone: "UTC", hour: 3, minute: 15, attachmentDirectory: key },
  }, 0o440);
  await json(configPath, {
    supervisor: { initialInstance: initial, tickMs: 2000 },
    instances: [{}],
  });
  await json(`${root}/runtime.json`, {
    model: {
      model: "default-fixture",
      baseUrl: `https://example.invalid/${sentinel}`,
      apiKeyFile: key,
      timeoutMs: 120000,
      maxOutputTokens: 8192,
      protocol: "chat",
      sessionId: sentinel,
      reasoningEffort: "high",
    },
    agentModels: { coding: { model: "code-fixture", apiKeyFile: key } },
    runtimeService: {
      settingsFile,
      accessMode: "restricted",
      keyFile: key,
      allowedRoots: [key],
      commandUseSystemd: true,
      commandIdentity: { uid: 1001, gid: 1001 },
    },
    privateFutureField: { token: sentinel, keep: [1, false] },
  });
  await json(`${root}/admin.json`, {
    admin: { tokenFile: key, publicOrigin: `https://${sentinel}.invalid` },
  });
  await json(`${root}/guardian.json`, { ownership: { leaseMs: 30_000 } });
  await json(`${root}/qq.json`, {
    qq: {
      sandbox: false,
      appIdFile: key,
      appSecretFile: `${key}.absent`,
      replyScope: "private",
      attachmentDownloads: { temporaryDirectory: key },
      ingress: { mode: "webhook", path: `/${sentinel}` },
    },
  });
  await json(settingsFile, { dailyTokenBudget: 99_000, timeZone: "Asia/Tokyo", paused: true });
  const budget = new SharedBudget(`${root}/budget`, settingsFile);
  await budget.initialize();
  const state: StudioState = { current: initial, paused: true, upgrade: null };
  const calls = { write: 0, pause: 0, read: 0 };
  const options: StudioSettingsOptions = {
    configPath,
    controller: {
      get state() {
        return structuredClone(state);
      },
      instance: (id) => ({
        id,
        configFile: `${root}/instances/${id}/config.json`,
        source: { commit },
      }),
    },
    runtime: {
      status: async () => ({
        budget: await budget.status(),
        throughSeq: ++calls.read,
        model: { current: { model: "selected-fixture", revision: 5 }, private: sentinel },
      }),
      settings: (patch) => {
        calls.write++;
        return budget.updateSettings(patch);
      },
    },
    changePause: async (paused) => {
      calls.pause++;
      await budget.updateSettings({ paused });
      state.paused = paused;
    },
    serialControl: (task) => task(),
  };
  return {
    root,
    options,
    store: createStudioSettings(options),
    configFile,
    settingsFile,
    state,
    budget,
    calls,
  };
}

Deno.test("studio：只投影允许的字段；不把部署值、动态选择与运行预算混为一谈", () =>
  fixture(async ({ store, root }) => {
    const snapshot = await store.read();
    const wire = JSON.stringify(snapshot);
    for (
      const forbidden of [
        root,
        sentinel,
        "example.invalid",
        "privateFutureField",
        "tokenFile",
        "apiKeyFile",
      ]
    ) {
      assert.ok(!wire.includes(forbidden));
    }
    assert.equal(snapshot.revision, (await store.read()).revision);
    assert.equal(snapshot.items.length, new Set(snapshot.items.map((entry) => entry.key)).size);
    for (const entry of snapshot.items) {
      assert.ok(entry.description && entry.label && entry.scope && entry.group);
      if (entry.mode === "restricted") assert.equal(entry.editable, false);
      if (entry.type === "secret") {
        assert.equal(typeof entry.isSet, "boolean");
        assert.equal(Object.hasOwn(entry, "value"), false);
      }
    }
    assert.equal(item(snapshot, "model.configured").value, "default-fixture");
    assert.equal(item(snapshot, "model.current").value, "selected-fixture");
    assert.equal(item(snapshot, "budget.dailyTokenBudget").value, 99000);
    assert.equal(item(snapshot, "budget.timeZone").editable, false);
    assert.equal(item(snapshot, "permissions.accessMode").value, "restricted");
    assert.equal(item(snapshot, "qq.attachmentDownloads.maxBytes").value, 64 * 1024 * 1024);
    assert.equal(item(snapshot, "agents.investigation.reasoningEffort").editable, false);
    assert.deepEqual(
      ["qq.appId", "qq.appSecret"].map((key) => {
        const field = item(snapshot, key);
        return field.type === "secret" && field.isSet;
      }),
      [true, false],
    );
  }));

Deno.test("studio：同来源成批原子替换，保留未知字段和文件权限，无变化不写", () =>
  fixture(async ({ store, root, configFile, options }) => {
    const old = await store.read(), path = `${root}/runtime.json`;
    const before = JSON.parse(await Deno.readTextFile(path));
    const next = await store.update({
      revision: old.revision,
      changes: { "model.reasoningEffort": "medium", "agents.coding.maxOutputTokens": 4096 },
    });
    assert.notEqual(next.revision, old.revision);
    before.model.reasoningEffort = "medium";
    before.agentModels.coding.maxOutputTokens = 4096;
    assert.deepEqual(JSON.parse(await Deno.readTextFile(path)), before);
    assert.equal((await createStudioSettings(options).read()).revision, next.revision);
    const same = await store.update({
      revision: next.revision,
      changes: { "model.reasoningEffort": "medium" },
    });
    assert.equal(same.revision, next.revision);
    const original = await Deno.lstat(configFile);
    const updated = await store.update({
      revision: same.revision,
      changes: { "nightly.hour": 22, "eve.pollMs": 1500, "scripts.defaultTimeoutMs": 12345 },
    });
    const after = await Deno.lstat(configFile);
    assert.equal(after.mode, original.mode);
    assert.equal(after.uid, original.uid);
    assert.equal(after.gid, original.gid);
    assert.notEqual(after.ino, original.ino);
    const instance = JSON.parse(await Deno.readTextFile(configFile));
    assert.equal(instance.nightlySelfCheck.attachmentDirectory, `${root}/private-${sentinel}`);
    assert.equal(instance.nightlySelfCheck.hour, 22);
    assert.equal(instance.eve.pollMs, 1500);
    assert.equal(instance.scripts.defaultTimeoutMs, 12345);
    await store.update({
      revision: updated.revision,
      changes: { "qq.ingress.maxInFlight": 12, "qq.attachmentDownloads.maxBytes": 100000 },
    });
    assert.equal(JSON.parse(await Deno.readTextFile(`${root}/qq.json`)).qq.ingress.maxInFlight, 12);
    for await (const file of Deno.readDir(root)) assert.ok(!file.name.endsWith(".tmp"));
  }));

Deno.test("studio：两条独立控制队列同修订竞争只成功一次；带外替换及 ABA 使旧请求过期", () =>
  fixture(async ({ store, options, root }) => {
    const second = createStudioSettings(options), { revision } = await store.read();
    const results = await Promise.allSettled(
      [store, second].map((service, offset) =>
        service.update({ revision, changes: { "supervisor.tickMs": 1110 + offset } })
      ),
    );
    assert.equal(results.filter((result) => result.status === "fulfilled").length, 1);
    for (const result of results) if (result.status === "rejected") failure(409)(result.reason);
    const next = await store.read(), path = `${root}/supervisor.json`;
    await store.update({ revision: next.revision, changes: { "supervisor.tickMs": 2000 } });
    await assert.rejects(
      () => store.update({ revision, changes: { "supervisor.tickMs": 900 } }),
      failure(409),
    );
    const readback = await store.read(), data = JSON.parse(await Deno.readTextFile(path));
    data.future = sentinel;
    await json(path, data);
    await assert.rejects(
      () => store.update({ revision: readback.revision, changes: { "supervisor.tickMs": 2500 } }),
      failure(409),
    );
    assert.deepEqual(JSON.parse(await Deno.readTextFile(path)), data);
  }));

Deno.test("studio：拒绝越权、未知字段、非法类型、范围、跨来源批次，整批均无副作用", () =>
  fixture(async ({ store, calls }) => {
    const { revision } = await store.read();
    for (
      const [changes, status] of [
        [{ "runtime.url": `https://${sentinel}.invalid` }, 400],
        [{ "scripts.denoBinary": sentinel }, 400],
        [{ "permissions.accessMode": "unrestricted" }, 403],
        [{ "qq.replyScope": "all" }, 403],
        [{ "model.apiKey": sentinel }, 403],
        [{ "budget.timeZone": "UTC" }, 403],
        [{ "model.reasoningEffort": "high", "agents.coding.maxOutputTokens": 0 }, 400],
        [{ "budget.dailyTokenBudget": "123" }, 400],
        [{ "budget.dailyTokenBudget": -1 }, 400],
        [{ "nightly.timeZone": "../../private" }, 400],
        [{ "nightly.hour": 24 }, 400],
        [{ "runtime.paused": false, "budget.dailyTokenBudget": 1000 }, 400],
        [{ "model.reasoningEffort": "low", "supervisor.tickMs": 200 }, 400],
      ] as const
    ) await assert.rejects(() => store.update({ revision, changes }), failure(status));
    for (
      const body of [
        null,
        [],
        { revision, changes: [] },
        { revision, changes: {} },
        { revision, changes: { "model.maxOutputTokens": 123 }, path: sentinel },
        JSON.parse(`{"revision":"${revision}","changes":{"__proto__":{}}}`),
        { revision, changes: { "runtime.constructor": 1 } },
        { expected: revision, changes: {} },
        { revision, changes: { "budget.dailyTokenBudget": 1.5 } },
        { revision, changes: { "model.reasoningEffort": { tool: sentinel } } },
      ]
    ) assert.throws(() => validateStudioUpdate(body), failure(400));
    assert.equal((await store.read()).revision, revision);
    assert.equal(calls.write + calls.pause, 0);
  }));

Deno.test("studio：预算与暂停走原出口，用量不使表单过期，账本重启不被旧快照覆盖", () =>
  fixture(async ({ store, budget, calls, root, settingsFile, state }) => {
    const before = await store.read();
    const running = await store.update({
      revision: before.revision,
      changes: { "runtime.paused": false },
    });
    assert.equal(state.paused, false);
    assert.equal(calls.pause, 1);
    assert.equal(item(running, "eve.pollMs").editable, false);
    await assert.rejects(
      () => store.update({ revision: running.revision, changes: { "eve.pollMs": 505 } }),
      failure(403),
    );
    await budget.reserve("fixture-turn", 10);
    assert.equal((await store.read()).revision, running.revision);
    const updated = await store.update({
      revision: running.revision,
      changes: { "budget.dailyTokenBudget": 0 },
    });
    assert.equal((await budget.status()).dailyTokenBudget, 0);
    assert.equal(calls.write, 1);
    await budget.updateSettings({ dailyTokenBudget: 555 });
    await budget.updateSettings({ dailyTokenBudget: 0 });
    await assert.rejects(
      () =>
        store.update({ revision: updated.revision, changes: { "budget.dailyTokenBudget": 123 } }),
      failure(409),
    );
    await budget.close();
    await json(settingsFile, { paused: true, dailyTokenBudget: 77777, timeZone: "UTC" });
    const restarted = new SharedBudget(`${root}/budget`, settingsFile);
    try {
      await restarted.initialize();
      assert.deepEqual(await restarted.settings(), {
        paused: false,
        dailyTokenBudget: 0,
        timeZone: "Asia/Tokyo",
      });
    } finally {
      await restarted.close();
    }
  }));

Deno.test("studio：不跟随替代实例、可写目录、符号链接或硬链接；错误不泄漏内容", () =>
  fixture(async ({ store, root, configFile, state, options }) => {
    const { revision } = await store.read();
    state.upgrade = { diagnostic: sentinel };
    await assert.rejects(
      () => store.update({ revision, changes: { "eve.pollMs": 2000 } }),
      failure(409),
    );
    assert.equal(item(await store.read(), "eve.pollMs").editable, false);
    state.upgrade = null;
    state.current = "../escape";
    await assert.rejects(() => store.read(), failure(503));
    state.current = initial;
    const secret = `${root}/private-${sentinel}`;
    const bad = createStudioSettings({
      ...options,
      controller: {
        state,
        instance: () => ({ id: initial, configFile: secret, source: { commit } }),
      },
    });
    await assert.rejects(() => bad.read(), failure(503));
    await Deno.rename(configFile, configFile + ".original");
    await Deno.symlink(secret, configFile);
    await assert.rejects(() => store.read(), failure(503));
    assert.equal(await Deno.readTextFile(secret), sentinel);
    await Deno.remove(configFile);
    await Deno.link(configFile + ".original", configFile);
    await assert.rejects(() => store.read(), failure(503));
    await Deno.remove(configFile);
    await Deno.rename(configFile + ".original", configFile);
    await Deno.chmod(`${root}/instances/${initial}`, 0o777);
    await assert.rejects(() => store.read(), failure(503));
    await Deno.chmod(`${root}/instances/${initial}`, 0o750);
    await Deno.writeTextFile(`${root}/qq.json`, `not json ${sentinel}`);
    await assert.rejects(() => store.read(), failure(503));
  }));

Deno.test("studio：失去热设置来源不得从配置猜测；提交后失联不重试、不泄漏异常", () =>
  fixture(async ({ store, options, budget, calls }) => {
    const offline = createStudioSettings({
      ...options,
      runtime: { ...options.runtime, status: () => Promise.reject(new Error(sentinel)) },
    });
    const view = await offline.read();
    assert.equal(item(view, "budget.dailyTokenBudget").value, null);
    assert.equal(item(view, "runtime.paused").editable, false);
    await assert.rejects(
      () => offline.update({ revision: view.revision, changes: { "runtime.paused": false } }),
      failure(403),
    );
    const uncertain = createStudioSettings({
      ...options,
      runtime: {
        ...options.runtime,
        settings: async (patch) => {
          await options.runtime.settings(patch);
          throw new Error(sentinel);
        },
      },
    });
    const { revision } = await store.read();
    await assert.rejects(
      () => uncertain.update({ revision, changes: { "budget.dailyTokenBudget": 1234 } }),
      failure(503),
    );
    assert.equal((await budget.status()).dailyTokenBudget, 1234);
    assert.equal(calls.write, 1);
    assert.equal(studioSettingsFailure(new Error(sentinel)).error.code, "unconfirmed");
  }));
