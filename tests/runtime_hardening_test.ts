import assert from "node:assert/strict";
import { KernelError } from "../kernel/protocol.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { SharedBudget } from "../services/runtime/budget.ts";
import { CommandSessions } from "../services/runtime/commands.ts";
import { FileTools } from "../services/runtime/files.ts";
import { MemoryStore } from "../services/runtime/memory.ts";
import { NoteStore } from "../services/runtime/notes.ts";
import { RestrictedFileTools } from "../services/runtime/restricted-files.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";

Deno.test("正则回溯只阻塞隔离 worker；超时后普通工具继续可用", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-regex-" });
  const notes = new NoteStore(directory);
  try {
    await notes.initialize();
    await notes.change("create", {
      title: "正则压力",
      content: "a".repeat(100000) + "!",
      sources: [{ kind: "experienced", at: new Date().toISOString(), description: "压力测试" }],
    }, "regex-note");
    const search = notes.search({ query: "(a+)+$", mode: "regex" });
    let responsive = false;
    const tick = setTimeout(() => responsive = true, 20);
    await assert.rejects(search, (e) => e instanceof KernelError && e.kind === "timeout");
    clearTimeout(tick);
    assert.equal(responsive, true);
    assert.equal(notes.list().items.length, 1);
    assert.equal((await notes.search({ query: "a{4}", mode: "regex" })).items.length, 1);
    await assert.rejects(
      () => notes.search({ query: "[", mode: "regex" }),
      (e) => e instanceof KernelError && e.kind === "rejected",
    );
  } finally {
    await notes.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("语义模型 TODO：已有记忆在未配置模型时明确按词法召回", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-lexical-" });
  const records = new FactJournal(`${directory}/records`);
  const memory = new MemoryStore(`${directory}/memory`, records);
  const notes = new NoteStore(`${directory}/notes`);
  try {
    await records.initialize();
    await memory.initialize();
    await notes.initialize();
    const source = await records.append("notice", { text: "我给花浇水" });
    const formed = await memory.remember({
      text: "我今天给花浇水。",
      sources: [{ recordSeq: source.seq, digest: source.digest, kind: "experienced" }],
      entities: ["浇水"],
    }, "formation");
    const recalled = await memory.recall({ query: "今天提到浇水了吗" });
    assert.equal(recalled.retrieval, "lexical");
    assert.equal(recalled.items[0].id, formed.id);
    assert.deepEqual((await memory.recall({ query: "无关事情" })).items, []);
    await assert.rejects(
      () => notes.search({ query: "花", mode: "semantic" }),
      (e) => e instanceof KernelError && e.kind === "unavailable" && /TODO/.test(e.message),
    );
  } finally {
    await notes.close();
    await memory.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("共享预算：非法用量保留预留，并发回执只结算一次，损坏记录拒绝恢复", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-budget-hardening-" });
  const settings = `${directory}/settings.json`;
  await writeDurableJson(settings, { dailyTokenBudget: 10000, paused: false, timeZone: "UTC" });
  const budget = new SharedBudget(`${directory}/ledger`, settings);
  try {
    await budget.initialize();
    await assert.rejects(() => budget.reserve("invalid", -10));
    await budget.reserve("one", 1000);
    await assert.rejects(() => budget.reserve("one", 1));
    await budget.response("one", {
      content: "",
      toolCalls: [],
      usage: {
        status: "reported",
        inputTokens: -1,
        cachedInputTokens: 0,
        outputTokens: 1,
      },
    });
    assert.equal((await budget.status()).unknownReservedTokens, 1000);
    const response = {
      content: "",
      toolCalls: [],
      usage: {
        status: "reported" as const,
        inputTokens: 100,
        cachedInputTokens: 80,
        outputTokens: 10,
      },
    };
    await Promise.all([budget.response("one", response), budget.response("one", response)]);
    assert.equal((await budget.status()).knownNoncachedTokens, 30);
    assert.equal((await budget.status()).unknownReservedTokens, 0);
    await assert.rejects(() => budget.response("missing", response));
    await budget.close();
    const journal = new FactJournal(`${directory}/ledger`);
    await journal.initialize();
    await journal.append("usage", { id: "one", known: -999, unknown: 0 });
    await journal.close();
    const restored = new SharedBudget(`${directory}/ledger`, settings);
    await assert.rejects(
      () => restored.initialize(),
      (e) => e instanceof KernelError && e.kind === "corrupt",
    );
  } finally {
    await budget.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("模型原响应落盘后崩溃恢复只补预算，不再次付费调用或强化记忆", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-response-recovery-" });
  const settingsFile = `${directory}/settings.json`;
  await writeDurableJson(settingsFile, { dailyTokenBudget: 10000, paused: false, timeZone: "UTC" });
  let calls = 0;
  const options = {
    directory: `${directory}/data`,
    workspace: `${directory}/workspace`,
    settingsFile,
    maxOutputTokens: 100,
    model: {
      complete() {
        calls++;
        throw new Error("不可重复调用");
      },
      close() {},
    },
  };
  let engine = new RuntimeEngine(options);
  try {
    await engine.initialize();
    await engine.budget.reserve("crashed-response", 1000);
    await engine.modelResponses.append("response", {
      id: "crashed-response",
      response: {
        content: "原响应",
        toolCalls: [],
        usage: {
          status: "reported",
          inputTokens: 100,
          cachedInputTokens: 80,
          outputTokens: 10,
        },
      },
    });
    assert.equal((await engine.budget.status()).unknownReservedTokens, 1000);
    await engine.close();
    engine = new RuntimeEngine(options);
    await engine.initialize();
    assert.equal((await engine.budget.status()).knownNoncachedTokens, 30);
    assert.equal((await engine.budget.status()).unknownReservedTokens, 0);
    assert.equal(engine.memory.journal.events.length, 0);
    assert.equal(calls, 0);
  } finally {
    await engine.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("命令进程组：取消会终结忽略 TERM 的子进程，排空后可以继续新命令", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-command-group-" });
  const files = new FileTools(directory),
    commands = new CommandSessions(`${directory}/sessions`, files);
  try {
    await commands.initialize();
    const started = await commands.start(
      { cmd: "trap '' TERM; printf ready; sleep 30 & wait" },
      "tree",
    );
    let ready = false;
    for (let i = 0; i < 100; i++) {
      if (
        (await commands.poll({ sessionId: started.sessionId, waitMs: 10 })).stdout.text === "ready"
      ) {
        ready = true;
        break;
      }
    }
    assert.equal(ready, true);
    await commands.cancelAll();
    await started.done;
    const ended = await commands.poll({ sessionId: started.sessionId });
    assert.equal(ended.state, "completed");
    assert.equal(ended.exitCode, 137);
    const next = await commands.start({ cmd: "printf next" }, "next");
    await next.done;
    assert.equal((await commands.poll({ sessionId: next.sessionId })).stdout.text, "next");
  } finally {
    await commands.close();
    await files.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("命令非 root 身份无法读取私钥或继承宿主敏感环境", async () => {
  const id = await new Deno.Command("/usr/bin/id", { args: ["-u"] }).output();
  if (new TextDecoder().decode(id.stdout).trim() !== "0") return;
  const directory = await Deno.makeTempDir({ dir: "/tmp", prefix: "eve-command-identity-" });
  await Deno.chmod(directory, 0o755);
  const privateFile = `${directory}/private-key`;
  await Deno.writeTextFile(privateFile, "fixture-secret", { mode: 0o600 });
  const files = new FileTools(directory);
  const commands = new CommandSessions(`${directory}/sessions`, files, { uid: 65534, gid: 65534 });
  Deno.env.set("EVE_TEST_HOST_SECRET", "must-not-inherit");
  try {
    await commands.initialize();
    const job = await commands.start({
      cmd: `id -u; cat '${privateFile}'; printf 'ENV:%s' "\${EVE_TEST_HOST_SECRET-unset}"`,
    }, "identity");
    await job.done;
    const view = await commands.poll({ sessionId: job.sessionId });
    assert.equal(view.stdout.text, "65534\nENV:unset");
    assert.match(view.stderr.text, /Permission denied/);
    assert.doesNotMatch(view.stdout.text, /fixture-secret/);
  } finally {
    Deno.env.delete("EVE_TEST_HOST_SECRET");
    await commands.close();
    await files.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("systemd 命令 cgroup：主命令结束会清理 setsid 后台进程，活动 unit 恢复拒绝重跑", async () => {
  const id = await new Deno.Command("/usr/bin/id", { args: ["-u"] }).output();
  if (new TextDecoder().decode(id.stdout).trim() !== "0") return;
  const directory = await Deno.makeTempDir({ dir: "/tmp", prefix: "eve-command-cgroup-" });
  await Deno.chmod(directory, 0o755);
  const workspace = `${directory}/workspace`;
  await Deno.mkdir(workspace, { mode: 0o755 });
  await Deno.chown(workspace, 65534, 65534);
  const files = new FileTools(workspace);
  const commands = new CommandSessions(
    `${directory}/sessions`,
    files,
    { uid: 65534, gid: 65534 },
    true,
  );
  try {
    await commands.initialize();
    const detached = await commands.start({
      cmd:
        `setsid /bin/bash -c 'trap "" TERM; printf "%s" "$$" > child.pid; sleep 30' >/dev/null 2>&1 & while [ ! -s child.pid ]; do sleep 0.01; done; printf main-done`,
    }, crypto.randomUUID());
    await detached.done;
    const result = await commands.poll({ sessionId: detached.sessionId });
    assert.equal(result.stdout.text, "main-done");
    // 后台进程故意忽略 TERM，unit 停止升级为 KILL，systemd 返回非零清理结果。
    assert.equal(result.exitCode, 1);
    const pid = (await Deno.readTextFile(`${workspace}/child.pid`)).trim();
    const state = await new Deno.Command("/bin/cat", {
      args: [`/proc/${pid}/stat`],
      stderr: "null",
    }).output();
    if (state.success) assert.match(new TextDecoder().decode(state.stdout), /\) Z /);
    const running = await commands.start({ cmd: "printf running; sleep 30" }, crypto.randomUUID());
    for (let i = 0; i < 100; i++) {
      if (
        (await commands.poll({ sessionId: running.sessionId, waitMs: 10 })).stdout.text ===
          "running"
      ) break;
    }
    const duplicate = new CommandSessions(`${directory}/sessions`, files, {
      uid: 65534,
      gid: 65534,
    }, true);
    await assert.rejects(
      () => duplicate.initialize(),
      (e) => e instanceof KernelError && e.kind === "unavailable",
    );
    await commands.cancelAll();
    await running.done;
    assert.equal((await commands.poll({ sessionId: running.sessionId })).state, "completed");
  } finally {
    await commands.close();
    await files.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("命令宿主 SIGKILL：恢复先终止原 unit，unknown 完成事实持久且原命令不重放", async () => {
  const id = await new Deno.Command("/usr/bin/id", { args: ["-u"] }).output();
  if (new TextDecoder().decode(id.stdout).trim() !== "0") return;
  const directory = await Deno.makeTempDir({ dir: "/tmp", prefix: "eve-command-crash-" });
  await Deno.chmod(directory, 0o755);
  const workspace = `${directory}/workspace`;
  await Deno.mkdir(workspace, { mode: 0o755 });
  await Deno.chown(workspace, 65534, 65534);
  const sessions = `${directory}/sessions`,
    helper = `${directory}/helper.ts`,
    ready = `${directory}/ready`;
  const requestId = crypto.randomUUID();
  await Deno.writeTextFile(
    helper,
    `
import {CommandSessions} from ${
      JSON.stringify(new URL("../services/runtime/commands.ts", import.meta.url).href)
    };
import {FileTools} from ${
      JSON.stringify(new URL("../services/runtime/files.ts", import.meta.url).href)
    };
const commands = new CommandSessions(${JSON.stringify(sessions)}, new FileTools(${
      JSON.stringify(workspace)
    }), {uid:65534,gid:65534}, true);
await commands.initialize();
const job = await commands.start({cmd:"printf once >> marker; printf ready; sleep 30"}, ${
      JSON.stringify(requestId)
    });
for(let i=0;i<300;i++) { if((await commands.poll({sessionId:job.sessionId,waitMs:10})).stdout.text === "ready") break; }
await Deno.writeTextFile(${JSON.stringify(ready)}, JSON.stringify({sessionId:job.sessionId}));
await job.done;
await commands.close();
`,
  );
  const child = new Deno.Command(Deno.execPath(), {
    args: ["run", "--allow-read", "--allow-write", "--allow-run", helper],
    stdout: "piped",
    stderr: "piped",
  }).spawn();
  const files = new FileTools(workspace);
  let restored: CommandSessions | undefined;
  try {
    let saved: { sessionId: string } | undefined;
    for (let i = 0; i < 500; i++) {
      try {
        saved = JSON.parse(await Deno.readTextFile(ready));
        break;
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    assert.ok(saved, "子宿主未建立真实运行中的命令");
    child.kill("SIGKILL");
    await child.output();
    restored = new CommandSessions(sessions, files, { uid: 65534, gid: 65534 }, true);
    await restored.initialize();
    assert.equal((await restored.poll({ sessionId: saved.sessionId })).state, "unknown");
    assert.equal(restored.completions().items[0].state, "unknown");
    assert.equal(restored.completions().items[0].sessionId, saved.sessionId);
    await restored.start({ cmd: "printf once >> marker; printf ready; sleep 30" }, requestId);
    assert.equal(await Deno.readTextFile(`${workspace}/marker`), "once");
    await restored.close();
    restored = new CommandSessions(sessions, files, { uid: 65534, gid: 65534 }, true);
    await restored.initialize();
    assert.equal(restored.completions().items.length, 1);
    assert.deepEqual(restored.completions({ afterSeq: 1 }).items, []);
  } finally {
    try {
      child.kill("SIGKILL");
    } catch { /* 已在测试中停止。 */ }
    await child.status;
    await restored?.close();
    await files.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("文件工具实际降权：写入普通文件，符号链接也不能越权读取 root 私钥", async () => {
  const id = await new Deno.Command("/usr/bin/id", { args: ["-u"] }).output();
  if (new TextDecoder().decode(id.stdout).trim() !== "0") return;
  const directory = await Deno.makeTempDir({ dir: "/tmp", prefix: "eve-files-identity-" });
  await Deno.chmod(directory, 0o755);
  const workspace = `${directory}/workspace`;
  await Deno.mkdir(workspace, { mode: 0o755 });
  await Deno.chown(workspace, 65534, 65534);
  const privateFile = `${directory}/private-key`;
  await Deno.writeTextFile(privateFile, "fixture-private-key", { mode: 0o600 });
  const source = `${directory}/source`;
  for (const relative of ["services/runtime", "kernel"]) {
    await Deno.mkdir(`${source}/${relative}`, { recursive: true, mode: 0o755 });
  }
  for (
    const path of [source, `${source}/services`, `${source}/services/runtime`, `${source}/kernel`]
  ) {
    await Deno.chmod(path, 0o755);
  }
  for (
    const relative of [
      "services/runtime/files-worker.ts",
      "services/runtime/files.ts",
      "kernel/protocol.ts",
    ]
  ) {
    await Deno.copyFile(new URL(`../${relative}`, import.meta.url), `${source}/${relative}`);
    await Deno.chmod(`${source}/${relative}`, 0o644);
  }
  // 同盘硬链接不重复下载/复制 Deno，临时公开入口在本轮结束删除。
  const binary = `${directory}/deno`;
  await Deno.link(Deno.execPath(), binary);
  const tools = new RestrictedFileTools(workspace, [directory], { uid: 65534, gid: 65534 }, {
    denoBinary: binary,
    workerPath: `${source}/services/runtime/files-worker.ts`,
    cacheDirectory: `${workspace}/cache`,
  });
  try {
    const written = await tools.write({
      path: "example",
      text: "ordinary",
      expectedSha256: "absent",
    });
    assert.equal((await tools.read({ path: "example" })).text, "ordinary");
    await tools.replace({
      path: "example",
      oldText: "ordinary",
      newText: "updated",
      expectedSha256: written.sha256,
    });
    assert.equal((await tools.read({ path: "example" })).text, "updated");
    assert.equal(new TextDecoder().decode(await tools.readBytes({ path: "example" })), "updated");
    await assert.rejects(() => tools.readBytes({ path: "example", maxBytes: 3 }), KernelError);
    await Deno.symlink(privateFile, `${workspace}/linked-private-key`);
    await assert.rejects(() => tools.read({ path: "linked-private-key" }), KernelError);
    await assert.rejects(() => tools.read({ path: privateFile }), KernelError);
    await assert.rejects(() => tools.readBytes({ path: "linked-private-key" }), KernelError);
    assert.equal(await Deno.readTextFile(privateFile), "fixture-private-key");
  } finally {
    await tools.close();
    await Deno.remove(directory, { recursive: true });
  }
});
