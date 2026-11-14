import { settleScript } from "./helpers/scripts.ts";
import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { startPhoneRpc } from "../adapters/qq/rpc.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import { composePhonePlugin } from "../composition/phone.ts";
import { composeScriptPlugin } from "../composition/scripts.ts";
import type { ScriptSummary } from "../contracts/script.ts";
import { Kernel } from "../kernel/kernel.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { ScriptStore } from "../plugins/script-runtime/store.ts";
import { renderScriptContracts } from "../composition/script-docs.ts";

const key = "fixture_script_phone_key_0123456789012345";
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((finish) => {
    resolve = finish;
  });
  return { promise, resolve };
}
async function waitFor<T>(get: () => Promise<T | undefined>) {
  for (let index = 0; index < 6000; index++) {
    const value = await get();
    if (value !== undefined) return value;
    await new Promise((done) => setTimeout(done, 10));
  }
  throw new Error("脚本回归在60秒内没有拿到实际结果");
}
async function fixture(handler?: (request: Request) => Promise<Response>) {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-runtime-" });
  let count = 0;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (request): Promise<Response> => {
      if (new URL(request.url).pathname === "/app/getAppAccessToken") {
        return Response.json({
          access_token: "fixture-token",
          expires_in: 7200,
        });
      }
      count++;
      return handler ? await handler(request) : Response.json({ id: `actual-${count}` });
    },
  );
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    }),
    `${directory}/actions`,
  );
  await actions.initialize();
  const timeline = new QqTimeline(`${directory}/timeline`);
  await timeline.initialize();
  const reading = new QqPhoneReading(`${directory}/phone-reading`);
  await reading.initialize();
  const rpc = startPhoneRpc(
    new QqPhone(() => [], actions, timeline, { id: "fixture-bot", displayName: "Eve" }, reading),
    key,
  );
  await Deno.writeTextFile(`${directory}/key`, key, { mode: 0o600 });
  const configPath = `${directory}/config.json`;
  await Deno.writeTextFile(
    configPath,
    JSON.stringify({
      phone: { adapterUrl: rpc.url, adapterKeyFile: `${directory}/key` },
      scripts: {
        directory: `${directory}/scripts`,
        denoBinary: Deno.execPath(),
        cacheDirectory: `${directory}/cache`,
        defaultTimeoutMs: 60_000,
      },
    }),
    { mode: 0o600 },
  );
  await composePhonePlugin(`${directory}/phone-plugin`, configPath);
  await composeScriptPlugin(`${directory}/scripts-plugin`, configPath);
  let kernel = new Kernel({ dataDir: `${directory}/kernel`, callTimeoutMs: 15_000 });
  async function start() {
    await kernel.register(`${directory}/phone-plugin`);
    await kernel.register(`${directory}/scripts-plugin`);
    await kernel.start();
  }
  await start();
  return {
    directory,
    actions,
    timeline,
    rpc,
    count: () => count,
    kernel: () => kernel,
    restart: async () => {
      assert.deepEqual(await kernel.stop(), []);
      kernel = new Kernel({ dataDir: `${directory}/kernel`, callTimeoutMs: 15_000 });
      await start();
    },
    close: async () => {
      await kernel.stop();
      await rpc.close();
      await actions.close();
      await timeline.close();
      await reading.close();
      await platform.shutdown();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("脚本：一个原生工具经类型接口发送，每个动作当场保存，并发/重启复用原脚本", async () => {
  const f = await fixture();
  try {
    const list = await f.kernel().call<{ name: string }[]>("tools", "list");
    assert.deepEqual(list.map((v) => v.name), ["execute_script"]);
    const call = {
      id: "original-model-tool-call",
      name: "execute_script",
      args: {
        mode: "sync",
        // This checks action reuse, not execution timing. Include cold type checking
        // and worker startup in the isolated, unprivileged validation environment.
        timeoutMs: 60_000,
        code:
          'const results = await Promise.all([tools.phone.send({conversationId:"qq:user:friend",text:"一个意思"}),tools.phone.send({conversationId:"qq:user:friend",text:"另一个意思"})]); return results;',
      },
    };
    const results = await Promise.all([
      f.kernel().call<{ output: string }>("tools", "execute", call),
      f.kernel().call<{ output: string }>("tools", "execute", call),
    ]);
    assert.deepEqual(results[0], results[1]);
    const job = await settleScript(f.kernel(), JSON.parse(results[0].output));
    const settled = await f.kernel().call<{ output: string }>("tools", "execute", call);
    assert.equal(JSON.parse(settled.output).status, "completed");
    assert.equal(job.status, "completed");
    assert.equal(job.scriptStarted, true);
    assert.equal(job.actions, 2);
    assert.equal(f.count(), 2);
    const stored = await f.kernel().call<{ items: { status: string; actionId: string }[] }>(
      "scripts",
      "receipts",
      { jobId: job.jobId },
    );
    assert.equal(stored.items.length, 2);
    assert.equal(stored.items.every((a) => a.status === "completed"), true);
    const ids = stored.items.map((a) => a.actionId);
    assert.equal(new Set(ids).size, 2);
    assert.equal(ids.every((id: string) => f.actions.status(id)?.status === "finished"), true);
    const result = await f.kernel().call<{ text: string }>("scripts", "result", {
      jobId: job.jobId,
    });
    assert.equal(JSON.parse(result.text).every((r: { state: string }) => r.state === "sent"), true);
    assert.equal(
      JSON.parse(
        (await f.kernel().call<{ text: string }>("scripts", "receipt", {
          jobId: job.jobId,
          seq: 1,
          section: "result",
        })).text,
      ).state,
      "sent",
    );
    await f.restart();
    assert.deepEqual(await f.kernel().call("tools", "execute", call), settled);
    assert.equal(f.count(), 2);
    await assert.rejects(
      f.kernel().call("tools", "execute", { ...call, args: { code: "return 7;" } }),
      (error) => error instanceof KernelError && error.kind === "rejected",
    );
    assert.equal(f.count(), 2);
  } finally {
    await f.close();
  }
});

Deno.test("脚本：执行期限区分未开始与已开始，超长期限不溢出成立即取消", async () => {
  const f = await fixture();
  try {
    const early = await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
      requestId: "expires-before-start",
      mode: "sync",
      timeoutMs: 1,
      code: "return 1;",
    });
    assert.equal(early.status, "cancelled");
    assert.equal(early.scriptStarted, false);
    assert.equal(f.count(), 0);
    const long = await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
      requestId: "long-deadline",
      timeoutMs: 2_147_484_000,
      code: "await new Promise(()=>{});",
    });
    await waitFor(async () => {
      const current = await f.kernel().call<ScriptSummary & Json>("scripts", "status", {
        jobId: long.jobId,
      });
      return current.scriptStarted ? current : undefined;
    });
    assert.equal(
      (await f.kernel().call<ScriptSummary & Json>("scripts", "status", { jobId: long.jobId }))
        .status,
      "running",
    );
    await f.kernel().call("scripts", "cancel", { jobId: long.jobId });
    // Include cold type checking before asserting expiry of an already-started script.
    const timed = await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
      requestId: "actual-deadline",
      timeoutMs: 20_000,
      code: "await new Promise(()=>{});",
    });
    await waitFor(async () => {
      const current = await f.kernel().call<ScriptSummary & Json>("scripts", "status", {
        jobId: timed.jobId,
      });
      return current.status === "unknown" ? current : undefined;
    });
    assert.equal(
      (await f.kernel().call<ScriptSummary & Json>("scripts", "status", { jobId: timed.jobId }))
        .scriptStarted,
      true,
    );
    assert.equal(f.count(), 0);
  } finally {
    await f.close();
  }
});

Deno.test("脚本：独立账本拒绝第二写者、保留损坏及未完成文件，存盘失败不执行新动作", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-store-" });
  const store = new ScriptStore(`${directory}/ledger`);
  try {
    await store.initialize();
    await store.change((state) => {
      state.schema = 1;
    });
    const original = await Deno.readTextFile(`${directory}/ledger/index.json`);
    const second = new ScriptStore(`${directory}/ledger`);
    await assert.rejects(
      second.initialize(),
      (error) => error instanceof KernelError && error.kind === "unavailable",
    );
    await Deno.rename(`${directory}/ledger`, `${directory}/original`);
    await assert.rejects(
      store.change((state) => {
        state.completions = [];
      }),
      (error) => error instanceof KernelError && error.kind === "storage",
    );
    assert.equal(store.available, false);
    await assert.rejects(store.close(), KernelError);
    assert.equal(await Deno.readTextFile(`${directory}/original/index.json`), original);
    await Deno.rename(`${directory}/original`, `${directory}/ledger`);
    const restored = new ScriptStore(`${directory}/ledger`);
    await restored.initialize();
    await restored.close();
    await Deno.writeTextFile(`${directory}/ledger/index.json`, "broken");
    const corrupt = new ScriptStore(`${directory}/ledger`);
    await assert.rejects(
      corrupt.initialize(),
      (error) => error instanceof KernelError && error.kind === "corrupt",
    );
    assert.equal(await Deno.readTextFile(`${directory}/ledger/index.json`), "broken");
    await Deno.writeTextFile(`${directory}/ledger/index.json`, original);
    await Deno.writeTextFile(`${directory}/ledger/incomplete.tmp`, "preserve");
    const unfinished = new ScriptStore(`${directory}/ledger`);
    await assert.rejects(unfinished.initialize(), KernelError);
    assert.equal(await Deno.readTextFile(`${directory}/ledger/incomplete.tmp`), "preserve");
  } finally {
    await store.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本：宿主发起前账本保存失败阻止发送，健康不可用；契约说明对应真实源码", async () => {
  const f = await fixture();
  try {
    await Deno.rename(`${f.directory}/scripts/ledger`, `${f.directory}/preserved-ledger`);
    await assert.rejects(
      f.kernel().call("scripts", "start", {
        requestId: "storage-denied",
        mode: "sync",
        code: 'return await tools.phone.send({conversationId:"qq:user:friend",text:"不应发"});',
      }),
      (error) => error instanceof KernelError && error.kind === "storage",
    );
    assert.equal(f.count(), 0);
    const description = await renderScriptContracts();
    assert.equal(
      description.includes(
        await Deno.readTextFile(new URL("../contracts/script.ts", import.meta.url)),
      ),
      true,
    );
    assert.equal(
      description.includes(
        await Deno.readTextFile(new URL("../contracts/phone.ts", import.meta.url)),
      ),
      true,
    );
  } finally {
    await f.close();
  }
});

Deno.test("脚本：实际插件换版失败回滚仍采用独立账本，不丢动作或重跑原脚本", async () => {
  const f = await fixture();
  try {
    const args = {
      requestId: "before-swap",
      mode: "sync",
      code: 'return await tools.phone.send({conversationId:"qq:user:friend",text:"换版前发出"});',
    };
    const job = await settleScript(
      f.kernel(),
      await f.kernel().call<ScriptSummary & Json>("scripts", "start", args),
    );
    assert.equal(job.status, "completed");
    assert.equal(f.count(), 1);
    const ledger = await Deno.readTextFile(`${f.directory}/scripts/ledger/index.json`);
    await Deno.mkdir(`${f.directory}/broken-plugin`);
    const manifest = JSON.parse(
      await Deno.readTextFile(`${f.directory}/scripts-plugin/manifest.json`),
    );
    manifest.version = "1.1.0";
    await Deno.writeTextFile(
      `${f.directory}/broken-plugin/manifest.json`,
      JSON.stringify(manifest),
    );
    const module = new URL("../plugins/script-runtime/mod.ts", import.meta.url).href;
    await Deno.writeTextFile(
      `${f.directory}/broken-plugin/mod.ts`,
      `import {createScriptPlugin} from ${
        JSON.stringify(module)
      };\nconst plugin=createScriptPlugin(${
        JSON.stringify(f.directory + "/config.json")
      });export default {...plugin,health(){throw new Error("fixture candidate unhealthy");}};\n`,
    );
    const swapped = await f.kernel().swap("script-runtime", `${f.directory}/broken-plugin`);
    assert.equal(swapped.outcome, "rolled_back");
    assert.equal(await Deno.readTextFile(`${f.directory}/scripts/ledger/index.json`), ledger);
    assert.deepEqual(await f.kernel().call("scripts", "start", args), job);
    assert.equal(f.count(), 1);
    assert.equal(
      (await f.kernel().call<{ items: Json[] }>("scripts", "completions", {})).items.length,
      1,
    );
  } finally {
    await f.close();
  }
});

Deno.test("脚本：类型或参数错误在执行前拒绝，不发生发送；正确脚本可自行修正", async () => {
  const f = await fixture();
  try {
    const bad = await settleScript(
      f.kernel(),
      await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
        requestId: "bad-types",
        mode: "sync",
        code: 'return await tools.phone.send({conversationId:"qq:user:friend",text:42});',
      }),
    );
    assert.equal(bad.status, "failed");
    assert.equal(bad.scriptStarted, false);
    assert.match(bad.error!.message, /not assignable|不能/);
    assert.equal(f.count(), 0);
    const falseId = await settleScript(
      f.kernel(),
      await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
        requestId: "fake-action-id",
        mode: "sync",
        code:
          'return await tools.phone.send({actionId:"fake",conversationId:"qq:user:friend",text:"不应发"});',
      }),
    );
    assert.equal(falseId.status, "failed");
    assert.equal(falseId.scriptStarted, false);
    assert.equal(f.count(), 0);
    const fixed = await settleScript(
      f.kernel(),
      await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
        requestId: "corrected",
        mode: "sync",
        code:
          'return await tools.phone.send({conversationId:"qq:user:friend",text:"实际正确类型"});',
      }),
    );
    assert.equal(fixed.status, "completed");
    assert.equal(f.count(), 1);
  } finally {
    await f.close();
  }
});

Deno.test("脚本：显式异步，忙worker可终止，完成通知持久且只记录一次", async () => {
  const f = await fixture();
  try {
    const live = await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
      requestId: "busy",
      mode: "async",
      // 本例验证主动取消；独立脚本的类型检查不应先耗尽默认的五秒期限。
      timeoutMs: 30_000,
      code: "while(true) {}",
    });
    assert.equal(["queued", "checking", "running"].includes(live.status), true);
    await waitFor(async () => {
      const status = await f.kernel().call<ScriptSummary & Json>("scripts", "status", {
        jobId: live.jobId,
      });
      return status.scriptStarted ? status : undefined;
    });
    const independent = await settleScript(
      f.kernel(),
      await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
        requestId: "independent",
        mode: "sync",
        code: "return 5;",
      }),
    );
    assert.equal(independent.status, "completed");
    const cancelled = await f.kernel().call<ScriptSummary & Json>("scripts", "cancel", {
      jobId: live.jobId,
    });
    assert.equal(cancelled.status, "cancelled");
    assert.deepEqual(await f.kernel().call("scripts", "cancel", { jobId: live.jobId }), cancelled);
    const first = await f.kernel().call<{ items: { seq: number }[]; throughSeq: number }>(
      "scripts",
      "completions",
      {},
    );
    assert.equal(first.items.length, 2);
    assert.deepEqual(first.items.map((e) => e.seq), [1, 2]);
    await f.restart();
    assert.deepEqual(await f.kernel().call("scripts", "completions", {}), first);
    assert.equal(
      (await f.kernel().call<{ items: Json[] }>("scripts", "completions", {
        afterSeq: first.throughSeq,
      })).items.length,
      0,
    );
    assert.equal(f.count(), 0);
  } finally {
    await f.close();
  }
});

Deno.test("脚本：取消等待中的真实动作保留未知，迟到回执存盘且原脚本不重放", async () => {
  const entered = deferred<void>();
  const release = deferred<void>();
  const f = await fixture(async () => {
    entered.resolve();
    await release.promise;
    return Response.json({ id: "actual-late-id" });
  });
  try {
    const request = {
      requestId: "cancelled-send",
      // Cancellation is exercised explicitly after dispatch, not by a cold-start deadline.
      timeoutMs: 30_000,
      code: 'return await tools.phone.send({conversationId:"qq:user:friend",text:"发送中取消"});',
    };
    const job = await f.kernel().call<ScriptSummary & Json>("scripts", "start", request);
    await entered.promise;
    assert.equal(f.count(), 1);
    const pending = await f.kernel().call<{ items: { status: string; actionId: string }[] }>(
      "scripts",
      "receipts",
      { jobId: job.jobId },
    );
    assert.equal(pending.items[0].status, "started");
    const actionId = pending.items[0].actionId;
    const cancelled = await f.kernel().call<ScriptSummary & Json>("scripts", "cancel", {
      jobId: job.jobId,
    });
    assert.equal(cancelled.status, "unknown");
    assert.equal(
      (await f.kernel().call<ScriptSummary & Json>("scripts", "start", request)).status,
      "unknown",
    );
    assert.equal(f.count(), 1);
    release.resolve();
    await waitFor(async () => {
      const result = await f.kernel().call<{ items: { status: string }[] }>("scripts", "receipts", {
        jobId: job.jobId,
      });
      return result.items[0]?.status === "completed" ? result : undefined;
    });
    const actual = f.actions.status(actionId);
    assert.ok(actual?.status === "finished" && actual.result.status === "sent");
    assert.equal(
      (await f.kernel().call<{ items: Json[] }>("scripts", "completions", {})).items.length,
      1,
    );
    await f.restart();
    assert.equal(
      (await f.kernel().call<ScriptSummary & Json>("scripts", "start", request)).status,
      "unknown",
    );
    assert.equal(f.count(), 1);
  } finally {
    release.resolve();
    await f.close();
  }
});

Deno.test("脚本社交：burst编号由宿主注入并持久化，quietwait类型接口贯通且不发送", async () => {
  const f = await fixture();
  try {
    const request = {
      requestId: "social-script-original",
      mode: "sync",
      timeoutMs: 60_000,
      code:
        'const burst = await tools.phone.sendBurst({conversationId:"qq:user:friend",messages:["第一句","第二句"]}); const waited = await tools.phone.waitForMessages({conversationId:"qq:user:friend",purpose:"reply",quietMs:100,maxWaitMs:20}); return {burst,waited};',
    };
    const job = await settleScript(
      f.kernel(),
      await f.kernel().call<ScriptSummary & Json>("scripts", "start", request),
    );
    assert.equal(job.status, "completed");
    assert.equal(job.actions, 2);
    assert.equal(f.count(), 2);
    const result = JSON.parse(
      (await f.kernel().call<{ text: string }>("scripts", "result", { jobId: job.jobId })).text,
    );
    assert.equal(result.burst.complete, true);
    assert.match(result.burst.actionId, /^script_/);
    assert.equal(result.burst.items.length, 2);
    assert.equal(result.waited.reason, "max_wait");
    assert.equal(result.waited.throughSeq, 2);
    assert.equal(result.waited.lastIncomingSeq, undefined);
    await f.restart();
    assert.deepEqual(await f.kernel().call("scripts", "start", request), job);
    assert.equal(f.count(), 2);
  } finally {
    await f.close();
  }
});

Deno.test("脚本社交恢复：原脚本unknown结束后取消队首，新脚本resume只发送剩余子条", async () => {
  let unknown = true, sentCount = 0;
  const f = await fixture(() => {
    sentCount++;
    return Promise.resolve(Response.json(unknown ? {} : { id: `sent-${sentCount}` }));
  });
  try {
    const initialJob = await settleScript(
      f.kernel(),
      await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
        requestId: "burst-original-unknown",
        mode: "sync",
        timeoutMs: 60_000,
        code:
          'return await tools.phone.sendBurst({conversationId:"qq:user:friend",messages:["第一条未知","第二条待发","第三条待发"]});',
      }),
    );
    assert.equal(initialJob.status, "completed");
    const initial = JSON.parse(
      (await f.kernel().call<{ text: string }>("scripts", "result", {
        jobId: initialJob.jobId,
        limit: 8192,
      }))
        .text,
    );
    assert.deepEqual(
      initial.items.map((item: { receipt: { state: string } }) => item.receipt.state),
      ["unknown", "blocked", "blocked"],
    );
    assert.equal(f.count(), 1);
    const head = (await f.actions.listSendQueue({ conversationId: "qq:user:friend" })).items.find((
      item,
    ) => item.actionId === initial.items[0].actionId)!;
    await f.actions.cancelSend({
      actionId: head.actionId,
      expectedFingerprint: head.fingerprint,
      expectedReceiptDigest: head.receiptDigest!,
    }, { actor: "eve" });
    unknown = false;
    const resumedJob = await settleScript(
      f.kernel(),
      await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
        requestId: "burst-new-script-resume",
        mode: "sync",
        timeoutMs: 60_000,
        code:
          'const resumed=await tools.phone.sendBurst({conversationId:"qq:user:friend",messages:["第一条未知","第二条待发","第三条待发"],resumeActionId:' +
          JSON.stringify(initial.actionId) +
          "}); return {resumed,status:await tools.phone.burstStatus({actionId:" +
          JSON.stringify(initial.actionId) + "})};",
      }),
    );
    assert.equal(resumedJob.status, "completed");
    const result = JSON.parse(
      (await f.kernel().call<{ text: string }>("scripts", "result", {
        jobId: resumedJob.jobId,
        limit: 8192,
      }))
        .text,
    );
    assert.equal(result.resumed.actionId, initial.actionId);
    assert.deepEqual(
      result.resumed.items.slice(1).map((item: { receipt: { state: string } }) =>
        item.receipt.state
      ),
      ["sent", "sent"],
    );
    assert.equal(result.resumed.items[0].receipt.resolution.kind, "unknown_retained");
    assert.deepEqual(result.status, result.resumed);
    assert.equal(f.count(), 3);
  } finally {
    await f.close();
  }
});

Deno.test("脚本：详细契约可按需读取并分页，未知名称拒绝，固定提示不常驻子代理长契约", async () => {
  const f = await fixture();
  try {
    const job = await f.kernel().call("scripts", "start", {
      requestId: "contracts-lookup",
      code: 'return await tools.scripts.contracts({name:"agents",limit:40});',
      mode: "sync",
    }) as unknown as ScriptSummary;
    const completed = await waitFor(async () => {
      const s = await f.kernel().call("scripts", "status", {
        jobId: job.jobId,
      }) as unknown as ScriptSummary;
      return s.status === "completed" ? s : undefined;
    });
    assert.equal(completed.status, "completed");
    const page = await f.kernel().call("scripts", "contracts", { name: "agents", limit: 40 }) as {
      text: string;
      nextChar: number;
    };
    assert.equal(Array.from(page.text).length, 40);
    assert.equal(page.nextChar, 40);
    await assert.rejects(() =>
      f.kernel().call("scripts", "contracts", { name: "../../etc/passwd" })
    );
    const prompt = await renderScriptContracts();
    assert.ok(prompt.includes('name:"agents"'));
    assert.equal(prompt.includes("export interface AgentView"), false);
    assert.equal(prompt.includes("export interface PhoneActionReceipt"), false);
  } finally {
    await f.close();
  }
});

Deno.test("脚本预热：空载待命且任务之间不共享全局状态，耗时回执重启后不变", async () => {
  const f = await fixture();
  try {
    const receipts: ScriptSummary[] = [];
    for (const requestId of ["prewarm-first", "prewarm-second"]) {
      const started = await f.kernel().call<ScriptSummary & Json>("scripts", "start", {
        requestId,
        code: `const state = globalThis as typeof globalThis & { previous?: string };
          const previous = state.previous ?? null; state.previous = "another job"; return previous;`,
      });
      const job = await settleScript(f.kernel(), started);
      const receipt = await f.kernel().call<ScriptSummary & Json>("scripts", "status", {
        jobId: job.jobId,
      });
      assert.equal(receipt.status, "completed");
      assert.equal(receipt.timing?.prewarmed, true);
      for (const field of ["checkMs", "workerWaitMs", "preparationMs", "executionMs"] as const) {
        assert.ok(Number.isFinite(receipt.timing?.[field]));
        assert.ok(receipt.timing![field]! >= 0);
      }
      const result = await f.kernel().call<{ text: string }>("scripts", "result", {
        jobId: job.jobId,
      });
      assert.equal(result.text, "null");
      assert.equal(f.count(), 0);
      receipts.push(receipt);
    }
    const completions = await f.kernel().call<{ items: { job: Json }[] }>(
      "scripts",
      "completions",
      {},
    );
    // Completion identity stays compatible with the previous runtime on rollback.
    assert.ok(completions.items.every((item) => !Object.hasOwn(item.job as object, "timing")));
    await f.restart();
    for (const receipt of receipts) {
      assert.deepEqual(
        await f.kernel().call("scripts", "status", { jobId: receipt.jobId }),
        receipt,
      );
    }
  } finally {
    await f.close();
  }
});
