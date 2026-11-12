import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { admitBurst, burstSegments, waitForQuiet } from "../adapters/qq/phone-social.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { startPhoneRpc } from "../adapters/qq/rpc.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import type { PhoneBurstReceipt, PhoneMessagesWaitResult } from "../contracts/phone.ts";
import { PhoneRpcClient, PhoneRpcError } from "../plugins/phone/client.ts";
import { SCRIPT_TOOL_METHODS } from "../services/admin/tool-catalog.ts";

const KEY = "fixture_social_phone_key_0123456789012345";
const CONVERSATION = "qq:user:friend";
async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-phone-social-" });
  const texts: string[] = [];
  let unknown = false;
  const platform = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    async (request) => {
      if (new URL(request.url).pathname === "/app/getAppAccessToken") {
        return Response.json({ access_token: "fixture-token", expires_in: 7200 });
      }
      const body = await request.json();
      assert.equal(body.msg_id, undefined);
      assert.equal(body.msg_seq, undefined);
      texts.push(body.markdown?.content ?? body.content);
      return Response.json(unknown ? {} : { id: `sent-${texts.length}` });
    },
  );
  const journal = new QqInboundJournal(`${directory}/incoming`);
  await journal.initialize();
  let actions!: QqSendActions, timeline!: QqTimeline, reading!: QqPhoneReading, phone!: QqPhone;
  let rpc: ReturnType<typeof startPhoneRpc> | undefined;
  let remote: PhoneRpcClient;
  async function start() {
    const client = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    });
    actions = new QqSendActions(client, `${directory}/actions`);
    await actions.initialize();
    timeline = new QqTimeline(`${directory}/timeline`);
    await timeline.initialize();
    reading = new QqPhoneReading(`${directory}/reading`);
    await reading.initialize();
    phone = new QqPhone(() => journal.records(), actions, timeline, {
      id: "bot",
      displayName: "Eve",
    }, reading);
    rpc = startPhoneRpc(phone, KEY);
    remote = new PhoneRpcClient(rpc.url, KEY);
  }
  async function stop() {
    remote.close();
    await rpc?.close();
    rpc = undefined;
    await actions.close();
    await timeline.close();
    await reading.close();
  }
  await start();
  return {
    directory,
    texts,
    phone: () => phone,
    actions: () => actions,
    remote: () => remote,
    unknown: (value: boolean) => unknown = value,
    restart: async () => {
      await stop();
      await start();
    },
    append: async (seq: number, user = "friend") => {
      const raw: QqDispatch = {
        op: 0,
        s: seq,
        t: "C2C_MESSAGE_CREATE",
        d: {
          id: `incoming-${seq}`,
          content: `real-${seq}`,
          timestamp: "2020-01-01T00:00:00Z",
          author: { user_openid: user },
        },
      };
      return await journal.append(qqMessage(raw)!, raw);
    },
    close: async () => {
      await stop();
      await platform.shutdown();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("手机分条：真实RPC顺序发送、父子编号冻结、重启重入不重复且目录无正文泄露", async () => {
  const f = await fixture();
  try {
    const args = {
      actionId: "burst-one",
      conversationId: CONVERSATION,
      messages: ["先说一个意思", "再补一句", "最后一句"],
    };
    const [result, concurrent] = await Promise.all([
      f.remote().call("sendBurst", args),
      f.remote().call("sendBurst", args),
    ]) as unknown as [PhoneBurstReceipt, PhoneBurstReceipt];
    assert.deepEqual(result, concurrent);
    assert.deepEqual(await f.remote().call("burstStatus", { actionId: args.actionId }), result);
    assert.equal(result.complete, true);
    assert.deepEqual(result.items.map((item) => item.index), [1, 2, 3]);
    assert.deepEqual(f.texts, args.messages);
    assert.equal(new Set(result.items.map((item) => item.actionId)).size, 3);
    assert.ok(
      result.items.every((item) => item.receipt.state === "sent" && item.actionId.length <= 128),
    );
    const children = f.actions().records().sort((a, b) => a.policy!.sequence - b.policy!.sequence);
    assert.deepEqual(children.map((item) => item.policy!.sequence), [1, 2, 3]);
    assert.deepEqual(children.map((item) => item.request.text), args.messages);
    for await (const item of Deno.readDir(`${f.directory}/actions/bursts`)) {
      assert.match(item.name, /^[a-f0-9]{64}\.json$/);
      const original = JSON.parse(
        await Deno.readTextFile(`${f.directory}/actions/bursts/${item.name}`),
      );
      assert.deepEqual(original.messages, args.messages);
      assert.deepEqual(original.segments, args.messages);
    }
    await f.restart();
    assert.deepEqual(await f.remote().call("sendBurst", args), result);
    assert.deepEqual(f.texts, args.messages);
    await assert.rejects(
      f.remote().call("sendBurst", { ...args, messages: ["不同正文"] }),
      (error) => error instanceof PhoneRpcError && error.category === "conflict",
    );
    assert.deepEqual(f.texts, args.messages);
  } finally {
    await f.close();
  }
});

Deno.test("手机分条：未知队首阻挡其后子动作，取消保留未知事实，重入仅发送余下子条", async () => {
  const f = await fixture();
  try {
    f.unknown(true);
    const args = {
      actionId: "burst-unknown",
      conversationId: CONVERSATION,
      messages: ["不确定送达", "先别发第二条", "再后一句"],
    };
    const initial = await f.phone().sendBurst(args);
    assert.equal(initial.complete, false);
    assert.deepEqual(initial.items.map((item) => item.receipt.state), [
      "unknown",
      "blocked",
      "blocked",
    ]);
    assert.deepEqual(f.texts, [args.messages[0]]);
    const page = await f.actions().listSendQueue({ conversationId: CONVERSATION });
    const head = page.items.find((item) => item.actionId === initial.items[0].actionId)!;
    assert.equal(head.canCancel, true);
    await f.phone().cancelSend({
      actionId: head.actionId,
      expectedFingerprint: head.fingerprint,
      expectedReceiptDigest: head.receiptDigest!,
    });
    f.unknown(false);
    await f.restart();
    const resumed = await f.phone().sendBurst(args);
    assert.equal(resumed.complete, false);
    assert.equal(resumed.items[0].receipt.resolution?.kind, "unknown_retained");
    assert.deepEqual(resumed.items.slice(1).map((item) => item.receipt.state), ["sent", "sent"]);
    assert.deepEqual(f.texts, args.messages);
    assert.deepEqual(await f.phone().sendBurst(args), resumed);
    assert.deepEqual(f.texts, args.messages);
  } finally {
    await f.close();
  }
});

Deno.test("手机分条：超四条先拒绝不发送，显式30字切分冻结原文与segments，代码不暗拆", async () => {
  const f = await fixture();
  try {
    const base = { actionId: "split", conversationId: CONVERSATION };
    await assert.rejects(f.phone().sendBurst({ ...base, messages: ["1", "2", "3", "4", "5"] }));
    await assert.rejects(
      f.phone().sendBurst({ ...base, messages: ["甲".repeat(121)], splitLongText: true }),
    );
    await assert.rejects(
      f.phone().sendBurst({
        ...base,
        messages: ["```ts\n" + "x".repeat(80) + "\n```"],
        splitLongText: true,
      }),
    );
    assert.deepEqual(f.texts, []);
    const text = "甲".repeat(65);
    const split = await f.phone().sendBurst({ ...base, messages: [text], splitLongText: true });
    assert.equal(split.items.length, 3);
    assert.equal(f.texts.join(""), text);
    assert.ok(f.texts.every((text) => Array.from(text).length <= 30));
    assert.equal(
      burstSegments({ ...base, messages: ["```ts\nhello\n```"] })[0],
      "```ts\nhello\n```",
    );
  } finally {
    await f.close();
  }
});

Deno.test("手机静默：新真实入站重置10秒窗口，别的会话不延长，连续输入有限到期", async () => {
  let clock = 100_000;
  let observation = { seq: 1, receivedAt: new Date(clock).toISOString() };
  let checkpointCalls = 0;
  const result = await waitForQuiet(
    { conversationId: CONVERSATION, purpose: "reply" },
    () => observation,
    {
      now: () => clock,
      checkpoint: () => {
        checkpointCalls++;
        return Promise.resolve();
      },
      sleep: (ms) => {
        clock += ms;
        if (clock === 109_000) observation = { seq: 2, receivedAt: new Date(clock).toISOString() };
        return Promise.resolve();
      },
    },
  );
  assert.equal(result.reason, "quiet");
  assert.equal(result.waitedMs, 19_000);
  assert.equal(result.lastSourceSeq, 2);
  assert.ok(checkpointCalls > 1);
  clock = 200_000;
  observation = { seq: 3, receivedAt: new Date(clock).toISOString() };
  const limited = await waitForQuiet(
    { conversationId: CONVERSATION, purpose: "reply", quietMs: 10_000, maxWaitMs: 1200 },
    () => observation,
    {
      now: () => clock,
      sleep: (ms) => {
        clock += ms;
        observation = { seq: observation.seq + 1, receivedAt: new Date(clock).toISOString() };
        return Promise.resolve();
      },
    },
  );
  assert.equal(limited.reason, "max_wait");
  assert.equal(limited.waitedMs, 1200);
});

Deno.test("手机静默：RPC不阻挡其他会话和发送，真实水位可重读，不采用远端旧sentAt", async () => {
  const f = await fixture();
  try {
    await f.append(1);
    const waiting = f.remote().call("waitForMessages", {
      conversationId: CONVERSATION,
      purpose: "reply",
      quietMs: 180,
      maxWaitMs: 1500,
    });
    await f.append(2, "elsewhere");
    const sent = await f.remote().call("send", {
      actionId: "during-wait",
      conversationId: "qq:user:elsewhere",
      text: "另一个会话照常发送",
    }) as { state: string };
    assert.equal(sent.state, "sent");
    const first = await waiting as unknown as PhoneMessagesWaitResult;
    assert.equal(first.reason, "quiet");
    assert.ok(first.waitedMs >= 100);
    assert.equal(first.lastIncomingSeq, 1);
    assert.equal(first.throughSeq, 3);
    const history = await f.phone().history({ conversationId: CONVERSATION });
    assert.equal(history.messages[0].handle.seq, first.lastIncomingSeq);
    assert.equal((await f.phone().notifications()).items.length, 2);
    const controller = new AbortController();
    const interrupted = f.phone().waitForMessages({
      conversationId: CONVERSATION,
      purpose: "reply",
      quietMs: 10_000,
    }, { signal: controller.signal });
    controller.abort();
    assert.equal((await interrupted).reason, "interrupted");
    assert.ok(SCRIPT_TOOL_METHODS.phone.includes("sendBurst"));
    assert.ok(SCRIPT_TOOL_METHODS.phone.includes("waitForMessages"));
  } finally {
    await f.close();
  }
});

Deno.test("手机分条恢复：只读汇总未准入子项、不创建失踪父请求，manifest损坏不发送", async () => {
  const f = await fixture();
  try {
    assert.equal(await f.remote().call("burstStatus", { actionId: "missing" }), null);
    await assert.rejects(
      f.phone().sendBurst({
        actionId: "new",
        resumeActionId: "missing",
        conversationId: CONVERSATION,
        messages: ["不发"],
      }),
    );
    const manifest = await admitBurst(f.actions().directory, {
      actionId: "prepared",
      conversationId: CONVERSATION,
      messages: ["未准入1", "未准入2"],
    });
    const status = await f.phone().burstStatus({ actionId: "prepared" });
    assert.deepEqual(status!.items.map((item) => item.receipt), [null, null]);
    assert.equal(status!.complete, false);
    const entries = [];
    for await (const item of Deno.readDir(`${f.directory}/actions/bursts`)) entries.push(item.name);
    const path = `${f.directory}/actions/bursts/${entries[0]}`,
      original = await Deno.readTextFile(path);
    const tampered = { ...manifest, segments: ["被改正文", "未准入2"] };
    await Deno.writeTextFile(path, JSON.stringify(tampered));
    await assert.rejects(f.phone().burstStatus({ actionId: "prepared" }));
    await assert.rejects(
      f.phone().sendBurst({
        actionId: "resume",
        resumeActionId: "prepared",
        conversationId: CONVERSATION,
        messages: ["未准入1", "未准入2"],
      }),
    );
    assert.deepEqual(f.texts, []);
    await Deno.writeTextFile(path, original);
  } finally {
    await f.close();
  }
});
