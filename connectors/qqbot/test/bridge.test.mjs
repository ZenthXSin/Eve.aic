import test from "node:test";
import assert from "node:assert/strict";
import { Readable } from "node:stream";
import { createBridge, consume, optionsFromEnv, MAX_MESSAGE_ID_BYTES, MAX_SEGMENTS } from "../bridge-core.mjs";
function fixture({ fail = false, limit = 128, appId = "1904159860", hold = null } = {}) {
  const frames = [], sent = [], handlers = new Map();
  const bot = {
    appId,
    on: (event, handler) => handlers.set(event, handler),
    stop: () => handlers.set("stopped", true),
    async sendText(target, text) {
      sent.push({ target, text });
      if (hold) await hold;
      if (typeof fail === "function" ? fail(sent.length) : fail) throw new Error("sensitive secret and message");
      return { id: "out-1" };
    },
  };
  const bridge = createBridge(bot, f => frames.push(f), limit);
  const message = (kind = "c2c", id = "in-1") => handlers.get("message")({}, {
    kind, messageId: id, senderId: "user-1", content: "你好",
    rawEventType: kind === "group" ? "GROUP_AT_MESSAGE_CREATE" : "C2C_MESSAGE_CREATE",
    groupOpenid: "group-1",
    replyTarget: { scope: kind, targetId: kind === "group" ? "group-1" : "user-1", msgId: id },
  });
  return { bridge, frames, sent, message, handlers };
}
test("固定默认 AppID，仅密钥必填，sandbox 与最小 intent", () => {
  for (const env of [{ QQBOT_APP_SECRET: "test-only" }, { QQBOT_APP_SECRET: "test-only", QQBOT_SANDBOX: "false" }]) {
    assert.equal(optionsFromEnv(env).baseUrl, "https://api.sgroup.qq.com");
    assert.equal(optionsFromEnv(env).intents, 1 << 25);
  }
  const opt = optionsFromEnv({ QQBOT_APP_SECRET: "test-only", QQBOT_SANDBOX: "true" });
  assert.equal(opt.appId, "1904159860");
  assert.equal(opt.intents, 1 << 25);
  assert.equal(opt.baseUrl, "https://sandbox.api.sgroup.qq.com");
  assert.throws(() => optionsFromEnv({}));
  assert.throws(() => optionsFromEnv({ QQBOT_APP_SECRET: "test", QQBOT_SANDBOX: "yes" }));
});
for (const scope of ["c2c", "group"]) test(scope + " 回复只能使用原目标与 msg_id", async () => {
  const f = fixture(); f.message(scope);
  assert.equal(f.frames[0].user_id, "user-1");
  await f.bridge.command({ type: "reply", version: 1, id: "in-1", text: "答复", target_id: "attacker" });
  assert.deepEqual(f.sent[0].target, { scope, targetId: scope === "group" ? "group-1" : "user-1", msgId: "in-1" });
  assert.equal(f.frames.at(-1).ok, true);
  await f.bridge.command({ type: "reply", version: 1, id: "in-1", text: "再次答复" });
  assert.equal(f.sent.length, 1);
});
test("有界 pending、重复与 finish，不产生额外发送", async () => {
  const f = fixture({ limit: 1 }); f.message(); f.message(); f.message("c2c", "in-2");
  assert.equal(f.frames.filter(x => x.type === "message").length, 1);
  await f.bridge.command({ type: "finish", version: 1, id: "in-1" });
  f.message("c2c", "in-2");
  assert.equal(f.frames.filter(x => x.type === "message").length, 2);
  assert.equal(f.sent.length, 0);
});
test("发送失败无重试，不泄漏正文或异常", async () => {
  const f = fixture({ fail: true }); f.message();
  await f.bridge.command({ type: "reply", id: "in-1", text: "答复" });
  assert.equal(f.frames.find(x => x.type === "delivery").ok, false);
  assert.equal(f.sent.length, 1);
  assert.ok(!JSON.stringify(f.frames).includes("sensitive"));
});
test("无效/超长 JSONL 可跳过，后续 stop 与 EOF 收尾", async () => {
  const f = fixture();
  await consume(Readable.from(["x\n", "a".repeat(70000), '\n{"type":"stop","version":1}\n']), f.bridge);
  assert.equal(f.handlers.get("stopped"), true);
  assert.ok(f.frames.some(x => x.code === "frame_limit"));
  assert.ok(f.frames.some(x => x.code === "invalid_json"));
});

test("官方不透明消息 ID 的标点保持原值，正文中的提及不被删除", async () => {
  const f = fixture();
  const id = "ROBOT1.0_.b6nx.CVryAO0nR58RXuU6SC.m92gc19j02qKqdm8ek!";
  f.handlers.get("message")({}, {
    kind: "c2c", messageId: id, senderId: "user-1", content: "转告 <@user-2> 你好",
    rawEventType: "C2C_MESSAGE_CREATE",
    replyTarget: { scope: "c2c", targetId: "user-1", msgId: id },
  });
  assert.equal(f.frames[0].id, id);
  assert.equal(f.frames[0].text, "转告 <@user-2> 你好");
  await f.bridge.command({ type: "reply", version: 1, id, text: "答复" });
  assert.equal(f.sent[0].target.msgId, id);
  assert.equal(f.frames.at(-1).ok, true);
});

test("超过旧上限的群消息 ID 原样回复，253 字节边界与 UTF-8 字节一致", async () => {
  assert.equal(MAX_MESSAGE_ID_BYTES, 253);
  for (const id of ["ROBOT1.0_" + "g".repeat(128), "x".repeat(253), "界".repeat(84) + "!"]) {
    const f = fixture(); f.message("group", id);
    assert.equal(f.frames.find(x => x.type === "message").id, id);
    await f.bridge.command({ type: "reply", version: 1, id, text: "答复" });
    assert.equal(f.sent[0].target.msgId, id);
    assert.equal(f.frames.at(-1).ok, true);
  }
  for (const id of ["x".repeat(254), "界".repeat(85), " x", "x\n", "x\u0000y"]) {
    const f = fixture(); f.message("group", id);
    assert.equal(f.frames.filter(x => x.type === "message").length, 0);
    assert.equal(f.sent.length, 0);
  }
});

test("群聊接纳官方 @ 事件，拒绝普通群消息与机器人消息", async () => {
  const f = fixture();
  const incoming = {
    kind: "group", messageId: "group-at-1", senderId: "user-1", content: "/train start",
    rawEventType: "GROUP_MESSAGE_CREATE", groupOpenid: "group-1",
    replyTarget: { scope: "group", targetId: "group-1", msgId: "group-at-1" },
  };
  f.handlers.get("message")({}, incoming);
  f.handlers.get("message")({}, { ...incoming, rawEventType: "GROUP_AT_MESSAGE_CREATE", senderIsBot: true });
  assert.equal(f.frames.filter(x => x.type === "message").length, 0);
  f.handlers.get("message")({}, { ...incoming, rawEventType: "GROUP_AT_MESSAGE_CREATE" });
  assert.equal(f.frames.filter(x => x.type === "message").length, 1);
  await f.bridge.command({ type: "reply", version: 1, id: "group-at-1", text: "你喜欢怎样的称呼？" });
  assert.deepEqual(f.sent, [{ target: incoming.replyTarget, text: "你喜欢怎样的称呼？" }]);
});

test("普通群事件携带服务端自身提及时可原路回复，不依赖 AppID 等于 openid", async () => {
  const f = fixture();
  const incoming = {
    kind: "group", messageId: "group-mention-1", senderId: "user-1", content: "你好 <@another-user>",
    rawEventType: "GROUP_MESSAGE_CREATE", groupOpenid: "group-1",
    mentions: [null, { member_openid: "bot-openid", is_you: true }, { is_you: false }],
    replyTarget: { scope: "group", targetId: "group-1", msgId: "group-mention-1" },
  };
  f.handlers.get("message")({}, incoming);
  assert.equal(f.frames.find(x => x.type === "message").text, incoming.content);
  await f.bridge.command({ type: "reply", version: 1, id: incoming.messageId, text: "答复" });
  assert.deepEqual(f.sent, [{ target: incoming.replyTarget, text: "答复" }]);
});

test("普通群事件识别当前 AppID 的两种 @ 标记，拒绝其他提及、伪标记与未知事件", () => {
  const incoming = {
    kind: "group", messageId: "group-mention-2", senderId: "user-1", content: "你好",
    rawEventType: "GROUP_MESSAGE_CREATE", groupOpenid: "group-1",
    replyTarget: { scope: "group", targetId: "group-1", msgId: "group-mention-2" },
  };
  for (const content of ["<@102075770> 你好", "你好 <@!102075770>"]) {
    const f = fixture({ appId: "102075770" });
    f.handlers.get("message")({}, { ...incoming, content });
    assert.equal(f.frames.find(x => x.type === "message").text, content.startsWith("<@") ? "你好" : content);
  }
  for (const change of [
    {}, { content: "@机器人 你好" }, { content: "<@!1904159860> 你好" },
    { content: "<@1020757700> 你好" }, { content: "<@102075770" },
    { mentions: [{ is_you: false }, { is_you: "true" }] }, { mentions: { is_you: true } },
    { content: "<@102075770> 你好", senderIsBot: true },
    { rawEventType: "UNKNOWN", mentions: [{ is_you: true }] },
    { rawEventType: "UNKNOWN", content: "<@102075770> 你好" },
  ]) {
    const f = fixture({ appId: "102075770" });
    f.handlers.get("message")({}, { ...incoming, ...change });
    assert.equal(f.frames.filter(x => x.type === "message").length, 0);
    assert.equal(f.sent.length, 0);
  }
  for (const appId of [undefined, "", ".*", "102075770>"]) {
    const f = fixture({ appId });
    f.handlers.get("message")({}, { ...incoming, content: "<@102075770> 你好" });
    assert.equal(f.frames.filter(x => x.type === "message").length, 0);
  }
});

test("原生训练命令只剥离开头已确认的自身 @，保留其他提及与原回复路由", async () => {
  for (const mention of [
    { member_openid: "bot-openid", is_you: true },
    { user_openid: "bot-openid", is_you: true },
    { id: "bot-openid", is_you: true },
  ]) {
    const f = fixture({ appId: "102075770" });
    const target = { scope: "group", targetId: "group-1", msgId: "native-1" };
    const incoming = { kind: "group", messageId: "native-1", senderId: "user-1",
      rawEventType: "GROUP_MESSAGE_CREATE", groupOpenid: "group-1", mentions: [mention],
      replyTarget: target, content: " <@!bot-openid> /train stats " };
    f.handlers.get("message")({}, incoming);
    assert.equal(f.frames.find(x => x.type === "message").text, "/train stats");
    await f.bridge.command({ type: "reply", version: 1, id: "native-1", text: "统计" });
    assert.deepEqual(f.sent[0].target, target);
  }
  for (const content of ["<@other-user> /train reset", "正文 <@bot-openid> /train stop"]) {
    const f = fixture();
    f.handlers.get("message")({}, { kind: "group", messageId: "native-2", senderId: "user-1",
      rawEventType: "GROUP_MESSAGE_CREATE", groupOpenid: "group-1",
      mentions: [{ member_openid: "bot-openid", is_you: true }],
      replyTarget: { scope: "group", targetId: "group-1", msgId: "native-2" }, content });
    assert.equal(f.frames.find(x => x.type === "message").text, content);
  }
  const f = fixture();
  f.handlers.get("message")({}, { kind: "group", messageId: "empty-1", senderId: "user-1",
    rawEventType: "GROUP_MESSAGE_CREATE", groupOpenid: "group-1",
    mentions: [{ member_openid: "bot-openid", is_you: true }],
    replyTarget: { scope: "group", targetId: "group-1", msgId: "empty-1" }, content: "<@bot-openid>" });
  assert.equal(f.frames.filter(x => x.type === "message").length, 0);
});

const segment = (index, count, text = `第${index + 1}段`, id = "in-1") =>
  ({ type: "segment", version: 1, id, index, count, text });
for (const scope of ["c2c", "group"]) test(scope + " 分段按序回复原目标，末段后释放 pending", async () => {
  const f = fixture(); f.message(scope);
  await f.bridge.command(segment(0, 3));
  await f.bridge.command(segment(1, 3));
  await f.bridge.command(segment(2, 3));
  const target = { scope, targetId: scope === "group" ? "group-1" : "user-1", msgId: "in-1" };
  assert.deepEqual(f.sent, [0, 1, 2].map(i => ({ target, text: `第${i + 1}段` })));
  assert.deepEqual(f.frames.filter(x => x.type === "delivery").map(x => [x.index, x.ok]), [[0, true], [1, true], [2, true]]);
  await f.bridge.command(segment(2, 3));
  await f.bridge.command({ type: "reply", version: 1, id: "in-1", text: "再次答复" });
  assert.equal(f.sent.length, 3);
  assert.equal(f.frames.at(-1).code, "unknown_reply");
});
test("分段拒绝乱序、重复、越界和整条回复混用", async () => {
  const f = fixture(); f.message();
  for (const bad of [segment(1, 2), segment(0, 1), segment(0, MAX_SEGMENTS + 1), segment(2, 2),
                     { ...segment(0, 2), index: "0" }]) {
    await f.bridge.command(bad);
  }
  assert.equal(f.sent.length, 0);
  await f.bridge.command(segment(0, 2));
  await f.bridge.command(segment(0, 2));
  await f.bridge.command({ ...segment(1, 3) });
  await f.bridge.command({ type: "reply", version: 1, id: "in-1", text: "整条" });
  assert.deepEqual(f.sent.map(x => x.text), ["第1段"]);
  const codes = f.frames.filter(x => x.type === "warning").map(x => x.code);
  for (const code of ["invalid_command", "segment_order", "duplicate_reply"]) assert.ok(codes.includes(code), code);
  await f.bridge.command(segment(1, 2));
  assert.deepEqual(f.sent.map(x => x.text), ["第1段", "第2段"]);
});
test("分段失败即结束，不重试也不继续后续片段", async () => {
  const f = fixture({ fail: n => n === 2 }); f.message();
  await f.bridge.command(segment(0, 3));
  await f.bridge.command(segment(1, 3));
  await f.bridge.command(segment(2, 3));
  assert.deepEqual(f.sent.map(x => x.text), ["第1段", "第2段"]);
  assert.deepEqual(f.frames.filter(x => x.type === "delivery").map(x => [x.index, x.ok]), [[0, true], [1, false]]);
  assert.ok(!JSON.stringify(f.frames).includes("sensitive"));
});
test("段间 finish 释放 pending；发送中的 finish 在本段结束后释放", async () => {
  const f = fixture(); f.message();
  await f.bridge.command(segment(0, 3));
  await f.bridge.command({ type: "finish", version: 1, id: "in-1" });
  await f.bridge.command(segment(1, 3));
  assert.equal(f.sent.length, 1);
  let release;
  const g = fixture({ hold: new Promise(resolve => { release = resolve; }) }); g.message();
  const sending = g.bridge.command(segment(0, 2));
  await g.bridge.command({ type: "finish", version: 1, id: "in-1" });
  release();
  await sending;
  await g.bridge.command(segment(1, 2));
  assert.equal(g.sent.length, 1);
  assert.equal(g.frames.at(-1).code, "unknown_reply");
});
test("QQ 无法承载的正文明确回失败回执并释放消息，不让 Rust 等到超时", async () => {
  for (const text of [" ", "\uFEFF\uFEFF"]) {
    const f = fixture(); f.message();
    await f.bridge.command({ type: "reply", version: 1, id: "in-1", text });
    assert.deepEqual(f.frames.filter(x => x.type === "delivery"), [{ version: 1, type: "delivery", id: "in-1", ok: false }]);
    await f.bridge.command({ type: "reply", version: 1, id: "in-1", text: "迟到的回复" });
    assert.equal(f.sent.length, 0);
    const g = fixture(); g.message();
    await g.bridge.command(segment(0, 3));
    await g.bridge.command(segment(1, 3, text));
    await g.bridge.command(segment(2, 3));
    assert.deepEqual(g.frames.filter(x => x.type === "delivery").map(x => [x.index, x.ok]), [[0, true], [1, false]]);
    assert.deepEqual(g.sent.map(x => x.text), ["第1段"]);
  }
});
