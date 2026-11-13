import assert from "node:assert/strict";
import { createMindControls } from "../plugins/mind-controls/mod.ts";
import { nightlySchedule } from "../plugins/mind-controls/nightly.ts";
import { checkCondition } from "../plugins/mind-controls/condition.ts";
import { ConsciousJournal } from "../plugins/eve/journal.ts";
import {
  activeEvents,
  contextEstimate,
  mechanicalCompression,
  pendingTools,
  render,
} from "../plugins/eve/view.ts";
import { handoverMind, handoverScripts } from "../supervisor/mind-handover.ts";
import { imageTextView } from "../sdk/image-text.ts";
import type { Handler, PluginContext } from "../sdk/plugin.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";

function context(read?: (namespace: string, method: string, args: Json) => Promise<Json>) {
  const handlers: Record<string, Handler> = {};
  const cleanups: (() => unknown)[] = [];
  const ctx = {
    manifest: {},
    provide(_contract: string, methods: Record<string, Handler>) {
      Object.assign(handlers, methods);
    },
    call: read ?? (() => {
      throw new KernelError("unavailable", "no dependency");
    }),
    getState() {},
    setState() {
      return Promise.resolve();
    },
    onDisable(fn: () => unknown) {
      cleanups.push(fn);
    },
    log() {},
  } as unknown as PluginContext;
  return {
    ctx,
    handlers,
    close: async () => {
      for (const close of cleanups.reverse()) await close();
    },
  };
}

Deno.test("未读消息：半小时提醒、并发去重、重启保留、读完停止、查询失败不阻塞其他提醒", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-unread-reminder-" });
  const interval = 30 * 60_000;
  let now = Date.parse("2026-11-14T00:00:00Z"), calls = 0, unavailable = false;
  let items: Json[] = [{ handle: { conversationId: "qq:user:a", seq: 1 } }];
  const read = (namespace: string, method: string, args: Json) => {
    assert.equal(namespace, "phone");
    assert.equal(method, "notifications");
    assert.deepEqual(args, { afterSeq: 0 });
    calls++;
    if (unavailable) throw new Error("phone unavailable");
    return Promise.resolve({ items, hasMore: false });
  };
  let host = context(read), plugin = createMindControls(directory, undefined, () => now, interval);
  const poll = () =>
    host.handlers.poll({ afterSeq: 0 }) as Promise<{
      items: { kind: string; args: { source?: string; items?: Json[] } }[];
    }>;
  try {
    await plugin.load!(host.ctx);
    await plugin.enable!(host.ctx);
    await poll();
    now += interval - 1;
    await poll();
    assert.equal(calls, 0);
    now++;
    const pages = await Promise.all([poll(), poll()]);
    assert.equal(calls, 1);
    assert.equal(pages[0].items.length, 1);
    assert.equal(pages[1].items.length, 1);
    assert.equal(pages[0].items[0].kind, "alarm");
    assert.equal(pages[0].items[0].args.source, "unread_messages");
    assert.deepEqual(pages[0].items[0].args.items, items);
    await host.close();
    host = context(read);
    plugin = createMindControls(directory, undefined, () => now, interval);
    await plugin.load!(host.ctx);
    await plugin.enable!(host.ctx);
    await poll();
    assert.equal(calls, 1);
    now += interval;
    assert.equal((await poll()).items.length, 2);
    items = [];
    now += interval;
    assert.equal((await poll()).items.length, 2);
    unavailable = true;
    now += interval;
    await host.handlers.remind({
      requestId: "independent",
      at: "2020-01-01T00:00:00Z",
      text: "其他提醒",
    });
    assert.equal((await poll()).items.length, 4);
    const status = await host.handlers.status(null) as {
      unreadReminder: { nextCheck: number; lastError: string };
    };
    assert.match(status.unreadReminder.lastError, /phone unavailable/);
    assert.equal(status.unreadReminder.nextCheck, now + 60_000);
    const previousCalls = calls;
    now += 59_999;
    await poll();
    assert.equal(calls, previousCalls);
    unavailable = false;
    now++;
    await poll();
    assert.equal(calls, previousCalls + 1);
    assert.equal(
      (await host.handlers.status(null) as { unreadReminder: { lastError?: string } })
        .unreadReminder.lastError,
      undefined,
    );
  } finally {
    await host.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("每晚自检：当地时间、重启恢复、停机补一次、并发不重复且不代替Eve删文件", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-nightly-" });
  const attachmentDirectory = `${directory}/attachments`;
  await Deno.mkdir(attachmentDirectory);
  await Deno.writeTextFile(`${attachmentDirectory}/kept.txt`, "仍由 Eve 决定");
  const config = { timeZone: "Asia/Tokyo", hour: 3, minute: 0, attachmentDirectory };
  let now = Date.parse("2026-11-12T17:59:00Z");
  let host = context(), plugin = createMindControls(`${directory}/mind`, config, () => now);
  try {
    // mind state only accepts its own files, so keep the attachment fixture outside it.
    const mind = `${directory}/mind`;
    plugin = createMindControls(mind, config, () => now);
    await plugin.load!(host.ctx);
    await plugin.enable!(host.ctx);
    const status = await host.handlers.status(null) as {
      nightlySelfCheck: { nextAt: string; timeZone: string };
    };
    assert.equal(status.nightlySelfCheck.nextAt, "2026-11-12T18:00:00.000Z");
    assert.equal(status.nightlySelfCheck.timeZone, "Asia/Tokyo");
    await host.close();
    host = context();
    now = Date.parse("2026-11-16T01:00:00Z");
    plugin = createMindControls(mind, config, () => now);
    await plugin.load!(host.ctx);
    await plugin.enable!(host.ctx);
    const results = await Promise.all([
      host.handlers.poll({ afterSeq: 0 }),
      host.handlers.poll({ afterSeq: 0 }),
    ]) as {
      items: { requestId: string; kind: string; args: { text: string; scheduledAt: string } }[];
    }[];
    assert.equal(results[0].items.length, 1);
    assert.equal(results[1].items.length, 1);
    assert.equal(results[0].items[0].kind, "alarm");
    assert.equal(results[0].items[0].requestId, "nightly_self_check_2026-11-16");
    assert.equal(results[0].items[0].args.scheduledAt, "2026-11-15T18:00:00.000Z");
    assert.ok(results[0].items[0].args.text.includes(attachmentDirectory));
    assert.equal(await Deno.readTextFile(`${attachmentDirectory}/kept.txt`), "仍由 Eve 决定");
    assert.deepEqual(await host.handlers.poll({ afterSeq: 1 }), {
      items: [],
      throughSeq: 1,
      hasMore: false,
    });
    await host.close();
    host = context();
    plugin = createMindControls(mind, config, () => now);
    await plugin.load!(host.ctx);
    await plugin.enable!(host.ctx);
    await host.handlers.poll({ afterSeq: 1 });
    const saved = JSON.parse(await Deno.readTextFile(`${mind}/index.json`));
    assert.equal(saved.events.length, 1);
    assert.equal(saved.nightlySelfCheck.nextAt, "2026-11-16T18:00:00.000Z");
    now = Date.parse("2026-11-16T18:00:00Z");
    await host.handlers.poll({ afterSeq: 1 });
    assert.equal(JSON.parse(await Deno.readTextFile(`${mind}/index.json`)).events.length, 2);
  } finally {
    await host.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("每晚自检：夏令时回拨同日不再唤醒，缺失时间跳到下一次，拒绝无效配置", () => {
  const config = {
    timeZone: "America/New_York",
    hour: 1,
    minute: 30,
    attachmentDirectory: "/workspace/tmp",
  };
  const schedule = nightlySchedule(config);
  assert.equal(schedule.latest(Date.parse("2026-11-01T06:35:00Z")).date, "2026-11-01");
  assert.equal(
    schedule.next(Date.parse("2026-11-01T05:30:00Z"), "2026-11-01").at,
    "2026-11-02T06:30:00.000Z",
  );
  const spring = nightlySchedule({ ...config, hour: 2 });
  assert.equal(spring.next(Date.parse("2026-03-08T06:00:00Z")).at, "2026-03-09T06:30:00.000Z");
  assert.throws(() => nightlySchedule({ ...config, timeZone: "invalid" }), KernelError);
  assert.throws(() => nightlySchedule({ ...config, hour: 24 }), KernelError);
  assert.throws(() => nightlySchedule({ ...config, attachmentDirectory: "relative" }), KernelError);
});

Deno.test("条件提醒：纯代码与只读事实检查，无模型、重启后迟到触发一次、可取消", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-conditions-" });
  let ready = false, reads = 0;
  const read = (namespace: string, method: string) => {
    assert.equal(namespace, "commands");
    assert.equal(method, "poll");
    reads++;
    return Promise.resolve({ status: ready ? "completed" : "running" });
  };
  let host = context(read), plugin = createMindControls(directory);
  try {
    await plugin.load!(host.ctx);
    await plugin.enable!(host.ctx);
    await host.handlers.update({ safeRemaining: 42 });
    await host.handlers.watch({
      requestId: "command_done",
      text: "我想回来接着看",
      everyMs: 1,
      code:
        'return facts.safeRemaining === 42 && (await tools.commands.poll({sessionId:"real"})).status === "completed";',
    });
    await host.handlers.watch({ requestId: "cancel_me", text: "已取消", code: "return true;" });
    await host.handlers.cancelReminder({ requestId: "cancel_control", reminderId: "cancel_me" });
    const first = await host.handlers.poll({ afterSeq: 0 }) as { items: { kind: string }[] };
    assert.equal(first.items.filter((e) => e.kind === "alarm").length, 0);
    await host.close();
    ready = true;
    await new Promise((resolve) => setTimeout(resolve, 10));
    host = context(read);
    plugin = createMindControls(directory);
    await plugin.load!(host.ctx);
    await plugin.enable!(host.ctx);
    const pages = await Promise.all([
      host.handlers.poll({ afterSeq: 3 }),
      host.handlers.poll({ afterSeq: 3 }),
    ]) as { items: { kind: string; args: { text: string; lateMs: number } }[] }[];
    assert.equal(pages[0].items.filter((e) => e.kind === "alarm").length, 1);
    const alarm = pages[0].items.find((e) => e.kind === "alarm")!;
    assert.equal(alarm.args.text, "我想回来接着看");
    assert.ok(alarm.args.lateMs >= 0);
    assert.equal(JSON.stringify(pages).includes("已取消"), false);
    assert.equal(reads, 2);
    const persisted = JSON.parse(await Deno.readTextFile(`${directory}/index.json`));
    assert.equal(persisted.events.filter((e: { kind: string }) => e.kind === "alarm").length, 1);
    assert.equal(
      persisted.conditions.find((c: { id: string }) => c.id === "cancel_me").status,
      "cancelled",
    );
  } finally {
    await host.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("条件提醒：无限循环能终止、无文件网络权限、写能力不可调用", async () => {
  const host = context();
  await assert.rejects(
    checkCondition(host.ctx, "while (true) {}", null),
    (e) => e instanceof KernelError && e.kind === "timeout",
  );
  assert.equal(
    await checkCondition(
      host.ctx,
      'try { await Deno.readTextFile("/etc/passwd"); return false; } catch { return true; }',
      null,
    ),
    true,
  );
  assert.equal(
    await checkCondition(
      host.ctx,
      "return tools.commands.start === undefined && tools.files.write === undefined;",
      null,
    ),
    true,
  );
  await assert.rejects(checkCondition(host.ctx, "return 1;", null), KernelError);
});

Deno.test("上下文：omit 移除整个调用对，保留其它调用及原件，raw 可以恢复", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-retain-pair-" });
  const journal = new ConsciousJournal(directory);
  try {
    await journal.initialize();
    const response = await journal.append("model_response", {
      requestId: "m",
      response: {
        content: "我的决定",
        toolCalls: [{ id: "a", name: "execute_script", args: { code: "secret one" } }, {
          id: "b",
          name: "execute_script",
          args: { code: "keep two" },
        }],
      },
    });
    const first = await journal.append("tool_result", {
      callId: `eve_${response.seq}_0`,
      result: { output: "secret result" },
    });
    await journal.append("tool_result", {
      callId: `eve_${response.seq}_1`,
      result: { output: "kept result" },
    });
    const original = await Deno.readTextFile(
      `${directory}/events/${String(first.seq).padStart(16, "0")}.json`,
    );
    await journal.append("retain", { responseSeq: response.seq, toolIndex: 0, mode: "omit" });
    const messages = render(journal.events, "identity");
    assert.deepEqual(messages.map((m) => m.role), ["system", "assistant", "tool", "user"]);
    assert.equal(messages[1].toolCalls?.length, 1);
    assert.equal(messages[1].toolCalls?.[0].id, `eve_${response.seq}_1`);
    assert.equal(JSON.stringify(messages).includes("secret"), false);
    assert.ok(messages[3].content.includes("此处清除过一次工具调用"));
    assert.equal(
      await Deno.readTextFile(`${directory}/events/${String(first.seq).padStart(16, "0")}.json`),
      original,
    );
    await journal.append("retain", { responseSeq: response.seq, toolIndex: 0, mode: "raw" });
    assert.equal(render(journal.events, "identity")[1].toolCalls?.length, 2);
  } finally {
    await journal.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("上下文：回滚候选不重放决定或吸收其摘要，事实回执和标记原件保留", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-rollback-view-" });
  const journal = new ConsciousJournal(directory);
  try {
    await journal.initialize();
    const old = await journal.append("notice", { source: "phone", items: ["先前经历"] });
    await journal.append("model_response", {
      requestId: "candidate",
      response: {
        content: "未确认候选的念头",
        toolCalls: [{ id: "c", name: "execute_script", args: { code: "should not run" } }],
      },
    });
    const candidate = await journal.append("compression", {
      throughSeq: old.seq,
      summary: "未确认候选重写记忆",
    });
    const marker = await journal.append("upgrade_rollback", {
      afterSeq: old.seq,
      throughSeq: candidate.seq,
      upgradeId: "u",
      sourceCommit: "b".repeat(40),
      reason: "rejected",
    });
    assert.deepEqual(activeEvents(journal.events).map((e) => e.seq), [old.seq, marker.seq]);
    assert.deepEqual(pendingTools(journal.events), []);
    const messages = render(journal.events, "identity");
    assert.equal(JSON.stringify(messages).includes("未确认候选的念头"), false);
    assert.equal(JSON.stringify(messages).includes("未确认候选重写记忆"), false);
    assert.ok(messages.at(-1)?.content.includes("回滚不撤销真实副作用"));
    assert.equal(journal.events.length, 4);
  } finally {
    await journal.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("上下文：图片描述在工具批次之后进入文字，主意识上下文不包含原图", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-image-context-" });
  const journal = new ConsciousJournal(directory);
  const image = {
    dataUrl: "data:image/png;base64,aGVsbG8=",
    provenance: {
      kind: "phone_attachment",
      conversationId: "c",
      sourceSeq: 1,
      attachmentId: "c:1:0",
      sha256: "a".repeat(64),
      mimeType: "image/png",
    },
  };
  try {
    await journal.initialize();
    const response = await journal.append("model_response", {
      requestId: "i",
      response: {
        content: "看看图",
        toolCalls: [{ id: "i", name: "execute_script", args: { code: "image" } }, {
          id: "j",
          name: "execute_script",
          args: { code: "other" },
        }],
      },
    });
    await journal.append("tool_result", {
      callId: `eve_${response.seq}_0`,
      result: { output: "image completed" },
    });
    await journal.append("notice", {
      source: "image_description",
      actionId: "s_1",
      observation: { description: "客观描述：一个小圆点", provenance: image.provenance },
    });
    await journal.append("tool_result", {
      callId: `eve_${response.seq}_1`,
      result: { output: "other completed" },
    });
    const messages = render(journal.events, "identity");
    assert.deepEqual(messages.map((m) => m.role), ["system", "assistant", "tool", "tool", "user"]);
    assert.equal(messages[4].images, undefined);
    assert.ok(messages[4].content.includes("一个小圆点"));
    assert.equal(messages[4].content.includes("aGVsbG8="), false);
    assert.ok(messages[4].content.includes("phone_attachment"));
    const projected = imageTextView({ nested: { image }, download: { path: "/attachment" } });
    assert.equal(JSON.stringify(projected).includes("aGVsbG8="), false);
    assert.ok(JSON.stringify(projected).includes("/attachment"));
  } finally {
    await journal.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("上下文：已压缩的工具原文不再构建历史消息，当前工具对和保留策略仍有效", () => {
  const events = [
    {
      seq: 1,
      kind: "model_response",
      get data(): Json {
        throw new Error("不应加载已移出的超大响应正文");
      },
    },
    {
      seq: 2,
      kind: "tool_result",
      get data(): Json {
        throw new Error("不应加载已移出的超大回执正文");
      },
    },
    { seq: 3, kind: "compression", data: { throughSeq: 2, summary: "我整理过的时间线" } },
    {
      seq: 4,
      kind: "model_response",
      data: {
        requestId: "current",
        response: {
          content: "当前决定",
          toolCalls: [{ id: "a", name: "execute_script", args: { code: "current" } }],
        },
      },
    },
    { seq: 5, kind: "tool_result", data: { callId: "eve_4_0", result: { output: "当前回执" } } },
    {
      seq: 6,
      kind: "retain",
      data: { responseSeq: 4, toolIndex: 0, mode: "summary", summary: "我的结果摘要" },
    },
  ] as unknown as import("../plugins/eve/journal.ts").ConsciousEvent[];
  const messages = render(events, "identity");
  assert.deepEqual(messages.map((m) => m.role), ["system", "user", "assistant", "tool"]);
  assert.ok(messages[1].content.includes("我整理过的时间线"));
  assert.ok(messages[3].content.includes("我的结果摘要"));
  assert.equal(messages[2].toolCalls![0].id, messages[3].toolCallId);
});

Deno.test("条件提醒：失败与恢复真实通知只在状态变化时追加，失败不丢条件", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-condition-recovery-" });
  const host = context(), plugin = createMindControls(directory);
  try {
    await plugin.load!(host.ctx);
    await plugin.enable!(host.ctx);
    await host.handlers.update({ ready: false });
    await host.handlers.watch({
      requestId: "retry_condition",
      everyMs: 1,
      text: "检查完回来",
      code: 'if (!facts.ready) throw new Error("当前条件数据不可用"); return false;',
    });
    await host.handlers.poll({});
    await new Promise((done) => setTimeout(done, 5));
    await host.handlers.poll({});
    let persisted = JSON.parse(await Deno.readTextFile(`${directory}/index.json`));
    assert.equal(
      persisted.events.filter((e: { kind: string }) => e.kind === "condition_error").length,
      1,
    );
    assert.equal(persisted.conditions[0].status, "pending");
    assert.ok(persisted.conditions[0].lastError.includes("数据不可用"));
    await host.handlers.update({ ready: true });
    await new Promise((done) => setTimeout(done, 5));
    await host.handlers.poll({});
    persisted = JSON.parse(await Deno.readTextFile(`${directory}/index.json`));
    assert.equal(
      persisted.events.filter((e: { kind: string }) => e.kind === "condition_recovered").length,
      1,
    );
    assert.equal(persisted.conditions[0].lastError, undefined);
    assert.equal(persisted.conditions[0].status, "pending");
  } finally {
    await host.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("提醒换版：旧实例停用后向独立候选交接，重启保留时间与条件提醒，活实例拒绝复制", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-mind-handover-" });
  const source = `${directory}/old`, target = `${directory}/candidate`;
  const old = context(), oldPlugin = createMindControls(source);
  const candidate = context(), newPlugin = createMindControls(target);
  try {
    await oldPlugin.load!(old.ctx);
    await oldPlugin.enable!(old.ctx);
    await old.handlers.remind({
      requestId: "old_alarm",
      at: new Date(Date.now() + 50).toISOString(),
      text: "接着原来的事",
    });
    await old.handlers.watch({
      requestId: "old_condition",
      everyMs: 1,
      text: "保留原条件",
      code: "return true;",
    });
    await assert.rejects(handoverMind(source, target, "upgrade_1"), KernelError);
    await old.close();
    const original = await Deno.readTextFile(`${source}/index.json`);
    const receipt = await handoverMind(source, target, "upgrade_1");
    assert.deepEqual(await handoverMind(source, target, "upgrade_1"), receipt);
    assert.equal(await Deno.readTextFile(`${source}/index.json`), original);
    await newPlugin.load!(candidate.ctx);
    await newPlugin.enable!(candidate.ctx);
    await new Promise((done) => setTimeout(done, 60));
    const poll = await candidate.handlers.poll({ afterSeq: 2 }) as {
      items: { kind: string; args: { text: string } }[];
    };
    assert.deepEqual(
      poll.items.filter((e) => e.kind === "alarm").map((e) => e.args.text).sort(),
      ["保留原条件", "接着原来的事"].sort(),
    );
    await assert.rejects(handoverMind(source, target, "upgrade_1"), KernelError);
  } finally {
    await old.close();
    await candidate.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本账本交接：保留未知动作与完成游标，拒绝目录及祖先符号链接", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-scripts-handover-" });
  try {
    const source = `${directory}/old-ledger`, target = `${directory}/new-ledger`;
    await Deno.mkdir(source);
    const facts = {
      schema: 1,
      jobs: [{
        jobId: "original",
        status: "unknown",
        actions: [{ actionId: "external", status: "unknown" }],
      }],
      completions: [{ seq: 1, job: { jobId: "original", status: "unknown" } }],
    };
    await Deno.writeTextFile(`${source}/index.json`, JSON.stringify(facts));
    const receipt = await handoverScripts(source, target, "upgrade_script_1");
    assert.deepEqual(await handoverScripts(source, target, "upgrade_script_1"), receipt);
    assert.deepEqual(JSON.parse(await Deno.readTextFile(`${target}/index.json`)), facts);
    await Deno.symlink(source, `${directory}/linked-source`);
    await assert.rejects(
      handoverScripts(`${directory}/linked-source`, `${directory}/other`, "symlink"),
      KernelError,
    );
    await Deno.symlink(target, `${directory}/linked-target`);
    await assert.rejects(
      handoverScripts(source, `${directory}/linked-target/new-ledger`, "ancestor"),
      KernelError,
    );
    await assert.rejects(
      handoverScripts(source, `${directory}/linked-target`, "symlink_target"),
      KernelError,
    );
    assert.deepEqual(JSON.parse(await Deno.readTextFile(`${source}/index.json`)), facts);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本账本交接：v2保留事件、对象及迁移原件，检查内层持有者且按同一清单补齐", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-v2-handover-" });
  const source = `${directory}/old`, target = `${directory}/candidate`;
  await Deno.mkdir(`${source}/commits/events`, { recursive: true });
  await Deno.mkdir(`${source}/objects`);
  const files = {
    "index.json": '{"schema":2,"layout":"content-addressed-journal-v1"}\n',
    "legacy-index.json": '{"schema":1,"jobs":[],"completions":[]}\n',
    "commits/events/000000000001.json": '{"fact":"原始回执不可改写"}\n',
    "objects/opaque.json": '{"original":"图片与大型动作原件"}\n',
  };
  for (const [path, value] of Object.entries(files)) {
    await Deno.writeTextFile(`${source}/${path}`, value);
  }
  const lock = await Deno.open(`${source}/commits/writer.lock`, {
    create: true,
    read: true,
    write: true,
  });
  let closed = false;
  try {
    assert.ok(await lock.tryLock(true));
    await assert.rejects(handoverScripts(source, target, "v2"), KernelError);
    lock.close();
    closed = true;
    const receipt = await handoverScripts(source, target, "v2");
    for (const [path, value] of Object.entries(files)) {
      assert.equal(await Deno.readTextFile(`${target}/${path}`), value);
    }
    await assert.rejects(Deno.stat(`${target}/commits/writer.lock`), Deno.errors.NotFound);
    await Deno.remove(`${target}/objects/opaque.json`);
    assert.deepEqual(await handoverScripts(source, target, "v2"), receipt);
    assert.equal(
      await Deno.readTextFile(`${target}/objects/opaque.json`),
      files["objects/opaque.json"],
    );
    await Deno.writeTextFile(`${target}/objects/opaque.json`, "candidate wrote different fact");
    await assert.rejects(handoverScripts(source, target, "v2"), KernelError);
    assert.equal(
      await Deno.readTextFile(`${source}/objects/opaque.json`),
      files["objects/opaque.json"],
    );
  } finally {
    if (!closed) lock.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("上下文：可替换计数器参与触线与完整工具对压缩，默认口径仍明确估算", () => {
  const events = [{
    seq: 1,
    kind: "model_response",
    data: {
      requestId: "m",
      response: {
        content: "counted_large",
        toolCalls: [{ id: "t", name: "execute_script", args: { code: "return 1;" } }],
      },
    },
  }, {
    seq: 2,
    kind: "tool_result",
    data: { callId: "eve_1_0", result: { output: "result" } },
  }] as unknown as import("../plugins/eve/journal.ts").ConsciousEvent[];
  const counter = {
    description: "测试模型适配计数器",
    count(messages: import("../contracts/model.ts").Message[]) {
      return messages.some((message) => message.content.includes("counted_large")) ? 510000 : 100;
    },
  };
  assert.ok(contextEstimate(render(events, "stable"), []) < 500000);
  assert.equal(contextEstimate(render(events, "stable"), [], counter), 510000);
  assert.equal(mechanicalCompression(events, "stable", [], counter)?.throughSeq, 2);
  assert.equal(events.length, 2);
});
