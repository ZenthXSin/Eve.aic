import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { startPhoneRpc } from "../adapters/qq/rpc.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import { composeEvePlugin, composeMindPlugin, writeEvePrompt } from "../composition/eve.ts";
import { composeModelPlugin } from "../composition/model.ts";
import { composePhonePlugin } from "../composition/phone.ts";
import { composeScriptPlugin } from "../composition/scripts.ts";
import { Kernel } from "../kernel/kernel.ts";
import type { Json } from "../kernel/protocol.ts";
import { KernelError } from "../kernel/protocol.ts";
import { type ConsciousEvent, ConsciousJournal } from "../plugins/eve/journal.ts";
import {
  accountingDay,
  budget,
  contextEstimate,
  pendingTools,
  render,
} from "../plugins/eve/view.ts";

function event(seq: number, user = "user-a", text = `实际消息${seq}`): QqDispatch {
  return {
    op: 0,
    s: seq,
    t: "C2C_MESSAGE_CREATE",
    d: { id: `platform-${seq}`, content: text, author: { user_openid: user, username: "对方" } },
  };
}
function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { resolve, promise };
}
async function waitFor(check: () => boolean | Promise<boolean>) {
  for (let i = 0; i < 1000; i++) {
    if (await check()) return;
    await new Promise((done) => setTimeout(done, 10));
  }
  throw new Error("意识流10秒内未完成实际观察");
}
const completion = (content = "这一步先休息", usage?: unknown) => ({
  choices: [{ finish_reason: "stop", message: { role: "assistant", content } }],
  ...(usage ? { usage } : {}),
});
const script = (code: string) => ({
  choices: [{
    finish_reason: "tool_calls",
    message: {
      role: "assistant",
      tool_calls: [{
        id: "upstream-reused-short-id",
        type: "function",
        function: { name: "execute_script", arguments: JSON.stringify({ code, mode: "sync" }) },
      }],
    },
  }],
});
async function fixture(
  handler: (body: Record<string, Json>, index: number) => Response | Promise<Response>,
  limit = 200_000_000,
  imageDescription?: string,
) {
  const directory = await Deno.makeTempDir({ prefix: "eve-consciousness-" });
  let sends = 0;
  const messages: Record<string, Json>[] = [];
  const qq = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    () => Response.json({ id: `actual-sent-${++sends}` }),
  );
  // 认证独立响应，不把凭据请求计作发送。
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    (r) =>
      new URL(r.url).pathname === "/app/getAppAccessToken"
        ? Response.json({ access_token: "fixture", expires_in: 7200 })
        : fetch(`http://127.0.0.1:${qq.addr.port}`, { method: "POST" }),
  );
  const model = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    const body = await r.json();
    assert.equal(body.temperature, undefined);
    assert.equal(body.reasoning_effort, "high");
    assert.equal(r.headers.get("session_id"), "eve-one-brain-test");
    messages.push(body);
    return handler(body, messages.length);
  });
  const journal = new QqInboundJournal(`${directory}/qq/incoming`);
  await journal.initialize();
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    }),
    `${directory}/qq/outbox`,
  );
  await actions.initialize();
  const timeline = new QqTimeline(`${directory}/qq/timeline`);
  await timeline.initialize();
  const reading = new QqPhoneReading(`${directory}/phone-reading`);
  await reading.initialize();
  const key = "fixture_conscious_phone_key_01234567890123456789";
  let unavailable = false;
  const phone = new QqPhone(() => journal.records(), actions, timeline, {
    id: "fixture-bot",
    displayName: "Eve",
  }, reading);
  const notifications = phone.notifications.bind(phone);
  phone.notifications = (args) => {
    if (unavailable) throw new Error("fixture phone unavailable");
    return notifications(args);
  };
  const rpc = startPhoneRpc(
    phone,
    key,
  );
  await Deno.writeTextFile(`${directory}/phone-key`, key, { mode: 0o600 });
  await Deno.writeTextFile(`${directory}/model-key`, "fixture", { mode: 0o600 });
  const prompt = `${directory}/prompt.md`;
  await writeEvePrompt(prompt);
  const configPath = `${directory}/config.json`;
  const config = {
    phone: {
      adapterUrl: rpc.url,
      adapterKeyFile: `${directory}/phone-key`,
      ownerPlatformUserId: "user-a",
    },
    model: {
      baseUrl: `http://127.0.0.1:${model.addr.port}`,
      model: "fixture",
      protocol: "chat",
      apiKeyFile: `${directory}/model-key`,
      reasoningEffort: "high",
      sessionId: "eve-one-brain-test",
      maxOutputTokens: 8192,
      timeoutMs: 10000,
    },
    scripts: {
      directory: `${directory}/scripts`,
      cacheDirectory: `${directory}/cache`,
      denoBinary: Deno.execPath(),
      defaultTimeoutMs: 10000,
    },
    eve: {
      directory: `${directory}/consciousness`,
      promptFile: prompt,
      sourceCommit: "588c628bf0e1a691a46173c662c8f53958b53871",
      modelTimeoutMs: 15000,
      maxOutputTokens: 8192,
      pollMs: 20,
      defaultSleepMs: 86400000,
      dailyTokenBudget: limit,
    },
  };
  await Deno.writeTextFile(configPath, JSON.stringify(config), { mode: 0o600 });
  await composeMindPlugin(`${directory}/mind-plugin`, `${directory}/mind`);
  await composePhonePlugin(`${directory}/phone-plugin`, configPath);
  await composeModelPlugin(`${directory}/model-plugin`, configPath);
  if (imageDescription) {
    await Deno.writeTextFile(
      `${directory}/model-plugin/mod.ts`,
      `import {createChatModelPlugin} from ${
        JSON.stringify(new URL("../plugins/model-chat/mod.ts", import.meta.url).href)
      };
const plugin=createChatModelPlugin(${
        JSON.stringify(configPath)
      });const load=plugin.load;plugin.load=(ctx)=>load({...ctx,provide:(contract,handlers)=>ctx.provide(contract,{...handlers,describeImage:(args)=>({description:${
        JSON.stringify(imageDescription)
      },provenance:args.image.provenance})})});export default plugin;`,
    );
  }

  await composeScriptPlugin(`${directory}/scripts-plugin`, configPath);
  await composeEvePlugin(`${directory}/eve-plugin`, configPath);
  let kernel = new Kernel({ dataDir: `${directory}/kernel`, callTimeoutMs: 20000 });
  const start = async () => {
    for (const name of ["eve", "scripts", "mind", "model", "phone"]) {
      await kernel.register(`${directory}/${name}-plugin`);
    }
    await kernel.start();
  };
  await start();
  const records = async () => {
    const files: string[] = [];
    for await (const entry of Deno.readDir(`${directory}/consciousness/events`)) {
      files.push(entry.name);
    }
    files.sort();
    return Promise.all(
      files.map(async (file) =>
        JSON.parse(
          await Deno.readTextFile(`${directory}/consciousness/events/${file}`),
        ) as ConsciousEvent
      ),
    );
  };
  return {
    directory,
    messages,
    journal,
    phone,
    actions,
    kernel: () => kernel,
    records,
    count: () => sends,
    outage: (value: boolean) => {
      unavailable = value;
    },
    append: async (seq: number, user = "user-a", text?: string) => {
      const raw = event(seq, user, text);
      await journal.append(qqMessage(raw)!, raw);
    },
    pulse: () => kernel.call("eve", "pulse"),
    restart: async () => {
      assert.deepEqual(await kernel.stop(), []);
      kernel = new Kernel({ dataDir: `${directory}/kernel`, callTimeoutMs: 20000 });
      await start();
    },
    close: async () => {
      assert.deepEqual(await kernel.stop(), []);
      await rpc.close();
      await actions.close();
      await timeline.close();
      await reading.close();
      await model.shutdown();
      await platform.shutdown();
      await qq.shutdown();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("意识流：真实模型HTTP→原生脚本→手机动作，回复先存档，重启采用旧动作且新消息进同一脑子", async () => {
  const f = await fixture((_body, index) =>
    Response.json(
      index === 1
        ? script(
          'return await tools.phone.send({conversationId:"qq:user:user-a",text:"实际通过手机发出",replyTo:{seq:1,sequence:1}});',
        )
        : completion(),
    )
  );
  try {
    await f.append(1);
    await f.pulse();
    const first = await f.records();
    assert.equal(first.some((e) => e.kind === "model_response"), true);
    assert.equal(f.count(), 0);
    assert.equal(pendingTools(first).length, 1);
    await f.restart();
    await f.pulse();
    assert.equal(f.count(), 1);
    const after = await f.records();
    assert.equal(pendingTools(after).length, 0);
    assert.equal(after.filter((e) => e.kind === "tool_result").length, 1);
    await f.restart();
    await f.pulse();
    assert.equal(f.count(), 1);
    const beforeNext = f.messages.length;
    await f.append(2, "other", "另一人发来的真实消息");
    await f.pulse();
    assert.equal(f.messages.length, beforeNext + 1);
    const original = f.messages[0].messages as Json[];
    const latest = f.messages.at(-1)!.messages as Json[];
    assert.deepEqual(latest.slice(0, original.length), original);
    assert.equal(JSON.stringify(latest).includes("另一人发来的真实消息"), true);
    assert.equal(
      latest.some((message) =>
        String((message as { content: string | null }).content ?? "").includes(
          '"relationship":"other"',
        )
      ),
      true,
    );
    assert.equal(
      latest[0] && (latest[0] as { content: string }).content.includes("你是 Eve"),
      true,
    );
    assert.equal(f.count(), 1);
    const quiet = f.messages.length;
    await f.pulse();
    await f.pulse();
    assert.equal(f.messages.length, quiet);
  } finally {
    await f.close();
  }
});

Deno.test("意识流：处理中的模型不被新私信打断，并发pulse只有一次真实模型调用", async () => {
  const entered = deferred(), release = deferred();
  const f = await fixture(async () => {
    entered.resolve();
    await release.promise;
    return Response.json(completion());
  });
  try {
    await f.append(1);
    const first = f.pulse();
    await entered.promise;
    await f.append(2, "user-a", "正在处理时补充");
    const same = f.pulse();
    assert.equal(f.messages.length, 1);
    release.resolve();
    await Promise.all([first, same]);
    assert.equal(JSON.stringify(f.messages[0]).includes("正在处理时补充"), false);
    await f.pulse();
    assert.equal(f.messages.length, 2);
    assert.equal(JSON.stringify(f.messages[1]).includes("正在处理时补充"), true);
    const notices = (await f.records()).filter((e) =>
      e.kind === "notice" && (e.data as { source: string }).source === "phone"
    );
    assert.equal(notices.length, 2);
  } finally {
    release.resolve();
    await f.close();
  }
});

Deno.test("意识流：发送已经结束却脑子未收尾，恢复查询原脚本而不重发", async () => {
  const f = await fixture(() =>
    Response.json(
      script('return await tools.phone.send({conversationId:"qq:user:user-a",text:"原动作"});'),
    )
  );
  try {
    await f.pulse();
    const events = await f.records();
    const pending = pendingTools(events)[0];
    // 外部先执行同一已保存决定，模拟动作已做而意识流未拿到结果的窗口。
    await f.kernel().call("tools", "execute", pending.call as unknown as Json);
    assert.equal(f.count(), 1);
    await f.kernel().stop();
    const raw = new ConsciousJournal(`${f.directory}/consciousness`);
    await raw.initialize();
    await raw.append("tool_start", {
      responseSeq: pending.responseSeq,
      index: 0,
      callId: pending.call.id,
      call: pending.call as unknown as Json,
    });
    await raw.close();
    await f.restart();
    await f.pulse();
    assert.equal(f.count(), 1);
    const result = (await f.records()).find((e) => e.kind === "tool_result")!;
    assert.equal((result.data as { result: { adopted: boolean } }).result.adopted, true);
  } finally {
    await f.close();
  }
});

Deno.test("意识流：模型失败保留新消息与有限退避，恢复后不丢失通知或重复发送", async () => {
  const f = await fixture((_body, index) =>
    index === 1 ? new Response("fixture unavailable", { status: 503 }) : Response.json(completion())
  );
  try {
    await f.append(1);
    await f.pulse();
    assert.equal(f.messages.length, 1);
    assert.equal((await f.records()).some((e) => e.kind === "model_failure"), true);
    await f.pulse();
    assert.equal(f.messages.length, 1);
    await new Promise((done) => setTimeout(done, 2100));
    await f.pulse();
    assert.equal(f.messages.length, 2);
    assert.equal(JSON.stringify(f.messages[1]).includes("实际消息1"), true);
    assert.equal(f.count(), 0);
  } finally {
    await f.close();
  }
});

Deno.test("意识流：永久上游拒绝不会因新QQ或重启重试，显式恢复才允许新请求", async () => {
  const f = await fixture((_body, index) =>
    index === 1
      ? new Response("provider private content", { status: 400 })
      : Response.json(completion())
  );
  try {
    await f.append(1);
    await f.pulse();
    const status = await f.kernel().call("eve", "status") as {
      modelBlocked: { requestId: string; httpStatus: number };
    };
    assert.equal(status.modelBlocked.httpStatus, 400);
    assert.equal(f.messages.length, 1);
    await f.append(2);
    await f.pulse();
    await f.restart();
    await f.pulse();
    assert.equal(f.messages.length, 1);
    assert.equal((await f.records()).filter((e) => e.kind === "model_failure").length, 1);
    assert.equal(JSON.stringify(await f.records()).includes("provider private content"), false);
    await assert.rejects(
      () => f.kernel().call("eve", "resumeModel", { expectedRequestId: "model_99999" }),
      KernelError,
    );
    await f.kernel().call("eve", "resumeModel", {
      expectedRequestId: status.modelBlocked.requestId,
    });
    await f.pulse();
    assert.equal(f.messages.length, 2);
    assert.equal(JSON.stringify(f.messages[1]).includes("实际消息2"), true);
    assert.equal(f.count(), 0);
  } finally {
    await f.close();
  }
});

Deno.test("意识流：明确缓存不计日预算，缺失缓存或用量保留未知，预算不足不调用模型", async () => {
  const f = await fixture(() =>
    Response.json(
      completion("实际文字", {
        prompt_tokens: 100,
        completion_tokens: 10,
        prompt_tokens_details: { cached_tokens: 90 },
      }),
    )
  );
  try {
    await f.pulse();
    const state = await f.kernel().call<
      { budget: { knownNoncachedTokens: number; unknownReservedTokens: number } }
    >("eve", "status");
    assert.equal(state.budget.knownNoncachedTokens, 20);
    assert.equal(state.budget.unknownReservedTokens, 0);
  } finally {
    await f.close();
  }
  const blocked = await fixture(() => {
    throw new Error("不该调用模型");
  }, 10);
  try {
    await blocked.pulse();
    assert.equal(blocked.messages.length, 0);
    assert.equal((await blocked.records()).some((e) => e.kind === "budget_wait"), true);
  } finally {
    await blocked.close();
  }
  const unknown = await fixture(() => Response.json(completion()));
  try {
    await unknown.pulse();
    const usage = budget(await unknown.records(), accountingDay());
    assert.equal(usage.knownNoncachedTokens, 0);
    assert.ok(usage.unknownReservedTokens > 0);
  } finally {
    await unknown.close();
  }
  const partial = await fixture(() =>
    Response.json(completion("实际输出", { prompt_tokens: 100, completion_tokens: 10 }))
  );
  try {
    await partial.pulse();
    const usage = budget(await partial.records(), accountingDay());
    assert.equal(usage.knownNoncachedTokens, 10);
    assert.equal(usage.unknownReservedTokens, 100);
  } finally {
    await partial.close();
  }
});

Deno.test("意识流：损坏或未完成记录阻止恢复，原件保留，第二脑子不能持同一流", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-conscious-journal-" });
  const journal = new ConsciousJournal(directory);
  try {
    await journal.initialize();
    const event = await journal.append("notice", { source: "test", text: "原件" });
    const second = new ConsciousJournal(directory);
    await assert.rejects(second.initialize(), KernelError);
    await journal.close();
    const path = `${directory}/events/0000000000000001.json`;
    const original = await Deno.readTextFile(path);
    await Deno.writeTextFile(path, JSON.stringify({ ...event, data: { source: "tampered" } }));
    const broken = new ConsciousJournal(directory);
    await assert.rejects(broken.initialize(), KernelError);
    assert.equal(JSON.parse(await Deno.readTextFile(path)).data.source, "tampered");
    await Deno.writeTextFile(path, original);
    await Deno.writeTextFile(`${directory}/events/unfinished.tmp`, "retain");
    const unfinished = new ConsciousJournal(directory);
    await assert.rejects(unfinished.initialize(), KernelError);
    assert.equal(await Deno.readTextFile(`${directory}/events/unfinished.tmp`), "retain");
  } finally {
    await journal.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("意识流：持续运行由新消息唤醒，普通模型文字不自动发送，无消息时不空转调用", async () => {
  const f = await fixture(() => Response.json(completion("这个念头留在意识里")));
  try {
    await f.kernel().call("eve", "start");
    await waitFor(() => f.messages.length === 1);
    await new Promise((done) => setTimeout(done, 150));
    assert.equal(f.messages.length, 1);
    assert.equal(f.count(), 0);
    await f.append(1);
    await waitFor(() => f.messages.length === 2);
    await new Promise((done) => setTimeout(done, 150));
    assert.equal(f.messages.length, 2);
    assert.equal(f.count(), 0);
  } finally {
    await f.close();
  }
});

Deno.test("意识流：Eve通过真实脚本决定休息，持久提醒到时再唤醒，已观察完成通知不反复触发", async () => {
  const f = await fixture((_body, index) =>
    Response.json(
      index === 1
        ? script(
          'await tools.mind.remind({at:new Date(Date.now()+200).toISOString(),text:"我想回来接着看"}); return await tools.mind.rest({wakeAt:new Date(Date.now()+60000).toISOString()});',
        )
        : completion(),
    )
  );
  try {
    await f.pulse();
    await f.pulse();
    await f.pulse();
    assert.equal(f.messages.length, 1);
    const status = await f.kernel().call<{ wakeAt: number }>("eve", "status");
    assert.ok(status.wakeAt > Date.now());
    await new Promise((done) => setTimeout(done, 300));
    await f.pulse();
    assert.equal(f.messages.length, 2);
    assert.equal(JSON.stringify(f.messages[1]).includes("我想回来接着看"), true);
    const before = await f.records();
    assert.equal(
      before.filter((event) =>
        event.kind === "notice" && (event.data as { source: string }).source === "reminder"
      ).length,
      1,
    );
    await f.restart();
    await f.pulse();
    assert.equal(f.messages.length, 2);
    assert.equal(f.count(), 0);
    const after = await f.records();
    assert.equal(
      after.filter((event) =>
        event.kind === "notice" && (event.data as { source: string }).source === "reminder"
      ).length,
      1,
    );
  } finally {
    await f.close();
  }
});

Deno.test("意识流：无8次请求上限，重复上游短编号被持久响应编号区分", async () => {
  const f = await fixture((_body, index) =>
    Response.json(index <= 9 ? script(`return ${index};`) : completion())
  );
  try {
    for (let i = 0; i < 9; i++) {
      await f.pulse();
      await f.pulse();
    }
    await f.pulse();
    assert.equal(f.messages.length, 10);
    const events = await f.records();
    const started = events.filter((event) => event.kind === "tool_start");
    assert.equal(started.length, 9);
    const ids = started.map((event) => (event.data as { callId: string }).callId);
    assert.equal(new Set(ids).size, 9);
    assert.equal(f.count(), 0);
  } finally {
    await f.close();
  }
});

Deno.test("意识流：回看自写摘要只改变视图，原文及工具配对保留，预算原件仍有效", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-conscious-view-" });
  const journal = new ConsciousJournal(directory);
  try {
    await journal.initialize();
    const notice = await journal.append("notice", { source: "phone", items: [{ text: "原话" }] });
    const response = await journal.append("model_response", {
      requestId: "model",
      response: {
        content: "我去看",
        toolCalls: [{ id: "x", name: "execute_script", args: { code: "return 1;" } }],
      },
    });
    await journal.append("tool_result", {
      callId: `eve_${response.seq}_0`,
      result: { status: "completed", output: "真实结果" },
    });
    const full = render(journal.events, "稳定身份");
    assert.equal(full[0].content, "稳定身份");
    assert.equal(full.at(-1)?.role, "tool");
    await journal.append("compression", {
      throughSeq: notice.seq,
      summary: "我记得对方说过这句话",
      source: "eve",
    });
    const compressed = render(journal.events, "稳定身份");
    assert.ok(compressed[1].content.includes("我记得"));
    assert.equal(compressed.at(-1)?.role, "tool");
    assert.equal(JSON.stringify(journal.events).includes("原话"), true);
    assert.deepEqual(compressed.at(-1), full.at(-1));
  } finally {
    await journal.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("意识流：手机依赖故障保留私信，恢复后只处理一次，恢复事件不撤销休息", async () => {
  const f = await fixture(() => Response.json(completion()));
  try {
    await f.pulse();
    f.outage(true);
    await f.append(1, "user-a", "恢复后仍应该看到我");
    await f.pulse();
    await f.pulse();
    assert.equal(f.messages.length, 1);
    assert.equal((await f.records()).filter((event) => event.kind === "channel_failure").length, 1);
    await f.restart();
    f.outage(false);
    await f.pulse();
    assert.equal(f.messages.length, 2);
    assert.ok(JSON.stringify(f.messages[1]).includes("恢复后仍应该看到我"));
    await f.pulse();
    await f.pulse();
    assert.equal(f.messages.length, 2);
    assert.equal(
      (await f.records()).filter((event) => event.kind === "channel_recovered").length,
      1,
    );
    assert.equal(f.count(), 0);
  } finally {
    f.outage(false);
    await f.close();
  }
});

Deno.test("意识流：单个超大片段按完整工具对机械移出，原件保留且最终请求低于估算线", async () => {
  const f = await fixture(() => Response.json(completion()));
  try {
    assert.deepEqual(await f.kernel().stop(), []);
    const raw = new ConsciousJournal(`${f.directory}/consciousness`);
    await raw.initialize();
    const response = await raw.append("model_response", {
      requestId: "large-prior-result",
      response: {
        content: "先前完整动作",
        toolCalls: [{ id: "old", name: "execute_script", args: { code: "return 1;" } }],
      },
    });
    const large = await raw.append("tool_result", {
      callId: `eve_${response.seq}_0`,
      result: { status: "completed", output: "原".repeat(550_000) },
    });
    const original = await Deno.readTextFile(
      `${f.directory}/consciousness/events/${String(large.seq).padStart(16, "0")}.json`,
    );
    await raw.close();
    await f.restart();
    await f.pulse();
    assert.equal(f.messages.length, 1);
    const body = f.messages[0];
    assert.ok(contextEstimate(body.messages as never, body.tools as never) < 500_000);
    assert.ok(JSON.stringify(body).includes("系统机械压缩，非 Eve 自己写"));
    assert.equal(JSON.stringify(body).includes(`eve_${response.seq}_0`), false);
    assert.equal(
      await Deno.readTextFile(
        `${f.directory}/consciousness/events/${String(large.seq).padStart(16, "0")}.json`,
      ),
      original,
    );
    assert.equal((await f.records()).filter((event) => event.kind === "compression").length, 1);
  } finally {
    await f.close();
  }
});

Deno.test("意识流：锚定身份本身超大时停止请求，不删除身份或循环追加机械压缩", async () => {
  const f = await fixture(() => Response.json(completion()));
  try {
    await Deno.writeTextFile(`${f.directory}/prompt.md`, "身".repeat(510_000));
    await f.restart();
    await f.pulse();
    await f.pulse();
    assert.equal(f.messages.length, 0);
    assert.equal(f.count(), 0);
    const state = await f.kernel().call<{ contextBlocked: boolean }>("eve", "status");
    assert.equal(state.contextBlocked, true);
    const records = await f.records();
    assert.equal(records.filter((event) => event.kind === "compression").length, 1);
    assert.equal(records.filter((event) => event.kind === "context_wait").length, 1);
    assert.equal((await Deno.readTextFile(`${f.directory}/prompt.md`)).length, 510_000);
  } finally {
    await f.close();
  }
});

Deno.test("意识流：工具恢复故障通知保留时间与来源，渲染时不插进调用与回执之间", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-recovery-view-" });
  const journal = new ConsciousJournal(directory);
  try {
    await journal.initialize();
    const response = await journal.append("model_response", {
      requestId: "two-tools",
      response: {
        content: "先做两件事",
        toolCalls: [
          { id: "one", name: "execute_script", args: { code: "return 1;" } },
          { id: "two", name: "execute_script", args: { code: "return 2;" } },
        ],
      },
    });
    const failure = await journal.append("channel_failure", {
      errorKind: "unavailable",
      reason: "原服务暂时不可用",
    });
    await journal.append("tool_result", {
      callId: `eve_${response.seq}_0`,
      result: { status: "completed", output: "1" },
    });
    await journal.append("tool_result", {
      callId: `eve_${response.seq}_1`,
      result: { status: "completed", output: "2" },
    });
    const messages = render(journal.events, "固定身份");
    assert.deepEqual(messages.map((m) => m.role), ["system", "assistant", "tool", "tool", "user"]);
    assert.equal(messages[2].toolCallId, messages[1].toolCalls![0].id);
    assert.equal(messages[3].toolCallId, messages[1].toolCalls![1].id);
    const notice = JSON.parse(messages[4].content);
    assert.equal(notice.at, failure.at);
    assert.equal(notice.record, failure.seq);
    assert.equal(notice.kind, "channel_failure");
    assert.ok(failure.seq < journal.events.at(-1)!.seq);
  } finally {
    await journal.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("意识流：手机模型描述进入下一轮主意识文字，原图不进入Eve人格上下文", async () => {
  const payload =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aMRsAAAAASUVORK5CYII=";
  const bytes = Uint8Array.from(atob(payload), (c) => c.charCodeAt(0));
  const sha256 = Array.from(
    new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
    (n) => n.toString(16).padStart(2, "0"),
  ).join("");
  const f = await fixture(
    (_body, index) =>
      Response.json(
        index === 1
          ? script(
            'await tools.phone.image({handle:{conversationId:"qq:user:user-a",seq:1},index:1}); return "我已经点开了图片";',
          )
          : completion(),
      ),
    200_000_000,
    "客观描述：白色背景中有一个小圆点；这是外部图片的模型描述，细节可能有误",
  );
  const source = {
    kind: "phone_attachment" as const,
    conversationId: "qq:user:user-a",
    sourceSeq: 1,
    attachmentId: "qq:user:user-a:1:0",
    senderPlatformUserId: "user-a",
  };
  let reads = 0;
  f.phone.image = () => {
    reads++;
    return Promise.resolve({
      image: {
        dataUrl: `data:image/png;base64,${payload}`,
        provenance: { ...source, sha256, mimeType: "image/png" as const },
      },
      download: {
        path: "/fixture/image.png",
        sizeBytes: bytes.length,
        sha256,
        mimeType: "image/png",
        source,
      },
    });
  };
  try {
    await f.pulse();
    await f.pulse();
    await f.pulse();
    assert.equal(reads, 1);
    assert.equal(f.messages.length, 2);
    const messages = f.messages[1].messages as unknown as {
      role: string;
      content: unknown;
      images?: unknown;
    }[];
    assert.equal(JSON.stringify(messages).includes(payload), false);
    assert.equal(messages.some((message) => message.images !== undefined), false);
    assert.equal(JSON.stringify(messages).includes("image_url"), false);
    assert.ok(JSON.stringify(messages).includes("客观描述：白色背景中有一个小圆点"));
    assert.ok(JSON.stringify(messages).includes("phone_attachment"));
    const imageNotices = (await f.records()).filter((e) =>
      e.kind === "notice" && (e.data as { source?: string }).source === "image_description"
    );
    assert.equal(imageNotices.length, 1);
    await f.restart();
    await f.pulse();
    assert.equal(reads, 1);
    assert.equal(f.messages.length, 2);
    assert.equal(
      (await f.records()).filter((e) =>
        e.kind === "notice" && (e.data as { source?: string }).source === "image_description"
      ).length,
      1,
    );
  } finally {
    await f.close();
  }
});

Deno.test("意识流：Eve明确选择original时才将真实原图交给主模型，文本不重复base64", async () => {
  const payload =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aMRsAAAAASUVORK5CYII=";
  const bytes = Uint8Array.from(atob(payload), (c) => c.charCodeAt(0));
  const sha256 = Array.from(
    new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
    (n) => n.toString(16).padStart(2, "0"),
  ).join("");
  const f = await fixture((_body, index) =>
    Response.json(
      index === 1
        ? script(
          'await tools.phone.image({handle:{conversationId:"qq:user:user-a",seq:1},index:1,mode:"original"}); return "我选了看原图";',
        )
        : completion(),
    )
  );
  const source = {
    kind: "phone_attachment" as const,
    conversationId: "qq:user:user-a",
    sourceSeq: 1,
    attachmentId: "qq:user:user-a:1:0",
    senderPlatformUserId: "user-a",
  };
  let reads = 0;
  f.phone.image = () => {
    reads++;
    return Promise.resolve({
      image: {
        dataUrl: `data:image/png;base64,${payload}`,
        provenance: { ...source, sha256, mimeType: "image/png" as const },
      },
      download: {
        path: "/fixture/image.png",
        sizeBytes: bytes.length,
        sha256,
        mimeType: "image/png",
        source,
      },
    });
  };
  try {
    await f.pulse();
    await f.pulse();
    await f.pulse();
    assert.equal(reads, 1);
    assert.equal(f.messages.length, 2);
    const messages = f.messages[1].messages as unknown as {
      role: string;
      content: string | { type: string; text?: string; image_url?: { url: string } }[];
    }[];
    const imageMessage = messages.find((m) =>
      Array.isArray(m.content) && m.content.some((p) => p.type === "image_url")
    );
    assert.ok(imageMessage);
    assert.equal(imageMessage.role, "user");
    const parts = imageMessage.content as {
      type: string;
      text?: string;
      image_url?: { url: string };
    }[];
    assert.equal(
      parts.find((p) => p.type === "image_url")?.image_url?.url,
      `data:image/png;base64,${payload}`,
    );
    assert.equal(
      parts.filter((p) => p.type === "text").some((p) => p.text?.includes(payload)),
      false,
    );
    assert.ok(JSON.stringify(parts).includes("Eve明确选择"));
    await f.restart();
    await f.pulse();
    assert.equal(reads, 1);
    assert.equal(f.messages.length, 2);
  } finally {
    await f.close();
  }
});
