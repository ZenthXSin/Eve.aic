import assert from "node:assert/strict";
import { READ_SCRIPT_TOOL, readScript } from "../plugins/eve/script-reader.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import type { Json } from "../kernel/protocol.ts";
Deno.test("原生脚本读取：同一原任务分页，不启动脚本或业务动作", async () => {
  const calls: unknown[] = [];
  const page = { text: "余页🙂", characters: 9000, truncated: false };
  const ctx = {
    call(namespace: string, method: string, args: Json) {
      calls.push({ namespace, method, args });
      assert.equal(namespace, "scripts");
      assert.ok(["status", "result"].includes(method));
      return Promise.resolve(method === "status" ? { jobId: "old", status: "completed" } : page);
    },
  } as unknown as PluginContext;
  assert.equal(READ_SCRIPT_TOOL.name, "read_script");
  assert.deepEqual(await readScript(ctx, { jobId: "old", startChar: 8192, limit: 808 }), {
    jobId: "old",
    status: "completed",
    result: page,
    resultStartChar: 8192,
  });
  assert.deepEqual(calls, [
    { namespace: "scripts", method: "status", args: { jobId: "old" } },
    { namespace: "scripts", method: "result", args: { jobId: "old", startChar: 8192, limit: 808 } },
  ]);
});
Deno.test("原生脚本读取：后台、失败和unknown保持真实状态，不取未存在结果或重放", async () => {
  for (const status of ["queued", "checking", "running", "failed", "cancelled", "unknown"]) {
    let calls = 0;
    const ctx = {
      call(namespace: string, method: string) {
        calls++;
        assert.equal(namespace, "scripts");
        assert.equal(method, "status");
        return Promise.resolve({ jobId: "old", status });
      },
    } as unknown as PluginContext;
    assert.deepEqual(await readScript(ctx, { jobId: "old" }), { jobId: "old", status });
    assert.equal(calls, 1);
  }
});
Deno.test("原生脚本读取：参数先校验，拒绝无效游标和额外动作字段", async () => {
  const ctx = {
    call() {
      throw new Error("无效参数不可派发");
    },
  } as unknown as PluginContext;
  for (
    const args of [
      null,
      [],
      {},
      { jobId: " " },
      { jobId: "old", startChar: -1 },
      { jobId: "old", limit: 8193 },
      { jobId: "old", limit: 1.5 },
      { jobId: "old", code: "repeat()" },
    ]
  ) {
    await assert.rejects(() => readScript(ctx, args as Json), /需要jobId/);
  }
});
