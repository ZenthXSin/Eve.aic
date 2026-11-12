import assert from "node:assert/strict";
import { qqModelCommand, QqModelCommands } from "../adapters/qq/model-command.ts";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { RuntimeModelControlClient } from "../services/runtime/model-control.ts";
import { KernelError } from "../kernel/protocol.ts";
import { startQqPhoneAdapter } from "../adapters/qq/service.ts";
import { PhoneRpcClient } from "../plugins/phone/client.ts";
import { QqReceiver } from "../adapters/qq/receiver.ts";

const KEY = "fixture_model_control_0123456789_abcdef";
function event(seq: number, text = "/model", owner = "owner", group?: string): QqDispatch {
  return {
    op: 0,
    s: seq,
    t: group ? "GROUP_AT_MESSAGE_CREATE" : "C2C_MESSAGE_CREATE",
    d: {
      id: `msg-${seq}`,
      author: group ? { member_openid: owner } : { user_openid: owner },
      ...(group ? { group_openid: group } : {}),
      content: text,
    },
  };
}
function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => resolve = done);
  return { resolve, promise };
}
async function until(check: () => boolean) {
  const deadline = Date.now() + 3000;
  while (!check()) {
    if (Date.now() > deadline) throw new Error("等待本机处理超时");
    await new Promise((r) => setTimeout(r, 5));
  }
}
async function fixture(configured = true) {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-model-" });
  const received: { content: string; msg_id?: string }[] = [];
  const selected: { actionId: string; model: string }[] = [];
  const results = new Map<string, unknown>();
  let available = ["model-b", "model-a"];
  let model = "model-a";
  let revision = 0;
  let statuses = 0;
  let rejectSelect = false;
  let corruptResult = false;
  let failNextSend = false;
  let beforeStatus: (() => Promise<void>) | undefined;
  let beforeSend: (() => Promise<void>) | undefined;
  const local = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const path = new URL(request.url).pathname;
    if (path === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture-token", expires_in: 7200 });
    }
    if (path === "/runtime-model-control") {
      assert.equal(request.headers.get("authorization"), `Bearer ${KEY}`);
      const { method, args } = await request.json();
      assert.equal(args.ownerPlatformUserId, "owner");
      if (method === "status") {
        statuses++;
        await beforeStatus?.();
        return Response.json({
          ok: true,
          value: { current: { model, revision }, availableModels: available },
        });
      }
      if (method === "receipt") {
        return Response.json(
          results.has(args.actionId)
            ? { ok: true, value: results.get(args.actionId) }
            : { ok: false, error: { kind: "not_found", message: "没有原回执" } },
        );
      }
      selected.push(args);
      model = args.model;
      revision++;
      const result = { ...args, revision, changed: true };
      results.set(args.actionId, result);
      if (rejectSelect) return new Response(null, { status: 503 });
      return Response.json({
        ok: true,
        value: corruptResult ? { ...result, model: "wrong-model" } : result,
      });
    }
    const body = await request.json();
    for (const field of ["msg_id", "msg_seq", "event_id"]) {
      assert.equal(field in body, false, `/model 回执必须主动发送，不带 ${field}`);
    }
    received.push(body);
    await beforeSend?.();
    if (failNextSend) {
      failNextSend = false;
      return Response.json({ code: 40034025, message: "fixture permanent failure" }, {
        status: 400,
      });
    }
    return Response.json({ id: `sent-${received.length}` });
  });
  const baseUrl = `http://127.0.0.1:${local.addr.port}`;
  await Deno.writeTextFile(
    `${directory}/connection.json`,
    JSON.stringify({ url: `${baseUrl}/runtime-model-control` }),
  );
  await Deno.writeTextFile(`${directory}/key`, KEY);
  const client = new RuntimeModelControlClient({
    connectionFile: `${directory}/connection.json`,
    keyFile: `${directory}/key`,
    ownerPlatformUserId: "owner",
  });
  const qq = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
    baseUrl,
    sendScope: "all",
  });
  const actions = new QqSendActions(qq, `${directory}/outbox`, "all", { retryDelaysMs: [1] });
  await actions.initialize();
  const journal = new QqInboundJournal(`${directory}/inbound`);
  await journal.initialize();
  const options = configured ? { ownerPlatformUserId: "owner", control: client } : {};
  let commands = new QqModelCommands(`${directory}/commands`, actions, options);
  await commands.initialize(journal.records());
  return {
    directory,
    received,
    selected,
    actions,
    journal,
    qq,
    commands: () => commands,
    statusCount: () => statuses,
    available: (next: string[]) => available = next,
    beforeStatus: (next?: () => Promise<void>) => beforeStatus = next,
    beforeSend: (next?: () => Promise<void>) => beforeSend = next,
    rejectSelect: () => rejectSelect = true,
    corruptResult: () => corruptResult = true,
    failNextSend: () => failNextSend = true,
    append: async (raw: QqDispatch) => {
      const { record } = await journal.append(qqMessage(raw)!, raw);
      commands.offer(record);
      return record;
    },
    restart: async () => {
      await commands.close();
      commands = new QqModelCommands(`${directory}/commands`, actions, options);
      await commands.initialize(journal.records());
    },
    close: async () => {
      actions.requestStop();
      qq.close();
      await commands.close();
      await actions.close();
      await local.shutdown();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("QQ /model：完整正文才识别，不从自然语言或附件提取命令", async () => {
  const f = await fixture();
  try {
    for (const text of ["请 /model", "测试/model model-b", "/models", "/model\nmodel-b"]) {
      assert.equal(
        qqModelCommand(await f.append(event(f.journal.records().length + 1, text))),
        undefined,
      );
    }
    const raw = event(5);
    (raw.d as Record<string, unknown>).attachments = [{
      content_type: "image/jpeg",
      filename: "/model model-b",
    }];
    assert.equal(qqModelCommand(await f.append(raw)), undefined);
    await f.commands().idle();
    assert.equal(f.statusCount(), 0);
    assert.equal(f.received.length, 0);
  } finally {
    await f.close();
  }
});
Deno.test("QQ /model：主人私聊查询及序号绑定已展示快照，重放与重启不再次切换", async () => {
  const f = await fixture();
  try {
    await f.append(event(1));
    await f.commands().idle();
    assert.equal(f.commands().receipt(f.journal.records()[0].identity)?.outcome, "listed");
    await until(() => f.received.length === 1 && f.actions.records()[0]?.status === "finished");
    f.available(["model-0", "model-a", "model-b"]);
    const record = await f.append(event(2, "/model 2"));
    await f.commands().idle();
    await until(() => f.received.length === 2);
    assert.equal(f.selected.length, 1);
    assert.equal(f.selected[0].model, "model-b");
    assert.equal(f.commands().receipt(record.identity)?.outcome, "selected");
    f.commands().offer(record);
    await f.commands().idle();
    await f.restart();
    await f.commands().idle();
    assert.equal(f.selected.length, 1);
    assert.equal(f.received.length, 2);
    assert.equal(f.journal.records()[1].message.message.text, "/model 2");
    assert.equal(f.commands().project(record).message.notify, false);
    assert.match(f.commands().project(record).message.message.text, /无需 Eve 重复执行/);
  } finally {
    await f.close();
  }
});
Deno.test("QQ /model：未绑定、非主人、群内同字符串身份均拒绝且不泄露列表", async () => {
  for (const configured of [true, false]) {
    const f = await fixture(configured);
    try {
      const records = [
        await f.append(event(1, "/model", "other")),
        await f.append(event(2, "<@!123456> /model model-b", "owner", "group")),
      ];
      if (!configured) records.push(await f.append(event(3, "/model", "owner")));
      await f.commands().idle();
      await until(() => f.received.length === records.length);
      assert.equal(f.statusCount(), 0);
      assert.equal(f.selected.length, 0);
      assert(
        f.received.every((r) => /未获授权/.test(r.content) && !r.content.includes("model-a")),
      );
      assert(
        records.every((record) => f.commands().receipt(record.identity)?.outcome === "denied"),
      );
    } finally {
      await f.close();
    }
  }
});
Deno.test("QQ /model：并发入站顺序切换，回复使用共享账本且不越过发送屏障", async () => {
  const f = await fixture();
  const gate = deferred();
  try {
    f.beforeSend(() => gate.promise);
    const old = f.actions.send("older-eve-action", {
      destination: { kind: "user", openId: "owner" },
      text: "原先消息",
      reply: { messageId: "older", sequence: 1 },
    });
    await until(() => f.received.length === 1);
    await Promise.all([f.append(event(1, "/model model-b")), f.append(event(2, "/model model-a"))]);
    await f.commands().idle();
    assert.deepEqual(f.selected.map((r) => r.model), ["model-b", "model-a"]);
    assert.equal(f.received.length, 1);
    gate.resolve();
    await old;
    await until(() => f.received.length === 3);
    assert.deepEqual(f.received.map((r) => r.msg_id), [undefined, undefined, undefined]);
  } finally {
    gate.resolve();
    await f.close();
  }
});
Deno.test("QQ /model：HTTP未知或不匹配成功体不冒称切换，重启不重新select", async () => {
  for (const corrupt of [false, true]) {
    const f = await fixture();
    try {
      if (corrupt) f.corruptResult();
      else f.rejectSelect();
      const record = await f.append(event(1, "/model model-b"));
      await f.commands().idle();
      await until(() => f.received.length === 1);
      assert.equal(f.commands().receipt(record.identity)?.outcome, "unknown");
      assert.match(f.received[0].content, /尚未确认/);
      await f.restart();
      await f.commands().idle();
      assert.equal(f.selected.length, 1);
      assert.equal(f.received.length, 1);
    } finally {
      await f.close();
    }
  }
});
Deno.test("QQ /model：首次启用不执行历史指令，存储未完成记录恢复仅receipt", async () => {
  const f = await fixture();
  try {
    const record = await f.append(event(1, "/model model-b"));
    await f.commands().idle();
    await until(() => f.received.length === 1);
    await f.commands().close();
    await Deno.writeTextFile(
      `${f.directory}/commands/${record.identity}.json`,
      JSON.stringify({
        schema: 1,
        identity: record.identity,
        inboundSeq: record.seq,
        argument: "model-b",
        phase: "selecting",
        model: "model-b",
      }),
    );
    // 保留原发送成功账本；恢复只取原runtime回执，原文相同不再发送。
    await f.restart();
    await f.commands().idle();
    assert.equal(f.selected.length, 1);
    assert.equal(f.commands().receipt(record.identity)?.outcome, "selected");
    assert.equal(f.received.length, 1);
  } finally {
    await f.close();
  }
});
Deno.test("QQ /model：接收器落盘后异步处理，慢模型不阻止第二条消息消费", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-command-receiver-" });
  const gate = deferred();
  const observed = deferred();
  const controller = new AbortController();
  const sockets: WebSocket[] = [];
  let persisted = 0;
  let heartbeat = 0;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    (request): Response => {
      const path = new URL(request.url).pathname;
      if (path === "/app/getAppAccessToken") {
        return Response.json({ access_token: "fixture", expires_in: 7200 });
      }
      if (path === "/gateway") {
        return Response.json({ url: `ws://127.0.0.1:${platform.addr.port}/socket` });
      }
      if (path === "/socket") {
        const { socket, response } = Deno.upgradeWebSocket(request);
        sockets.push(socket);
        socket.onopen = () =>
          socket.send(JSON.stringify({ op: 10, d: { heartbeat_interval: 20 } }));
        socket.onmessage = (message) => {
          const data = JSON.parse(message.data);
          if (data.op === 1) {
            heartbeat++;
            socket.send(JSON.stringify({ op: 11 }));
          } else if (data.op === 2) {
            socket.send(JSON.stringify({ op: 0, s: 0, t: "READY", d: { session_id: "session" } }));
            socket.send(JSON.stringify(event(1, "/model model-b")));
            socket.send(JSON.stringify(event(2, "第二条普通消息")));
          }
        };
        return response;
      }
      return Response.json({ id: "sent" });
    },
  );
  const client = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
    baseUrl: `http://127.0.0.1:${platform.addr.port}`,
  });
  const actions = new QqSendActions(client, `${directory}/outbox`);
  await actions.initialize();
  const commands = new QqModelCommands(`${directory}/commands`, actions, {
    ownerPlatformUserId: "owner",
    control: {
      status: async () => {
        await gate.promise;
        return {
          current: { model: "model-a", revision: 0 },
          availableModels: ["model-a", "model-b"],
        };
      },
      select: (actionId, model) => Promise.resolve({ actionId, model, revision: 1, changed: true }),
      receipt: () => Promise.reject(new KernelError("not_found", "没有原回执")),
    },
  });
  const receiver = new QqReceiver(client, directory, {
    initialized: (records) => commands.initialize(records),
    persisted: (record) => {
      commands.offer(record);
      if (++persisted === 2) observed.resolve();
    },
  });
  const running = receiver.run(controller.signal);
  try {
    await observed.promise;
    assert.equal(receiver.messages().length, 2);
    assert.equal(commands.receipt(receiver.messages()[0].identity)?.phase, "started");
    await until(() => heartbeat > 0);
    assert.equal(persisted, 2);
  } finally {
    commands.requestStop();
    gate.resolve();
    controller.abort();
    await running;
    actions.requestStop();
    await commands.close();
    client.close();
    await actions.close();
    for (const socket of sockets) socket.close();
    await platform.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ /model：启用边界跳过历史命令，未提交select的中断记录也不自动重放", async () => {
  const f = await fixture();
  let fresh: QqModelCommands | undefined;
  try {
    await f.commands().close();
    const oldRaw = event(1, "/model model-b");
    await f.journal.append(qqMessage(oldRaw)!, oldRaw);
    await Deno.remove(`${f.directory}/commands`, { recursive: true });
    const control = {
      status: () =>
        Promise.resolve({
          current: { model: "model-a", revision: 0 },
          availableModels: ["model-a", "model-b"],
        }),
      select: () => {
        throw new Error("禁止重放select");
      },
      receipt: () => Promise.reject(new KernelError("not_found", "原动作没有回执")),
    };
    fresh = new QqModelCommands(`${f.directory}/commands`, f.actions, {
      ownerPlatformUserId: "owner",
      control,
    });
    await fresh.initialize(f.journal.records());
    await fresh.idle();
    assert.equal(fresh.handled(f.journal.records()[0]), false);
    assert.equal(f.received.length, 0);
    await fresh.close();
    const newRaw = event(2, "/model model-b");
    const { record } = await f.journal.append(qqMessage(newRaw)!, newRaw);
    await Deno.writeTextFile(
      `${f.directory}/commands/${record.identity}.json`,
      JSON.stringify({
        schema: 1,
        identity: record.identity,
        inboundSeq: record.seq,
        argument: "model-b",
        phase: "selecting",
        model: "model-b",
      }),
    );
    fresh = new QqModelCommands(`${f.directory}/commands`, f.actions, {
      ownerPlatformUserId: "owner",
      control,
    });
    await fresh.initialize(f.journal.records());
    await fresh.idle();
    await until(() => f.received.length === 1);
    assert.equal(fresh.receipt(record.identity)?.outcome, "unknown");
    assert.match(f.received[0].content, /尚未确认/);
    assert.equal(f.selected.length, 0);
  } finally {
    await fresh?.close();
    await f.close();
  }
});

Deno.test("QQ /model：长模型列表回复有界，旧失败屏障阻止命令回执越过", async () => {
  const f = await fixture();
  try {
    // 一个不能重试的旧错误真实写入发送账本，后续命令仍可执行但不能越过回执屏障。
    f.failNextSend();
    const older = await f.actions.send("failed-before-model-command", {
      destination: { kind: "user", openId: "owner" },
      text: "旧消息",
      reply: { messageId: "old-msg", sequence: 1 },
    });
    assert.equal(older.status, "finished");
    f.available(
      Array.from(
        { length: 200 },
        (_, index) => `model-${String(index).padStart(3, "0")}-${"x".repeat(80)}`,
      ),
    );
    const record = await f.append(event(1));
    await f.commands().idle();
    await until(() => f.actions.status(`qq_model_reply_${record.identity}`)?.status === "blocked");
    assert.equal(f.received.length, 1);
    const saved = f.commands().receipt(record.identity)!;
    assert.equal(saved.outcome, "listed");
    assert(saved.reply!.length < 2000);
    assert(saved.availableModels!.length < 200);
    assert.match(saved.reply!, /其余/);
    const head = (await f.actions.listSendQueue({ failedHeadsOnly: true })).items[0];
    await f.actions.cancelSend({
      actionId: head.actionId,
      expectedFingerprint: head.fingerprint,
      expectedReceiptDigest: head.receiptDigest!,
    });
    // 无重启、无新推送，队首取消事件唤醒原命令回复；仍是同一动作且只主动发送一次。
    await until(() => {
      const status = f.actions.status(`qq_model_reply_${record.identity}`);
      return status?.status === "finished" && status.result.status === "sent";
    });
    assert.equal(f.received.length, 2);
    assert.equal(f.received[1].msg_id, undefined);
    assert.equal(
      f.actions.records().filter((r) => r.actionId === `qq_model_reply_${record.identity}`).length,
      1,
    );
    assert.equal(f.statusCount(), 1);
  } finally {
    await f.close();
  }
});

Deno.test("QQ /model：未启用服务不落盘或接管，后续显式启用跳过旧命令", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-model-enable-" });
  const sockets = new Set<WebSocket>();
  let lookups = 0;
  let sends = 0;
  let adapter: Awaited<ReturnType<typeof startQqPhoneAdapter>> | undefined;
  let rpc: PhoneRpcClient | undefined;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (request): Promise<Response> => {
      const path = new URL(request.url).pathname;
      if (path === "/app/getAppAccessToken") {
        return Response.json({
          access_token: "fixture-token",
          expires_in: 7200,
        });
      }
      if (path === "/users/@me") return Response.json({ id: "fixture-bot", username: "Eve" });
      if (path === "/gateway") {
        return Response.json({
          url: `ws://127.0.0.1:${platform.addr.port}/socket`,
        });
      }
      if (path.startsWith("/v2/")) {
        await request.json();
        return Response.json({ id: `sent-${++sends}` });
      }
      const { socket, response } = Deno.upgradeWebSocket(request);
      sockets.add(socket);
      socket.onclose = () => sockets.delete(socket);
      socket.onopen = () => socket.send(JSON.stringify({ op: 10, d: { heartbeat_interval: 100 } }));
      socket.onmessage = (message) => {
        const data = JSON.parse(message.data);
        if (data.op === 1) socket.send(JSON.stringify({ op: 11 }));
        else if (data.op === 2) {
          socket.send(JSON.stringify({ op: 0, t: "READY", s: 0, d: { session_id: "session" } }));
          socket.send(JSON.stringify(event(1)));
        } else if (data.op === 6) {
          assert.equal(data.d.seq, 1);
          socket.send(JSON.stringify({ op: 0, t: "RESUMED", s: 1, d: {} }));
          socket.send(JSON.stringify(event(2)));
        }
      };
      return response;
    },
  );
  const client = () =>
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    });
  async function history(count: number) {
    const deadline = Date.now() + 3000;
    while (Date.now() < deadline) {
      const result = await rpc!.call("history", { conversationId: "qq:user:owner" }) as {
        messages: { preview: string }[];
      };
      if (result.messages.length >= count) return result;
      await new Promise((r) => setTimeout(r, 5));
    }
    throw new Error("未看到真实本机入站");
  }
  try {
    adapter = await startQqPhoneAdapter(client(), directory, KEY);
    rpc = new PhoneRpcClient(adapter.url, KEY);
    await history(1);
    await assert.rejects(Deno.stat(`${directory}/model-commands`), Deno.errors.NotFound);
    assert.equal(sends, 0);
    rpc.close();
    rpc = undefined;
    await adapter.close();
    adapter = undefined;
    adapter = await startQqPhoneAdapter(client(), directory, KEY, undefined, undefined, {
      modelCommands: {
        ownerPlatformUserId: "owner",
        control: {
          status: () => {
            lookups++;
            return Promise.resolve({
              current: { model: "model-a", revision: 0 },
              availableModels: ["model-a", "model-b"],
            });
          },
          select: () => Promise.reject(new Error("只应查询")),
          receipt: () => Promise.reject(new Error("只应查询")),
        },
      },
    });
    rpc = new PhoneRpcClient(adapter.url, KEY);
    await history(2);
    await until(() => sends === 1);
    assert.equal(lookups, 1);
    const boundary = JSON.parse(
      await Deno.readTextFile(`${directory}/model-commands/boundary.json`),
    );
    assert.equal(boundary.throughSeq, 1);
    assert.equal(boundary.ownerPlatformUserId, "owner");
    const stored = new QqInboundJournal(`${directory}/inbound`);
    await stored.initialize();
    assert.deepEqual(stored.records().map((record) => record.message.message.text), [
      "/model",
      "/model",
    ]);
  } finally {
    rpc?.close();
    await adapter?.close();
    for (const socket of sockets) socket.close();
    await platform.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
