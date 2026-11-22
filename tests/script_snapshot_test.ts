import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { canonical } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { type JobRecord, ScriptStore } from "../plugins/script-runtime/store.ts";

const job = (id: string): JobRecord => ({
  jobId: id,
  fingerprint: "a".repeat(64),
  request: { requestId: id, code: "return null;" },
  status: "completed",
  scriptStarted: true,
  startedAt: "2026-11-22T00:00:00Z",
  result: "a".repeat(8192),
  actions: [],
});
async function fixture() {
  const root = await Deno.makeTempDir();
  const directory = root + "/ledger";
  let store = new ScriptStore(directory);
  await store.initialize();
  await store.change((state) => state.jobs.push(job("first"), job("second")));
  store.job("first");
  store.job("second");
  await store.close();
  return {
    root,
    directory,
    snapshot: store.snapshotDirectory + "/snapshot.json",
    restart: async () => {
      store = new ScriptStore(directory);
      await store.initialize();
      return store;
    },
    close: async () => {
      await store.close().catch(() => {});
      await Deno.remove(root, { recursive: true });
    },
  };
}
const digest = (state: Json) => createHash("sha256").update(canonical(state)).digest("hex");

Deno.test("script snapshot: external cache reconciles indices, primes checked lookup, preserves whitelist", async () => {
  const f = await fixture();
  try {
    const store = await f.restart();
    assert.equal(store.snapshotMatched, true);
    assert.equal(store.jobIndex("second"), 1);
    assert.equal(store.job("first").result, job("first").result);
    assert.equal(
      Array.from(Deno.readDirSync(f.directory)).some((x) => x.name === "snapshot.json"),
      false,
    );
  } finally {
    await f.close();
  }
});

for (
  const damage of [
    "missing",
    "invalid-json",
    "checksum",
    "wrong-ledger",
    "stale",
    "symlink",
  ] as const
) {
  Deno.test("script snapshot: " + damage + " safely falls back to full replay", async () => {
    const f = await fixture();
    try {
      if (damage === "missing") await Deno.remove(f.snapshot);
      else if (damage === "invalid-json") await Deno.writeTextFile(f.snapshot, "{");
      else if (damage === "symlink") {
        await Deno.rename(f.snapshot, f.root + "/original-cache.json");
        await Deno.symlink(f.root + "/original-cache.json", f.snapshot);
      } else {
        const value = JSON.parse(await Deno.readTextFile(f.snapshot));
        if (damage === "checksum") value.digest = "0".repeat(64);
        if (damage === "wrong-ledger") value.state.ledger = "/not-this-ledger";
        if (damage === "stale") value.state.through++;
        await Deno.writeTextFile(f.snapshot, JSON.stringify(value));
      }
      const store = await f.restart();
      assert.equal(store.snapshotMatched, false);
      assert.deepEqual(JSON.parse(JSON.stringify(store.state.jobs)), [job("first"), job("second")]);
    } finally {
      await f.close();
    }
  });
}

Deno.test("script snapshot: recomputed checksum cannot authorize forged refs or job IDs", async () => {
  const f = await fixture();
  try {
    const value = JSON.parse(await Deno.readTextFile(f.snapshot));
    value.state.jobs.reverse();
    value.state.jobIds = ["forged", "forged"];
    value.digest = digest(value.state);
    await Deno.writeTextFile(f.snapshot, JSON.stringify(value));
    const store = await f.restart();
    assert.equal(store.snapshotMatched, false);
    assert.equal(store.jobIndex("forged"), -1);
    assert.deepEqual(JSON.parse(JSON.stringify(store.state.jobs)), [job("first"), job("second")]);
  } finally {
    await f.close();
  }
});

for (const damage of ["same-size-hash", "size", "symlink", "missing"] as const) {
  Deno.test(
    "script snapshot: valid cache does not conceal " + damage + " reachable original",
    async () => {
      const f = await fixture();
      try {
        const blob = Array.from(Deno.readDirSync(f.directory + "/objects")).find((entry) => {
          const value = JSON.parse(Deno.readTextFileSync(f.directory + "/objects/" + entry.name));
          return value === job("first").result;
        })!;
        const path = f.directory + "/objects/" + blob.name;
        if (damage === "same-size-hash") {
          await Deno.writeTextFile(path, JSON.stringify("b".repeat(8192)) + "\n");
        }
        if (damage === "size") {
          await Deno.writeTextFile(path, '"wrong"\n');
        }
        if (damage === "missing") await Deno.remove(path);
        if (damage === "symlink") {
          await Deno.rename(path, f.root + "/original.json");
          await Deno.symlink(f.root + "/original.json", path);
        }
        await assert.rejects(f.restart(), KernelError);
      } finally {
        await f.close();
      }
    },
  );
}

Deno.test("script snapshot: blocked cache writes do not fail ledger initialization or commits", async () => {
  const root = await Deno.makeTempDir();
  const store = new ScriptStore(root + "/ledger");
  try {
    await Deno.writeTextFile(store.snapshotDirectory, "not a directory");
    await store.initialize();
    await store.change((state) => state.jobs.push(job("first")));
    assert.equal(store.available, true);
    await store.close();
  } finally {
    await store.close().catch(() => {});
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("script snapshot: old in-ledger snapshot remains rejected and untouched", async () => {
  const f = await fixture();
  try {
    await Deno.writeTextFile(f.directory + "/snapshot.json", "old cache");
    await assert.rejects(f.restart(), /未完成或未知文件/);
    assert.equal(await Deno.readTextFile(f.directory + "/snapshot.json"), "old cache");
  } finally {
    await f.close();
  }
});
