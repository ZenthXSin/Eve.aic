import assert from "node:assert/strict";
import { type JobRecord, type ScriptState, ScriptStore } from "../plugins/script-runtime/store.ts";
import { KernelError } from "../kernel/protocol.ts";

function job(id: number, data = "小结果"): JobRecord {
  const at = "2026-11-12T01:00:00.000Z";
  return {
    jobId: String(id).padStart(64, "0"),
    fingerprint: "a".repeat(64),
    request: { requestId: `test-${id}`, code: "return null;" },
    status: "completed",
    scriptStarted: true,
    startedAt: at,
    finishedAt: at,
    result: { image: { dataUrl: data } },
    actions: [{
      seq: 1,
      actionId: `action-${id}`,
      namespace: "phone",
      method: "image",
      args: { mode: "original" },
      status: "completed",
      startedAt: at,
      finishedAt: at,
      value: { image: { dataUrl: data } },
    }],
  };
}
async function objects(directory: string) {
  const result: { name: string; value: unknown }[] = [];
  for await (const file of Deno.readDir(`${directory}/objects`)) {
    result.push({
      name: file.name,
      value: JSON.parse(await Deno.readTextFile(`${directory}/objects/${file.name}`)),
    });
  }
  return result;
}

Deno.test("脚本账本：超过旧128MiB逻辑原件仍全局去重，按需读取，提交不重写历史或原图", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-blobs-" });
  let store = new ScriptStore(directory);
  const image = "data:image/png;base64," + "a".repeat(4_194_304);
  try {
    await store.initialize();
    await store.change((state) => {
      for (let id = 1; id <= 20; id++) state.jobs.push(job(id, image));
    });
    assert.equal(store.state.jobs.length, 20);
    const all = await objects(directory);
    assert.equal(all.filter((object) => object.value === image).length, 1);
    const imagePath = `${directory}/objects/${all.find((object) => object.value === image)!.name}`;
    const original = await Deno.stat(imagePath);
    const marker = await Deno.readTextFile(`${directory}/index.json`);
    assert.ok(marker.length < 256);
    assert.deepEqual(store.job(job(1).jobId).result, job(1, image).result);
    await store.change((state) => {
      const saved = state.jobs.find((value) => value.jobId === job(1).jobId)!;
      saved.status = "unknown";
      saved.error = { kind: "unknown", message: "保留实际回执，禁止重跑" };
    });
    assert.equal(await Deno.readTextFile(`${directory}/index.json`), marker);
    assert.equal((await Deno.stat(imagePath)).mtime?.getTime(), original.mtime?.getTime());
    assert.equal((await objects(directory)).filter((object) => object.value === image).length, 1);
    await store.close();
    store = new ScriptStore(directory);
    await store.initialize();
    assert.equal(store.job(job(1).jobId).status, "unknown");
    assert.equal(store.job(job(1).jobId).actions[0].status, "completed");
    assert.deepEqual(store.job(job(1).jobId).actions[0].value, job(1, image).actions[0].value);
    assert.deepEqual(store.job(job(2).jobId).result, job(2, image).result);
  } finally {
    await store.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本账本：读取状态不展开大值，原件损坏在实际读取时拒绝并阻止后续提交", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-lazy-" });
  const store = new ScriptStore(directory);
  const data = "图片原件".repeat(8192);
  try {
    await store.initialize();
    await store.change((state) => {
      state.jobs.push(job(1, data));
    });
    const blob = (await objects(directory)).find((object) => object.value === data)!;
    await Deno.writeTextFile(`${directory}/objects/${blob.name}`, JSON.stringify("篡改"));
    assert.equal(store.job(job(1).jobId).status, "completed");
    assert.equal(store.job(job(1).jobId).actions[0].status, "completed");
    assert.throws(
      () => (store.job(job(1).jobId).result as { image: { dataUrl: string } }).image.dataUrl,
      KernelError,
    );
    assert.equal(store.available, false);
    await assert.rejects(
      store.change((state) => {
        state.jobs = [];
      }),
      KernelError,
    );
    assert.equal(JSON.parse(await Deno.readTextFile(`${directory}/objects/${blob.name}`)), "篡改");
  } finally {
    await store.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本账本：旧账本迁移保留字节原件，中断迁移可验证恢复且不新增重复提交", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-migrate-" });
  let store = new ScriptStore(directory);
  const legacy: ScriptState = { schema: 1, jobs: [job(1, "旧原图".repeat(8192))], completions: [] };
  const original = JSON.stringify(legacy, null, 2) + "\n";
  try {
    // 模拟 rename 旧 index 后、迁移提交前宿主退出。
    await Deno.writeTextFile(`${directory}/legacy-index.json`, original);
    await store.initialize();
    assert.deepEqual(store.job(job(1).jobId), legacy.jobs[0]);
    assert.equal(await Deno.readTextFile(`${directory}/legacy-index.json`), original);
    await store.close();
    // 模拟提交已持久保存但小格式标记响应丢失，恢复不重新导入一次。
    await Deno.remove(`${directory}/index.json`);
    store = new ScriptStore(directory);
    await store.initialize();
    const commits = [];
    for await (const entry of Deno.readDir(`${directory}/commits/events`)) commits.push(entry.name);
    assert.equal(commits.length, 1);
    assert.deepEqual(store.job(job(1).jobId), legacy.jobs[0]);
    assert.equal(await Deno.readTextFile(`${directory}/legacy-index.json`), original);
  } finally {
    await store.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本账本：大量任务分块引用，普通草稿字段修改和数组替换重启后准确恢复", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-many-" });
  let store = new ScriptStore(directory);
  try {
    await store.initialize();
    await store.change((state) => {
      for (let i = 1; i <= 600; i++) state.jobs.push(job(i));
    });
    await store.change((state) => {
      const last = state.jobs.find((value) => value.jobId === job(600).jobId)!;
      (last.actions[0].args as { mode: string }).mode = "description";
      (last.result as { image: { dataUrl: string } }).image.dataUrl = "后来明确写入的新值";
      state.completions.push({
        seq: 1,
        job: {
          jobId: last.jobId,
          status: last.status,
          scriptStarted: true,
          actions: 1,
          startedAt: last.startedAt,
        },
      });
    });
    await store.close();
    store = new ScriptStore(directory);
    await store.initialize();
    assert.equal(store.state.jobs.length, 600);
    assert.equal(
      (store.job(job(600).jobId).actions[0].args as { mode: string }).mode,
      "description",
    );
    assert.deepEqual(store.job(job(600).jobId).result, {
      image: { dataUrl: "后来明确写入的新值" },
    });
    assert.equal(store.state.completions.length, 1);
    await store.change((state) => {
      state.completions = [];
    });
    await store.close();
    store = new ScriptStore(directory);
    await store.initialize();
    assert.equal(store.state.jobs.length, 600);
    assert.equal(store.state.completions.length, 0);
    assert.equal(store.jobIndex(job(600).jobId), 599);
    assert.equal(store.findJob("missing"), undefined);
    await store.change((state) => {
      state.jobs[599].jobId = job(601).jobId;
    });
    assert.equal(store.findJob(job(600).jobId), undefined);
    assert.equal(store.jobIndex(job(601).jobId), 599);
    await store.change((state) => {
      state.jobs.length = 599;
    });
    assert.equal(store.findJob(job(601).jobId), undefined);
  } finally {
    await store.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本账本：缺少内容原件、临时文件或同一账本第二写者均拒绝且保留证据", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-corrupt-" });
  let store = new ScriptStore(directory);
  try {
    await store.initialize();
    await store.change((state) => {
      state.jobs.push(job(1, "唯一原件".repeat(8192)));
    });
    await assert.rejects(new ScriptStore(directory).initialize(), { kind: "unavailable" });
    await store.close();
    const blob = (await objects(directory)).find((object) => typeof object.value === "string")!;
    await Deno.rename(`${directory}/objects/${blob.name}`, `${directory}/saved-original`);
    // 根目录未知文件也必须拒绝；先保留在测试目录外以专门验证缺失引用。
    await Deno.rename(`${directory}/saved-original`, `${directory}-saved-original`);
    store = new ScriptStore(directory);
    await assert.rejects(store.initialize(), KernelError);
    await Deno.rename(`${directory}-saved-original`, `${directory}/objects/${blob.name}`);
    const temporary = `${directory}/objects/unfinished.tmp`;
    await Deno.writeTextFile(temporary, "中断写入必须保留");
    store = new ScriptStore(directory);
    await assert.rejects(store.initialize(), KernelError);
    assert.equal(await Deno.readTextFile(temporary), "中断写入必须保留");
  } finally {
    await store.close().catch(() => {});
    await Deno.remove(`${directory}-saved-original`).catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本账本：正常旧index迁移与rename后未提交恢复，归档原件更改拒绝启动", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-legacy-source-" });
  let store = new ScriptStore(`${directory}/normal`);
  try {
    await Deno.mkdir(`${directory}/normal`);
    const legacy = { schema: 1, jobs: [job(1)], completions: [] };
    const raw = JSON.stringify(legacy, null, 2) + "\n";
    await Deno.writeTextFile(`${directory}/normal/index.json`, raw);
    await store.initialize();
    assert.equal(await Deno.readTextFile(`${directory}/normal/legacy-index.json`), raw);
    assert.deepEqual(store.job(job(1).jobId), legacy.jobs[0]);
    await store.close();
    await Deno.writeTextFile(
      `${directory}/normal/legacy-index.json`,
      JSON.stringify({ ...legacy, jobs: [] }),
    );
    store = new ScriptStore(`${directory}/normal`);
    await assert.rejects(store.initialize(), KernelError);
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${directory}/normal/legacy-index.json`)).jobs.length,
      0,
    );
    await Deno.mkdir(`${directory}/interrupted`);
    await Deno.writeTextFile(`${directory}/interrupted/legacy-index.json`, raw);
    store = new ScriptStore(`${directory}/interrupted`);
    await store.initialize();
    assert.deepEqual(store.job(job(1).jobId), legacy.jobs[0]);
  } finally {
    await store.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});
