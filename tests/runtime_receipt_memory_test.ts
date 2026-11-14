import assert from "node:assert/strict";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import type { WebApi } from "../contracts/web.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

async function fixture(web?: WebApi) {
  const root = await Deno.makeTempDir({ dir: Deno.cwd(), prefix: "eve-receipt-memory-test-" });
  const options = {
    directory: root + "/data",
    workspace: root + "/workspace",
    settingsFile: root + "/settings.json",
    model: {
      complete() {
        throw new Error("must not call model");
      },
      close() {},
    },
    maxOutputTokens: 100,
    web,
  };
  await writeDurableJson(options.settingsFile, {
    dailyTokenBudget: 100000,
    paused: false,
    timeZone: "UTC",
  });
  let engine = new RuntimeEngine(options);
  return {
    root,
    get engine() {
      return engine;
    },
    async reopen() {
      await engine.close();
      engine = new RuntimeEngine(options);
      await engine.initialize();
    },
    async close() {
      await engine.close().catch(() => {});
      await Deno.remove(root, { recursive: true });
    },
  };
}
const args = { url: "https://synthetic.invalid/receipt" };
async function request(journal: FactJournal, id: string) {
  const record = {
    id,
    namespace: "web",
    method: "fetch",
    hash: await fingerprint({ namespace: "web", method: "fetch", args }),
    recovery: null,
    terminal: false,
  };
  await journal.append("request", record);
  return record;
}
const isKind = (kind: string) => (e: unknown) => e instanceof KernelError && e.kind === kind;

Deno.test("receipt index: falsy/empty/error bodies and exact public metadata survive recovery; reads are isolated in scoped cache", async () => {
  const f = await fixture();
  const journal = new FactJournal(f.root + "/data/actions");
  const cases: Json[] = [null, false, 0, "", [], {}, { nested: [{ text: "原件" }] }];
  const records: Awaited<ReturnType<typeof request>>[] = [];
  try {
    await journal.initialize();
    for (let i = 0; i < cases.length; i++) {
      records.push(await request(journal, "value_" + i));
      await journal.append("result", { id: "value_" + i, value: cases[i] });
    }
    const errorRecord = await request(journal, "error");
    const error = { kind: "rejected", message: "saved failure" };
    await journal.append("result", { id: "error", error });
    await journal.close();
    await f.engine.initialize();
    for (let i = 0; i < cases.length; i++) {
      assert.deepEqual(f.engine.receipt("value_" + i), {
        ...records[i],
        terminal: true,
        value: cases[i],
      });
      assert.deepEqual(await f.engine.dispatch("web", "fetch", args, "value_" + i), cases[i]);
    }
    assert.deepEqual(f.engine.receipt("error"), { ...errorRecord, terminal: true, error });
    await assert.rejects(f.engine.dispatch("web", "fetch", args, "error"), isKind("rejected"));
    assert.equal(f.engine.receipt("absent"), undefined);
    f.engine.transactions.withReadCache(() => {
      const first = f.engine.receipt("value_6")!;
      (first.value as { nested: { text: string }[] }).nested[0].text = "caller mutation";
      first.hash = "changed";
      first.recovery = { injected: true };
      assert.deepEqual(f.engine.receipt("value_6"), {
        ...records[6],
        terminal: true,
        value: cases[6],
      });
      const failed = f.engine.receipt("error")!;
      failed.error!.message = "changed";
      assert.equal(f.engine.receipt("error")!.error!.message, error.message);
      // Nested cache scopes also return independent JSON.parse objects.
      f.engine.transactions.withReadCache(() => {
        const raw = f.engine.transactions.events[13].data as {
          value: { nested: { text: string }[] };
        };
        raw.value.nested[0].text = "raw mutation";
        assert.deepEqual(f.engine.receipt("value_6")!.value, cases[6]);
      });
    });
    const repeated = await f.engine.dispatch("web", "fetch", args, "value_6") as {
      nested: { text: string }[];
    };
    repeated.nested[0].text = "mutated repeat";
    assert.deepEqual(await f.engine.dispatch("web", "fetch", args, "value_6"), cases[6]);
    await assert.rejects(
      f.engine.validateRepeat("web", "fetch", { url: "different" }, "value_6"),
      isKind("rejected"),
    );
    await assert.rejects(
      f.engine.dispatch("web", "fetch", { url: "different" }, "value_6"),
      isKind("rejected"),
    );
    assert.equal(f.engine.transactions.count, 16);
  } finally {
    await journal.close().catch(() => {});
    await f.close();
  }
});

Deno.test("receipt index: live duplicate shares one invocation; new successful/error bodies are persisted not caller-owned", async () => {
  let calls = 0;
  let release!: () => void;
  const entered = Promise.withResolvers<void>();
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const expected = { nested: { text: "large original" } };
  const web = {
    async fetch() {
      calls++;
      entered.resolve();
      await gate;
      return structuredClone(expected);
    },
  } as unknown as WebApi;
  const f = await fixture(web);
  try {
    await f.engine.initialize();
    const first = f.engine.dispatch("web", "fetch", args, "live");
    await entered.promise;
    assert.equal(f.engine.receipt("live"), undefined);
    const second = f.engine.dispatch("web", "fetch", args, "live");
    await assert.rejects(
      f.engine.dispatch("web", "fetch", { url: "different" }, "live"),
      isKind("rejected"),
    );
    release();
    const [a, b] = await Promise.all([first, second]);
    assert.equal(calls, 1);
    assert.deepEqual(a, b);
    (a as { nested: { text: string } }).nested.text = "caller-owned initial result";
    assert.deepEqual(f.engine.receipt("live")!.value, expected);
    assert.deepEqual(await f.engine.dispatch("web", "fetch", args, "live"), expected);
    await f.reopen();
    assert.deepEqual(await f.engine.dispatch("web", "fetch", args, "live"), expected);
    assert.equal(calls, 1);
  } finally {
    release();
    await f.close();
  }
});

for (const damage of ["missing", "digest", "valid-rewrite"]) {
  Deno.test(
    "receipt index: " + damage + " original fails closed after cache scope, never reinvokes",
    async () => {
      let calls = 0;
      const f = await fixture({
        fetch() {
          calls++;
          return Promise.resolve({ content: "original" });
        },
      } as unknown as WebApi);
      try {
        await f.engine.initialize();
        await f.engine.dispatch("web", "fetch", args, "saved");
        f.engine.transactions.withReadCache(() => assert.ok(f.engine.receipt("saved")));
        const path = f.root + "/data/actions/events/0000000000000002.json";
        if (damage === "missing") await Deno.rename(path, path + ".preserved");
        else {
          const original = JSON.parse(await Deno.readTextFile(path));
          original.data.value.content = "tampered";
          if (damage === "valid-rewrite") {
            const { digest: _digest, ...body } = original;
            original.digest = await fingerprint(body);
          }
          await Deno.writeTextFile(path, JSON.stringify(original));
        }
        assert.throws(() => f.engine.receipt("saved"), isKind("corrupt"));
        await assert.rejects(
          f.engine.dispatch("web", "fetch", args, "saved"),
          isKind("unavailable"),
        );
        assert.equal(calls, 1);
        assert.equal(f.engine.transactions.count, 2);
      } finally {
        await f.close();
      }
    },
  );
}

Deno.test("receipt index: interrupted unconfirmed request is unknown, never reexecuted", async () => {
  const f = await fixture();
  const journal = new FactJournal(f.root + "/data/actions");
  try {
    await journal.initialize();
    const metadata = await request(journal, "interrupted");
    await journal.close();
    await f.engine.initialize();
    const receipt = f.engine.receipt("interrupted")!;
    assert.deepEqual({ ...receipt, error: undefined }, {
      ...metadata,
      terminal: true,
      error: undefined,
    });
    assert.equal(receipt.error!.kind, "unknown");
    await assert.rejects(f.engine.dispatch("web", "fetch", args, "interrupted"), isKind("unknown"));
    await f.reopen();
    assert.deepEqual(f.engine.receipt("interrupted"), receipt);
    assert.equal(f.engine.transactions.count, 2);
  } finally {
    await journal.close().catch(() => {});
    await f.close();
  }
});

Deno.test("receipt index: new error receipt is isolated, repeated failure never calls provider again", async () => {
  let calls = 0;
  const f = await fixture({
    fetch() {
      calls++;
      return Promise.reject(new KernelError("rejected", "original failure"));
    },
  } as unknown as WebApi);
  try {
    await f.engine.initialize();
    await assert.rejects(f.engine.dispatch("web", "fetch", args, "failure"), isKind("rejected"));
    f.engine.receipt("failure")!.error!.message = "caller mutation";
    assert.equal(f.engine.receipt("failure")!.error!.message, "original failure");
    await assert.rejects(f.engine.dispatch("web", "fetch", args, "failure"), isKind("rejected"));
    await f.reopen();
    await assert.rejects(f.engine.dispatch("web", "fetch", args, "failure"), isKind("rejected"));
    assert.equal(calls, 1);
  } finally {
    await f.close();
  }
});

Deno.test("receipt index: invalid sequence/kind/id mapping fails closed without changing durable originals", async () => {
  for (const seq of [0, -1, NaN, 999, 1, 2]) {
    let calls = 0;
    const f = await fixture({
      fetch() {
        calls++;
        return Promise.resolve({ content: "original" });
      },
    } as unknown as WebApi);
    try {
      await f.engine.initialize();
      await f.engine.dispatch("web", "fetch", args, "first");
      const append = f.engine.transactions.append.bind(f.engine.transactions);
      // Inject a bad in-memory return value only. Real originals and hashes stay
      // intact. seq=1 targets request; seq=2 targets another request's result.
      f.engine.transactions.append = async (kind, data) => {
        const event = await append(kind, data);
        return kind === "result" ? { ...event, seq } : event;
      };
      await f.engine.dispatch("web", "fetch", args, "second");
      assert.throws(() => f.engine.receipt("second"), isKind("corrupt"));
      await assert.rejects(f.engine.dispatch("web", "fetch", args, "second"), isKind("corrupt"));
      assert.equal(calls, 2);
      assert.equal(f.engine.transactions.count, 4);
      assert.equal((f.engine.transactions.events[3].data as { id: string }).id, "second");
      await f.reopen();
      assert.deepEqual(f.engine.receipt("second")!.value, { content: "original" });
    } finally {
      await f.close();
    }
  }
});

Deno.test("receipt index: synthetic large completed history is unchanged and retrievable without a second body cache", async () => {
  const f = await fixture();
  const journal = new FactJournal(f.root + "/data/actions");
  try {
    await journal.initialize();
    for (let i = 0; i < 24; i++) {
      await request(journal, "large_" + i);
      await journal.append("result", { id: "large_" + i, value: { i, text: "中".repeat(131072) } });
    }
    const digests = journal.events.map((event) => event.digest);
    await journal.close();
    await f.engine.initialize();
    for (let i = 0; i < 24; i++) {
      const value = f.engine.receipt("large_" + i)!.value as { i: number; text: string };
      assert.equal(value.i, i);
      assert.equal(value.text.length, 131072);
      value.text = "mutated";
      assert.equal(
        (await f.engine.dispatch("web", "fetch", args, "large_" + i) as { text: string }).text
          .length,
        131072,
      );
    }
    assert.deepEqual(f.engine.transactions.events.map((event) => event.digest), digests);
    assert.equal(f.engine.transactions.count, 48);
  } finally {
    await journal.close().catch(() => {});
    await f.close();
  }
});

Deno.test("receipt index: notes/timeline/model interrupted results reconcile from originals without executing again", async () => {
  const f = await fixture();
  try {
    await f.engine.initialize();
    const notesArgs = {
      title: "synthetic",
      content: "retained note",
      sources: [{
        kind: "imagined",
        at: "2026-01-01T00:00:00.000Z",
        description: "synthetic fixture",
      }],
    };
    const note = await f.engine.notes.change("create", notesArgs, "note_recover");
    await f.engine.transactions.append("request", {
      id: "note_recover",
      namespace: "notes",
      method: "create",
      recovery: null,
      terminal: false,
      hash: await fingerprint({ namespace: "notes", method: "create", args: notesArgs }),
    });
    const timelineArgs = {
      expectedSeq: 0,
      expectedPrevious: "0".repeat(64),
      kind: "synthetic",
      data: { text: "original" },
    };
    const event = await f.engine.records.append(timelineArgs.kind, timelineArgs.data);
    await f.engine.transactions.append("request", {
      id: "timeline_recover",
      namespace: "timeline",
      method: "append",
      terminal: false,
      hash: await fingerprint({ namespace: "timeline", method: "append", args: timelineArgs }),
      recovery: {
        expectedSeq: 0,
        digest: await fingerprint([timelineArgs.kind, timelineArgs.data]),
      },
    });
    const modelArgs = { messages: [{ role: "user", content: "synthetic" }] };
    const response = {
      content: "saved model original",
      toolCalls: [],
      usage: {
        status: "reported",
        inputTokens: 10,
        cachedInputTokens: 0,
        outputTokens: 2,
      },
    };
    await f.engine.budget.reserve("model_recover", 100);
    await f.engine.modelResponses.append("response", { id: "model_recover", response });
    await f.engine.transactions.append("request", {
      id: "model_recover",
      namespace: "model",
      method: "complete",
      terminal: false,
      hash: await fingerprint({ namespace: "model", method: "complete", args: modelArgs }),
      recovery: { modelName: "", modelRevision: 0 },
    });
    await f.reopen();
    assert.deepEqual(f.engine.receipt("note_recover")!.value, note);
    assert.deepEqual(f.engine.receipt("timeline_recover")!.value, event);
    assert.deepEqual(f.engine.receipt("model_recover")!.value, response);
    assert.deepEqual(await f.engine.dispatch("notes", "create", notesArgs, "note_recover"), note);
    assert.deepEqual(
      await f.engine.dispatch("timeline", "append", timelineArgs, "timeline_recover"),
      event,
    );
    assert.deepEqual(
      await f.engine.dispatch("model", "complete", modelArgs, "model_recover"),
      response,
    );
    assert.equal(f.engine.notes.journal.count, 1);
    assert.equal(f.engine.records.count, 1);
    assert.equal(f.engine.transactions.count, 6);
    const receipts = ["note_recover", "timeline_recover", "model_recover"].map((id) =>
      f.engine.receipt(id)
    );
    await f.reopen();
    assert.deepEqual(
      ["note_recover", "timeline_recover", "model_recover"].map((id) => f.engine.receipt(id)),
      receipts,
    );
  } finally {
    await f.close();
  }
});

Deno.test("receipt index: duplicate dispatch itself detects a missing original before invoking", async () => {
  let calls = 0;
  const f = await fixture({
    fetch() {
      calls++;
      return Promise.resolve({ content: "original" });
    },
  } as unknown as WebApi);
  try {
    await f.engine.initialize();
    await f.engine.dispatch("web", "fetch", args, "saved");
    const path = f.root + "/data/actions/events/0000000000000002.json";
    await Deno.rename(path, path + ".preserved");
    await assert.rejects(f.engine.dispatch("web", "fetch", args, "saved"), isKind("corrupt"));
    assert.equal(calls, 1);
    assert.equal(f.engine.transactions.count, 2);
    await f.engine.close().catch(() => {});
    // Do not recreate the request after a failed journal; no new event written.
    assert.equal(f.engine.available, false);
  } finally {
    await f.close();
  }
});
