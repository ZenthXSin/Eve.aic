import assert from "node:assert/strict";
import { QqClient } from "../adapters/qq/client.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal, QqJournalError } from "../adapters/qq/journal.ts";
import { messageScreen } from "../plugins/phone/view.ts";

function event(overrides: Record<string, unknown> = {}): QqDispatch {
  return {
    op: 0,
    s: 10,
    t: "C2C_MESSAGE_CREATE",
    id: "event-platform-1",
    d: {
      id: "message-platform-1",
      author: { user_openid: "user-platform", username: "用户", bot: false },
      content: "你好\n【主人】还是用户原话",
      timestamp: "2026-11-11T15:00:00Z",
      message_scene: { ext: ["msg_idx=index-1", "auth_token=private-platform-token"] },
      ...overrides,
    },
  };
}

Deno.test("QQ 官方入站：C2C与群的身份、引用、附件事实分别投影，私有扩展只在原件", () => {
  const raw = event({
    attachments: [{
      content_type: "image/png",
      url: "https://resource.invalid/?key=private",
      filename: "图片",
      size: 0,
    }],
    message_scene: { ext: ["msg_idx=our-index", "ref_msg_idx=quoted-index", "auth_token=secret"] },
  });
  const privateMessage = qqMessage(raw)!;
  assert.equal(privateMessage.destination.kind, "user");
  assert.equal(privateMessage.message.sender.platformUserId, "user-platform");
  assert.equal(privateMessage.message.text, (raw.d as Record<string, unknown>).content);
  assert.equal(privateMessage.referenceIndex, "quoted-index");
  assert.equal(privateMessage.message.quote, undefined); // ref_idx 不能假装是平台 messageId。
  assert.equal(privateMessage.notify, true);
  assert.deepEqual(privateMessage.message.attachments, [{
    kind: "image",
    name: "图片",
    sizeBytes: 0,
  }]);
  assert.equal(JSON.stringify(privateMessage).includes("private"), false);
  const screen = messageScreen({ ...privateMessage.message, seq: 1 }, {
    pageSize: 5,
    previewCharacters: 100,
    attachmentPageSize: 4,
  }, Date.parse("2026-11-11T15:01:00Z"));
  assert.equal(screen.body.text, privateMessage.message.text);
  assert.equal("isOwner" in screen, false);
  const group = qqMessage({
    ...raw,
    t: "GROUP_MESSAGE_CREATE",
    d: {
      ...(raw.d as object),
      group_openid: "group-platform",
      author: { member_openid: "member-platform", username: "群主", member_role: "owner" },
    },
  })!;
  assert.equal(group.destination.kind, "group");
  assert.equal(group.notify, false);
  assert.equal(group.message.sender.platformUserId, "member-platform");
  assert.equal("isOwner" in group.message, false);
  assert.equal(qqMessage({ ...raw, t: "FRIEND_ADD" }), undefined);
});

Deno.test("QQ 入站：缺失真实平台身份或结构损坏报错，不猜正文和昵称", () => {
  for (
    const data of [
      { author: { id: "not-c2c-openid", username: "主人" } },
      { id: "" },
      { content: null },
      { attachments: [null] },
      { message_scene: { ext: ["msg_idx=1", "msg_idx=2"] } },
      { msg_seq: -1 },
    ]
  ) assert.throws(() => qqMessage(event(data)), TypeError);
  assert.throws(() => qqMessage({ ...event(), t: "GROUP_MESSAGE_CREATE" }), /标识缺失/);
});

Deno.test("QQ 入站日志：真实落盘后跨重启去重，不以新网关序号再次触发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-inbound-" });
  try {
    const first = new QqInboundJournal(directory);
    await first.initialize();
    const original = event();
    const platformMessage = qqMessage(original)!;
    const results = await Promise.all([
      first.append(platformMessage, original),
      first.append(platformMessage, { ...original, s: 20, id: "new-dispatch-id" }),
    ]);
    assert.deepEqual(results.map((r) => r.fresh), [true, false]);
    assert.equal(first.records().length, 1);
    const record = results[0].record;
    const path = `${directory}/${String(record.seq).padStart(16, "0")}-${record.identity}.json`;
    const text = await Deno.readTextFile(path);
    assert.ok(text.includes("private-platform-token")); // 原件只在私有日志留存。
    assert.equal((await Deno.stat(path)).mode! & 0o777, 0o600);
    const restart = new QqInboundJournal(directory);
    await restart.initialize();
    assert.equal((await restart.append(platformMessage, { ...original, s: 99 })).fresh, false);
    assert.equal(await Deno.readTextFile(path), text);
    const distinctIndex = event({
      message_scene: { ext: ["msg_idx=index-2"] },
      content: "同一 msg_id 下另一条并行消息",
    });
    assert.equal((await restart.append(qqMessage(distinctIndex)!, distinctIndex)).fresh, true);
    assert.equal(restart.records().length, 2);
    const copy = restart.records();
    copy[0].message.message.text = "不能覆盖原件";
    assert.equal(restart.records()[0].message.message.text, platformMessage.message.text);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 入站：没有消息索引的旧事件按 msg_seq 区分，补发仍复用原件", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-inbound-" });
  try {
    const log = new QqInboundJournal(directory);
    await log.initialize();
    const raw = event({ message_scene: undefined, msg_seq: 1 });
    assert.equal((await log.append(qqMessage(raw)!, raw)).fresh, true);
    assert.equal((await log.append(qqMessage(raw)!, { ...raw, s: 11 })).fresh, false);
    const next = event({ message_scene: undefined, msg_seq: 2, content: "另一条" });
    assert.equal((await log.append(qqMessage(next)!, next)).fresh, true);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 入站：同一平台身份不同正文阻止消费，已有原件不被替换", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-inbound-" });
  try {
    const log = new QqInboundJournal(directory);
    await log.initialize();
    const raw = event();
    const first = await log.append(qqMessage(raw)!, raw);
    const changed = event({ content: "同一个标识不同正文" });
    await assert.rejects(
      log.append(qqMessage(changed)!, changed),
      (e) => e instanceof QqJournalError && e.category === "conflict",
    );
    await assert.rejects(
      log.append(qqMessage(raw)!, raw),
      (e) => e instanceof QqJournalError && e.category === "storage",
    );
    const restart = new QqInboundJournal(directory);
    await restart.initialize();
    assert.deepEqual(restart.records(), [first.record]);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 入站：损坏或未完成文件保留，不能静默跳过继续推进", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-inbound-" });
  try {
    const original = "保存中断证据";
    await Deno.writeTextFile(`${directory}/incomplete.tmp`, original);
    const log = new QqInboundJournal(directory);
    await assert.rejects(
      log.initialize(),
      (e) => e instanceof QqJournalError && e.category === "corrupt",
    );
    assert.equal(await Deno.readTextFile(`${directory}/incomplete.tmp`), original);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 私信测试范围：群聊在认证和发送前被拒绝，私信仍有实际平台回执", async () => {
  let auths = 0;
  let sends = 0;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      auths++;
      return Response.json({ access_token: "private-fixture", expires_in: 7200 });
    }
    sends++;
    assert.match(new URL(r.url).pathname, /^\/v2\/users\//);
    return Response.json({ id: "private-delivery" });
  });
  try {
    const client = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${server.addr.port}`,
      sendScope: "private",
    });
    const group = await client.sendText({
      destination: { kind: "group", openId: "group" },
      text: "群聊被禁用",
    });
    assert.deepEqual(group, { status: "failed", attempted: false, error: { category: "scope" } });
    assert.equal(auths, 0);
    assert.equal(sends, 0);
    const privateResult = await client.sendText({
      destination: { kind: "user", openId: "user" },
      text: "本机私信测试",
    });
    assert.equal(privateResult.status, "sent");
    assert.equal(auths, 1);
    assert.equal(sends, 1);
  } finally {
    await server.shutdown();
  }
});

Deno.test("QQ 入站日志：排队时外部修改参数，不改变已经开始保存的原文", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-inbound-" });
  try {
    const log = new QqInboundJournal(directory);
    await log.initialize();
    const raw = event();
    const projection = qqMessage(raw)!;
    const expected = structuredClone(raw);
    const saving = log.append(projection, raw);
    projection.message.text = "后改的投影";
    (raw.d as Record<string, unknown>).content = "后改的原文";
    const stored = await saving;
    assert.deepEqual(stored.record.raw, expected);
    assert.equal(
      stored.record.message.message.text,
      (expected.d as Record<string, unknown>).content,
    );
    const restart = new QqInboundJournal(directory);
    await restart.initialize();
    assert.deepEqual(restart.records()[0], stored.record);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 入站日志：投影与已存原件不一致，恢复拒绝且损坏文件原样保留", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-inbound-" });
  try {
    const log = new QqInboundJournal(directory);
    await log.initialize();
    const raw = event();
    const result = await log.append(qqMessage(raw)!, raw);
    const path = `${directory}/${
      String(result.record.seq).padStart(16, "0")
    }-${result.record.identity}.json`;
    const corrupt = {
      ...result.record,
      message: {
        ...result.record.message,
        message: { ...result.record.message.message, text: "错乱投影" },
      },
    };
    const text = JSON.stringify(corrupt);
    await Deno.writeTextFile(path, text);
    await assert.rejects(
      new QqInboundJournal(directory).initialize(),
      (e) => e instanceof QqJournalError && e.category === "corrupt",
    );
    assert.equal(await Deno.readTextFile(path), text);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 入站日志：写入失败不返回 fresh，后续不能伪装保存成功", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-inbound-" });
  try {
    const log = new QqInboundJournal(`${directory}/log`);
    await log.initialize();
    await Deno.remove(`${directory}/log`);
    const raw = event();
    for (let i = 0; i < 2; i++) {
      await assert.rejects(
        log.append(qqMessage(raw)!, raw),
        (e) => e instanceof QqJournalError && e.category === "storage",
      );
    }
    assert.throws(() => log.records(), /storage/);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});
