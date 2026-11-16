import { settleScript } from "./helpers/scripts.ts";
import assert from "node:assert/strict";
import { composePhonePlugin } from "../composition/phone.ts";
import { composeScriptPlugin } from "../composition/scripts.ts";
import type { ScriptSummary } from "../contracts/script.ts";
import type { ToolResult } from "../contracts/tools.ts";
import { Kernel } from "../kernel/kernel.ts";
import type { Json } from "../kernel/protocol.ts";

type ResultPage = { text: string; characters: number; truncated: boolean; nextChar?: number };
type NativeResult = ScriptSummary & { result?: ResultPage };
function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
async function fixture(slowCheck = false) {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-native-result-" });
  const key = "fixture_native_result_phone_01234567890123456789";
  const requested = deferred(), release = deferred();
  let phoneCalls = 0;
  const small = {
    items: [{ conversationId: "qq:user:fixture", label: "真实手机回执" }],
    snapshotThroughSeq: 3,
  };
  const large = {
    download: {
      path: "/fixture/attachment.txt",
      sizeBytes: 40_000,
      sha256: "a".repeat(64),
      mimeType: "text/plain",
      source: {
        kind: "phone_attachment",
        conversationId: "qq:user:fixture",
        sourceSeq: 3,
        attachmentId: "attachment",
        senderPlatformUserId: "fixture",
      },
    },
    offsetBytes: 0,
    text: '附件原文🙂包含"引号"与\\反斜线\n'.repeat(600),
  };
  const phone = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    assert.equal(request.headers.get("authorization"), `Bearer ${key}`);
    const body = await request.json();
    if (body.method === "health") return Response.json({ ok: true, value: { available: true } });
    phoneCalls++;
    if (body.method === "conversations") return Response.json({ ok: true, value: small });
    assert.equal(body.method, "file", "本测试只允许只读手机方法");
    requested.resolve();
    await release.promise;
    return Response.json({ ok: true, value: large });
  });
  // Real type checking overlaps a seven-second preparation floor, independent of host load.
  const denoBinary = slowCheck ? `${directory}/slow-deno` : Deno.execPath();
  if (slowCheck) {
    const executable = "'" + Deno.execPath().replaceAll("'", "'\\''") + "'";
    await Deno.writeTextFile(
      denoBinary,
      `#!/bin/sh\n/bin/sleep 7 &\ndelay=$!\n${executable} "$@"\nresult=$?\nwait "$delay"\nexit "$result"\n`,
      { mode: 0o700 },
    );
  }
  const configPath = `${directory}/config.json`;
  await Deno.writeTextFile(`${directory}/phone.key`, key);
  await Deno.writeTextFile(
    configPath,
    JSON.stringify({
      phone: {
        adapterUrl: `http://127.0.0.1:${phone.addr.port}/rpc`,
        adapterKeyFile: `${directory}/phone.key`,
      },
      scripts: {
        directory: `${directory}/scripts`,
        denoBinary,
        cacheDirectory: `${directory}/cache`,
        defaultTimeoutMs: 60_000,
      },
    }),
  );
  await composePhonePlugin(`${directory}/phone-plugin`, configPath);
  await composeScriptPlugin(`${directory}/script-plugin`, configPath);
  let kernel: Kernel;
  const start = async () => {
    kernel = new Kernel({ dataDir: `${directory}/kernel`, callTimeoutMs: 30_000 });
    await kernel.register(`${directory}/phone-plugin`);
    await kernel.register(`${directory}/script-plugin`);
    await kernel.start();
  };
  await start();
  return {
    small,
    large,
    async waitForRequest() {
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        await Promise.race([
          requested.promise,
          new Promise<never>((_, reject) => {
            timer = setTimeout(() => reject(new Error("异步脚本没有发起实际手机读取")), 60_000);
          }),
        ]);
      } finally {
        clearTimeout(timer);
      }
    },
    release,
    phoneCalls: () => phoneCalls,
    kernel: () => kernel,
    async execute(id: string, code: string, mode?: string, settle = mode !== "async") {
      let result = await kernel.call<ToolResult & Json>("tools", "execute", {
        id,
        name: "execute_script",
        args: { code, ...(mode === undefined ? {} : { mode }) },
      });
      assert.equal(result.status, "completed");
      const job = JSON.parse(result.output) as NativeResult;
      if (settle && ["queued", "checking", "running"].includes(job.status)) {
        await settleScript(kernel, job);
        // Reuse the original request to retrieve its complete native result page.
        result = await kernel.call<ToolResult & Json>("tools", "execute", {
          id,
          name: "execute_script",
          args: { code, ...(mode === undefined ? {} : { mode }) },
        });
      }
      return JSON.parse(result.output) as NativeResult;
    },
    async restart() {
      assert.deepEqual(await kernel.stop(), []);
      await start();
    },
    async close() {
      release.resolve();
      await kernel.stop();
      await phone.shutdown();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("原生脚本返回值：同步return手机真实回执，重试与重启保留摘要和结果且不重放", async () => {
  const f = await fixture();
  try {
    const code = "return await tools.phone.conversations();";
    const first = await f.execute("sync-phone-return", code);
    assert.equal(first.status, "completed");
    assert.equal(first.scriptStarted, true);
    assert.equal(first.actions, 1);
    assert.ok(first.result);
    assert.equal(first.result.truncated, false);
    assert.deepEqual(JSON.parse(first.result.text), f.small);
    assert.equal(f.phoneCalls(), 1);
    assert.deepEqual(await f.execute("sync-phone-return", code), first);
    await f.restart();
    assert.deepEqual(await f.execute("sync-phone-return", code), first);
    assert.equal(f.phoneCalls(), 1);
    const nothing = await f.execute("undefined-return", "return;");
    assert.equal(nothing.result?.text, "null");
    const failed = await f.execute("failed-return", 'throw new Error("实际失败");');
    assert.equal(failed.status, "failed");
    assert.equal(failed.result, undefined);
  } finally {
    await f.close();
  }
});

Deno.test("原生脚本返回值：异步手机大原件完成后可通过原生读取脚本分页，保持原job游标", async () => {
  const f = await fixture();
  try {
    const code =
      'return await tools.phone.file({handle:{conversationId:"qq:user:fixture",seq:3},index:1});';
    const pending = await f.execute("async-phone-return", code, "async");
    assert.notEqual(pending.status, "completed");
    assert.equal(pending.result, undefined);
    await f.waitForRequest();
    f.release.resolve();
    for (let attempt = 0;; attempt++) {
      const status = await f.kernel().call<ScriptSummary & Json>("scripts", "status", {
        jobId: pending.jobId,
      });
      if (status.status === "completed") break;
      assert.ok(attempt < 1000, "异步脚本应完成");
      await new Promise((done) => setTimeout(done, 10));
    }
    const completed = await f.execute("async-phone-return", code, "async");
    assert.equal(completed.jobId, pending.jobId);
    assert.ok(completed.result);
    assert.equal(completed.result.truncated, true);
    assert.equal(Array.from(completed.result.text).length, 8192);
    assert.equal(completed.result.nextChar, 8192);
    let text = completed.result.text;
    let cursor: number | undefined = completed.result.nextChar;
    while (cursor !== undefined) {
      // 读取原任务的下一页，脚本本身也经原生工具返回；无需追逐新脚本编号。
      const read = await f.execute(
        `read-original-${cursor}`,
        `return await tools.scripts.result({jobId:${
          JSON.stringify(pending.jobId)
        },startChar:${cursor},limit:2048});`,
      );
      assert.equal(read.status, "completed");
      assert.equal(read.actions, 1);
      assert.ok(read.result);
      assert.equal(read.result.truncated, false, "包裹后的结果页也须完整可见");
      const page = JSON.parse(read.result.text) as ResultPage;
      assert.ok(Array.from(page.text).length <= 2048);
      assert.equal(page.characters, completed.result.characters);
      assert.equal(page.truncated, page.nextChar !== undefined);
      if (page.nextChar !== undefined) assert.ok(page.nextChar > cursor);
      text += page.text;
      cursor = page.nextChar;
    }
    assert.equal(Array.from(text).length, completed.result.characters);
    assert.deepEqual(JSON.parse(text), f.large);
    assert.equal(f.phoneCalls(), 1, "分页读取不能重新请求手机");
    const receipt = await f.execute(
      "read-original-receipt",
      `return await tools.scripts.receipt({jobId:${
        JSON.stringify(pending.jobId)
      },seq:1,section:"result",limit:2048});`,
    );
    assert.ok(receipt.result);
    assert.equal(receipt.result.truncated, false);
    assert.equal(
      (JSON.parse(receipt.result.text) as ResultPage).text,
      Array.from(text).slice(0, 2048).join(""),
    );
    await f.restart();
    assert.deepEqual(await f.execute("async-phone-return", code, "async"), completed);
    assert.equal(f.phoneCalls(), 1);
  } finally {
    await f.close();
  }
});

Deno.test("原生脚本：默认最多15秒且sync仍最多5秒，原任务后台并行，迟到结果只执行一次", async () => {
  for (const mode of [undefined, "sync"]) {
    const f = await fixture();
    try {
      const started = performance.now();
      const job = await f.execute(
        "bounded-background",
        'return await tools.phone.file({handle:{conversationId:"qq:user:fixture",seq:3},index:1});',
        mode,
        false,
      );
      const waitMs = mode === "sync" ? 5_000 : 15_000;
      const elapsed = performance.now() - started;
      assert.ok(elapsed >= waitMs - 100, "不能提前结束声明的等待");
      assert.ok(elapsed < waitMs + 2_000, "到期必须返回，不等实际工具结束");
      console.log("bounded wait", JSON.stringify({ mode: mode ?? "default", elapsed }));
      assert.ok(["queued", "checking", "running"].includes(job.status));
      assert.equal(job.result, undefined);
      await f.waitForRequest();
      assert.equal(f.phoneCalls(), 1);
      const independent = await f.execute(
        "parallel-short",
        "return await tools.phone.conversations();",
      );
      assert.equal(independent.status, "completed");
      assert.equal(f.phoneCalls(), 2);
      f.release.resolve();
      for (let attempt = 0; attempt < 500; attempt++) {
        const status = await f.kernel().call<ScriptSummary & Json>("scripts", "status", {
          jobId: job.jobId,
        });
        if (status.status === "completed") break;
        assert.ok(attempt < 499, "后台任务应接收真实结果");
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      const result = await f.execute(
        "bounded-background",
        'return await tools.phone.file({handle:{conversationId:"qq:user:fixture",seq:3},index:1});',
        mode,
      );
      assert.equal(result.jobId, job.jobId);
      assert.equal(result.status, "completed");
      assert.ok(result.result);
      assert.equal(f.phoneCalls(), 2);
    } finally {
      await f.close();
    }
  }
});

Deno.test("原生脚本等待：默认覆盖超过5秒的真实检查准备并直接返回结果，async不等待准备", async () => {
  const f = await fixture(true);
  try {
    // Prime Deno's real check cache so full-suite host load does not extend our 7s floor.
    // Every request still runs the checker; both measured calls retain the preparation delay.
    const code = "return 42;";
    const prime = await f.execute("slow-preparation-prime", code, "async", false);
    assert.equal((await settleScript(f.kernel(), prime)).status, "completed");
    const started = performance.now();
    const job = await f.execute(
      "slow-preparation-default",
      code,
      undefined,
      false,
    );
    assert.equal(job.status, "completed", "超过旧5秒等待的准备应直接带回结果，不补查原任务");
    assert.ok(performance.now() - started >= 6900);
    assert.ok(job.timing!.checkMs! >= 6900);
    assert.ok(job.result);
    assert.equal(JSON.parse(job.result.text), 42);
    assert.equal(f.phoneCalls(), 0);
    console.log("slow preparation default receipt", JSON.stringify(job.timing));

    const asyncStarted = performance.now();
    const pending = await f.execute(
      "slow-preparation-async",
      code,
      "async",
      false,
    );
    assert.ok(performance.now() - asyncStarted < 4900, "async不能等待七秒检查准备");
    assert.ok(["queued", "checking", "running"].includes(pending.status));
    assert.equal(pending.result, undefined);
    assert.equal((await settleScript(f.kernel(), pending)).status, "completed");
    assert.equal(f.phoneCalls(), 0);
  } finally {
    await f.close();
  }
});
