import assert from "node:assert/strict";
import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { RemoteJournal } from "../plugins/eve/remote-journal.ts";
import { type FactEvent, FactJournal } from "../sdk/fact-journal.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

function connection(source: FactJournal) {
  const pages: number[] = [];
  let loseAppendResponse = false;
  let alterAppendResponse = false;
  const appendAttempts: Record<string, Json>[] = [];
  let beforeAppend: (() => Promise<void>) | undefined;
  let appendError: KernelError | undefined;
  let mutatePage: ((items: FactEvent[]) => Promise<FactEvent[]>) | undefined;
  const ctx = {
    async call(contract: string, method: string, input: Record<string, Json>) {
      assert.equal(contract, "timeline");
      if (method === "page") {
        const after = Number(input.afterSeq);
        pages.push(after);
        const page = source.readPage(after);
        return {
          items: mutatePage ? await mutatePage(page.events) : page.events,
          throughSeq: page.nextSeq,
          hasMore: page.hasMore,
        };
      }
      assert.equal(method, "append");
      appendAttempts.push(structuredClone(input));
      await beforeAppend?.();
      if (appendError) throw appendError;
      if (
        input.expectedSeq !== source.count ||
        input.expectedPrevious !== (source.events.at(-1)?.digest ?? "0".repeat(64))
      ) throw new KernelError("rejected", "意识流前件已变化，拒绝旧视图写入");
      const event = await source.append(String(input.kind), input.data);
      if (loseAppendResponse) throw new KernelError("unknown", "远端保存之后连接断开");
      if (alterAppendResponse) {
        const { digest: _digest, ...body } = { ...event, data: "与本次调用无关" };
        return { ...body, digest: await fingerprint(body as unknown as Json) };
      }
      return event;
    },
  } as unknown as PluginContext;
  return {
    ctx,
    pages,
    appendAttempts,
    beforeAppend(fn: typeof beforeAppend) {
      beforeAppend = fn;
    },
    failAppend(error: KernelError) {
      appendError = error;
    },
    loseResponse() {
      loseAppendResponse = true;
    },
    alterResponse() {
      alterAppendResponse = true;
    },
    mutatePage(fn: typeof mutatePage) {
      mutatePage = fn;
    },
  };
}

Deno.test("远端事实镜像：原始时间摘要不变，重启核验尾锚后仅拉增量，提交原件随后导入", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-mirror-" });
  const source = new FactJournal(`${directory}/source`);
  let mirror: RemoteJournal | undefined;
  try {
    await source.initialize();
    for (let i = 0; i < 19; i++) await source.append("notice", { i, text: "长期原文" });
    const first = connection(source);
    mirror = new RemoteJournal(first.ctx, `${directory}/instance`);
    await mirror.initialize();
    assert.deepEqual(first.pages, [0, 8, 16]);
    assert.deepEqual([...mirror.events], [...source.events]);
    mirror.events[0].at = "调用者的修改";
    assert.equal(mirror.events[0].at, source.events[0].at);
    await mirror.close();
    await source.append("notice", { text: "新版本留下的增量" });
    const next = connection(source);
    mirror = new RemoteJournal(next.ctx, `${directory}/instance`);
    await mirror.initialize();
    assert.deepEqual(next.pages, [18]);
    const appended = await mirror.append("notice", { text: "旧脑恢复后自己的下一件事" });
    assert.deepEqual(appended, source.events.at(-1));
    assert.deepEqual(mirror.events.at(-1), appended);
    assert.equal(mirror.events.length, 21);
  } finally {
    await mirror?.close().catch(() => {});
    await source.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("远端事实镜像：远端提交后响应丢失保留未知，重启从原件恢复而不重放", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-unknown-" });
  const source = new FactJournal(`${directory}/source`);
  let mirror: RemoteJournal | undefined;
  try {
    await source.initialize();
    await source.append("notice", "已有记录");
    const first = connection(source);
    mirror = new RemoteJournal(first.ctx, `${directory}/instance`);
    await mirror.initialize();
    first.loseResponse();
    await assert.rejects(mirror.append("notice", "已提交但回执丢失"), { kind: "unknown" });
    assert.equal(mirror.available, false);
    assert.equal(mirror.events.length, 1);
    assert.equal(source.count, 2);
    await assert.rejects(mirror.append("notice", "禁止继续猜测"), KernelError);
    assert.equal(source.count, 2);
    await mirror.close();
    const recovered = connection(source);
    mirror = new RemoteJournal(recovered.ctx, `${directory}/instance`);
    await mirror.initialize();
    assert.equal(mirror.events.length, 2);
    assert.equal(source.count, 2);
    assert.deepEqual(recovered.pages, [0]);
    assert.equal(mirror.events.at(-1)!.data, "已提交但回执丢失");
  } finally {
    await mirror?.close().catch(() => {});
    await source.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("远端事实镜像：远端落后、同序号分叉及无关回执拒绝，镜像原件保留", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-chain-" });
  let source = new FactJournal(`${directory}/source`);
  let mirror: RemoteJournal | undefined;
  try {
    await source.initialize();
    await source.append("notice", "唯一原件");
    mirror = new RemoteJournal(connection(source).ctx, `${directory}/instance`);
    await mirror.initialize();
    await mirror.close();
    const original = await Deno.readTextFile(
      `${directory}/instance/remote-mirror/events/0000000000000001.json`,
    );
    const fork = connection(source);
    fork.mutatePage(async (items) => {
      const { digest: _digest, ...body } = { ...items[0], data: "同序号另一个历史" };
      return [{ ...body, digest: await fingerprint(body as unknown as Json) }];
    });
    mirror = new RemoteJournal(fork.ctx, `${directory}/instance`);
    await assert.rejects(mirror.initialize(), KernelError);
    assert.equal(
      await Deno.readTextFile(`${directory}/instance/remote-mirror/events/0000000000000001.json`),
      original,
    );
    const wrongReceipt = connection(source);
    wrongReceipt.alterResponse();
    mirror = new RemoteJournal(wrongReceipt.ctx, `${directory}/instance`);
    await mirror.initialize();
    await assert.rejects(mirror.append("notice", "本次实际要求"), KernelError);
    assert.equal(mirror.events.length, 1);
    assert.equal(source.count, 2);
    await mirror.close();
    await source.close();
    await Deno.remove(`${directory}/source/events/0000000000000001.json`);
    await Deno.remove(`${directory}/source/events/0000000000000002.json`);
    source = new FactJournal(`${directory}/source`);
    await source.initialize();
    mirror = new RemoteJournal(connection(source).ctx, `${directory}/instance`);
    await assert.rejects(mirror.initialize(), KernelError);
    assert.equal(
      await Deno.readTextFile(`${directory}/instance/remote-mirror/events/0000000000000001.json`),
      original,
    );
  } finally {
    await mirror?.close().catch(() => {});
    await source.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("远端事实镜像：镜像存盘失败不声称提交成功，未完成原件阻止重启且不访问远端", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-storage-" });
  const source = new FactJournal(`${directory}/source`);
  let mirror: RemoteJournal | undefined;
  try {
    await source.initialize();
    const remote = connection(source);
    mirror = new RemoteJournal(remote.ctx, `${directory}/instance`);
    await mirror.initialize();
    const unexpected = `${directory}/instance/remote-mirror/events/0000000000000001.json`;
    await Deno.writeTextFile(unexpected, "意外的原件，禁止覆盖");
    await assert.rejects(mirror.append("notice", "远端已经提交"), { kind: "storage" });
    assert.equal(source.count, 1);
    assert.equal(mirror.available, false);
    assert.equal(await Deno.readTextFile(unexpected), "意外的原件，禁止覆盖");
    await mirror.close().catch(() => {});
    await Deno.remove(unexpected);
    const temporary = `${directory}/instance/remote-mirror/events/unfinished.tmp`;
    await Deno.writeTextFile(temporary, "保留中断现场");
    const blocked = connection(source);
    mirror = new RemoteJournal(blocked.ctx, `${directory}/instance`);
    await assert.rejects(mirror.initialize(), KernelError);
    assert.deepEqual(blocked.pages, []);
    assert.equal(await Deno.readTextFile(temporary), "保留中断现场");
  } finally {
    await mirror?.close().catch(() => {});
    await source.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("远端事实镜像：并发宿主通知后尾锚增量同步再提交，双方原件完整保留", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-concurrent-" });
  const source = new FactJournal(`${directory}/source`);
  let mirror: RemoteJournal | undefined;
  try {
    await source.initialize();
    await source.append("notice", "原有经历");
    const remote = connection(source);
    mirror = new RemoteJournal(remote.ctx, `${directory}/instance`);
    await mirror.initialize();
    let inject = true;
    remote.beforeAppend(async () => {
      if (!inject) return;
      inject = false;
      // An independent host writes after the instance captured its original CAS arguments.
      for (let i = 0; i < 19; i++) await source.append("notice", { from: "host", i });
    });
    const event = await mirror.append("model_failure", { text: "自己的新经历" });
    assert.equal(mirror.available, true);
    assert.equal(event.seq, 21);
    assert.deepEqual([...mirror.events], [...source.events]);
    assert.deepEqual(remote.pages, [0, 0, 8, 16]);
    assert.equal(remote.appendAttempts.length, 2);
    assert.equal(remote.appendAttempts[0].expectedSeq, 1);
    assert.equal(remote.appendAttempts[1].expectedSeq, 20);
    assert.notEqual(remote.appendAttempts[0]._actionId, remote.appendAttempts[1]._actionId);
    assert.equal(source.events.filter((item) => item.kind === "model_failure").length, 1);
    const next = await mirror.append("notice", "后续仍正常");
    assert.equal(next.seq, 22);
  } finally {
    await mirror?.close().catch(() => {});
    await source.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("远端事实镜像：冲突同步后的未知回执不再换ID或重放", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-rebase-unknown-" });
  const source = new FactJournal(`${directory}/source`);
  let mirror: RemoteJournal | undefined;
  try {
    await source.initialize();
    const remote = connection(source);
    mirror = new RemoteJournal(remote.ctx, `${directory}/instance`);
    await mirror.initialize();
    await source.append("notice", "外部已追加");
    remote.loseResponse();
    await assert.rejects(mirror.append("notice", "远端已提交但回执未知"), { kind: "unknown" });
    assert.equal(remote.appendAttempts.length, 2);
    assert.equal(mirror.available, false);
    assert.equal(mirror.events.length, 1);
    assert.equal(source.count, 2);
    await assert.rejects(mirror.append("notice", "禁止重放"), KernelError);
    assert.equal(remote.appendAttempts.length, 2);
  } finally {
    await mirror?.close().catch(() => {});
    await source.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("远端事实镜像：timeout、storage及非CAS拒绝不触发同步或重放", async () => {
  for (const kind of ["timeout", "storage", "unknown", "rejected"] as const) {
    const directory = await Deno.makeTempDir({ prefix: "eve-remote-no-replay-" });
    const source = new FactJournal(`${directory}/source`);
    let mirror: RemoteJournal | undefined;
    try {
      await source.initialize();
      const remote = connection(source);
      mirror = new RemoteJournal(remote.ctx, `${directory}/instance`);
      await mirror.initialize();
      await source.append("notice", "远端已推进也不能把其他失败当CAS");
      remote.failAppend(new KernelError(kind, "不是已确认CAS拒绝"));
      await assert.rejects(mirror.append("notice", "不可猜测"), { kind });
      assert.equal(remote.appendAttempts.length, 1);
      assert.deepEqual(remote.pages, [0]);
      assert.equal(mirror.events.length, 0);
      assert.equal(mirror.available, false);
    } finally {
      await mirror?.close().catch(() => {});
      await source.close().catch(() => {});
      await Deno.remove(directory, { recursive: true });
    }
  }
});

Deno.test("远端事实镜像：冲突期间历史分叉拒绝且原件保留，不提交新动作", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-rebase-corrupt-" });
  const source = new FactJournal(`${directory}/source`);
  let mirror: RemoteJournal | undefined;
  try {
    await source.initialize();
    await source.append("notice", "不可变原件");
    const remote = connection(source);
    mirror = new RemoteJournal(remote.ctx, `${directory}/instance`);
    await mirror.initialize();
    const original = await Deno.readTextFile(
      `${directory}/instance/remote-mirror/events/0000000000000001.json`,
    );
    await source.append("notice", "外部追加");
    remote.mutatePage(async (items) => {
      const { digest: _digest, ...body } = { ...items[0], data: "伪造历史分叉" };
      return [{ ...body, digest: await fingerprint(body as unknown as Json) }, ...items.slice(1)];
    });
    await assert.rejects(mirror.append("notice", "禁止写入分叉"), { kind: "corrupt" });
    assert.equal(remote.appendAttempts.length, 1);
    assert.equal(mirror.available, false);
    assert.equal(source.count, 2);
    assert.equal(
      await Deno.readTextFile(`${directory}/instance/remote-mirror/events/0000000000000001.json`),
      original,
    );
  } finally {
    await mirror?.close().catch(() => {});
    await source.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("远端事实镜像：连续并发CAS拒绝最多三次同步后停止，不无限换ID", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-rebase-bounded-" });
  const source = new FactJournal(`${directory}/source`);
  let mirror: RemoteJournal | undefined;
  try {
    await source.initialize();
    const remote = connection(source);
    mirror = new RemoteJournal(remote.ctx, `${directory}/instance`);
    await mirror.initialize();
    remote.beforeAppend(async () => {
      await source.append("notice", "持续并发写入");
    });
    await assert.rejects(mirror.append("notice", "有界竞争"), { kind: "rejected" });
    assert.equal(remote.appendAttempts.length, 4);
    assert.equal(source.count, 4);
    assert.equal(mirror.events.length, 3);
    assert.equal(mirror.available, false);
    assert.equal(source.events.some((event) => event.data === "有界竞争"), false);
  } finally {
    await mirror?.close().catch(() => {});
    await source.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("远端事实镜像：真实RuntimeEngine并发CAS拒绝留原回执，换尾锚后仅提交一次", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-remote-engine-cas-" });
  const settingsFile = `${directory}/settings.json`;
  await writeDurableJson(settingsFile, { dailyTokenBudget: 1000, paused: false, timeZone: "UTC" });
  const engine = new RuntimeEngine({
    directory: `${directory}/runtime`,
    workspace: `${directory}/workspace`,
    settingsFile,
    maxOutputTokens: 100,
    model: {
      complete() {
        throw new Error("CAS acceptance must not call a model");
      },
      close() {},
    },
  });
  const attempted: string[] = [];
  let inject = true;
  const ctx = {
    async call(namespace: string, method: string, input: Record<string, Json>) {
      const args = structuredClone(input);
      const id = typeof args._actionId === "string"
        ? args._actionId
        : `read_${crypto.randomUUID()}`;
      delete args._actionId;
      if (method === "append") {
        attempted.push(id);
        if (inject) {
          inject = false;
          await engine.dispatch("timeline", "append", {
            expectedSeq: engine.records.count,
            expectedPrevious: engine.records.events.at(-1)?.digest ?? "0".repeat(64),
            kind: "notice",
            data: "开发宿主的真实通知",
          }, "authorized_host_notice");
        }
      }
      return await engine.dispatch(namespace, method, args, id);
    },
  } as unknown as PluginContext;
  const mirror = new RemoteJournal(ctx, `${directory}/instance`);
  try {
    await engine.initialize();
    await mirror.initialize();
    const appended = await mirror.append("model_failure", "本实例后续经历");
    assert.equal(appended.seq, 2);
    assert.equal(mirror.available, true);
    assert.equal(engine.available, true);
    assert.deepEqual([...mirror.events], [...engine.records.events]);
    assert.equal(attempted.length, 2);
    assert.notEqual(attempted[0], attempted[1]);
    const rejected = engine.receipt(attempted[0]);
    assert.equal(rejected?.error?.kind, "rejected");
    assert.equal(rejected?.error?.message, "意识流前件已变化，拒绝旧视图写入");
    assert.deepEqual(engine.receipt(attempted[1])?.value, appended);
    // The original rejected action stays terminal even after the new action succeeded.
    assert.equal(engine.receipt(attempted[0])?.error?.kind, "rejected");
    assert.equal(engine.records.count, 2);
  } finally {
    await mirror.close().catch(() => {});
    await engine.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});
