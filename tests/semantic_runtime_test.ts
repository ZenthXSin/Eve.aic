import assert from "node:assert/strict";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import type { SearchHit, SearchStatus } from "../contracts/search.ts";

Deno.test("semantic results revalidate current notes and rolled back history without changing originals", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-semantic-runtime-" });
  const hits: SearchHit[] = [];
  const index = {
    model: "fixture",
    revision: "v1",
    dimensions: 384,
    collections: {},
    chunks: 2,
    discoveryStarted: true,
    errors: 0,
    lastError: null,
  } as SearchStatus;
  const engine = new RuntimeEngine({
    directory,
    workspace: directory,
    settingsFile: `${directory}/settings.json`,
    maxOutputTokens: 100,
    model: { complete: () => Promise.resolve({ content: "ok", toolCalls: [] }), close() {} },
    searchIndex: {
      status: () => Promise.resolve(index),
      search: () => Promise.resolve({ items: structuredClone(hits), retrieval: "hybrid", index }),
    },
  });
  await Deno.writeTextFile(
    `${directory}/settings.json`,
    JSON.stringify({ dailyTokenBudget: 1000000, paused: false, timeZone: "UTC" }),
  );
  try {
    await engine.initialize();
    const first = await engine.records.append("thought", { text: "accepted" });
    const second = await engine.records.append("thought", { text: "candidate" });
    const note = await engine.notes.change("create", {
      title: "budget",
      content: "old",
      sources: [{ kind: "experienced", at: first.at, description: "owner" }],
    }, "note-create");
    await engine.notes.change("update", { id: note.id, content: "new" }, "note-update");
    const hit = (source: SearchHit["source"]): SearchHit => ({
      source,
      at: first.at,
      kind: "thought",
      title: "test",
      excerpt: "text",
      startChar: 0,
      endChar: 4,
      score: 1,
      semanticScore: 0.9,
      untrusted: true,
    });
    hits.push(
      hit({ collection: "notes", id: note.id, revision: 1, seq: 1 }),
      hit({ collection: "notes", id: note.id, revision: 2, seq: 2 }),
      hit({ collection: "records", seq: first.seq, digest: first.digest }),
      hit({ collection: "records", seq: second.seq, digest: second.digest }),
    );
    const notes = await engine.notes.search({ query: "budget", mode: "hybrid" });
    assert.deepEqual(notes.items.map((n) => n.revision), [2]);
    await engine.records.append("upgrade_rollback", {
      afterSeq: first.seq,
      throughSeq: second.seq,
    });
    const result = await engine.dispatch(
      "records",
      "search",
      { query: "test" },
      "search-one",
    ) as unknown as { items: SearchHit[] };
    assert.deepEqual(result.items.map((x) => [x.source.collection, x.source.seq]), [[
      "records",
      first.seq,
    ]]);
    assert.equal(engine.records.events[second.seq - 1].digest, second.digest);
    assert.equal(engine.notes.journal.events.length, 2);
  } finally {
    await engine.close();
    await Deno.remove(directory, { recursive: true });
  }
});
