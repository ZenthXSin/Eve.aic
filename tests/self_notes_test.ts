import assert from "node:assert/strict";
import { NoteStore } from "../services/runtime/notes.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
const sources = [{ kind: "experienced", at: "2026-11-13T00:00:00Z", description: "自己的想法" }];
Deno.test("自我便签：2000 token按分词计数，超额不改原文；默认3条原话去重、不含已完成项", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-self-notes-" });
  let counts = 0;
  const count = (text: string) => {
    counts++;
    return Promise.resolve({ tokens: Array.from(text).length, encoding: "test_codepoints" });
  };
  let notes = new NoteStore(directory, undefined, undefined, count);
  try {
    await notes.initialize();
    assert.deepEqual(notes.self(), { aboutMe: null, ideas: [] });
    const original =
      "我喜欢观察\n## 最近想做的事\n- [ ] 了解星星 🌟\n- [ ] 做小工具\n- [x] 已经完成\n## 其他\n这不是待办";
    await notes.change(
      "create",
      { title: "关于我", alias: "关于我", content: original, sources },
      "self-create",
    );
    await notes.change("create", {
      title: "想弄明白的事",
      alias: "想弄明白的事",
      content: "- [ ] 研究分词\n- [ ] 做小工具\n- [ ] 读书\n- [X] 完成的疑问",
      sources,
    }, "curiosity-create");
    const state = notes.self({ idle: true });
    assert.equal(state.aboutMe?.content, original);
    assert.equal(state.ideas.length, 3);
    assert.equal(new Set(state.ideas.map((i) => i.text)).size, 3);
    for (const idea of state.ideas) {
      assert.ok(
        ["- [ ] 了解星星 🌟", "- [ ] 做小工具", "- [ ] 研究分词", "- [ ] 读书"].includes(idea.text),
      );
      assert.equal(idea.revision, 1);
    }
    assert.equal(notes.self().ideas.length, 0);
    notes.self({ idle: true });
    assert.equal(counts, 1);
    await assert.rejects(
      notes.change("update", { id: "关于我", content: "星".repeat(2001) }, "too-big"),
      /2001 token.*2000/,
    );
    assert.equal(notes.get({ id: "关于我" }).text, original);
    assert.equal(notes.journal.events.length, 2);
    await notes.change("update", { id: "关于我", content: "🌟".repeat(2000) }, "exact-limit");
    assert.equal(notes.self().aboutMe?.tokens, 2000);
    await notes.close();
    notes = new NoteStore(directory, undefined, undefined, count);
    await notes.initialize();
    assert.equal(notes.self().aboutMe?.content, "🌟".repeat(2000));
    assert.equal(notes.self({ idle: true, limit: 10 }).ideas.length, 3);
    assert.throws(() => notes.self({ limit: 11 }), /1至10/);
  } finally {
    await notes.close();
    await Deno.remove(directory, { recursive: true });
  }
});
Deno.test("自我便签：回滚排除候选修改与候选兴趣，保留可读取的原版本", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-self-rollback-" }),
    records = new FactJournal(`${directory}/records`);
  const notes = new NoteStore(
    `${directory}/notes`,
    undefined,
    records,
    (text) => Promise.resolve({ tokens: text.length, encoding: "test" }),
  );
  try {
    await records.initialize();
    await notes.initialize();
    await records.append("activation", {});
    await notes.change("create", {
      title: "关于我",
      alias: "关于我",
      content: "最初的偏好",
      sources,
    }, "original");
    await records.append("model_response", {});
    await notes.change(
      "update",
      { id: "关于我", content: "## 最近想做的事\n候选版本的打算" },
      "candidate",
    );
    assert.equal(notes.self({ idle: true }).ideas.length, 1);
    await records.append("upgrade_rollback", { afterSeq: 1, throughSeq: 2 });
    assert.equal(notes.self().aboutMe?.content, "最初的偏好");
    assert.equal(notes.self({ idle: true }).ideas.length, 0);
    assert.equal(notes.get({ id: "关于我", revision: 2 }).note.quarantined, true);
  } finally {
    await notes.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});
