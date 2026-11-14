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
import type { Json } from "../kernel/protocol.ts";

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((finish) => {
    resolve = finish;
  });
  return { resolve, promise };
}
async function wait(get: () => Promise<boolean>) {
  for (let attempt = 0; attempt < 1500; attempt++) {
    if (await get()) return;
    await new Promise((done) => setTimeout(done, 10));
  }
  throw new Error("真实脚本子进程15秒内未完成目标步骤");
}

Deno.test("脚本进程：SIGKILL后保留已完成动作及未完成未知，恢复不重跑脚本、不重复通知", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-script-process-" });
  const entered = deferred();
  const release = deferred();
  let sends = 0;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (request): Promise<Response> => {
      if (new URL(request.url).pathname === "/app/getAppAccessToken") {
        return Response.json({
          access_token: "fixture",
          expires_in: 7200,
        });
      }
      sends++;
      if (sends === 2) {
        entered.resolve();
        await release.promise;
      }
      return Response.json({ id: `real-${sends}` });
    },
  );
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    }),
    `${directory}/outbox`,
  );
  await actions.initialize();
  const timeline = new QqTimeline(`${directory}/timeline`);
  await timeline.initialize();
  const reading = new QqPhoneReading(`${directory}/phone-reading`);
  await reading.initialize();
  const key = "fixture_script_process_key_01234567890123456789";
  const rpc = startPhoneRpc(
    new QqPhone(() => [], actions, timeline, { id: "fixture-bot", displayName: "Eve" }, reading),
    key,
  );
  const config = `${directory}/config.json`;
  await Deno.writeTextFile(`${directory}/key`, key, { mode: 0o600 });
  await Deno.writeTextFile(
    config,
    JSON.stringify({
      phone: { adapterUrl: rpc.url, adapterKeyFile: `${directory}/key` },
      scripts: {
        directory: `${directory}/scripts`,
        cacheDirectory: `${directory}/cache`,
        denoBinary: Deno.execPath(),
        defaultTimeoutMs: 60_000,
      },
    }),
    { mode: 0o600 },
  );
  await composePhonePlugin(`${directory}/phone`, config);
  await composeScriptPlugin(`${directory}/scripts-plugin`, config);
  const request = {
    requestId: "persistent-model-call",
    code:
      'await tools.phone.send({conversationId:"qq:user:friend",text:"已完成"}); return await tools.phone.send({conversationId:"qq:user:friend",text:"发送中"});',
  };
  const source = `${directory}/child.ts`;
  await Deno.writeTextFile(
    source,
    `import {Kernel} from ${
      JSON.stringify(new URL("../kernel/kernel.ts", import.meta.url).href)
    };\n` +
      `const kernel=new Kernel({dataDir:${
        JSON.stringify(directory + "/kernel")
      },callTimeoutMs:15000});await kernel.register(${
        JSON.stringify(directory + "/phone")
      });await kernel.register(${
        JSON.stringify(directory + "/scripts-plugin")
      });await kernel.start();\n` +
      `const job=await kernel.call("scripts","start",${JSON.stringify(request)});
      for(;;){const receipts=await kernel.call("scripts","receipts",{jobId:job.jobId});
      if(receipts.items.length===2 && receipts.items[1].status==="started"){
      const firstResult=await kernel.call("scripts","receipt",{jobId:job.jobId,seq:1,section:"result",limit:8192});
      await Deno.writeTextFile(${
        JSON.stringify(directory + "/before-kill.json")
      },JSON.stringify({job,receipts,firstResult}));break;}
      const status=await kernel.call("scripts","status",{jobId:job.jobId});
      if(!["queued","checking","running"].includes(status.status)){
      throw new Error("script stopped before the second action: "+JSON.stringify(status));}
      await new Promise(done=>setTimeout(done,5));}await new Promise(()=>{});\n`,
  );
  let child: Deno.ChildProcess | undefined;
  let stderr: Promise<string> | undefined;
  let kernel: Kernel | undefined;
  try {
    child = new Deno.Command(Deno.execPath(), {
      args: [
        "run",
        "--unstable-worker-options",
        "--allow-read",
        "--allow-write",
        "--allow-run",
        "--allow-env",
        "--allow-net=127.0.0.1",
        source,
      ],
      stdout: "null",
      stderr: "piped",
    }).spawn();
    stderr = new Response(child.stderr).text();
    let enteredTimer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        entered.promise,
        child.status.then(async (status) => {
          throw new Error(`脚本进程提前退出${status.code}: ${await stderr}`);
        }),
        new Promise<never>((_, reject) => {
          enteredTimer = setTimeout(() => reject(new Error("脚本60秒内未发起第二个动作")), 60_000);
        }),
      ]);
    } finally {
      clearTimeout(enteredTimer);
    }
    let original: {
      job: ScriptSummary;
      receipts: { items: { status: string; actionId: string }[] };
      firstResult: Json;
    } | undefined;
    await wait(async () => {
      try {
        original = JSON.parse(await Deno.readTextFile(`${directory}/before-kill.json`));
        return true;
      } catch (error) {
        if (error instanceof Deno.errors.NotFound || error instanceof SyntaxError) return false;
        throw error;
      }
    });
    const job = { jobId: original!.job.jobId, actions: original!.receipts.items };
    assert.equal(job.actions[0].status, "completed");
    assert.equal(job.actions[1].status, "started");
    child.kill("SIGKILL");
    await child.status;
    await stderr;
    child = undefined;
    stderr = undefined;
    release.resolve();
    await wait(() => {
      const result = actions.status(job.actions[1].actionId);
      return Promise.resolve(result?.status === "finished");
    });
    kernel = new Kernel({ dataDir: `${directory}/kernel`, callTimeoutMs: 15_000 });
    await kernel.register(`${directory}/phone`);
    await kernel.register(`${directory}/scripts-plugin`);
    await kernel.start();
    const restored = await kernel.call<ScriptSummary & Json>("scripts", "status", {
      jobId: job.jobId,
    });
    assert.equal(restored.status, "unknown");
    assert.equal(restored.actions, 2);
    const after = await kernel.call<{ items: { status: string; actionId: string }[] }>(
      "scripts",
      "receipts",
      { jobId: job.jobId },
    );
    assert.deepEqual(after.items[0], job.actions[0]);
    assert.deepEqual(
      await kernel.call("scripts", "receipt", {
        jobId: job.jobId,
        seq: 1,
        section: "result",
        limit: 8192,
      }),
      original!.firstResult,
    );
    assert.equal(after.items[1].status, "unknown");
    assert.equal(after.items[1].actionId, job.actions[1].actionId);
    assert.equal(
      (await kernel.call<ScriptSummary & Json>("scripts", "start", request)).status,
      "unknown",
    );
    assert.equal(sends, 2);
    const first = await kernel.call<{ items: { seq: number }[] }>("scripts", "completions", {});
    assert.equal(first.items.length, 1);
    const platformFact = await kernel.call<{ state: string }>("phone", "action", {
      actionId: job.actions[1].actionId,
    });
    assert.equal(platformFact.state, "sent");
    assert.deepEqual(await kernel.stop(), []);
    kernel = undefined;
    kernel = new Kernel({ dataDir: `${directory}/kernel` });
    await kernel.register(`${directory}/phone`);
    await kernel.register(`${directory}/scripts-plugin`);
    await kernel.start();
    assert.deepEqual(await kernel.call("scripts", "completions", {}), first);
    assert.equal(sends, 2);
  } finally {
    release.resolve();
    if (child) {
      try {
        child.kill("SIGKILL");
      } catch { /* 已停 */ }
      await child.status;
      await stderr;
    }
    await kernel?.stop();
    await rpc.close();
    await actions.close();
    await timeline.close();
    await reading.close();
    await platform.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
