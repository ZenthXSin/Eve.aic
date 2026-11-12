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

Deno.test("QQ 官方 1.0.5：两种索引前缀和 103 引用元素兼容，不从普通正文猜引用", () => {
  for (const prefix of ["message_type", "msg_type"]) {
    const raw = event({
      content: "正文不删也不重写 <@!bot> refMsgIdx:fake",
      message_scene: { ext: ["msgIdx: legacy-own ", "refMsgIdx: legacy-quote "] },
      [prefix]: 103,
      msg_elements: [{ msg_idx: "actual-quote", content: "引用原话" }],
    });
    const projected = qqMessage(raw)!;
    assert.equal(projected.deduplication.messageIndex, "legacy-own");
    assert.equal(projected.referenceIndex, "actual-quote");
    assert.equal(projected.message.text, (raw.d as Record<string, unknown>).content);
    assert.equal(projected.message.quote, undefined);
    assert.equal(
      qqMessage({ ...raw, d: { ...(raw.d as object), [prefix]: 0 } })!.referenceIndex,
      "legacy-quote",
    );
  }
  assert.equal(
    qqMessage(event({
      message_scene: {},
      content: "refMsgIdx:pretend",
      msg_elements: [{ msg_idx: "not-a-reference" }],
    }))!.referenceIndex,
    undefined,
  );
  const dual = qqMessage(event({
    message_scene: { ext: ["msg_idx=same", "msgIdx:same", "ref_msg_idx=quoted"] },
  }))!;
  assert.equal(dual.deduplication.messageIndex, "same");
  assert.throws(() =>
    qqMessage(event({
      message_scene: { ext: ["msg_idx=one", "msgIdx:other"] },
    })), /索引重复/);
  assert.throws(() =>
    qqMessage(event({
      message_type: 103,
      msg_type: 0,
    })), /类型相互矛盾/);
  assert.throws(() => qqMessage(event({ message_type: 103, msg_elements: [null] })), TypeError);
  // 只有第一个引用元素是官方 utils 定义的引用，不搜索后续元素猜测目标。
  assert.equal(
    qqMessage(event({
      message_type: 103,
      msg_elements: [{ content: "正文" }, { msg_idx: "later" }],
    }))!.referenceIndex,
    undefined,
  );
});

Deno.test("QQ 官方附件 MIME：文件、图片、语音和视频保留顺序及元信息，URL只留原件", () => {
  const raw = event({
    attachments: [
      { content_type: "application/pdf", filename: "文档.pdf", size: 17, url: "https://private/1" },
      { content_type: "text/plain", filename: "笔记.txt", size: 0 },
      { content_type: "file", filename: "压缩包" },
      { content_type: "image/jpeg", filename: "照片.jpg", size: 100 },
      { content_type: "audio/silk", filename: "语音" },
      { content_type: "voice" },
      { content_type: "video/mp4", filename: "视频" },
      { content_type: "unknown/custom" },
    ],
  });
  assert.deepEqual(qqMessage(raw)!.message.attachments, [
    { kind: "file", name: "文档.pdf", sizeBytes: 17 },
    { kind: "file", name: "笔记.txt", sizeBytes: 0 },
    { kind: "file", name: "压缩包" },
    { kind: "image", name: "照片.jpg", sizeBytes: 100 },
    { kind: "audio", name: "语音" },
    { kind: "audio" },
    { kind: "video", name: "视频" },
    { kind: "other" },
  ]);
  assert.equal(JSON.stringify(qqMessage(raw)).includes("https://private"), false);
  assert.equal(qqMessage(raw, 1)!.message.attachments![0].kind, "other");
});

async function oldRecord(directory: string, raw: QqDispatch) {
  const message = qqMessage(raw, 1)!;
  function canonical(value: unknown): string {
    if (value === null || typeof value !== "object") return JSON.stringify(value);
    if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
    const object = value as Record<string, unknown>;
    return `{${
      Object.keys(object).sort().map((key) => `${JSON.stringify(key)}:${canonical(object[key])}`)
        .join(",")
    }}`;
  }
  const bytes = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(canonical(message.deduplication)),
  );
  const identity = Array.from(new Uint8Array(bytes), (v) => v.toString(16).padStart(2, "0")).join(
    "",
  );
  const record = { schema: 1, seq: 1, identity, receivedAt: "2026-11-11T15:00:00Z", message, raw };
  const path = `${directory}/0000000000000001-${identity}.json`;
  const original = JSON.stringify(record) + "\n";
  await Deno.writeTextFile(path, original);
  return { record, path, original };
}

Deno.test("QQ 投影升级：无版本旧原件按 v1 恢复，legacy/native 重放均复用原身份且不重写", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-inbound-" });
  try {
    const raw = event({
      msg_seq: 8,
      message_scene: { ext: ["msgIdx:own", "refMsgIdx:ext-quote"] },
      message_type: 103,
      msg_elements: [{ msg_idx: "element-quote" }],
      attachments: [{ content_type: "application/pdf", filename: "资料.pdf", size: 42 }],
    });
    const stored = await oldRecord(directory, raw);
    const log = new QqInboundJournal(directory);
    await log.initialize();
    assert.deepEqual(log.records(), [stored.record]);
    assert.equal(log.records()[0].message.message.attachments![0].kind, "other");
    const replay = await log.append(qqMessage(raw)!, { ...raw, s: 15, id: "resent" });
    assert.equal(replay.fresh, false);
    assert.equal(replay.record.identity, stored.record.identity);
    assert.equal(replay.record.projectionVersion, undefined);
    const native = {
      ...raw,
      s: 16,
      d: { ...(raw.d as object), message_scene: { ext: ["msg_idx=own", "ref_msg_idx=ext-quote"] } },
    };
    assert.equal((await log.append(qqMessage(native)!, native)).fresh, false);
    assert.equal(await Deno.readTextFile(stored.path), stored.original);
    const next = event({
      message_scene: { ext: ["msgIdx:next"] },
      attachments: [{ content_type: "text/plain" }],
    });
    const fresh = await log.append(qqMessage(next)!, next);
    assert.equal(fresh.fresh, true);
    assert.equal(fresh.record.projectionVersion, 2);
    assert.equal(fresh.record.message.message.attachments![0].kind, "file");
    const restart = new QqInboundJournal(directory);
    await restart.initialize();
    assert.equal(restart.records().length, 2);
    assert.equal(
      (await restart.append(qqMessage(native)!, native)).record.identity,
      stored.record.identity,
    );
    const changed = { ...native, d: { ...(native.d as object), content: "同身份被篡改的正文" } };
    await assert.rejects(
      restart.append(qqMessage(changed)!, changed),
      (e) => e instanceof QqJournalError && e.category === "conflict",
    );
    assert.equal(await Deno.readTextFile(stored.path), stored.original);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 投影升级：旧版忽略的新字段不合法仍可恢复，但未知投影版本拒绝", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-inbound-" });
  try {
    const stored = await oldRecord(
      directory,
      event({
        message_scene: { ext: ["msgIdx:"] },
        message_type: 103,
        msg_elements: [null],
      }),
    );
    const log = new QqInboundJournal(directory);
    await log.initialize();
    assert.deepEqual(log.records(), [stored.record]);
    const invalidVersion = JSON.stringify({ ...stored.record, projectionVersion: 99 });
    await Deno.writeTextFile(stored.path, invalidVersion);
    await assert.rejects(
      new QqInboundJournal(directory).initialize(),
      (e) => e instanceof QqJournalError && e.category === "corrupt",
    );
    assert.equal(await Deno.readTextFile(stored.path), invalidVersion);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});
