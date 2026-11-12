import assert from "node:assert/strict";
import { KernelError } from "../kernel/protocol.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import { MemoryStore } from "../services/runtime/memory.ts";
import { NoteStore } from "../services/runtime/notes.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

for (const mode of ["lexical", "semantic"] as const) {
  Deno.test(`回滚${mode}召回隔离：候选来源、引用旧来源的新形成及候选强化均不混入已接受记忆`, async () => {
    const directory = await Deno.makeTempDir({ prefix: "eve-memory-quarantine-" });
    let records = new FactJournal(`${directory}/records`);
    const embed = mode === "semantic" ? () => Promise.resolve([1, 0]) : undefined;
    let memory = new MemoryStore(`${directory}/memory`, records, embed);
    try {
      await records.initialize();
      await memory.initialize();
      const accepted = await records.append("activation", { text: "已接受的garden" });
      const good = await memory.remember({
        text: "我记得已接受的garden。",
        entities: ["garden"],
        sources: [{ recordSeq: accepted.seq, digest: accepted.digest, kind: "experienced" }],
      }, "good");
      await memory.used({ ids: [good.id] });
      assert.equal(memory.get({ id: good.id }).strength, 3);
      const candidate = await records.append("activation", { text: "候选garden" });
      const bad = await memory.remember({
        text: "我把候选garden当作自己的经历。",
        entities: ["garden"],
        sources: [{ recordSeq: candidate.seq, digest: candidate.digest, kind: "experienced" }],
      }, "candidate");
      const oldSource = await memory.remember({
        text: "我在候选期间引用旧garden来源形成新说法。",
        entities: ["garden"],
        sources: [{ recordSeq: accepted.seq, digest: accepted.digest, kind: "heard" }],
      }, "candidate-old-source");
      await memory.used({ ids: [good.id] });
      assert.equal((await memory.recall({ query: "garden", limit: 8 })).items.length, 3);
      assert.equal(memory.get({ id: good.id }).strength, 5.1);
      const realEffect = await records.append("tool_result", {
        actualEffect: "原始动作回执仍保留",
      });
      const through = await memory.remember({
        text: "我记住garden候选末端的动作。",
        entities: ["garden"],
        sources: [{ recordSeq: realEffect.seq, digest: realEffect.digest, kind: "experienced" }],
      }, "candidate-through");
      await records.append("upgrade_rollback", {
        afterSeq: accepted.seq,
        throughSeq: realEffect.seq,
      });
      assert.equal(memory.get({ id: good.id }).strength, 3);
      for (const id of [bad.id, oldSource.id, through.id]) {
        assert.equal(memory.get({ id }).quarantined, true);
        assert.ok(memory.get({ id }).unconfirmedRecordSeqs!.some((seq) => seq > accepted.seq));
      }
      const recalled = await memory.recall({ query: "garden", limit: 8 });
      assert.equal(recalled.retrieval, mode);
      assert.deepEqual(recalled.items.map((item) => item.id), [good.id]);
      assert.equal(memory.list().items.length, 4);
      await assert.rejects(() => memory.used({ ids: [bad.id] }), KernelError);
      await assert.rejects(() =>
        memory.remember({
          text: "我不能直接把未确认来源重建成已接受的记忆。",
          sources: [{ recordSeq: candidate.seq, digest: candidate.digest, kind: "experienced" }],
        }, "reuse-unconfirmed"), KernelError);
      const before = memory.get({ id: bad.id }).strength;
      await memory.observe("garden");
      assert.equal(memory.get({ id: bad.id }).strength, before);
      assert.equal(records.events[realEffect.seq - 1].digest, realEffect.digest);
      assert.equal(memory.journal.events.filter((event) => event.kind === "formation").length, 4);
      await memory.close();
      await records.close();
      records = new FactJournal(`${directory}/records`);
      memory = new MemoryStore(`${directory}/memory`, records, embed);
      await records.initialize();
      await memory.initialize();
      assert.equal(memory.get({ id: oldSource.id }).quarantined, true);
      assert.deepEqual(
        (await memory.recall({ query: "garden", limit: 8 })).items.map((item) => item.id),
        [good.id],
      );
      assert.equal(
        (memory.journal.events.find((event) =>
          event.kind === "formation" &&
          (event.data as { id: string }).id === bad.id
        )!.data as { text: string }).text,
        bad.text,
      );
    } finally {
      await memory.close();
      await records.close();
      await Deno.remove(directory, { recursive: true });
    }
  });
}

Deno.test("便签回滚保留候选原件：默认恢复旧版本，后续修改基于旧前件且不继承候选字段", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-note-quarantine-" });
  let records = new FactJournal(`${directory}/records`);
  let notes = new NoteStore(`${directory}/notes`, undefined, records);
  try {
    await records.initialize();
    await notes.initialize();
    const accepted = await records.append("activation", { owner: "accepted" });
    const sources = [{
      kind: "experienced",
      at: new Date().toISOString(),
      description: "已接受来源",
    }];
    const original = await notes.change("create", {
      title: "accepted",
      content: "safe original",
      group: "garden",
      summary: "safe summary",
      sources,
    }, "accepted-note");
    const candidate = await records.append("activation", { owner: "candidate" });
    await notes.change("update", {
      id: original.id,
      expectedRevision: 1,
      title: "candidate",
      group: "candidate-group",
      content: "poison edit",
      summary: "poison summary",
    }, "candidate-edit");
    const onlyCandidate = await notes.change("create", {
      title: "candidate-only",
      content: "poison new",
      sources,
    }, "candidate-only");
    assert.equal(notes.get({ id: original.id }).note.revision, 2);
    await records.append("upgrade_rollback", { afterSeq: accepted.seq, throughSeq: candidate.seq });
    assert.equal(notes.get({ id: original.id }).text, "safe original");
    assert.equal(notes.get({ id: original.id }).note.title, "accepted");
    assert.equal(notes.get({ id: original.id }).note.revision, 1);
    assert.throws(() => notes.get({ id: onlyCandidate.id }), KernelError);
    assert.equal(notes.list().items.length, 1);
    assert.equal(notes.list({ includeUnconfirmed: true }).items.length, 2);
    assert.equal(notes.get({ id: original.id, revision: 2 }).note.quarantined, true);
    assert.equal(
      notes.get({ id: onlyCandidate.id, includeUnconfirmed: true }).note.quarantined,
      true,
    );
    assert.equal(notes.versions({ id: original.id }).items[1].quarantined, true);
    assert.equal((await notes.search({ query: "poison" })).items.length, 0);
    assert.equal((await notes.search({ query: "poison", mode: "regex" })).items.length, 0);
    const archived = await notes.search({ query: "poison", includeUnconfirmed: true });
    assert.equal(archived.items.length, 2);
    assert.ok(archived.items.every((note) => note.quarantined));
    const resumed = await notes.change("update", {
      id: original.id,
      expectedRevision: 1,
      content: "accepted revision",
    }, "resumed-edit");
    assert.equal(resumed.revision, 3);
    assert.equal(resumed.basedOnRevision, 1);
    assert.equal(resumed.title, "accepted");
    assert.equal(resumed.group, "garden");
    assert.equal(resumed.summary, "safe summary");
    assert.deepEqual(resumed.sources, sources);
    assert.equal(notes.get({ id: original.id, revision: 2 }).text, "poison edit");
    await notes.close();
    await records.close();
    records = new FactJournal(`${directory}/records`);
    notes = new NoteStore(`${directory}/notes`, undefined, records);
    await records.initialize();
    await notes.initialize();
    assert.equal(notes.get({ id: original.id }).note.revision, 3);
    assert.equal(notes.get({ id: original.id }).note.quarantined, undefined);
    assert.equal(notes.get({ id: original.id, revision: 2 }).note.quarantined, true);
    assert.equal(notes.journal.events.length, 4);
  } finally {
    await notes.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("召回等待嵌入期间新增回滚标记，结果返回前再次排除候选记忆", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-recall-rollback-race-" });
  const records = new FactJournal(`${directory}/records`);
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => release = resolve);
  const memory = new MemoryStore(`${directory}/memory`, records, async (_text, purpose) => {
    if (purpose === "query") await blocked;
    return [1, 0];
  });
  try {
    await records.initialize();
    await memory.initialize();
    const candidate = await records.append("activation", {});
    await memory.remember({
      text: "我在候选里形成了记忆。",
      sources: [
        { recordSeq: candidate.seq, digest: candidate.digest, kind: "experienced" },
      ],
    }, "racing-memory");
    const recalled = memory.recall({ query: "候选" });
    await records.append("upgrade_rollback", { afterSeq: 0, throughSeq: candidate.seq });
    release();
    assert.deepEqual((await recalled).items, []);
    assert.equal(memory.journal.events.filter((event) => event.kind === "recalled").length, 0);
  } finally {
    release();
    await memory.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("便签异步语义搜索期间回滚，拒绝返回过期候选结果并保留标记原件", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-note-rollback-race-" });
  const records = new FactJournal(`${directory}/records`);
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => release = resolve);
  const notes = new NoteStore(`${directory}/notes`, async (_text, purpose) => {
    if (purpose === "query") await blocked;
    return [1, 0];
  }, records);
  try {
    await records.initialize();
    await notes.initialize();
    const candidate = await records.append("activation", {});
    const created = await notes.change("create", {
      title: "candidate",
      content: "候选便签",
      sources: [
        { kind: "experienced", at: new Date().toISOString(), description: "候选" },
      ],
    }, "candidate-note");
    const search = notes.search({ query: "候选", mode: "semantic" });
    await records.append("upgrade_rollback", { afterSeq: 0, throughSeq: candidate.seq });
    release();
    await assert.rejects(() => search, (e) => e instanceof KernelError && e.kind === "rejected");
    assert.deepEqual(notes.list().items, []);
    assert.equal(notes.get({ id: created.id, revision: 1 }).note.quarantined, true);
  } finally {
    release();
    await notes.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("实例不能伪造监督回滚范围来改变记忆和便签接受状态", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-rollback-authority-" });
  const settingsFile = `${directory}/settings.json`;
  await writeDurableJson(settingsFile, { dailyTokenBudget: 10000, paused: false, timeZone: "UTC" });
  const engine = new RuntimeEngine({
    directory: `${directory}/data`,
    workspace: `${directory}/workspace`,
    settingsFile,
    maxOutputTokens: 100,
    model: {
      complete() {
        throw new Error("不应调用模型");
      },
      close() {},
    },
  });
  try {
    await engine.initialize();
    await assert.rejects(() =>
      engine.dispatch("timeline", "append", {
        expectedSeq: 0,
        expectedPrevious: "0".repeat(64),
        kind: "upgrade_rollback",
        data: { afterSeq: 0, throughSeq: 0 },
      }, "forged-rollback"), (e) => e instanceof KernelError && e.kind === "rejected");
    assert.equal(engine.records.events.length, 0);
  } finally {
    await engine.close();
    await Deno.remove(directory, { recursive: true });
  }
});
