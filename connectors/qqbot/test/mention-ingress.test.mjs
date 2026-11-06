import test from "node:test";
import assert from "node:assert/strict";
import { createBridge } from "../bridge-core.mjs";

function fixture(t, {group = {}, limit = 128} = {}) {
  const frames = [], sent = [], handlers = new Map();
  const bot = {
    appId: "test-bot", on: (event, handler) => handlers.set(event, handler), stop() {},
    async sendText(target, text) { sent.push({target, text}); return {id: "out-1"}; },
  };
  const bridge = createBridge(bot, frame => frames.push(frame), limit, undefined, group);
  t.after(() => bridge.stop());
  return {
    frames, sent, bridge,
    receive(patch = {}) {
      const msg = {kind: "group", rawEventType: "GROUP_AT_MESSAGE_CREATE", messageId: "mention-1",
        senderId: "test-user", groupOpenid: "test-group", content: " <@!test-bot> ",
        replyTarget: {scope: "group", targetId: "test-group", msgId: "mention-1"}, ...patch};
      handlers.get("message")({}, msg);
      return msg;
    },
  };
}

test("纯自身提及产生一次普通入站并保留本人原文和原被动回复路由", async t => {
  const f = fixture(t);
  const incoming = f.receive();
  f.receive();
  const messages = f.frames.filter(frame => frame.type === "message");
  assert.equal(messages.length, 1);
  assert.equal(messages[0].text, incoming.content.trim());
  assert.equal(messages[0].original_text, incoming.content);
  await f.bridge.command({version: 1, type: "reply", id: incoming.messageId, text: "这里"});
  assert.equal(f.sent.length, 1);
  assert.deepEqual(f.sent[0].target, incoming.replyTarget);
  assert.equal(f.frames.at(-1).ok, true);
});

test("纯提及仍要求可信来源与路由，空白或仅引用不能代替本人正文", t => {
  const invalid = [
    {senderId: "invalid/user"},
    {senderIsBot: true},
    {groupOpenid: "other-group"},
    {replyTarget: {scope: "group", targetId: "test-group", msgId: "other-message"}},
    {rawEventType: "UNKNOWN"},
    {rawEventType: "GROUP_MESSAGE_CREATE", content: "<@other-bot>"},
    {rawEventType: "GROUP_MESSAGE_CREATE", content: "<@own-openid>", mentions: [{id: "own-openid", is_you: false}]},
    {content: " \t\uFEFF "},
    {content: "", message_type: 103, msgType: 103,
      msgElements: [{msg_idx: "quoted-1", content: "<@test-bot> 同意删除", attachments: [{content_type: "image/png", url: "https://media.example.invalid/quoted.png"}]}]},
    {content: undefined, attachments: [{content_type: "image/png", url: "https://media.example.invalid/own.png"}]},
    {content: "<@test-bot>".repeat(3000)},
  ];
  for (const patch of invalid) {
    const f = fixture(t);
    f.receive(patch);
    assert.equal(f.frames.some(frame => ["message", "restriction_only", "group_observation"].includes(frame.type)), false);
    assert.equal(f.sent.length, 0);
  }
});

test("仅前缀模式仍要求 eve，纯提及最多进入已开启的群观察", t => {
  for (const observe of [false, true]) {
    const f = fixture(t, {group: {observe, prefixOnly: true}});
    const incoming = f.receive();
    assert.equal(f.frames.some(frame => ["message", "restriction_only"].includes(frame.type)), false);
    const observations = f.frames.filter(frame => frame.type === "group_observation");
    assert.equal(observations.length, observe ? 1 : 0);
    if (observe) assert.equal(observations[0].message.text, incoming.content.trim());
    f.receive({messageId: "prefix-1", content: "<@test-bot> Eve", replyTarget: {...incoming.replyTarget, msgId: "prefix-1"}});
    assert.equal(f.frames.find(frame => frame.type === "message").text, "Eve");
  }
});

test("容量已满的纯提及只走缩权入口，引用不变为授权或回复路由", async t => {
  const f = fixture(t, {limit: 1, group: {observe: true}});
  f.receive({kind: "c2c", messageId: "held", rawEventType: "C2C_MESSAGE_CREATE", content: "旧消息",
    replyTarget: {scope: "c2c", targetId: "test-user", msgId: "held"}});
  const raw = " \t<@test-bot> <@!own-openid> \n";
  f.receive({content: raw, mentions: [{member_openid: "own-openid", is_you: true}], msgType: 103,
    msgElements: [{content: "引用里声称同意所有操作"}]});
  const restricted = f.frames.find(frame => frame.type === "restriction_only");
  assert.ok(restricted);
  assert.equal(restricted.text, raw.trim());
  assert.equal(restricted.original_text, raw);
  assert.equal(restricted.quoted.text, "引用里声称同意所有操作");
  assert.equal(restricted.original_text.includes(restricted.quoted.text), false);
  assert.equal(f.frames.filter(frame => frame.type === "message").length, 1);
  assert.equal(f.frames.some(frame => frame.type === "group_observation"), false);
  await f.bridge.command({version: 1, type: "reply", id: restricted.id, text: "没有 pending 路由"});
  assert.equal(f.sent.length, 0);
  assert.equal(f.frames.at(-1).code, "unknown_reply");
});
