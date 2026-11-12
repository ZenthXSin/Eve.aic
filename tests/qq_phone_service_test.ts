import assert from "node:assert/strict";
import { readQqTestClient } from "../composition/qq.ts";
import { QqRequestError } from "../adapters/qq/client.ts";
import { startQqPhoneAdapter } from "../adapters/qq/service.ts";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { startPhoneRpc } from "../adapters/qq/rpc.ts";
import { QqTimeline, QqTimelineError } from "../adapters/qq/timeline.ts";
import { composePhonePlugin } from "../composition/phone.ts";
import { Kernel } from "../kernel/kernel.ts";
import { PhoneRpcClient, PhoneRpcError } from "../plugins/phone/client.ts";
import { KernelError } from "../sdk/plugin.ts";
import type { OpenedConversation } from "../contracts/phone.ts";

const KEY = "fixture_phone_connection_key_0123456789";
function event(seq: number, user = "user-a", text = `消息-${seq}`, group?: string): QqDispatch {
  return {
    op: 0,
    s: seq,
    t: group ? "GROUP_MESSAGE_CREATE" : "C2C_MESSAGE_CREATE",
    d: {
      id: `platform-${seq}`,
      content: text,
      timestamp: "2026-11-12T01:00:00Z",
      author: group
        ? { member_openid: user, username: "群友", isOwner: true }
        : { user_openid: user, username: "主人", isOwner: true },
      ...(group ? { group_openid: group } : {}),
    },
  };
}
async function fixture(options: { attachmentReader?: (path: string) => Promise<Uint8Array> } = {}) {
  const directory = await Deno.makeTempDir({ prefix: "eve-phone-service-" });
  let sends = 0;
  let result: unknown = { id: "actual-sent-id" };
  const received: unknown[] = [];
  let beforeReply: (() => Promise<void>) | undefined;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (r): Promise<Response> => {
      if (new URL(r.url).pathname === "/app/getAppAccessToken") {
        return Response.json({ access_token: "fixture-token", expires_in: 7200 });
      }
      sends++;
      received.push(await r.json());
      await beforeReply?.();
      return Response.json(
        new URL(r.url).pathname.endsWith("/files") ? { file_info: "fixture-upload-info" } : result,
      );
    },
  );
  const journal = new QqInboundJournal(`${directory}/incoming`);
  await journal.initialize();
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
      attachmentReader: options.attachmentReader,
    }),
    `${directory}/actions`,
  );
  await actions.initialize();
  const timeline = new QqTimeline(`${directory}/timeline`);
  await timeline.initialize();
  const reading = new QqPhoneReading(`${directory}/phone-reading`);
  await reading.initialize();
  const phone = new QqPhone(() => journal.records(), actions, timeline, {
    id: "fixture-bot",
    displayName: "Eve",
  }, reading);
  const rpc = startPhoneRpc(phone, KEY);
  const client = new PhoneRpcClient(rpc.url, KEY);
  return {
    directory,
    journal,
    actions,
    timeline,
    reading,
    phone,
    rpc,
    client,
    received,
    count: () => sends,
    result: (next: unknown) => {
      result = next;
    },
    beforeReply: (next: () => Promise<void>) => {
      beforeReply = next;
    },
    append: (raw: QqDispatch) => journal.append(qqMessage(raw)!, raw),
    close: async (expectedTimelineFailure = false, expectedReadingFailure = false) => {
      client.close();
      await rpc.close();
      await actions.close();
      try {
        await timeline.close();
      } catch (error) {
        if (!expectedTimelineFailure) throw error;
      }
      try {
        await reading.close();
      } catch (error) {
        if (!expectedReadingFailure) throw error;
      }
      await platform.shutdown();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("QQ 手机：打开最新屏只消除本会话通知，上方未展示消息不冒充已读，重启保持水位", async () => {
  const f = await fixture();
  let restored: QqPhoneReading | undefined;
  try {
    for (let seq = 1; seq <= 18; seq++) await f.append(event(seq));
    await f.append(event(19, "user-b"));
    const firstNotices = await f.phone.notifications();
    assert.deepEqual(firstNotices.items.map((m) => m.handle.seq), [1, 2, 3, 4, 5, 6, 7, 8]);
    const history = await f.phone.history({ conversationId: "qq:user:user-a" });
    await f.phone.history({ conversationId: "qq:user:user-a", cursor: history.older });
    await f.phone.search({ query: "消息" });
    await f.phone.message({ handle: { conversationId: "qq:user:user-a", seq: 1 } });
    const list = await f.phone.conversations();
    assert.equal(
      list.items.find((c) => c.conversationId === "qq:user:user-a")!.pendingNotifications,
      18,
    );
    assert.equal(f.reading.get("qq:user:user-a"), null);
    const opened = await f.client.call("open", {
      conversationId: "qq:user:user-a",
    }) as unknown as OpenedConversation;
    assert.deepEqual(opened.displayedSeqs, [11, 12, 13, 14, 15, 16, 17, 18]);
    assert.equal(opened.snapshotThroughSeq, 19);
    assert.equal(opened.notificationThroughSeq, 18);
    assert.equal(opened.earlierMessagesNotShown, 10);
    assert.match(opened.notice, /上方未展示的消息并非真的已读/);
    const saved = {
      notificationThroughSeq: 18,
      lastOpened: { snapshotThroughSeq: 19, displayedSeqs: opened.displayedSeqs },
    };
    assert.deepEqual(f.reading.get("qq:user:user-a"), saved);
    await f.append(event(20));
    assert.deepEqual(
      (await f.phone.notifications({ afterSeq: firstNotices.throughSeq })).items.map((m) =>
        m.handle.seq
      ),
      [19, 20],
    );
    await f.phone.history({ conversationId: "qq:user:user-a", cursor: opened.older });
    await f.phone.search({ query: "消息", conversationId: "qq:user:user-a" });
    assert.deepEqual(f.reading.get("qq:user:user-a"), saved);
    await f.reading.close();
    restored = new QqPhoneReading(`${f.directory}/phone-reading`);
    await restored.initialize();
    const phone = new QqPhone(() => f.journal.records(), f.actions, f.timeline, {
      id: "bot",
      displayName: "Eve",
    }, restored);
    assert.deepEqual((await phone.notifications()).items.map((m) => m.handle.seq), [19, 20]);
    assert.deepEqual(restored.get("qq:user:user-a"), saved);
    const empty = await phone.open({ conversationId: "qq:user:empty" });
    assert.deepEqual(empty.displayedSeqs, []);
    assert.equal(empty.notificationThroughSeq, 0);
    assert.equal(restored.get("qq:user:empty"), null);
    assert.equal(f.count(), 0);
  } finally {
    await restored?.close();
    await f.close();
  }
});

Deno.test("QQ 手机：打开屏幕先提交，保存失败返回错误且不发布成功水位，原件不动", async () => {
  const f = await fixture();
  try {
    await f.append(event(1));
    await f.phone.open({ conversationId: "qq:user:user-a" });
    const path = `${f.directory}/phone-reading`;
    const original = await Deno.readTextFile(`${path}/index.json`);
    await f.append(event(2));
    await Deno.rename(path, `${f.directory}/preserved-reading`);
    await assert.rejects(
      () => f.client.call("open", { conversationId: "qq:user:user-a" }),
      (e) => e instanceof PhoneRpcError && e.category === "storage",
    );
    assert.equal(f.reading.available, false);
    assert.equal(await Deno.readTextFile(`${f.directory}/preserved-reading/index.json`), original);
    assert.equal(f.journal.records().length, 2);
    await assert.rejects(() => f.phone.notifications(), KernelError);
    await assert.rejects(() => f.reading.close(), KernelError);
  } finally {
    await f.close(false, true);
  }
});

Deno.test("QQ 手机：真实日志经 RPC 有界浏览、搜索和长文本，翻页快照重启仍稳定", async () => {
  const f = await fixture();
  try {
    for (let seq = 1; seq <= 18; seq++) await f.append(event(seq));
    const first = await f.phone.history({ conversationId: "qq:user:user-a" });
    assert.deepEqual(first.messages.map((m) => m.handle.seq), [11, 12, 13, 14, 15, 16, 17, 18]);
    const originalIndex = await Deno.readTextFile(`${f.directory}/timeline/index.json`);
    await f.append(event(19, "user-a", "喵".repeat(1500)));
    const older = await f.phone.history({ conversationId: "qq:user:user-a", cursor: first.older });
    assert.deepEqual(older.messages.map((m) => m.handle.seq), [3, 4, 5, 6, 7, 8, 9, 10]);
    const latest = await f.client.call("history", { conversationId: "qq:user:user-a" }) as {
      messages: {
        handle: { seq: number };
        body: { text: string; characters: number; truncated: boolean };
      }[];
    };
    assert.equal(latest.messages.at(-1)!.body.text.length, 160);
    assert.equal(latest.messages.at(-1)!.body.characters, 1500);
    const page = await f.client.call("text", {
      handle: { conversationId: "qq:user:user-a", seq: 19 },
      startChar: 1024,
      limit: 1024,
    }) as { text: string; truncated: boolean };
    assert.equal(page.text, "喵".repeat(476));
    assert.equal(page.truncated, false);
    const search = await f.phone.search({ query: "消息" });
    assert.equal(search.messages.length, 8);
    assert.equal(search.messages.at(-1)!.handle.seq, 18);
    assert.equal(f.count(), 0);
    const index = JSON.parse(await Deno.readTextFile(`${f.directory}/timeline/index.json`));
    assert.deepEqual(index.entries.slice(0, 18), JSON.parse(originalIndex).entries);
    assert.equal(JSON.stringify(index).includes("消息"), false);
    await f.timeline.close();
    const restart = new QqTimeline(`${f.directory}/timeline`);
    await restart.initialize();
    try {
      const phone = new QqPhone(() => f.journal.records(), f.actions, restart, {
        id: "fixture-bot",
        displayName: "Eve",
      }, f.reading);
      assert.deepEqual(
        await phone.history({ conversationId: "qq:user:user-a", cursor: first.older }),
        older,
      );
    } finally {
      await restart.close();
    }
  } finally {
    await f.close();
  }
});

Deno.test("QQ 手机：RPC观察超时保留未知，实际发送继续，查询原动作采用迟到真实回执", async () => {
  const f = await fixture();
  let allowReply!: () => void;
  const delayed = new Promise<void>((resolve) => {
    allowReply = resolve;
  });
  const quick = new PhoneRpcClient(f.rpc.url, KEY, 30);
  try {
    await f.append(event(1));
    f.beforeReply(() => delayed);
    const args = {
      actionId: "slow",
      conversationId: "qq:user:user-a",
      text: "晚一点的结果",
      replyTo: { seq: 1, sequence: 1 },
    };
    await assert.rejects(
      quick.call("send", args),
      (error) =>
        error instanceof PhoneRpcError && error.category === "unknown" && error.kind === "unknown",
    );
    assert.equal(f.count(), 1);
    allowReply();
    const result = await f.client.call("send", args) as { state: string };
    assert.equal(result.state, "sent");
    assert.equal(
      (await f.client.call("action", { actionId: "slow" }) as { state: string }).state,
      "sent",
    );
    assert.equal(f.count(), 1);
  } finally {
    allowReply();
    quick.close();
    await f.close();
  }
});

Deno.test("QQ 手机：插件远程错误保留unknown与storage，坏发送回包也不伪装安全失败", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-phone-error-" });
  let category = "unknown";
  let requests = 0;
  const rpc = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const body = await request.json();
    if (body.method === "health") return Response.json({ ok: true, value: { available: true } });
    requests++;
    if (category === "malformed") return new Response("invalid JSON");
    return Response.json({ ok: false, error: { category } });
  });
  let kernel: Kernel | undefined;
  const url = `http://127.0.0.1:${rpc.addr.port}/rpc`;
  const client = new PhoneRpcClient(url, KEY);
  try {
    await Deno.writeTextFile(`${directory}/key`, KEY, { mode: 0o600 });
    await Deno.writeTextFile(
      `${directory}/config.json`,
      JSON.stringify({ phone: { adapterUrl: url, adapterKeyFile: `${directory}/key` } }),
      { mode: 0o600 },
    );
    await composePhonePlugin(`${directory}/plugin`, `${directory}/config.json`);
    kernel = new Kernel({ dataDir: `${directory}/kernel` });
    await kernel.register(`${directory}/plugin`);
    await kernel.start();
    const args = { actionId: "fixed", conversationId: "qq:user:user", text: "一条消息" };
    await assert.rejects(
      kernel.call("phone", "send", args),
      (error) => error instanceof KernelError && error.kind === "unknown",
    );
    category = "storage";
    await assert.rejects(
      kernel.call("phone", "send", { ...args, actionId: "second" }),
      (error) => error instanceof KernelError && error.kind === "storage",
    );
    category = "malformed";
    await assert.rejects(
      client.call("send", { ...args, actionId: "third" }),
      (error) => error instanceof PhoneRpcError && error.kind === "unknown",
    );
    assert.equal(requests, 3);
  } finally {
    await kernel?.stop();
    client.close();
    await rpc.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 手机：真实独立适配器子进程、网关入站与worker发送贯通，重启复用消息及回执", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-phone-process-" });
  const sockets: WebSocket[] = [];
  let sends = 0;
  let connections = 0;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (r): Promise<Response> => {
      const path = new URL(r.url).pathname;
      if (path === "/app/getAppAccessToken") {
        return Response.json({ access_token: "fixture-token", expires_in: 7200 });
      }
      if (path === "/users/@me") {
        return Response.json({ id: "actual-fixture-bot", username: "Eve fixture" });
      }
      if (path === "/gateway") {
        return Response.json({
          url: `ws://127.0.0.1:${platform.addr.port}/websocket`,
          session_start_limit: { remaining: 100 },
        });
      }
      if (path.startsWith("/v2/users/")) {
        sends++;
        const body = await r.json();
        assert.equal(body.markdown?.content ?? body.content, "独立手机里发出");
        return Response.json({ id: "actual-process-sent-id" });
      }
      const { socket, response } = Deno.upgradeWebSocket(r);
      sockets.push(socket);
      connections++;
      socket.onopen = () => socket.send(JSON.stringify({ op: 10, d: { heartbeat_interval: 100 } }));
      socket.onmessage = (incoming) => {
        const data = JSON.parse(incoming.data);
        if (data.op === 1) socket.send(JSON.stringify({ op: 11 }));
        else if (data.op === 2) {
          assert.equal(data.d.intents, 1 << 25);
          socket.send(
            JSON.stringify({
              op: 0,
              s: 0,
              t: "READY",
              d: { session_id: "actual-fixture-session" },
            }),
          );
          socket.send(JSON.stringify(event(1)));
        } else if (data.op === 6) {
          assert.equal(data.d.seq, 1);
          socket.send(JSON.stringify({ op: 0, s: 1, t: "RESUMED", d: {} }));
        }
      };
      return response;
    },
  );
  const endpoint = `${directory}/endpoint.json`;
  const script = `${directory}/adapter.ts`;
  const serviceUrl = new URL("../adapters/qq/service.ts", import.meta.url).href;
  const clientUrl = new URL("../adapters/qq/client.ts", import.meta.url).href;
  const journalUrl = new URL("../adapters/qq/journal.ts", import.meta.url).href;
  let child: Deno.ChildProcess | undefined;
  let childError: Promise<string> | undefined;
  let kernel: Kernel | undefined;
  async function waitFor<T>(get: () => Promise<T | undefined>) {
    for (let attempt = 0; attempt < 500; attempt++) {
      const result = await get();
      if (result !== undefined) return result;
      await new Promise((done) => setTimeout(done, 10));
    }
    throw new Error("本地适配器未在5秒内完成实际交互");
  }
  async function start() {
    child = new Deno.Command(Deno.execPath(), {
      args: ["run", "--allow-read", "--allow-write", "--allow-net=127.0.0.1", script],
      stdout: "null",
      stderr: "piped",
    }).spawn();
    childError = new Response(child.stderr).text();
    const value = await waitFor(async () => {
      try {
        return JSON.parse(await Deno.readTextFile(endpoint)) as { url: string };
      } catch (error) {
        if (error instanceof Deno.errors.NotFound) return undefined;
        throw error;
      }
    });
    await Deno.writeTextFile(`${directory}/key`, KEY, { mode: 0o600 });
    await Deno.writeTextFile(
      `${directory}/config.json`,
      JSON.stringify({
        phone: {
          adapterUrl: value.url,
          adapterKeyFile: `${directory}/key`,
          ownerPlatformUserId: "user-a",
        },
      }),
      { mode: 0o600 },
    );
    await composePhonePlugin(`${directory}/plugin`, `${directory}/config.json`);
    kernel = new Kernel({ dataDir: `${directory}/kernel` });
    await kernel.register(`${directory}/plugin`);
    await kernel.start();
  }
  async function stop() {
    if (kernel) {
      assert.deepEqual(await kernel.stop(), []);
      kernel = undefined;
    }
    if (child) {
      child.kill("SIGTERM");
      const result = await child.status;
      child = undefined;
      const stderr = await childError;
      childError = undefined;
      assert.equal(result.code, 0, stderr);
    }
  }
  try {
    await Deno.writeTextFile(
      script,
      `import {startQqPhoneAdapter} from ${JSON.stringify(serviceUrl)};\n` +
        `import {QqClient} from ${JSON.stringify(clientUrl)};\n` +
        `import {writeAtomic} from ${JSON.stringify(journalUrl)};\n` +
        `const adapter=await startQqPhoneAdapter(new QqClient({appId:"fixture",appSecret:"fixture"},{baseUrl:${
          JSON.stringify(`http://127.0.0.1:${platform.addr.port}`)
        }}),${JSON.stringify(directory + "/data")},${
          JSON.stringify(KEY)
        },undefined,undefined,{captureActivationBoundary:true});\n` +
        `let done;const stopping=new Promise(resolve=>done=resolve);const signal=()=>done();Deno.addSignalListener("SIGTERM",signal);\n` +
        `await writeAtomic(${
          JSON.stringify(endpoint)
        },{url:adapter.url});try{await Promise.race([adapter.stopped,stopping]);}finally{Deno.removeSignalListener("SIGTERM",signal);await adapter.close();}\n`,
    );
    await start();
    const activationBoundaryBefore = await Deno.readTextFile(
      `${directory}/data/activation-boundary.json`,
    );
    const startupBoundary = JSON.parse(activationBoundaryBefore);
    assert.equal(startupBoundary.schema, 1);
    assert.equal("items" in startupBoundary, false);
    assert.equal("messages" in startupBoundary, false);
    const page = await waitFor(async () => {
      const value = await kernel!.call<
        {
          messages: {
            handle: { seq: number };
            sender: { platformUserId: string; relationship: string };
          }[];
        }
      >("phone", "history", { conversationId: "qq:user:user-a" });
      return value.messages.length ? value : undefined;
    });
    assert.equal(page.messages[0].sender.relationship, "owner");
    const args = {
      actionId: "process-send",
      conversationId: "qq:user:user-a",
      text: "独立手机里发出",
      replyTo: { seq: page.messages[0].handle.seq, sequence: 1 },
    };
    const receipt = await kernel!.call("phone", "send", args);
    assert.deepEqual(receipt, {
      actionId: "process-send",
      state: "sent",
      platformMessageId: "actual-process-sent-id",
    });
    const before = await kernel!.call<
      { messages: { handle: { seq: number }; sender: { platformUserId: string } }[] }
    >("phone", "history", { conversationId: "qq:user:user-a" });
    assert.equal(before.messages.length, 2);
    assert.equal(before.messages[1].sender.platformUserId, "actual-fixture-bot");
    const opened = await kernel!.call("phone", "open", { conversationId: "qq:user:user-a" });
    const readingBeforeRestart = await Deno.readTextFile(
      `${directory}/data/phone-reading/index.json`,
    );
    await stop();
    await Deno.remove(endpoint);
    await start();
    assert.equal(
      await Deno.readTextFile(`${directory}/data/activation-boundary.json`),
      activationBoundaryBefore,
    );
    assert.deepEqual(
      await kernel!.call("phone", "history", { conversationId: "qq:user:user-a" }),
      before,
    );
    assert.deepEqual(await kernel!.call("phone", "send", args), receipt);
    assert.equal(
      await Deno.readTextFile(`${directory}/data/phone-reading/index.json`),
      readingBeforeRestart,
    );
    assert.deepEqual(
      await kernel!.call("phone", "open", { conversationId: "qq:user:user-a" }),
      opened,
    );
    assert.deepEqual(await kernel!.call("phone", "notifications"), {
      items: [],
      throughSeq: 2,
      hasMore: false,
    });
    assert.equal(sends, 1);
    assert.equal(connections, 2);
    const raw = await Deno.readDir(`${directory}/data/inbound`);
    let originals = 0;
    for await (const entry of raw) if (entry.isFile) originals++;
    assert.equal(originals, 1);
  } finally {
    try {
      await stop();
    } finally {
      if (child) {
        try {
          child.kill("SIGKILL");
        } catch { /* 已停 */ }
        await child.status;
        await childError;
      }
      for (const socket of sockets) {
        try {
          socket.close();
        } catch { /* 已停 */ }
      }
      await platform.shutdown();
      await Deno.remove(directory, { recursive: true });
    }
  }
});

Deno.test("QQ 手机：会话列表翻页不漏或挤动，通知有界且普通群消息不亮屏", async () => {
  const f = await fixture();
  try {
    for (let seq = 1; seq <= 12; seq++) await f.append(event(seq, `user-${seq}`));
    await f.append(event(13, "group-user", "普通群消息", "group-1"));
    const first = await f.phone.conversations();
    assert.equal(first.items.length, 8);
    const firstIds = first.items.map((i) => i.conversationId);
    await f.append(event(14, "new-user"));
    const next = await f.phone.conversations({ cursor: first.older });
    assert.equal(next.items.length, 5);
    assert.equal(next.items.some((i) => firstIds.includes(i.conversationId)), false);
    assert.equal(next.items.some((i) => i.conversationId.includes("new-user")), false);
    const notice = await f.phone.notifications();
    assert.equal(notice.items.length, 8);
    assert.equal(notice.hasMore, true);
    const rest = await f.phone.notifications({ afterSeq: notice.throughSeq });
    assert.equal(rest.items.length, 5);
    assert.equal(rest.items.some((i) => i.handle.conversationId.includes("group")), false);
    assert.equal(rest.hasMore, false);
    assert.equal((await f.phone.notifications({ afterSeq: rest.throughSeq })).items.length, 0);
    await assert.rejects(f.phone.notifications({ afterSeq: rest.throughSeq + 1 }));
    await assert.rejects(
      f.phone.conversations({
        cursor: { snapshotThroughSeq: rest.throughSeq + 1, beforeLastSeq: 1 },
      }),
    );
    assert.equal(f.count(), 0);
  } finally {
    await f.close();
  }
});

Deno.test("QQ 手机：旧回复句柄仍校验但统一主动，unknown也显示且不重发", async () => {
  const f = await fixture();
  try {
    await f.append(event(1));
    await f.append(event(2, "other"));
    const args = {
      actionId: "action-1",
      conversationId: "qq:user:user-a",
      text: "嗯，我在",
      replyTo: { seq: 1, sequence: 1 },
    };
    const receipt = await f.client.call("send", args);
    assert.deepEqual(receipt, {
      actionId: "action-1",
      state: "sent",
      platformMessageId: "actual-sent-id",
    });
    assert.deepEqual(f.received, [{
      content: "嗯，我在",
      msg_type: 0,
    }]);
    assert.deepEqual(await f.client.call("send", args), receipt);
    assert.equal(f.count(), 1);
    let screen = await f.phone.history({ conversationId: "qq:user:user-a" });
    assert.equal(screen.messages.at(-1)!.direction, "outgoing");
    assert.equal(screen.messages.at(-1)!.delivery?.state, "sent");
    await assert.rejects(
      f.client.call("send", { ...args, actionId: "cross", replyTo: { seq: 2, sequence: 1 } }),
      PhoneRpcError,
    );
    await assert.rejects(
      f.client.call("send", { ...args, actionId: "own", replyTo: { seq: 3, sequence: 2 } }),
      PhoneRpcError,
    );
    assert.equal(f.count(), 1);
    f.result({});
    const unknown = {
      ...args,
      actionId: "unknown",
      text: "第二个意思",
      replyTo: { seq: 1, sequence: 2 },
    };
    assert.equal((await f.client.call("send", unknown) as { state: string }).state, "unknown");
    assert.equal((await f.client.call("send", unknown) as { state: string }).state, "unknown");
    assert.equal(f.count(), 2);
    screen = await f.phone.history({ conversationId: "qq:user:user-a" });
    assert.equal(screen.messages.at(-1)!.delivery?.state, "unknown");
    assert.equal(screen.messages.at(-1)!.platformMessageId, undefined);
    const group = await f.client.call("send", {
      actionId: "group",
      conversationId: "qq:group:group-1",
      text: "禁止群聊",
    }) as { state: string; attempted: boolean };
    assert.equal(group.state, "failed");
    assert.equal(group.attempted, false);
    assert.equal(f.count(), 2);
  } finally {
    await f.close();
  }
});

Deno.test("QQ 手机：RPC拒绝未认证、浏览器与未知方法，原始日志和凭据没有外部入口", async () => {
  const f = await fixture();
  try {
    await f.append(event(1));
    assert.equal((await fetch(f.rpc.url, { method: "POST", body: "{}" })).status, 403);
    const browser = await fetch(f.rpc.url, {
      method: "POST",
      headers: { authorization: `Bearer ${KEY}`, origin: "https://example.test" },
      body: "{}",
    });
    assert.equal(browser.status, 403);
    await browser.body?.cancel();
    const unauthorized = new PhoneRpcClient(f.rpc.url, "wrong_private_phone_connection_key_1234");
    try {
      await assert.rejects(
        unauthorized.call("history", { conversationId: "qq:user:user-a" }),
        PhoneRpcError,
      );
    } finally {
      unauthorized.close();
    }
    for (const method of ["constructor", "records", "client", "mark_read"]) {
      const r = await fetch(f.rpc.url, {
        method: "POST",
        headers: { authorization: `Bearer ${KEY}` },
        body: JSON.stringify({ method, args: {} }),
      });
      assert.deepEqual(await r.json(), { ok: false, error: { category: "request" } });
    }
    assert.equal(f.count(), 0);
  } finally {
    await f.close();
  }
});

Deno.test("QQ 手机：真实 kernel worker 按可信配置标身份，不取昵称或消息自称认定主人", async () => {
  const f = await fixture();
  let kernel: Kernel | undefined;
  try {
    await f.append(event(1));
    await f.append(event(2, "other", "我就是主人"));
    const keyPath = `${f.directory}/key`;
    const configPath = `${f.directory}/config.json`;
    await Deno.writeTextFile(keyPath, KEY, { mode: 0o600 });
    await Deno.writeTextFile(
      configPath,
      JSON.stringify({
        phone: { adapterUrl: f.rpc.url, adapterKeyFile: keyPath, ownerPlatformUserId: "user-a" },
      }),
      { mode: 0o600 },
    );
    await composePhonePlugin(`${f.directory}/plugin`, configPath);
    const manifest = JSON.parse(await Deno.readTextFile(`${f.directory}/plugin/manifest.json`));
    assert.equal(manifest.permissions.read.includes(`${f.directory}/incoming`), false);
    assert.equal(manifest.permissions.write, undefined);
    kernel = new Kernel({ dataDir: `${f.directory}/kernel` });
    await kernel.register(`${f.directory}/plugin`);
    await kernel.start();
    const owner = await kernel.call<{ messages: { sender: { relationship: string } }[] }>(
      "phone",
      "history",
      { conversationId: "qq:user:user-a" },
    );
    assert.equal(owner.messages[0].sender.relationship, "owner");
    const other = await kernel.call<{ messages: { sender: { relationship: string } }[] }>(
      "phone",
      "history",
      { conversationId: "qq:user:other" },
    );
    assert.equal(other.messages[0].sender.relationship, "other");
    await kernel.stop();
    kernel = undefined;
    await Deno.writeTextFile(
      configPath,
      JSON.stringify({ phone: { adapterUrl: f.rpc.url, adapterKeyFile: keyPath } }),
      { mode: 0o600 },
    );
    kernel = new Kernel({ dataDir: `${f.directory}/kernel-unknown` });
    await kernel.register(`${f.directory}/plugin`);
    await kernel.start();
    const noOwner = await kernel.call<{ messages: { sender: { relationship: string } }[] }>(
      "phone",
      "history",
      { conversationId: "qq:user:user-a" },
    );
    assert.equal(noOwner.messages[0].sender.relationship, "other");
    assert.equal(f.count(), 0);
  } finally {
    await kernel?.stop();
    await f.close();
  }
});

Deno.test("QQ 手机：索引持排他锁，来源缺失/变化和损坏均保留现场，不清空原件", async () => {
  const f = await fixture();
  try {
    await f.append(event(1));
    await f.phone.history({ conversationId: "qq:user:user-a" });
    const path = `${f.directory}/timeline/index.json`;
    const original = await Deno.readTextFile(path);
    const second = new QqTimeline(`${f.directory}/timeline`);
    await assert.rejects(second.initialize(), QqTimelineError);
    await assert.rejects(f.timeline.snapshot([]), QqTimelineError);
    assert.equal(await Deno.readTextFile(path), original);
    assert.equal(f.journal.records().length, 1);
    await assert.rejects(f.timeline.close(), QqTimelineError);
    await Deno.writeTextFile(path, "broken");
    const corrupt = new QqTimeline(`${f.directory}/timeline`);
    await assert.rejects(corrupt.initialize(), QqTimelineError);
    assert.equal(await Deno.readTextFile(path), "broken");
  } finally {
    await f.close(true);
  }
});

Deno.test("QQ 手机：平台已送达但浏览索引保存失败，原动作仍可查，恢复补引用而不重发", async () => {
  const f = await fixture();
  let restored: QqTimeline | undefined;
  try {
    await f.append(event(1));
    await f.phone.history({ conversationId: "qq:user:user-a" });
    const path = `${f.directory}/timeline`;
    const original = await Deno.readTextFile(`${path}/index.json`);
    await Deno.rename(path, `${f.directory}/preserved-timeline`);
    const args = {
      actionId: "index-failed",
      conversationId: "qq:user:user-a",
      text: "已经由平台确认",
      replyTo: { seq: 1, sequence: 1 },
    };
    await assert.rejects(
      f.client.call("send", args),
      (error) => error instanceof PhoneRpcError && error.category === "storage",
    );
    assert.equal(f.count(), 1);
    const receipt = await f.client.call("action", { actionId: "index-failed" });
    assert.deepEqual(receipt, {
      actionId: "index-failed",
      state: "sent",
      platformMessageId: "actual-sent-id",
    });
    assert.equal(await Deno.readTextFile(`${f.directory}/preserved-timeline/index.json`), original);
    assert.equal(f.timeline.available, false);
    await assert.rejects(f.timeline.close(), QqTimelineError);
    await Deno.rename(`${f.directory}/preserved-timeline`, path);
    restored = new QqTimeline(path);
    await restored.initialize();
    const phone = new QqPhone(() => f.journal.records(), f.actions, restored, {
      id: "fixture-bot",
      displayName: "Eve",
    }, f.reading);
    assert.equal(
      (await phone.history({ conversationId: "qq:user:user-a" })).messages.at(-1)!.delivery?.state,
      "sent",
    );
    assert.deepEqual(await phone.send(args), receipt);
    assert.equal(f.count(), 1);
  } finally {
    await restored?.close();
    await f.close(true);
  }
});

Deno.test("QQ 引用视图：旧入站原件按同会话索引解析，未知/歧义不猜，重启不改原件", async () => {
  const f = await fixture();
  const indexed = (seq: number, index?: string, quoted?: string, user = "user-a") => {
    const raw = event(seq, user);
    (raw.d as Record<string, unknown>).message_scene = {
      ext: [
        ...(index === undefined ? [] : [`msg_idx=${index}`]),
        ...(quoted === undefined ? [] : [`ref_msg_idx=${quoted}`]),
      ],
    };
    return raw;
  };
  try {
    await f.append(indexed(1, "own-1"));
    await f.append(indexed(2, "own-2", "own-1"));
    await f.append(indexed(3, "own-3", "missing"));
    await f.append(indexed(4, "own-4", "foreign"));
    await f.append(indexed(5, "foreign", undefined, "user-b"));
    await f.append(indexed(6, "duplicate"));
    await f.append(indexed(7, "duplicate"));
    await f.append(indexed(8, "own-8", "duplicate"));
    await f.append(indexed(9, "own-9", "own-9"));
    const original = f.journal.records();
    assert.equal(original[1].message.referenceIndex, "own-1");
    assert.equal(original[1].message.message.quote, undefined);
    assert.equal(original[1].message.message.reference, undefined);
    const files = [];
    for await (const file of Deno.readDir(`${f.directory}/incoming`)) {
      files.push([file.name, await Deno.readTextFile(`${f.directory}/incoming/${file.name}`)]);
    }
    const restored = new QqInboundJournal(`${f.directory}/incoming`);
    await restored.initialize();
    assert.deepEqual(restored.records(), original);
    const phone = new QqPhone(() => restored.records(), f.actions, f.timeline, {
      id: "fixture-bot",
      displayName: "Eve",
    }, f.reading);
    const screen = await phone.message({ handle: { conversationId: "qq:user:user-a", seq: 2 } });
    assert.equal(screen.referenceIndex, "own-2");
    assert.deepEqual(screen.reference, {
      index: "own-1",
      target: { conversationId: "qq:user:user-a", seq: 1 },
    });
    assert.deepEqual(screen.quote, { platformMessageId: "platform-1" });
    assert.equal((await phone.message({ handle: screen.reference!.target! })).body.text, "消息-1");
    for (
      const [seq, index] of [[3, "missing"], [4, "foreign"], [8, "duplicate"], [
        9,
        "own-9",
      ]] as const
    ) {
      const unknown = await phone.message({ handle: { conversationId: "qq:user:user-a", seq } });
      assert.deepEqual(unknown.reference, { index });
      assert.equal(unknown.quote, undefined);
    }
    const history = await phone.history({ conversationId: "qq:user:user-a" });
    assert.deepEqual(history.messages.find((m) => m.handle.seq === 2)!.reference, screen.reference);
    const notices = await phone.notifications();
    assert.deepEqual(notices.items.find((m) => m.handle.seq === 2)!.reference, screen.reference);
    assert.deepEqual(f.journal.records(), original);
    for (const [name, content] of files) {
      assert.equal(await Deno.readTextFile(`${f.directory}/incoming/${name}`), content);
    }
    assert.equal(f.count(), 0);
  } finally {
    await f.close();
  }
});

Deno.test("QQ 引用视图：真实出站回执索引可被引用，引用展示不改变主动发送", async () => {
  const f = await fixture();
  try {
    const raw = event(1);
    (raw.d as Record<string, unknown>).message_scene = { ext: ["msg_idx=incoming-index"] };
    await f.append(raw);
    f.result({ id: "outgoing-platform-id", ext_info: { ref_idx: "outgoing-index" } });
    const sent = await f.phone.send({
      actionId: "reference-fixture",
      conversationId: "qq:user:user-a",
      text: "回复早先消息",
      referenceIndex: "incoming-index",
      replyTo: { seq: 1, sequence: 1 },
    });
    assert.equal(sent.state, "sent");
    const outgoing = (await f.phone.history({ conversationId: "qq:user:user-a" })).messages.at(-1)!;
    assert.equal(outgoing.referenceIndex, "outgoing-index");
    assert.deepEqual(outgoing.reference, {
      index: "incoming-index",
      target: { conversationId: "qq:user:user-a", seq: 1 },
    });
    const reply = event(2, "user-a", "引用 Eve 上一句");
    (reply.d as Record<string, unknown>).message_scene = {
      ext: ["msg_idx=next-incoming-index", "ref_msg_idx=outgoing-index"],
    };
    await f.append(reply);
    const incoming = (await f.phone.history({ conversationId: "qq:user:user-a" })).messages.at(-1)!;
    assert.deepEqual(incoming.reference, { index: "outgoing-index", target: outgoing.handle });
    assert.deepEqual(incoming.quote, { platformMessageId: "outgoing-platform-id" });
    assert.equal(f.count(), 1);
    assert.deepEqual(f.received, [{
      content: "回复早先消息",
      msg_type: 0,
      message_reference: { message_id: "incoming-index" },
    }]);
  } finally {
    await f.close();
  }
});

Deno.test("QQ组合配置沿用缺省private，接受all并拒绝无效scope", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-scope-config-" });
  const configPath = `${directory}/config.json`;
  try {
    await Deno.writeTextFile(`${directory}/id`, "fixture-app\n");
    await Deno.writeTextFile(`${directory}/secret`, "fixture-secret\n");
    for (const replyScope of [undefined, "private", "all", null, "group", true, ["all"], {}]) {
      await Deno.writeTextFile(
        configPath,
        JSON.stringify({
          qq: {
            sandbox: false,
            replyScope,
            appIdFile: `${directory}/id`,
            appSecretFile: `${directory}/secret`,
          },
        }),
      );
      if (replyScope === undefined || replyScope === "private" || replyScope === "all") {
        const client = await readQqTestClient(configPath);
        assert.equal(client.sendScope, replyScope ?? "private");
        client.close();
      } else await assert.rejects(readQqTestClient(configPath), QqRequestError);
    }
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("真实QQ适配器范围贯穿健康、群REST和私信，重启保持原回执不重发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-phone-group-scope-" });
  const paths: string[] = [];
  const sockets = new Set<WebSocket>();
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (r): Promise<Response> => {
      const path = new URL(r.url).pathname;
      if (path === "/app/getAppAccessToken") {
        return Response.json({ access_token: "fixture-token", expires_in: 7200 });
      }
      if (path === "/users/@me") return Response.json({ id: "scope-bot", username: "Eve" });
      if (path === "/gateway") {
        return Response.json({
          url: `ws://127.0.0.1:${platform.addr.port}/websocket`,
          session_start_limit: { remaining: 100 },
        });
      }
      if (path.startsWith("/v2/")) {
        paths.push(path);
        const body = await r.json();
        assert.equal(body.msg_seq, undefined);
        assert.equal(body.msg_id, undefined);
        assert.equal(body.event_id, undefined);
        return Response.json({ id: `real-scope-send-${paths.length}` });
      }
      assert.equal(path, "/websocket");
      const { socket, response } = Deno.upgradeWebSocket(r);
      sockets.add(socket);
      socket.onclose = () => sockets.delete(socket);
      socket.onopen = () => socket.send(JSON.stringify({ op: 10, d: { heartbeat_interval: 100 } }));
      socket.onmessage = (incoming) => {
        const data = JSON.parse(incoming.data);
        if (data.op === 1) socket.send(JSON.stringify({ op: 11 }));
        else if (data.op === 2) {
          socket.send(
            JSON.stringify({ op: 0, s: 0, t: "READY", d: { session_id: "scope-session" } }),
          );
          socket.send(JSON.stringify(event(1, "group-user", "群消息", "scope-group")));
          socket.send(JSON.stringify(event(2, "user-a", "私信")));
        } else if (data.op === 6) {
          assert.equal(data.d.seq, 2);
          socket.send(JSON.stringify({ op: 0, s: 2, t: "RESUMED", d: {} }));
        }
      };
      return response;
    },
  );
  let adapter: Awaited<ReturnType<typeof startQqPhoneAdapter>> | undefined;
  let rpc: PhoneRpcClient | undefined;
  async function start(scope: "private" | "all") {
    adapter = await startQqPhoneAdapter(
      new QqClient({ appId: "fixture", appSecret: "fixture" }, {
        baseUrl: `http://127.0.0.1:${platform.addr.port}`,
        sendScope: scope,
      }),
      `${directory}/${scope}`,
      KEY,
    );
    rpc = new PhoneRpcClient(adapter.url, KEY);
    for (let attempt = 0; attempt < 500; attempt++) {
      const page = await rpc.call("history", { conversationId: "qq:user:user-a" }) as {
        messages: unknown[];
      };
      if (page.messages.length) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    throw new Error("本地QQ适配器未收到测试消息");
  }
  async function stop() {
    rpc?.close();
    rpc = undefined;
    await adapter?.close();
    adapter = undefined;
  }
  try {
    for (const scope of ["private", "all"] as const) {
      await start(scope);
      const health = await rpc!.call("health") as { available: boolean; replyScope: string };
      assert.equal(health.available, true);
      assert.equal(health.replyScope, scope);
      const group = {
        actionId: "scope-group-action",
        conversationId: "qq:group:scope-group",
        text: "自然群聊",
        replyTo: { seq: 1, sequence: 1 },
      };
      const userHistory = await rpc!.call("history", { conversationId: "qq:user:user-a" }) as {
        messages: { handle: { seq: number } }[];
      };
      const user = {
        actionId: "scope-user-action",
        conversationId: "qq:user:user-a",
        text: "私信正常",
        replyTo: { seq: userHistory.messages[0].handle.seq, sequence: 1 },
      };
      const before = paths.length;
      const groupReceipt = await rpc!.call("send", group) as { state: string; attempted?: boolean };
      assert.equal(groupReceipt.state, scope === "all" ? "sent" : "failed");
      if (scope === "private") assert.equal(groupReceipt.attempted, false);
      const userReceipt = await rpc!.call("send", user) as { state: string };
      assert.equal(userReceipt.state, "sent");
      assert.deepEqual(
        paths.slice(before),
        scope === "all"
          ? ["/v2/groups/scope-group/messages", "/v2/users/user-a/messages"]
          : ["/v2/users/user-a/messages"],
      );
      const groupBytes = await Deno.readTextFile(
        `${directory}/${scope}/outbox/records/scope-group-action.json`,
      );
      const userBytes = await Deno.readTextFile(
        `${directory}/${scope}/outbox/records/scope-user-action.json`,
      );
      const sentCount = paths.length;
      await stop();
      await start(scope);
      assert.deepEqual(await rpc!.call("send", group), groupReceipt);
      assert.deepEqual(await rpc!.call("send", user), userReceipt);
      assert.equal(paths.length, sentCount);
      assert.equal(
        await Deno.readTextFile(`${directory}/${scope}/outbox/records/scope-group-action.json`),
        groupBytes,
      );
      assert.equal(
        await Deno.readTextFile(`${directory}/${scope}/outbox/records/scope-user-action.json`),
        userBytes,
      );
      await stop();
    }
  } finally {
    await stop();
    for (const socket of sockets) socket.close();
    await platform.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("手机主动发送：旧sequence忽略，不同动作无被动字段，同action重入不重复发送", async () => {
  const f = await fixture();
  try {
    await f.append(event(1));
    const page = await f.phone.open({ conversationId: "qq:user:user-a" });
    const args = {
      actionId: "auto-first",
      conversationId: "qq:user:user-a",
      text: "first",
      replyTo: { seq: page.messages[0].handle.seq },
    };
    assert.equal((await f.client.call("send", args) as { state: string }).state, "sent");
    assert.equal(
      (await f.client.call("send", { ...args, actionId: "auto-second", text: "second" }) as {
        state: string;
      }).state,
      "sent",
    );
    assert.equal((await f.client.call("send", args) as { state: string }).state, "sent");
    assert.equal(f.count(), 2);
    assert.equal(
      (await f.client.call("send", {
        ...args,
        actionId: "legacy-explicit",
        replyTo: { ...args.replyTo, sequence: 1 },
      }) as { state: string }).state,
      "sent",
    );
    // 同一动作即使旧被动序号改变，仍返回原回执，不增加发送。
    assert.equal(
      (await f.client.call("send", {
        ...args,
        replyTo: { ...args.replyTo, sequence: 99 },
      }) as { state: string }).state,
      "sent",
    );
    assert.equal(f.count(), 3);
    for (const received of f.received as Record<string, unknown>[]) {
      assert.equal(received.msg_seq, undefined);
      assert.equal(received.msg_id, undefined);
      assert.equal(received.event_id, undefined);
    }
    await assert.rejects(
      f.client.call("send", {
        ...args,
        actionId: "invalid-sequence",
        replyTo: { ...args.replyTo, sequence: 0 },
      }),
      PhoneRpcError,
    );
    assert.equal(f.count(), 3);
  } finally {
    await f.close();
  }
});

Deno.test("手机主动发送：唤醒兼容旧回复句柄，附件上传与最终消息均无被动字段", async () => {
  const uploadedPaths: string[] = [];
  const f = await fixture({
    attachmentReader: (path) => {
      uploadedPaths.push(path);
      return Promise.resolve(new Uint8Array([1, 2, 3]));
    },
  });
  try {
    await f.append(event(1));
    const wakeup = await f.phone.send({
      actionId: "active-wakeup",
      conversationId: "qq:user:user-a",
      text: "回来看看",
      wakeup: true,
      replyTo: { seq: 1, sequence: 9 },
    });
    assert.equal(wakeup.state, "sent");
    assert.equal((f.received[0] as Record<string, unknown>).is_wakeup, true);
    for (const kind of ["image", "file"] as const) {
      const path = `/fixture/attachment.${kind === "image" ? "png" : "txt"}`;
      const sent = await f.phone.sendFile({
        actionId: `active-${kind}`,
        conversationId: "qq:user:user-a",
        path,
        kind,
        text: "附件",
        replyTo: { seq: 1, sequence: 1 },
      });
      assert.equal(sent.state, "sent");
      const upload = f.received.at(-2) as Record<string, unknown>;
      assert.equal(upload.srv_send_msg, false);
      const wire = f.received.at(-1) as Record<string, unknown>;
      assert.equal(wire.msg_type, 7);
      assert.deepEqual(wire.media, { file_info: "fixture-upload-info" });
      // 文件动作重入不再次上传或发送，旧序号变化也不改变动作。
      assert.deepEqual(
        await f.phone.sendFile({
          actionId: `active-${kind}`,
          conversationId: "qq:user:user-a",
          path,
          kind,
          text: "附件",
          replyTo: { seq: 1, sequence: 77 },
        }),
        sent,
      );
    }
    assert.deepEqual(uploadedPaths, ["/fixture/attachment.png", "/fixture/attachment.txt"]);
    assert.equal(f.count(), 5);
    for (const wire of f.received as Record<string, unknown>[]) {
      assert.equal(wire.msg_id, undefined);
      assert.equal(wire.msg_seq, undefined);
      assert.equal(wire.event_id, undefined);
    }
  } finally {
    await f.close();
  }
});
