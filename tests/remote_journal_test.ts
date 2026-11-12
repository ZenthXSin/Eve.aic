import assert from "node:assert/strict";
import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { RemoteJournal } from "../plugins/eve/remote-journal.ts";
import { type FactEvent, FactJournal } from "../sdk/fact-journal.ts";
import type { PluginContext } from "../sdk/plugin.ts";

function connection(source: FactJournal) {
  const pages: number[] = [];
  let loseAppendResponse = false;
  let alterAppendResponse = false;
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
      assert.equal(input.expectedSeq, source.count);
      assert.equal(input.expectedPrevious, source.events.at(-1)?.digest ?? "0".repeat(64));
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
