import assert from "node:assert/strict";
import { writeDurableJson } from "../sdk/durable-json.ts";

Deno.test("durable JSON: file sync precedes rename, directory sync follows rename", async () => {
  const directory = await Deno.makeTempDir();
  const path = directory + "/value.json";
  const sync = Deno.FsFile.prototype.sync, rename = Deno.rename;
  const calls: string[] = [];
  try {
    Deno.FsFile.prototype.sync = async function () {
      calls.push(calls.includes("rename") ? "directory-sync" : "file-sync");
      await sync.call(this);
    };
    Deno.rename = async (from, to) => {
      calls.push("rename");
      await rename(from, to);
    };
    await writeDurableJson(path, { saved: true });
    assert.deepEqual(calls, ["file-sync", "rename", "directory-sync"]);
    assert.equal(await Deno.readTextFile(path), '{"saved":true}\n');
  } finally {
    Deno.FsFile.prototype.sync = sync;
    Deno.rename = rename;
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("durable JSON: failed file sync preserves original and unfinished temporary", async () => {
  const directory = await Deno.makeTempDir();
  const path = directory + "/value.json";
  const sync = Deno.FsFile.prototype.sync;
  try {
    await writeDurableJson(path, { saved: 1 });
    Deno.FsFile.prototype.sync = () => Promise.reject(new Error("sync failed"));
    await assert.rejects(writeDurableJson(path, { saved: 2 }), /sync failed/);
    assert.equal(await Deno.readTextFile(path), '{"saved":1}\n');
    const files = Array.from(Deno.readDirSync(directory));
    assert.equal(files.filter((file) => file.name.endsWith(".tmp")).length, 1);
  } finally {
    Deno.FsFile.prototype.sync = sync;
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("durable JSON: directory sync failure is reported even after rename", async () => {
  const directory = await Deno.makeTempDir();
  const sync = Deno.FsFile.prototype.sync;
  let calls = 0;
  try {
    Deno.FsFile.prototype.sync = async function () {
      if (++calls === 2) throw new Error("directory sync failed");
      await sync.call(this);
    };
    await assert.rejects(
      writeDurableJson(directory + "/value.json", null),
      /directory sync failed/,
    );
    assert.equal(await Deno.readTextFile(directory + "/value.json"), "null\n");
  } finally {
    Deno.FsFile.prototype.sync = sync;
    await Deno.remove(directory, { recursive: true });
  }
});
