import assert from "node:assert/strict";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { KernelError } from "../kernel/protocol.ts";

Deno.test("手机阅读状态：并发与迟到旧屏幕不回退，真实展示编号保留空隙", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-phone-reading-" });
  const reading = new QqPhoneReading(directory);
  try {
    await reading.initialize();
    await Promise.all([
      reading.opened("qq:user:a", 12, [3, 5, 9]),
      reading.opened("qq:user:b", 14, [2, 7, 14]),
      reading.opened("qq:user:a", 16, [3, 5, 9, 15]),
      reading.opened("qq:user:a", 12, [3, 5, 9]),
    ]);
    assert.deepEqual(reading.get("qq:user:a"), {
      notificationThroughSeq: 15,
      lastOpened: { snapshotThroughSeq: 16, displayedSeqs: [3, 5, 9, 15] },
    });
    assert.equal(reading.get("qq:user:b")!.notificationThroughSeq, 14);
    const copy = reading.get("qq:user:a")!;
    copy.lastOpened.displayedSeqs.push(16);
    assert.deepEqual(reading.get("qq:user:a")!.lastOpened.displayedSeqs, [3, 5, 9, 15]);
    const second = new QqPhoneReading(directory);
    await assert.rejects(
      () => second.initialize(),
      (e) => e instanceof KernelError && e.kind === "unavailable",
    );
  } finally {
    await reading.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("手机阅读状态：损坏、重复会话、未完成写入和链接保留现场并拒绝启动", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-phone-reading-corrupt-" });
  const entry = {
    conversationId: "qq:user:a",
    notificationThroughSeq: 3,
    lastOpened: { snapshotThroughSeq: 5, displayedSeqs: [1, 3] },
  };
  try {
    const cases = [
      "{broken",
      JSON.stringify({ schema: 1, entries: [entry, entry] }),
      JSON.stringify({ schema: 1, entries: [{ ...entry, notificationThroughSeq: 4 }] }),
      JSON.stringify({
        schema: 1,
        entries: [{ ...entry, lastOpened: { snapshotThroughSeq: 2, displayedSeqs: [1, 3] } }],
      }),
    ];
    for (const [index, content] of cases.entries()) {
      const directory = `${root}/${index}`;
      await Deno.mkdir(directory);
      await Deno.writeTextFile(`${directory}/index.json`, content);
      const reading = new QqPhoneReading(directory);
      await assert.rejects(() => reading.initialize(), KernelError);
      assert.equal(await Deno.readTextFile(`${directory}/index.json`), content);
    }
    for (const kind of ["temporary", "symlink"]) {
      const directory = `${root}/${kind}`;
      await Deno.mkdir(directory);
      if (kind === "temporary") {
        await Deno.writeTextFile(`${directory}/index.json.unfinished.tmp`, "incomplete");
      } else await Deno.symlink(`${root}/0/index.json`, `${directory}/index.json`);
      await assert.rejects(() => new QqPhoneReading(directory).initialize(), KernelError);
      assert.equal(await Deno.readTextFile(`${root}/0/index.json`), cases[0]);
    }
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("手机阅读状态：恢复水位须对应原会话事实，不能因日志缺失静默清零", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-phone-reading-restore-" });
  let reading = new QqPhoneReading(directory);
  try {
    await reading.initialize();
    await reading.opened("qq:user:a", 4, [1, 3]);
    await reading.close();
    reading = new QqPhoneReading(directory);
    await reading.initialize();
    const original = await Deno.readTextFile(`${directory}/index.json`);
    assert.throws(() =>
      reading.validateSnapshot([
        { message: { seq: 1, conversationId: "qq:user:a" } },
        { message: { seq: 3, conversationId: "qq:user:b" } },
        { message: { seq: 4, conversationId: "qq:user:b" } },
      ]), (e: unknown) => e instanceof KernelError && e.kind === "corrupt");
    assert.equal(reading.available, false);
    assert.equal(await Deno.readTextFile(`${directory}/index.json`), original);
    await assert.rejects(() => reading.close(), KernelError);
  } finally {
    await reading.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});
