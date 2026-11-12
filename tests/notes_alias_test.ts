import assert from "node:assert/strict";
import type { NoteRevision } from "../contracts/notes.ts";
import { KernelError } from "../kernel/protocol.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import { NoteStore } from "../services/runtime/notes.ts";

const sources = [{
  kind: "experienced",
  at: "2026-11-12T00:00:00Z",
  description: "本地便签功能测试",
}];
const createArgs = { title: "便签", content: "原文", sources };

Deno.test("便签短名：设置、读取、改名、移除与重启保留原版本和回执", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-note-alias-" });
  let notes = new NoteStore(directory);
  try {
    await notes.initialize();
    const created = await notes.change("create", { ...createArgs, alias: "日常" }, "create");
    assert.equal(notes.get({ id: "日常" }).note.id, created.id);
    const updated = await notes.change("update", { id: "日常", content: "新版" }, "edit");
    assert.equal(updated.revision, 2);
    assert.equal(updated.basedOnRevision, 1);
    assert.equal(notes.get({ id: created.id, revision: 1 }).text, "原文");
    const renamed = await notes.change("update", { id: "日常", alias: "随手记" }, "rename");
    assert.equal(renamed.alias, "随手记");
    assert.throws(() => notes.get({ id: "日常" }), KernelError);
    assert.equal(notes.get({ id: "随手记" }).text, "新版");
    assert.equal(notes.list().items[0].alias, "随手记");
    assert.equal((await notes.search({ query: "随手记" })).items[0].alias, "随手记");
    assert.deepEqual(notes.versions({ id: "随手记" }).items.map((n) => n.alias), [
      "日常",
      "日常",
      "随手记",
    ]);
    await notes.close();
    notes = new NoteStore(directory);
    await notes.initialize();
    assert.equal(notes.get({ id: "随手记" }).note.id, created.id);
    // 原动作重查返回原回执，不因短名已改而重新解析/重写。
    assert.deepEqual(
      await notes.change("update", { id: "日常", alias: "随手记" }, "rename"),
      renamed,
    );
    const removed = await notes.change("update", { id: "随手记", alias: null }, "remove");
    assert.equal(removed.alias, undefined);
    assert.equal(notes.get({ id: created.id }).note.revision, 4);
    assert.throws(() => notes.get({ id: "随手记" }), KernelError);
    assert.equal(notes.journal.events.length, 4);
  } finally {
    await notes.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("便签短名：唯一、不能冒充永久 ID、无效短名不写入，移除后可复用", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-note-alias-validation-" });
  const notes = new NoteStore(directory);
  try {
    await notes.initialize();
    const first = await notes.change("create", { ...createArgs, alias: "常用" }, "first");
    for (
      const [index, alias] of [
        "常用",
        first.id,
        "A".repeat(64),
        "",
        "空 格",
        "换\n行",
        "a/b",
        "a\\b",
        "零\u200b宽",
        "长".repeat(65),
      ].entries()
    ) {
      await assert.rejects(
        () => notes.change("create", { ...createArgs, alias }, `invalid_${index}`),
        KernelError,
      );
    }
    assert.equal(notes.journal.events.length, 1);
    await notes.change("update", { id: first.id, alias: null }, "clear");
    const second = await notes.change("create", { ...createArgs, alias: "常用" }, "second");
    assert.equal(notes.get({ id: "常用" }).note.id, second.id);
    await assert.rejects(
      () => notes.change("update", { id: first.id, alias: "常用" }, "reclaim"),
      KernelError,
    );
    assert.equal(notes.get({ id: first.id }).note.alias, undefined);
  } finally {
    await notes.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("便签省略版本仍 CAS：并发修改、短名改动与旧显式前件不能覆盖", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-note-alias-cas-" });
  const notes = new NoteStore(directory);
  try {
    await notes.initialize();
    const original = await notes.change("create", { ...createArgs, alias: "常用" }, "create");
    const edits = await Promise.allSettled([
      notes.change("update", { id: "常用", alias: "新名字", content: "第一条" }, "edit_1"),
      notes.change("update", { id: "常用", content: "第二条" }, "edit_2"),
    ]);
    assert.equal(edits[0].status, "fulfilled");
    assert.equal(edits[1].status, "rejected");
    assert.equal(notes.get({ id: "新名字" }).text, "第一条");
    assert.equal(notes.journal.events.length, 2);
    await assert.rejects(
      () =>
        notes.change("update", { id: original.id, expectedRevision: 1, content: "过期" }, "stale"),
      KernelError,
    );
    const compatible = await notes.change("update", {
      id: original.id,
      expectedRevision: 2,
      content: "显式版本兼容",
    }, "explicit");
    assert.equal(compatible.revision, 3);
  } finally {
    await notes.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("便签短名回滚：恢复已接受的名字，候选原件可查，后续编辑不继承候选字段", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-note-alias-rollback-" });
  const records = new FactJournal(`${directory}/records`);
  const notes = new NoteStore(`${directory}/notes`, undefined, records);
  try {
    await records.initialize();
    await notes.initialize();
    const accepted = await records.append("activation", { owner: "accepted" });
    const original = await notes.change("create", { ...createArgs, alias: "已接受" }, "create");
    const candidate = await records.append("activation", { owner: "candidate" });
    await notes.change("update", { id: "已接受", alias: "候选名字", content: "候选" }, "candidate");
    await records.append("upgrade_rollback", { afterSeq: accepted.seq, throughSeq: candidate.seq });
    assert.equal(notes.get({ id: "已接受" }).text, "原文");
    assert.throws(() => notes.get({ id: "候选名字" }), KernelError);
    assert.equal(notes.get({ id: original.id, revision: 2 }).note.quarantined, true);
    const next = await notes.change("update", { id: "已接受", content: "恢复后的修改" }, "resumed");
    assert.equal(next.revision, 3);
    assert.equal(next.basedOnRevision, 1);
    assert.equal(next.alias, "已接受");
  } finally {
    await notes.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("便签短名歧义：回滚复活旧名时拒绝猜测目标，可用永久 ID 修复", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-note-alias-conflict-" });
  const records = new FactJournal(`${directory}/records`);
  let notes = new NoteStore(`${directory}/notes`, undefined, records);
  try {
    await records.initialize();
    await notes.initialize();
    const accepted = await records.append("activation", { owner: "accepted" });
    const first = await notes.change("create", { ...createArgs, alias: "冲突名" }, "first");
    const candidate = await records.append("activation", { owner: "candidate" });
    await notes.change("update", { id: first.id, alias: null }, "candidate-clear");
    await records.append("activation", { owner: "accepted-again" });
    const second = await notes.change("create", { ...createArgs, alias: "冲突名" }, "second");
    await records.append("upgrade_rollback", { afterSeq: accepted.seq, throughSeq: candidate.seq });
    await notes.close();
    notes = new NoteStore(`${directory}/notes`, undefined, records);
    await notes.initialize();
    assert.throws(() => notes.get({ id: "冲突名" }), /冲突/);
    await assert.rejects(
      () => notes.change("update", { id: "冲突名", content: "不能猜" }, "guess"),
      /冲突/,
    );
    await notes.change("update", { id: first.id, alias: null }, "repair");
    assert.equal(notes.get({ id: "冲突名" }).note.id, second.id);
    assert.equal(notes.get({ id: first.id }).note.alias, undefined);
    assert.equal((notes.journal.events[0].data as unknown as NoteRevision).alias, "冲突名");
  } finally {
    await notes.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});
