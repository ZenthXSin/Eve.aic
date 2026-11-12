import assert from "node:assert/strict";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { RuntimeClient } from "../services/runtime/client.ts";
import { startRuntimeRpc } from "../services/runtime/rpc.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { composeRuntimePlugin } from "../composition/runtime.ts";
import { composeScriptPlugin } from "../composition/scripts.ts";
import { Kernel } from "../kernel/kernel.ts";
import type { ScriptSummary } from "../contracts/script.ts";
import { FileTools } from "../services/runtime/files.ts";
import { NoteStore } from "../services/runtime/notes.ts";
import { MemoryStore } from "../services/runtime/memory.ts";
import { FactJournal } from "../sdk/fact-journal.ts";

const key = "fixture_runtime_bridge_012345678901234567890";
const oldKey = "fixture_runtime_old_key_012345678901234567890";
const nextKey = "fixture_runtime_next_key_01234567890123456789";
async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-runtime-service-" });
  const settingsFile = `${directory}/settings.json`;
  await writeDurableJson(settingsFile, {
    dailyTokenBudget: 200_000_000,
    paused: false,
    timeZone: "Asia/Shanghai",
  });
  const authority = new OwnershipAuthority(`${directory}/guardian`, 60000);
  await authority.initialize();
  for (const [instanceId, credential] of [["old", oldKey], ["next", nextKey]]) {
    await authority.register({ instanceId, credential, sourceCommit: "a".repeat(40) });
  }
  const granted = await authority.grant({
    instanceId: "old",
    requestId: "initial",
    expectedEpoch: authority.status().epoch,
  });
  if (granted.status !== "granted") throw new Error("not granted");
  let paid = 0;
  const model = {
    complete() {
      paid++;
      return Promise.resolve({
        content: "真实模型协议测试",
        toolCalls: [],
        usage: {
          status: "reported" as const,
          inputTokens: 100,
          cachedInputTokens: 80,
          outputTokens: 10,
        },
      });
    },
    close() {},
  };
  const options = {
    directory: `${directory}/data`,
    workspace: `${directory}/workspace`,
    settingsFile,
    model,
    maxOutputTokens: 100,
  };
  let engine = new RuntimeEngine(options);
  await engine.initialize();
  let fence = new OwnershipFence(`${directory}/fence`, "runtime", authority);
  await fence.initialize();
  let rpc = await startRuntimeRpc(engine, key, fence);
  await Deno.writeTextFile(`${directory}/key`, key);
  const proofFile = `${directory}/proof.json`;
  await writeDurableJson(proofFile, {
    instanceId: "old",
    credential: oldKey,
    epoch: granted.owner.epoch,
  });
  const connectionFile = `${directory}/connection.json`;
  await writeDurableJson(connectionFile, { url: rpc.url });
  const client = new RuntimeClient({ connectionFile, keyFile: `${directory}/key`, proofFile });
  return {
    directory,
    settingsFile,
    authority,
    client,
    proofFile,
    connectionFile,
    paid: () => paid,
    engine: () => engine,
    async restart() {
      await rpc.close();
      engine = new RuntimeEngine(options);
      await engine.initialize();
      fence = new OwnershipFence(`${directory}/fence`, "runtime", authority);
      await fence.initialize();
      rpc = await startRuntimeRpc(engine, key, fence);
      await writeDurableJson(connectionFile, { url: rpc.url });
    },
    async close() {
      client.close();
      await rpc.close();
      await authority.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("独立执行出口：意识流/文件/模型共用令牌与原回执，换版和重启不重放，预算动态且共享", async () => {
  const f = await fixture();
  try {
    const event = await f.client.call("timeline", "append", {
      expectedSeq: 0,
      expectedPrevious: "0".repeat(64),
      kind: "activation",
      data: { origin: "fixture" },
    }, "event_1");
    const written = await f.client.call("files", "write", {
      path: "example.txt",
      text: "原件",
      expectedSha256: "absent",
    }, "write_1");
    const request = { messages: [{ role: "user", content: "fixture" }], tools: [] };
    const response = await f.client.call("model", "complete", request, "model_1");
    assert.equal(f.paid(), 1);
    assert.equal(
      (await f.client.call("model", "budget") as { knownNoncachedTokens: number })
        .knownNoncachedTokens,
      30,
    );
    const grant = await f.authority.grant({
      instanceId: "next",
      requestId: "upgrade",
      expectedEpoch: f.authority.status().epoch,
    });
    if (grant.status !== "granted") throw new Error("not granted");
    await assert.rejects(
      () =>
        f.client.call("files", "write", {
          path: "forbidden.txt",
          text: "old",
          expectedSha256: "absent",
        }, "old_write"),
      (e) => e instanceof KernelError && e.kind === "unavailable",
    );
    await assert.rejects(() =>
      f.client.call("timeline", "append", {
        expectedSeq: 1,
        expectedPrevious: (event as { digest: string }).digest,
        kind: "old",
        data: null,
      }, "old_event")
    );
    await writeDurableJson(f.proofFile, {
      instanceId: "next",
      credential: nextKey,
      epoch: grant.owner.epoch,
    });
    assert.deepEqual(
      await f.client.call("files", "write", {
        path: "example.txt",
        text: "原件",
        expectedSha256: "absent",
      }, "write_1"),
      written,
    );
    assert.deepEqual(await f.client.call("model", "complete", request, "model_1"), response);
    await f.restart();
    assert.deepEqual(await f.client.call("model", "result", { requestId: "model_1" }), response);
    assert.deepEqual(
      await f.client.call("timeline", "append", {
        expectedSeq: 0,
        expectedPrevious: "0".repeat(64),
        kind: "activation",
        data: { origin: "fixture" },
      }, "event_1"),
      event,
    );
    assert.equal(f.engine().records.events.length, 1);
    await writeDurableJson(f.settingsFile, {
      dailyTokenBudget: 0,
      paused: false,
      timeZone: "Asia/Shanghai",
    });
    await assert.rejects(() => f.client.call("model", "complete", request, "model_2"));
    assert.equal(f.paid(), 1);
    assert.equal(
      (await f.client.call("model", "budget") as { knownNoncachedTokens: number })
        .knownNoncachedTokens,
      30,
    );
  } finally {
    await f.close();
  }
});

Deno.test("异步命令：先返回会话，进程结束前持有原许可，交接排空后不重跑原命令", async () => {
  const f = await fixture();
  try {
    const args = { cmd: "printf '开始'; read value; printf '收到:%s' \"$value\"", waitMs: 0 };
    const started = await f.client.call("commands", "start", args, "command_1") as {
      sessionId: string;
      state: string;
    };
    assert.equal(started.state, "running");
    assert.equal(f.authority.status().activePermits, 1);
    const before = await f.client.call("commands", "write", {
      sessionId: started.sessionId,
      text: "测试\n",
    }, "stdin_1");
    assert.ok(before);
    let result;
    for (let i = 0; i < 100; i++) {
      result = await f.client.call("commands", "poll", {
        sessionId: started.sessionId,
        waitMs: 100,
      }) as { state: string; stdout: { text: string } };
      if (result.state === "completed") break;
    }
    assert.equal(result!.stdout.text, "开始收到:测试");
    const repeated = await f.client.call("commands", "start", args, "command_1");
    assert.equal((repeated as { state: string }).state, "completed");
    await f.restart();
    assert.deepEqual(await f.client.call("commands", "start", args, "command_1"), repeated);
    await assert.rejects(() =>
      f.client.call("commands", "start", { cmd: "touch invalid-start", waitMs: -1 }, "bad_wait")
    );
    assert.equal(
      (await f.client.call("files", "list") as { items: { name: string }[] }).items.some((e) =>
        e.name === "invalid-start"
      ),
      false,
    );
  } finally {
    await f.close();
  }
});

Deno.test("文件与便签：旧哈希拒绝覆盖，精确替换保留美元字面，便签保留来源与旧版本", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-notes-files-" });
  const files = new FileTools(directory), notes = new NoteStore(`${directory}/notes`);
  try {
    const first = await files.write({
      path: "a.txt",
      text: "原文😀\n第二行",
      expectedSha256: "absent",
    });
    await assert.rejects(
      () => files.write({ path: "a.txt", text: "覆盖", expectedSha256: "absent" }),
      KernelError,
    );
    const replaced = await files.replace({
      path: "a.txt",
      oldText: "原文😀",
      newText: "$& $` $'",
      expectedSha256: first.sha256,
    });
    assert.equal((await files.read({ path: "a.txt" })).text, "$& $` $'\n第二行");
    await assert.rejects(
      () =>
        files.replace({
          path: "a.txt",
          oldText: "第二行",
          newText: "",
          expectedSha256: first.sha256,
        }),
      KernelError,
    );
    assert.notEqual(replaced.sha256, first.sha256);
    await notes.initialize();
    const created = await notes.change("create", {
      title: "记事",
      content: "测试原文",
      sources: [{ kind: "experienced", at: new Date().toISOString(), description: "测试现场" }],
    }, "note_1");
    const next = await notes.change("update", {
      id: created.id,
      expectedRevision: 1,
      content: "修改后",
    }, "note_2");
    assert.equal(next.revision, 2);
    assert.equal(notes.get({ id: created.id, revision: 1 }).text, "测试原文");
    assert.equal(notes.versions({ id: created.id }).items.length, 2);
    assert.equal((await notes.search({ query: "修改" })).items.length, 1);
    await assert.rejects(() =>
      notes.change("update", { id: created.id, expectedRevision: 1, content: "过期" }, "note_3")
    );
  } finally {
    await files.close();
    await notes.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("记忆：只接受原记录来源，自动浮现与强强化独立于模型，淡忘只缩短视图而不删原件", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-memory-" });
  const records = new FactJournal(`${directory}/records`);
  let now = Date.now();
  const memory = new MemoryStore(
    `${directory}/memory`,
    records,
    () => Promise.resolve([1, 0]),
    () => now,
  );
  try {
    await records.initialize();
    const source = await records.append("notice", { text: "今天讨论了给花浇水" });
    await memory.initialize();
    await assert.rejects(() =>
      memory.remember({
        text: "无来源",
        sources: [{ recordSeq: 99, digest: source.digest, kind: "experienced" }],
      }, "bad")
    );
    const formed = await memory.remember({
      text: "我今天讨论了给花浇水。后来又聊了园艺。",
      sources: [{ recordSeq: 1, digest: source.digest, kind: "experienced" }],
      entities: ["浇水"],
    }, "formed");
    assert.equal((await memory.recall({ query: "花" })).items[0].id, formed.id);
    await memory.observe("我还记得浇水这件事");
    assert.ok(memory.get({ id: formed.id }).strength > 3);
    now += 90 * 86400000;
    assert.ok(memory.get({ id: formed.id }).level >= 3);
    const original = memory.journal.events.find((e) => e.kind === "formation")!;
    assert.equal((original.data as { text: string }).text, formed.text);
  } finally {
    await memory.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("真实内核 worker→脚本→独立工具出口：便签与文件动作各有原回执，重启不再执行", async () => {
  const f = await fixture();
  let kernel: Kernel | undefined;
  try {
    const configPath = `${f.directory}/config.json`;
    await writeDurableJson(configPath, {
      runtime: {
        connectionFile: f.connectionFile,
        keyFile: `${f.directory}/key`,
        proofFile: f.proofFile,
      },
      scripts: {
        directory: `${f.directory}/scripts`,
        cacheDirectory: `${f.directory}/cache`,
        denoBinary: Deno.execPath(),
        defaultTimeoutMs: 10000,
      },
    });
    await composeRuntimePlugin(`${f.directory}/runtime-plugin`, configPath);
    await composeScriptPlugin(`${f.directory}/scripts-plugin`, configPath);
    const phone = `${f.directory}/phone`;
    await Deno.mkdir(phone);
    await Deno.writeTextFile(
      `${phone}/manifest.json`,
      JSON.stringify({ name: "fixture-phone", version: "1.0.0", provides: { phone: "1.0.0" } }),
    );
    await Deno.writeTextFile(
      `${phone}/mod.ts`,
      `export default {load(ctx){ctx.provide("phone",{notifications:()=>({items:[],throughSeq:0,hasMore:false})})}};`,
    );
    kernel = new Kernel({ dataDir: `${f.directory}/kernel`, callTimeoutMs: 20000 });
    for (const dir of [phone, `${f.directory}/runtime-plugin`, `${f.directory}/scripts-plugin`]) {
      await kernel.register(dir);
    }
    await kernel.start();
    const request = {
      requestId: "full_tools",
      mode: "sync",
      code:
        'const file=await tools.files.write({path:"from-script.txt",text:"脚本实际写入",expectedSha256:"absent"}); await tools.notes.create({title:"现场",alias:"现场",content:"写入了文件",sources:[{kind:"experienced",at:new Date().toISOString(),description:file.path}]}); await tools.notes.update({id:"现场",content:"短名修改已完成"}); const note=await tools.notes.get({id:"现场"}); return {file,note};',
    };
    const job = await kernel.call("scripts", "start", request) as unknown as ScriptSummary;
    assert.equal(job.status, "completed");
    assert.equal(job.actions, 4);
    const receipts = await kernel.call("scripts", "receipts", { jobId: job.jobId }) as {
      items: Json[];
    };
    assert.equal(receipts.items.length, 4);
    assert.equal(
      await Deno.readTextFile(`${f.directory}/workspace/from-script.txt`),
      "脚本实际写入",
    );
    assert.deepEqual(await kernel.call("scripts", "start", request), job);
    assert.equal(f.engine().notes.list().items.length, 1);
    assert.equal(f.engine().notes.get({ id: "现场" }).text, "短名修改已完成");
    assert.equal(f.engine().notes.versions({ id: "现场" }).items.length, 2);
  } finally {
    if (kernel) assert.deepEqual(await kernel.stop(), []);
    await f.close();
  }
});

Deno.test("执行出口：观察分页加速仍受字节上限约束，记录摘要保持8条", async () => {
  const f = await fixture();
  try {
    for (let n = 0; n < 10; n++) await f.engine().records.append("fixture", { n });
    const summary = await f.client.call("records", "page") as {
      items: { seq: number }[];
      throughSeq: number;
    };
    assert.equal(summary.items.length, 8);
    await f.engine().records.append("fixture", { text: "x".repeat(6_100_000) });
    await f.engine().records.append("fixture", { text: "x".repeat(6_100_000) });
    const first = await f.client.call("timeline", "page") as {
      items: { seq: number }[];
      throughSeq: number;
      hasMore: boolean;
    };
    assert.equal(first.items.length, 11);
    assert.equal(first.throughSeq, 11);
    assert.equal(first.hasMore, true);
    const second = await f.client.call("timeline", "page", { afterSeq: first.throughSeq }) as {
      items: { seq: number }[];
      throughSeq: number;
      hasMore: boolean;
    };
    assert.equal(second.items.length, 1);
    assert.equal(second.throughSeq, 12);
    assert.equal(second.hasMore, false);
    assert.equal(f.paid(), 0);
  } finally {
    await f.close();
  }
});
