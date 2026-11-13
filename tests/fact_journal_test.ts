import assert from "node:assert/strict";
import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { FactJournal } from "../sdk/fact-journal.ts";

Deno.test("事实日志：8MiB原图编码可保存、独占分页并原样恢复及镜像导入", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-fact-image-limit-" });
  let journal = new FactJournal(`${root}/source`);
  const mirror = new FactJournal(`${root}/mirror`);
  try {
    await journal.initialize();
    // Exact base64 length of the largest accepted binary image, including padding.
    const data = "A".repeat(Math.ceil(8_388_608 / 3) * 4 - 1) + "=";
    const original = await journal.append("notice", {
      source: "image",
      image: { mimeType: "image/png", data },
    });
    await journal.append("notice", { source: "next" });
    const page = journal.readPage(0, 8);
    assert.equal(page.events.length, 1);
    assert.equal(page.nextSeq, 1);
    assert.equal(page.hasMore, true);
    await journal.close();
    journal = new FactJournal(`${root}/source`);
    await journal.initialize();
    assert.equal(journal.events[0].digest, original.digest);
    await mirror.initialize();
    await mirror.importOriginal(journal.events[0]);
    assert.equal((mirror.events[0].data as { image: { data: string } }).image.data, data);
    await assert.rejects(journal.append("notice", "x".repeat(12_582_912)), KernelError);
    assert.equal(journal.count, 2);
    assert.equal(journal.available, true);
  } finally {
    await journal.close().catch(() => {});
    await mirror.close().catch(() => {});
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("事实日志：惰性数组兼容读取、原件不随调用者修改、分页固定上界", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-fact-pages-" });
  let journal = new FactJournal(directory);
  try {
    await journal.initialize();
    const writes = Array.from(
      { length: 23 },
      (_, i) => journal.append("notice", { text: `原件${i}` }),
    );
    const saved = await Promise.all(writes);
    assert.equal(journal.count, 23);
    assert.equal(journal.events.length, 23);
    assert.equal(Object.getOwnPropertyDescriptor(journal.events, "length")!.value, 23);
    assert.equal(Array.isArray(journal.events), true);
    assert.deepEqual(journal.events.slice(3, 5), saved.slice(3, 5));
    assert.equal(journal.events.at(-1)!.seq, 23);
    assert.equal(journal.events.find((e) => e.seq === 9)!.seq, 9);
    assert.deepEqual(journal.events.filter((e) => e.seq % 10 === 0).map((e) => e.seq), [10, 20]);
    assert.equal([...journal.events].length, 23);
    assert.equal(Object.keys(journal.events).length, 23);
    assert.deepEqual(JSON.parse(JSON.stringify(journal.events)), saved);
    journal.events[0].data = { text: "被调用者改写" };
    assert.deepEqual(journal.events[0], saved[0]);
    assert.throws(() => journal.events.push(saved[0]), KernelError);
    assert.throws(() => {
      journal.events[0] = saved[0];
    }, KernelError);
    const snapshot = journal.readPage();
    await journal.append("notice", { text: "下一份快照" });
    const middle = journal.readPage(snapshot.nextSeq, 8, snapshot.throughSeq);
    const end = journal.readPage(middle.nextSeq, 8, snapshot.throughSeq);
    assert.deepEqual([...snapshot.events, ...middle.events, ...end.events], saved);
    assert.equal(end.hasMore, false);
    assert.equal(end.throughSeq, 23);
    for (const args of [[-1, 8], [0, 0], [0, 129], [25, 8], [0, 8, 25], [2, 8, 1]]) {
      assert.throws(() => journal.readPage(...args as [number, number, number?]), KernelError);
    }
    await journal.close();
    journal = new FactJournal(directory);
    await journal.initialize();
    assert.equal(journal.count, 24);
    assert.deepEqual(journal.events[0], saved[0]);
    assert.equal((await journal.append("notice", null)).seq, 25);
  } finally {
    await journal.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("事实日志：按需读取检测启动后改写，即使重新计算摘要也保留并拒绝提交", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-fact-replaced-" });
  const journal = new FactJournal(directory);
  try {
    await journal.initialize();
    const event = await journal.append("notice", { text: "原件" });
    const { digest: _old, ...body } = { ...event, data: { text: "替换原件" } };
    const tampered = { ...body, digest: await fingerprint(body as unknown as Json) };
    const path = `${directory}/events/0000000000000001.json`;
    await Deno.writeTextFile(path, JSON.stringify(tampered));
    assert.throws(() => journal.readPage(), KernelError);
    assert.equal(journal.available, false);
    await assert.rejects(journal.append("notice", null), KernelError);
    assert.deepEqual(JSON.parse(await Deno.readTextFile(path)), tampered);
  } finally {
    await journal.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("事实日志：单写锁、空洞、临时文件与符号链接都拒绝恢复且保留原件", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-fact-invalid-" });
  const journal = new FactJournal(directory);
  try {
    await journal.initialize();
    await journal.append("notice", "第一条");
    await journal.append("notice", "第二条");
    const competing = new FactJournal(directory);
    await assert.rejects(competing.initialize(), KernelError);
    assert.equal(journal.available, true);
    await journal.close();
    const path = `${directory}/events/0000000000000001.json`;
    const first = await Deno.readTextFile(path);
    await Deno.remove(path);
    await assert.rejects(new FactJournal(directory).initialize(), KernelError);
    assert.ok(await Deno.stat(`${directory}/events/0000000000000002.json`));
    await Deno.writeTextFile(path, first);
    const temporary = `${directory}/events/interrupted.tmp`;
    await Deno.writeTextFile(temporary, "未完成原件");
    await assert.rejects(new FactJournal(directory).initialize(), KernelError);
    assert.equal(await Deno.readTextFile(temporary), "未完成原件");
    await Deno.remove(temporary);
    await Deno.rename(path, `${directory}/original.json`);
    await Deno.symlink(`${directory}/original.json`, path);
    await assert.rejects(new FactJournal(directory).initialize(), KernelError);
    assert.equal(await Deno.readTextFile(`${directory}/original.json`), first);
  } finally {
    await journal.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("事实日志：长正文恢复后按页读取，关闭期间已接收提交完成，后续提交拒绝", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-fact-long-" });
  let journal = new FactJournal(directory);
  try {
    await journal.initialize();
    const text = "长期原文🌙".repeat(16_384);
    for (let i = 0; i < 80; i++) await journal.append("notice", { text, i });
    const pending = journal.append("last", { text: "关闭前已请求" });
    const closing = journal.close();
    assert.equal((await pending).seq, 81);
    await closing;
    await assert.rejects(journal.append("after-close", null), KernelError);
    journal = new FactJournal(directory);
    await journal.initialize();
    const bounded = journal.readPage(0, 128);
    assert.ok(bounded.events.length > 1 && bounded.events.length < 81);
    assert.equal(bounded.hasMore, true);
    assert.equal(bounded.nextSeq, bounded.events.length);
    const page = journal.readPage(40, 2);
    assert.deepEqual(page.events.map((event) => (event.data as { i: number }).i), [40, 41]);
    assert.equal((page.events[0].data as { text: string }).text, text);
    assert.equal(page.nextSeq, 42);
    assert.equal(page.hasMore, true);
    assert.equal(journal.events.at(-1)!.kind, "last");
  } finally {
    await journal.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("事实日志：运行中意外出现的目标原件禁止覆盖，并发初始化和关闭释放单写锁", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-fact-races-" });
  const journal = new FactJournal(directory);
  try {
    const initializing = journal.initialize();
    await assert.rejects(journal.initialize(), KernelError);
    const closing = journal.close();
    await initializing;
    await closing;
    const recovered = new FactJournal(directory);
    try {
      await recovered.initialize();
      const path = `${directory}/events/0000000000000001.json`;
      await Deno.writeTextFile(path, "意外出现的唯一原件");
      await assert.rejects(recovered.append("notice", "新数据"), KernelError);
      assert.equal(await Deno.readTextFile(path), "意外出现的唯一原件");
      assert.equal(recovered.available, false);
    } finally {
      await recovered.close().catch(() => {});
    }
  } finally {
    await journal.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("事实日志：导入原件保留历史序号时间摘要，重复幂等且冲突拒绝", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-fact-import-" });
  const journal = new FactJournal(directory);
  try {
    await journal.initialize();
    const body = {
      schema: 1 as const,
      seq: 1,
      at: "2020-01-02T03:04:05.000Z",
      kind: "notice",
      data: { text: "原始时间也属于事实" },
      previous: "0".repeat(64),
    };
    const event = { ...body, digest: await fingerprint(body as unknown as Json) };
    assert.deepEqual(await journal.importOriginal(event), event);
    assert.deepEqual(await journal.importOriginal(event), event);
    assert.equal(journal.count, 1);
    assert.deepEqual(journal.events[0], event);
    const next = await journal.append("notice", "后来追加");
    assert.equal(next.previous, event.digest);
    const forkBody = { ...body, at: "2021-01-02T03:04:05.000Z" };
    const fork = { ...forkBody, digest: await fingerprint(forkBody as unknown as Json) };
    await assert.rejects(journal.importOriginal(fork), KernelError);
    assert.equal(journal.available, false);
    assert.deepEqual(
      JSON.parse(await Deno.readTextFile(`${directory}/events/0000000000000001.json`)),
      event,
    );
  } finally {
    await journal.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("事实日志：同步投影缓存不共享可变正文，退出后重验原件，空返回只执行一次", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-fact-scope-" });
  const journal = new FactJournal(directory);
  try {
    await journal.initialize();
    await journal.append("notice", { text: "原件" });
    let called = 0;
    const { withFactReadCache } = await import("../sdk/fact-journal.ts");
    withFactReadCache(journal.events, () => {
      called++;
      journal.events[0].data = "临时修改";
      assert.deepEqual(journal.events[0].data, { text: "原件" });
      Deno.writeTextFileSync(`${directory}/events/0000000000000001.json`, "{}");
      assert.deepEqual(journal.events[0].data, { text: "原件" });
    });
    assert.equal(called, 1);
    assert.throws(() => journal.events[0], KernelError);
    assert.equal(journal.available, false);
  } finally {
    await journal.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});
