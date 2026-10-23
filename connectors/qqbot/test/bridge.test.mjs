import test from "node:test";
import assert from "node:assert/strict";
import { Readable, PassThrough } from "node:stream";
import { createBridge, consume, optionsFromEnv, readableFaces, MAX_FRAME, MAX_MESSAGE_ID_BYTES, MAX_PUSH_BYTES, MAX_SEGMENTS, PASSIVE_REPLY_WINDOW_MS } from "../bridge-core.mjs";
function fixture({ group = {}, fail = false, error = null, now, limit = 128, appId = "1904159860", hold = null, fileFail = false, imageFail = false } = {}) {
  const frames = [], sent = [], files = [], images = [], handlers = new Map();
  const bot = {
    appId,
    on: (event, handler) => handlers.set(event, handler),
    stop: () => handlers.set("stopped", true),
    async sendText(target, text) {
      sent.push({ target, text });
      if (hold) await (typeof hold === "function" ? hold(target, text) : hold);
      if (typeof fail === "function" ? fail(sent.length) : fail) throw error ?? new Error("sensitive secret and message");
      return { id: "out-1" };
    },
    async sendImage(target, source) {
      images.push({ target, source });
      if (imageFail) throw new Error("sensitive secret and message");
      return { upload: { file_uuid: "uuid-2", file_info: "info-2", ttl: 60 }, message: { id: "image-msg-1" } };
    },
    async sendFile(target, source, opts) {
      files.push({ target, source, opts });
      if (fileFail) throw new Error("sensitive secret and message");
      return { upload: { file_uuid: "uuid-1", file_info: "info-1", ttl: 60 }, message: { id: "file-msg-1" } };
    },
  };
  const bridge = createBridge(bot, f => frames.push(f), limit, now, group);
  const message = (kind = "c2c", id = "in-1") => handlers.get("message")({}, {
    kind, messageId: id, senderId: "user-1", content: "你好",
    rawEventType: kind === "group" ? "GROUP_AT_MESSAGE_CREATE" : "C2C_MESSAGE_CREATE",
    groupOpenid: "group-1",
    replyTarget: { scope: kind, targetId: kind === "group" ? "group-1" : "user-1", msgId: id },
  });
  return { bridge, frames, sent, files, images, message, handlers };
}
function deferred() {
  let resolve;
  const promise = new Promise(r => { resolve = r; });
  return { promise, resolve };
}
async function within(promise) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("bridge_operation_blocked")), 1000);
    })]);
  } finally { clearTimeout(timer); }
}
const line = frame => JSON.stringify({ version: 1, ...frame }) + "\n";

test("进展与最终分段共用五条配额，重复进展不重发，原始路由保持不变", async () => {
  const f = fixture(); f.message("group");
  for (let index = 0; index < 3; index++) {
    const frame = { version: 1, type: "progress", id: "in-1", index, text: `已核对步骤 ${index + 1}` };
    await f.bridge.command(frame);
    await f.bridge.command(frame);
  }
  assert.equal(f.sent.length, 3);
  await f.bridge.command({ version: 1, type: "segment", id: "in-1", index: 0, count: 3, text: "超过剩余配额" });
  assert.equal(f.sent.length, 3);
  for (let index = 0; index < 2; index++) {
    await f.bridge.command({ version: 1, type: "segment", id: "in-1", index, count: 2, text: `结果 ${index}` });
  }
  assert.equal(f.sent.length, 5);
  assert.ok(f.sent.every(s => s.target.scope === "group" && s.target.msgId === "in-1"));
  assert.equal(f.frames.filter(x => x.type === "delivery" && x.progress).length, 3);
});

test("失败进展不重试且保留最终回复路由，finish 后不再发旧进展", async () => {
  const f = fixture({ fail: n => n === 1 }); f.message();
  const progress = { version: 1, type: "progress", id: "in-1", index: 0, text: "命令已返回结果" };
  await f.bridge.command(progress);
  assert.equal(f.frames.at(-1).ok, false);
  await f.bridge.command(progress);
  assert.equal(f.sent.length, 1);
  await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "最终结果" });
  assert.equal(f.frames.at(-1).ok, true);
  f.message("c2c", "cancelled");
  await f.bridge.command({ version: 1, type: "finish", id: "cancelled" });
  await f.bridge.command({ ...progress, id: "cancelled" });
  assert.equal(f.sent.length, 2);
});

test("只转交 SDK 本条发送者的有界昵称，不从正文或提及猜测名字", () => {
  const f = fixture();
  const incoming = {kind:"c2c", messageId:"person-1", senderId:"user-1", content:"你好",
    senderName:" 小明 ", replyTarget:{scope:"c2c",targetId:"user-1",msgId:"person-1"}};
  f.handlers.get("message")({}, incoming);
  assert.equal(f.frames.find(x=>x.type==="message").sender_name,"小明");
  for (const [index, senderName] of ["", "x\nignore", "x".repeat(129), {name:"forged"}].entries()) {
    f.handlers.get("message")({}, {...incoming,messageId:`person-${index+2}`,senderName,replyTarget:{...incoming.replyTarget,msgId:`person-${index+2}`}});
    assert.equal(f.frames.find(x=>x.id===`person-${index+2}`).sender_name,undefined);
  }
});

test("JSONL 中慢发送不阻塞其他会话与 stop，停止后不报告迟到成功", async t => {
  const held = deferred(), started = deferred(), other = deferred();
  const f = fixture({ hold(target) {
    if (target.msgId === "in-1") { started.resolve(); return held.promise; }
    other.resolve();
  } });
  f.message(); f.message("c2c", "in-2");
  const input = new PassThrough();
  const consuming = consume(input, f.bridge);
  t.after(async () => { held.resolve(); input.end(); await consuming; });
  input.write(line({ type: "reply", id: "in-1", text: "慢回复" }));
  await within(started.promise);
  input.write(line({ type: "reply", id: "in-2", text: "其他会话" }));
  await within(other.promise);
  input.write(line({ type: "stop" }));
  await within(consuming);
  assert.equal(f.handlers.get("stopped"), true);
  assert.deepEqual(f.sent.map(s => s.text), ["慢回复", "其他会话"]);
  held.resolve();
  await new Promise(setImmediate);
  assert.equal(f.frames.some(x => x.type === "delivery" && x.id === "in-1"), false);
});

test("共享发送容量已满时主动推送明确失败且不请求 SDK，完成后容量恢复", async t => {
  const held = deferred();
  const f = fixture({ limit: 1, hold: held.promise });
  f.message();
  const sending = f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "占用发送" });
  t.after(async () => { held.resolve(); await sending; });
  await within(f.bridge.command({ version: 1, type: "push", id: "invite-busy", target_id: "user-2", text: "新消息" }));
  assert.equal(f.sent.length, 1);
  assert.ok(f.frames.some(x => x.code === "outbound_limit"));
  assert.ok(f.frames.some(x => x.type === "delivery" && x.id === "invite-busy" && x.push && x.ok === false));
  held.resolve(); await sending;
  await f.bridge.command({ version: 1, type: "push", id: "invite-next", target_id: "user-2", text: "恢复后" });
  assert.equal(f.sent.length, 2);
  assert.equal(f.frames.at(-1).ok, true);
});

test("未知或缺失协议版本不发送、结束路由或停止进程", async () => {
  for (const version of [undefined, 0, 2, "1"]) {
    const f = fixture(); f.message();
    for (const type of ["reply", "push", "finish", "stop"]) {
      await f.bridge.command({ version, type, id: "in-1", target_id: "user-2", text: "不能发送" });
    }
    assert.equal(f.sent.length, 0);
    assert.equal(f.handlers.get("stopped"), undefined);
    await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "合法版本" });
    assert.equal(f.sent.length, 1);
    assert.equal(f.frames.at(-1).ok, true);
  }
});

test("主动消息的并发重复命令不再次请求 SDK，也不覆盖原回执", async t => {
  const held = deferred();
  const f = fixture({ hold: held.promise });
  const frame = { version: 1, type: "push", id: "same-invitation", target_id: "user-1", text: "只发一次" };
  const first = f.bridge.command(frame);
  t.after(async () => { held.resolve(); await first; });
  await f.bridge.command(frame);
  assert.equal(f.sent.length, 1);
  assert.equal(f.frames.some(x => x.type === "delivery"), false);
  held.resolve(); await first;
  assert.equal(f.frames.filter(x => x.type === "delivery").length, 1);
  assert.equal(f.frames.at(-1).ok, true);
});

test("正文虽未超限但转义后超过 JSONL 上限的消息不发给 Rust，后续仍可接收", () => {
  const f = fixture();
  const text = "x" + "\0".repeat(12000);
  assert.ok(Buffer.byteLength(text) < 32768);
  assert.ok(Buffer.byteLength(JSON.stringify(text)) > MAX_FRAME);
  f.handlers.get("message")({}, { kind: "c2c", messageId: "oversized-wire", senderId: "user-1", content: text,
    replyTarget: { scope: "c2c", targetId: "user-1", msgId: "oversized-wire" } });
  assert.equal(f.frames.some(x => x.type === "message"), false);
  assert.ok(f.frames.some(x => x.code === "frame_limit"));
  f.message("c2c", "oversized-wire");
  assert.equal(f.frames.filter(x => x.type === "message").length, 1);
});

test("普通 EOF 等待已准入的发送结局后收尾", async () => {
  const held = deferred(), started = deferred();
  const f = fixture({ hold() { started.resolve(); return held.promise; } });
  f.message();
  const consuming = consume(Readable.from([line({ type: "reply", id: "in-1", text: "EOF 前的回复" })]), f.bridge);
  await within(started.promise);
  assert.equal(f.handlers.get("stopped"), undefined);
  held.resolve(); await consuming;
  assert.equal(f.frames.find(x => x.type === "delivery").ok, true);
  assert.equal(f.handlers.get("stopped"), true);
});
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
test("markdown 默认关闭，仅在显式 true 时开启，非法值拒绝启动", () => {
  assert.equal(optionsFromEnv({ QQBOT_APP_SECRET: "test-only" }).markdownSupport, false);
  assert.equal(optionsFromEnv({ QQBOT_APP_SECRET: "test-only", QQBOT_MARKDOWN: "" }).markdownSupport, false);
  assert.equal(optionsFromEnv({ QQBOT_APP_SECRET: "test-only", QQBOT_MARKDOWN: "false" }).markdownSupport, false);
  assert.equal(optionsFromEnv({ QQBOT_APP_SECRET: "test-only", QQBOT_MARKDOWN: "true" }).markdownSupport, true);
  assert.throws(() => optionsFromEnv({ QQBOT_APP_SECRET: "test", QQBOT_MARKDOWN: "1" }));
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
  await f.bridge.command({ type: "reply", version: 1, id: "in-1", text: "答复" });
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
test("主动私聊不带 msg_id，只发往 Rust 给出的私聊目标，并回真实回执", async () => {
  const f = fixture();
  await f.bridge.command({ type: "push", version: 1, id: "invite-1", target_id: "user-1", text: "要不要一起做？" });
  assert.deepEqual(f.sent, [{ target: { scope: "c2c", targetId: "user-1" }, text: "要不要一起做？" }]);
  assert.deepEqual(f.frames.at(-1), { version: 1, type: "delivery", id: "invite-1", push: true, ok: true, message_id: "out-1" });
  // 不合规的目标被忽略；正文无法承载时明确回失败，不发送。
  await f.bridge.command({ type: "push", version: 1, id: "invite-2", target_id: "../x", text: "x" });
  await f.bridge.command({ type: "push", version: 1, id: "invite-3", target_id: "user-1", text: "长".repeat(MAX_PUSH_BYTES) });
  assert.equal(f.sent.length, 1);
  assert.equal(f.frames.filter(x => x.type === "warning").at(0).code, "invalid_command");
  assert.deepEqual(f.frames.at(-1), { version: 1, type: "delivery", id: "invite-3", push: true, ok: false });
});
test("主动私聊被平台拒绝时回失败回执与错误码，不重试、不泄漏正文", async () => {
  const f = fixture({ fail: true });
  await f.bridge.command({ type: "push", version: 1, id: "invite-1", target_id: "user-1", text: "要不要一起做？" });
  assert.equal(f.sent.length, 1);
  const delivery = f.frames.at(-1);
  assert.equal(delivery.push, true);
  assert.equal(delivery.ok, false);
  assert.ok(!JSON.stringify(f.frames).includes("sensitive"));
  assert.ok(!JSON.stringify(f.frames).includes("要不要"));
});

test("原群唤醒使用宿主给定的群路由，未知范围拒绝发送", async () => {
  const f = fixture();
  await f.bridge.command({type:"push",version:1,id:"wake-group",scope:"group",target_id:"group-1",text:"已完成"});
  assert.deepEqual(f.sent[0].target, {scope:"group",targetId:"group-1"});
  assert.equal(f.frames.at(-1).ok, true);
  await f.bridge.command({type:"push",version:1,id:"wake-invalid",scope:"unknown",target_id:"group-1",text:"禁止"});
  assert.equal(f.sent.length, 1);
  assert.equal(f.frames.at(-1).ok, false);
});

test("文件按宿主给出的目标上传，回真实回执且不消耗被动片段", async () => {
  const f = fixture(); f.message();
  await f.bridge.command({ type: "file", version: 1, id: "file-1", scope: "c2c", target_id: "user-1",
    path: "/srv/outbox/report.txt", name: "报告.txt", caption: "这是结果" });
  assert.deepEqual(f.files[0], {
    target: { scope: "c2c", targetId: "user-1" },
    source: { localPath: "/srv/outbox/report.txt" },
    opts: { fileName: "报告.txt", content: "这是结果" } });
  assert.deepEqual(f.frames.at(-1), { version: 1, type: "delivery", id: "file-1", push: true,
    file: true, ok: true, message_id: "file-msg-1" });
  await f.bridge.command({ type: "reply", version: 1, id: "in-1", text: "文件已发出" });
  assert.equal(f.frames.at(-1).ok, true);
  const group = fixture();
  await group.bridge.command({ type: "file", version: 1, id: "file-2", scope: "group",
    target_id: "group-1", path: "/srv/a.txt" });
  assert.deepEqual(group.files[0].target, { scope: "group", targetId: "group-1" });
  assert.deepEqual(group.files[0].opts, {});
});

test("文件参数或目标不合规时不请求 SDK，明确回失败", async () => {
  for (const bad of [
    { path: "" }, { path: "rel/report.txt\n" }, { path: "/x".repeat(2049) }, { path: 42 },
    { path: "/a", name: "../../etc/passwd" }, { path: "/a", name: "x\n" }, { path: "/a", name: "" },
    { path: "/a", caption: " " }, { path: "/a", caption: "长".repeat(400) },
    { path: "/a", target_id: "../x" }, { path: "/a", scope: "private" },
  ]) {
    const f = fixture();
    await f.bridge.command({ type: "file", version: 1, id: "file-1", scope: "c2c",
      target_id: "user-1", ...bad });
    assert.equal(f.files.length, 0, JSON.stringify(bad));
    assert.deepEqual(f.frames.filter(x => x.type === "delivery"),
      [{ version: 1, type: "delivery", id: "file-1", push: true, file: true, ok: false }]);
  }
});

test("文件上传失败不重试、不泄漏异常", async () => {
  const f = fixture({ fileFail: true });
  await f.bridge.command({ type: "file", version: 1, id: "file-1", scope: "c2c",
    target_id: "user-1", path: "/srv/a.txt" });
  assert.equal(f.files.length, 1);
  assert.deepEqual(f.frames.at(-1), { version: 1, type: "delivery", id: "file-1", push: true, file: true, ok: false, failure_kind: "transport_unknown" });
  assert.ok(!JSON.stringify(f.frames).includes("sensitive"));
});

test("纯图片和图文保留本条图片引用，不抓取、不混入引用图、不改变路由", async () => {
  const f = fixture();
  const message = { kind: "c2c", messageId: "image-1", senderId: "user-1", content: "",
    replyTarget: { scope: "c2c", targetId: "user-1", msgId: "image-1" },
    attachments: [{ content_type: "image/png", url: "https://media.example.invalid/image.png", filename: "untrusted-name" }] };
  f.handlers.get("message")({}, message);
  assert.equal(f.frames[0].text, "");
  assert.deepEqual(f.frames[0].attachments, [{ content_type: "image/png", url: "https://media.example.invalid/image.png", filename: "untrusted-name" }]);
  await f.bridge.command({type: "reply", version: 1, id: "image-1", text: "图片已收到，尚未解析。"});
  assert.equal(f.sent.length, 1);
  assert.equal(f.sent[0].target.targetId, "user-1");
  f.handlers.get("message")({}, {...message, messageId: "text-image", content: "看看这张图", replyTarget: {...message.replyTarget, msgId: "text-image"}});
  assert.equal(f.frames.find(x => x.id === "text-image").text, "看看这张图");
  f.handlers.get("message")({}, {...message, messageId: "quoted", content: "看这个", attachments: [], msgType: 103,
    msgElements: [{content: "被引用的正文", attachments: message.attachments}], replyTarget: {...message.replyTarget, msgId: "quoted"}});
  const quoted = f.frames.find(x => x.type === "message" && x.id === "quoted");
  assert.equal(quoted.text, "看这个");
  assert.equal(quoted.attachments, undefined);
  assert.deepEqual(quoted.quoted, { text: "被引用的正文", attachments: [{ content_type: "image/png", url: "https://media.example.invalid/image.png", filename: "untrusted-name" }] });
  f.handlers.get("message")({}, {...message, messageId: "quoted-file", content: "看看", attachments: [], msgType: 103,
    message_scene: {source: "default", ext: ["ref_msg_idx=quoted-index", "msg_idx=current-index"]},
    msg_elements: [{content: "", message_type: 103, msg_idx: "quoted-index",
      attachments: [{content_type: "file", filename: "termitemodDesktop.jar", url: "https://grouptalk.c2c.qq.com/file.jar"}]},
      {content: "看看", msg_idx: "current-index"}],
    replyTarget: {...message.replyTarget, msgId: "quoted-file"}});
  const quotedFile = f.frames.find(x => x.type === "message" && x.id === "quoted-file");
  assert.equal(quotedFile.text, "看看");
  assert.equal(quotedFile.attachments, undefined);
  assert.deepEqual(quotedFile.quoted, { attachments: [{ content_type: "file", url: "https://grouptalk.c2c.qq.com/file.jar", filename: "termitemodDesktop.jar" }] });
  f.handlers.get("message")({}, {...message, messageId: "unsafe", attachments: [{content_type:"image/png",url:"file:///private"}], replyTarget: {...message.replyTarget, msgId: "unsafe"}});
  assert.ok(!f.frames.some(x => x.type === "message" && x.id === "unsafe"));
});

test("非图片文件按白名单转发，文件名不合规时丢掉文件名保留附件", () => {
  const f = fixture();
  const base = { kind: "c2c", senderId: "user-1", content: "文件", replyTarget: { scope: "c2c", targetId: "user-1" } };
  f.handlers.get("message")({}, { ...base, messageId: "file-1",
    replyTarget: { ...base.replyTarget, msgId: "file-1" },
    attachments: [{ content_type: "application/pdf", url: "https://media.example.invalid/a.pdf", filename: "报告.pdf" }] });
  assert.deepEqual(f.frames[0].attachments, [{ content_type: "application/pdf", url: "https://media.example.invalid/a.pdf", filename: "报告.pdf" }]);
  for (const [index, filename] of ["a/b.pdf", "x\ny", "../x", "x".repeat(256)].entries()) {
    const id = `file-${index + 2}`;
    f.handlers.get("message")({}, { ...base, messageId: id, replyTarget: { ...base.replyTarget, msgId: id },
      attachments: [{ content_type: "text/plain", url: "https://media.example.invalid/n.txt", filename }] });
    assert.deepEqual(f.frames.find(x => x.id === id).attachments, [{ content_type: "text/plain", url: "https://media.example.invalid/n.txt" }]);
  }
  f.handlers.get("message")({}, { ...base, messageId: "file-bad", replyTarget: { ...base.replyTarget, msgId: "file-bad" },
    attachments: [{ content_type: "application/x-msdownload", url: "https://media.example.invalid/a.exe" }] });
  const bad = f.frames.find(x => x.id === "file-bad");
  assert.equal(bad.attachments, undefined);
});

test("群图片只接收艾特当前机器人的消息", () => {
  const f = fixture();
  const incoming = {kind:"group", rawEventType:"GROUP_AT_MESSAGE_CREATE", groupOpenid:"group-1",
    messageId:"group-image", senderId:"user-1", content:"", attachments:[{content_type:"image/jpeg",url:"https://media.example.invalid/photo.jpg"}],
    replyTarget:{scope:"group",targetId:"group-1",msgId:"group-image"}};
  f.handlers.get("message")({}, incoming);
  assert.equal(f.frames[0].scope, "group");
  assert.equal(f.frames[0].attachments.length, 1);
  f.handlers.get("message")({}, {...incoming,rawEventType:"GROUP_MESSAGE_CREATE",messageId:"no-at",replyTarget:{...incoming.replyTarget,msgId:"no-at"}});
  assert.equal(f.frames.filter(frame => frame.type === "message").length, 1);
});

test("耗时回复在原群或私聊主动投递一次，新消息仍有自己的被动窗口", async () => {
  for (const scope of ["c2c", "group"]) {
    let clock = 10;
    const f = fixture({ now: () => clock }); f.message(scope);
    clock += PASSIVE_REPLY_WINDOW_MS;
    await f.bridge.command({ version: 1, type: "progress", id: "in-1", index: 0, text: "进度" });
    assert.equal(f.sent.length, 0);
    assert.equal(f.frames.at(-1).failure_kind, "passive_window_elapsed");
    await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "完整结果" });
    assert.deepEqual(f.sent[0].target, { scope, targetId: scope === "c2c" ? "user-1" : "group-1" });
    assert.equal(f.frames.at(-1).delivery_mode, "proactive");
    assert.equal(f.frames.at(-1).ok, true);
    await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "不能重发" });
    assert.equal(f.sent.length, 1);
    f.message(scope, "in-2");
    await f.bridge.command({ version: 1, type: "reply", id: "in-2", text: "新消息" });
    assert.equal(f.sent[1].target.msgId, "in-2");
    assert.equal(f.frames.at(-1).delivery_mode, "passive");
  }
});

test("窗口用尽后的分段全部沿原会话主动投递，不重发已成功片段", async () => {
  let clock = 0;
  const f = fixture({ now: () => clock }); f.message("group");
  clock = PASSIVE_REPLY_WINDOW_MS;
  for (const index of [0, 1]) {
    await f.bridge.command({ version: 1, type: "segment", id: "in-1", count: 2, index, text: `段落${index}` });
    assert.equal(f.frames.at(-1).ok, true);
  }
  assert.equal(f.sent.length, 2);
  assert.ok(f.sent.every(s => !s.target.msgId && s.target.targetId === "group-1"));
});

test("SDK 明确拒绝后单独通知一次，正文仍报告失败且错误内容不泄漏", async () => {
  const error = Object.assign(new Error("secret-token sensitive-body"), { httpStatus: 400, bizCode: 40034005 });
  const f = fixture({ fail: n => n === 1, error }); f.message("group");
  await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "原始正文" });
  assert.equal(f.sent.length, 2);
  assert.equal(f.sent[0].target.msgId, "in-1");
  assert.deepEqual(f.sent[1].target, { scope: "group", targetId: "group-1" });
  assert.match(f.sent[1].text, /\/last-reply/);
  assert.equal(f.frames.at(-1).ok, false);
  assert.equal(f.frames.at(-1).notice_status, "sent");
  assert.equal(f.frames.at(-1).http_status, 400);
  assert.equal(f.frames.at(-1).biz_code, 40034005);
  assert.ok(!JSON.stringify(f.frames).includes("secret-token"));
  await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "不能重发" });
  assert.equal(f.sent.length, 2);
});

test("通知本身失败如实保留两个错误码，不把通知算作正文送达", async () => {
  const error = Object.assign(new Error("private content"), { httpStatus: 400, bizCode: 12345 });
  const f = fixture({ fail: true, error }); f.message();
  await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "结果" });
  assert.equal(f.sent.length, 2);
  assert.equal(f.frames.at(-1).ok, false);
  assert.equal(f.frames.at(-1).notice_status, "failed");
  assert.equal(f.frames.at(-1).notice_biz_code, 12345);
});

test("未知网络结果、鉴权失败和限流不重发正文或追加主动通知", async () => {
  for (const error of [new Error("unknown outcome"), Object.assign(new Error("timeout"), { name: "TimeoutError" }),
    ...[401, 403, 429, 500].map(httpStatus => Object.assign(new Error("rejected"), { httpStatus, bizCode: 99 }))]) {
    const f = fixture({ fail: true, error }); f.message();
    await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "结果" });
    assert.equal(f.sent.length, 1);
    assert.equal(f.frames.at(-1).ok, false);
    assert.equal(f.frames.at(-1).notice_status, "not_attempted");
  }
});

test("进度和主动消息读取真实 SDK 错误字段且不输出异常正文", async () => {
  const error = Object.assign(new Error("credential and user content"), { httpStatus: 429, bizCode: 22009 });
  const f = fixture({ fail: true, error }); f.message();
  for (const command of [
    { type: "progress", id: "in-1", index: 0, text: "进度" },
    { type: "push", id: "push-1", target_id: "user-1", text: "提醒" },
  ]) {
    await f.bridge.command({ version: 1, ...command });
    assert.equal(f.frames.at(-1).http_status, 429);
    assert.equal(f.frames.at(-1).biz_code, 22009);
  }
  assert.ok(!JSON.stringify(f.frames).includes("credential"));
});

test("私聊进度占用三次后只剩一次正文配额，不能继续分两段", async () => {
  const f = fixture(); f.message();
  for (let index = 0; index < 3; index++) {
    await f.bridge.command({ version: 1, type: "progress", id: "in-1", index, text: "进度" });
  }
  await f.bridge.command({ version: 1, type: "segment", id: "in-1", count: 2, index: 0, text: "超额分段" });
  assert.equal(f.sent.length, 3);
  await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "完整正文" });
  assert.equal(f.sent.length, 4);
  assert.equal(f.frames.at(-1).ok, true);
});

test("第二段被拒绝只提醒一次，已发送第一段不重发且不再发送剩余段", async () => {
  const error = Object.assign(new Error("sensitive"), { httpStatus: 400, bizCode: 123 });
  const f = fixture({ fail: n => n === 2, error }); f.message("group");
  for (const index of [0, 1, 2]) {
    await f.bridge.command({ version: 1, type: "segment", id: "in-1", count: 3, index, text: `原始片段${index}` });
  }
  assert.deepEqual(f.sent.map(s => s.text).slice(0, 2), ["原始片段0", "原始片段1"]);
  assert.equal(f.sent.length, 3);
  const receipts = f.frames.filter(x => x.type === "delivery");
  assert.equal(receipts.length, 2);
  assert.equal(receipts[0].ok, true);
  assert.equal(receipts[1].ok, false);
  assert.equal(receipts[1].notice_status, "sent");
});

test("停止后的迟到平台拒绝不会再发通知，也不伪造送达", async () => {
  const held = deferred();
  const error = Object.assign(new Error("rejected"), { httpStatus: 400, bizCode: 123 });
  const f = fixture({ fail: true, error, hold: held.promise }); f.message();
  const sending = f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "回复" });
  await f.bridge.command({ version: 1, type: "stop" });
  held.resolve(); await sending;
  assert.equal(f.sent.length, 1);
  assert.equal(f.frames.some(x => x.type === "delivery"), false);
});

test("失败提醒没有回执时有界返回未知，不重发也不把正文改成成功", async () => {
  const held = deferred();
  const error = Object.assign(new Error("rejected"), { httpStatus: 400, bizCode: 123 });
  const f = fixture({ fail: n => n === 1, error, hold: target => target.msgId ? null : held.promise });
  f.message();
  try {
    await f.bridge.command({ version: 1, type: "reply", id: "in-1", text: "原始结果" });
    assert.equal(f.sent.length, 2);
    assert.equal(f.frames.at(-1).notice_status, "unknown");
    assert.equal(f.frames.at(-1).ok, false);
  } finally { held.resolve(); }
});

test("表情包只跟在正文之后走原消息的被动目标，失败、过期或未预告都不发送也不重试", async () => {
  let clock = 0;
  const f = fixture({ now: () => clock }); f.message("group");
  // 没有预告的 sticker 帧不发送。
  await f.bridge.command({ version: 1, type: "sticker", id: "in-1", path: "/tmp/s.png" });
  assert.equal(f.images.length, 0);
  for (let index = 0; index < 2; index++) {
    await f.bridge.command({ version: 1, type: "segment", id: "in-1", index, count: 2, text: `第 ${index} 段`,
      ...(index === 1 ? { sticker_follows: true } : {}) });
  }
  await f.bridge.command({ version: 1, type: "sticker", id: "in-1", path: "/tmp/s.png" });
  assert.deepEqual(f.images, [{ target: { scope: "group", targetId: "group-1", msgId: "in-1" }, source: { localPath: "/tmp/s.png" } }]);
  assert.deepEqual(f.frames.at(-1), { type: "delivery", version: 1, id: "in-1", sticker: true, ok: true, message_id: "image-msg-1" });
  await f.bridge.command({ version: 1, type: "sticker", id: "in-1", path: "/tmp/s.png" });
  assert.equal(f.images.length, 1, "同一条回复只发一次");

  const late = fixture({ now: () => clock }); late.message("c2c", "in-2");
  await late.bridge.command({ version: 1, type: "reply", id: "in-2", text: "正文", sticker_follows: true });
  clock = PASSIVE_REPLY_WINDOW_MS;
  await late.bridge.command({ version: 1, type: "sticker", id: "in-2", path: "/tmp/s.png" });
  assert.equal(late.images.length, 0);
  assert.equal(late.frames.at(-1).failure_kind, "passive_window_elapsed");

  const failing = fixture({ imageFail: true }); failing.message("c2c", "in-3");
  await failing.bridge.command({ version: 1, type: "reply", id: "in-3", text: "正文", sticker_follows: true });
  await failing.bridge.command({ version: 1, type: "sticker", id: "in-3", path: "/tmp/s.png" });
  assert.equal(failing.frames.at(-1).ok, false);
  assert.ok(!JSON.stringify(failing.frames.at(-1)).includes("secret"));
  await failing.bridge.command({ version: 1, type: "sticker", id: "in-3", path: "/tmp/s.png" });
  assert.equal(failing.images.length, 1);
});


test("容量拒绝只通知原会话，不重试、不声称回复已保存", async () => {
  for (const fail of [false, true]) {
    const f = fixture({ fail, error: { response: { status: 400, data: { code: 40034090 } } } });
    f.message("group");
    await f.bridge.command({ version: 1, type: "capacity", id: "in-1" });
    await f.bridge.command({ version: 1, type: "capacity", id: "in-1" });
    assert.equal(f.sent.length, 1);
    assert.equal(f.sent[0].target.targetId, "group-1");
    assert.equal(f.sent[0].target.msgId, "in-1");
    assert.match(f.sent[0].text, /本条请求尚未执行/);
    assert.ok(!f.sent[0].text.includes("/last-reply"));
    const [delivery] = f.frames.filter(f => f.type === "delivery");
    assert.equal(delivery.capacity_notice, true);
    assert.equal(delivery.ok, !fail);
  }
});

test("QQ 表情标签转成可读的【表情：名字】，解不出名字时只标出表情", () => {
  const ext = name => Buffer.from(JSON.stringify({ text: name })).toString("base64");
  assert.equal(readableFaces(`<faceType=1,faceId="13",ext="${ext("[呲牙]")}">`), "【表情：呲牙】");
  assert.equal(readableFaces(`好的<faceType=1,faceId="76",ext="${ext("赞")}">谢谢`), "好的【表情：赞】谢谢");
  assert.equal(readableFaces(`<faceType=1,faceId="1",ext="!!">`), "<faceType=1,faceId=\"1\",ext=\"!!\">", "不是 base64 的标签不改");
  assert.equal(readableFaces(`<faceType=1,faceId="1",ext="${Buffer.from("x").toString("base64")}">`), "【表情】");
  assert.equal(readableFaces(`<faceType=1,faceId="1",ext="${ext("a".repeat(40))}">`), "【表情】", "过长的名字不转述");
  assert.equal(readableFaces(`<faceType=1,faceId="1",ext="${ext("坏】名字")}">`), "【表情】");
  assert.equal(readableFaces("[<face,id=13/>]哈"), "【表情】哈");
  assert.equal(readableFaces("普通文字 <b>"), "普通文字 <b>");
});

test("只发一个表情的消息带着可读标签交给 Eve，不再是编码串", () => {
  const f = fixture();
  const ext = Buffer.from(JSON.stringify({ text: "[呲牙]" })).toString("base64");
  f.handlers.get("message")({}, { kind: "c2c", messageId: "face-1", senderId: "user-1",
    content: `<faceType=1,faceId="13",ext="${ext}">`, rawEventType: "C2C_MESSAGE_CREATE",
    replyTarget: { scope: "c2c", targetId: "user-1", msgId: "face-1" } });
  const frame = f.frames.find(x => x.type === "message");
  assert.equal(frame.text, "【表情：呲牙】");
});

test("全量群消息只进入观察，艾特消息同时保留普通任务入口", () => {
  const f=fixture({group:{observe:true,mention:true}});
  const msg={kind:"group",messageId:"full-1",senderId:"member-1",content:"今天这个方法挺好",rawEventType:"GROUP_MESSAGE_CREATE",groupOpenid:"group-1",replyTarget:{scope:"group",targetId:"group-1",msgId:"full-1"}};
  f.handlers.get("message")({},msg);
  assert.equal(f.frames.filter(x=>x.type==="group_observation").length,1);
  assert.equal(f.frames.filter(x=>x.type==="message").length,0);
  assert.equal(f.frames[0].message.user_id,"member-1");
  f.handlers.get("message")({}, {...msg,messageId:"at-1",rawEventType:"GROUP_AT_MESSAGE_CREATE",replyTarget:{...msg.replyTarget,msgId:"at-1"}});
  assert.equal(f.frames.filter(x=>x.type==="group_observation").length,2);
  assert.equal(f.frames.filter(x=>x.type==="message").length,1);
  assert.equal(f.sent.length,0);
  f.handlers.get("message")({}, {...msg,senderIsBot:true});
  assert.equal(f.frames.filter(x=>x.type==="group_observation").length,2);
});
test("群回复首段只艾特平台原发送者，私聊不增加艾特", async () => {
  const f=fixture({group:{observe:true,mention:true}}); f.message("group","g-at");
  await f.bridge.command({type:"segment",version:1,id:"g-at",index:0,count:2,text:"知道了"});
  await f.bridge.command({type:"segment",version:1,id:"g-at",index:1,count:2,text:"我继续看看"});
  assert.deepEqual(f.sent.map(x=>x.text),["<@user-1> 知道了","我继续看看"]);
  f.message("c2c","private-no-at");
  await f.bridge.command({type:"reply",version:1,id:"private-no-at",text:"好的"});
  assert.equal(f.sent.at(-1).text,"好的");
});
test("主动群插话复用发送接口，分条并只在首段艾特来源发送者", async () => {
  const f=fixture({group:{observe:true,mention:true}});
  await f.bridge.command({version:1,type:"push",id:"chime-first",scope:"group",target_id:"group-1",mention_user_id:"member-2",text:"这个思路可行\n可以试试",parts:[["这个思路可行",0],["可以试试",0]]});
  assert.deepEqual(f.sent.map(x=>x.text),["<@member-2> 这个思路可行","可以试试"]);
  assert.ok(f.sent.every(x=>x.target.scope==="group"&&!x.target.msgId));
  assert.equal(f.frames.at(-1).ok,true);
  await f.bridge.command({version:1,type:"push",id:"bad-route",scope:"c2c",target_id:"member-2",mention_user_id:"member-2",text:"测试"});
  assert.equal(f.frames.at(-1).ok,false);
  assert.equal(f.sent.length,2);
});
