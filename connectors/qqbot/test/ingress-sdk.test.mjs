// 实际 SDK 1.0.4 事件分派 → handleInboundMessage → 生产 bridge；只用合成事件，不启动网络。
import test from "node:test";
import assert from "node:assert/strict";
import { QQBot } from "@tencent-connect/qqbot-nodejs";
import { dispatchEvent } from "@tencent-connect/qqbot-nodejs/protocol";
import { createBridge } from "../bridge-core.mjs";

function fixture(t) {
  const bot = new QQBot({ appId: "test-bot", appSecret: "test-only-secret" });
  const frames = [];
  const bridge = createBridge(bot, frame => frames.push(frame), 128, undefined, {observe: true});
  t.after(() => bridge.stop());
  let sequence = 0;
  assert.deepEqual(bot.middlewares, []);
  return {
    async receive(event, patch = {}) {
      const raw = { id: `sdk-input-${++sequence}`, content: "你好", group_openid: "test-group",
        author: {user_openid: "test-user", member_openid: "test-user"}, ...patch };
      const dispatched = dispatchEvent(event, raw, "test-bot", {debug() {}});
      assert.equal(dispatched.action, "message");
      const target = bot.deriveReplyTarget(dispatched.msg);
      assert.deepEqual(target, {scope: dispatched.msg.kind, msgId: raw.id,
        targetId: dispatched.msg.kind === "group" ? raw.group_openid : raw.author.user_openid});
      const begin = frames.length;
      await bot.handleInboundMessage(dispatched.msg);
      return frames.slice(begin);
    },
  };
}
const image = {content_type: "image/png", url: "https://media.example.invalid/own.png"};

test("SDK 的正常私聊、群提及、表情及附件消息通过，原文不被诊断改写", async t => {
  const f = fixture(t);
  for (const [event, patch, expected] of [
    ["C2C_MESSAGE_CREATE", {content: "  你好 \n"}, "你好"],
    ["GROUP_AT_MESSAGE_CREATE", {content: "<@!test-bot> 你好"}, "你好"],
    ["GROUP_MESSAGE_CREATE", {content: "<@own-openid> 你好", mentions: [{id: "own-openid", is_you: true}]}, "你好"],
    ["C2C_MESSAGE_CREATE", {content: "[<face,id=14>]"}, "【表情】"],
    ["C2C_MESSAGE_CREATE", {content: "", attachments: [image]}, ""],
    ["GROUP_AT_MESSAGE_CREATE", {content: "<@test-bot>", attachments: [image]}, "<@test-bot>"],
    ["C2C_MESSAGE_CREATE", {content: "", msg_elements: [{attachments: [image]}]}, ""],
  ]) {
    const frames = await f.receive(event, patch);
    assert.equal(frames.some(frame => frame.type === "warning"), false);
    const message = frames.find(frame => frame.type === "message");
    assert.ok(message);
    assert.equal(message.text, expected);
    assert.equal(message.original_text, patch.content);
    if (expected === "") assert.equal(message.attachments.length, 1);
  }
});

test("SDK 缺失或非字符串正文保留具体类型，不从附件或 raw 补造正文", async t => {
  for (const [content, type] of [[undefined, "undefined"], [null, "null"], [false, "boolean"], [[], "array"], [{text: "PRIVATE"}, "object"]]) {
    const frames = await fixture(t).receive("C2C_MESSAGE_CREATE", {content, attachments: [image]});
    assert.equal(frames.length, 1);
    const detail = frames[0].ingress;
    assert.equal(detail.reason, "content_type");
    assert.equal(detail.last.content_type, type);
    assert.equal(detail.last.content_bytes, null);
    assert.equal(detail.last.accepted_attachment_count, 1);
    assert.equal(detail.last.normalized_text_bytes, null);
    assert.equal(JSON.stringify(detail).includes("PRIVATE"), false);
  }
});

test("空白、不支持的附件和仅引用内容仍按归一化空正文拒绝", async t => {
  for (const [event, patch, attachments, elements, rejections] of [
    ["C2C_MESSAGE_CREATE", {content: " \t\uFEFF "}, null, null],
    ["C2C_MESSAGE_CREATE", {content: "", attachments: [{content_type: "audio/ogg", url: "https://media.example.invalid/voice.ogg"}]}, 1, null, {unsupported_type: 1}],
    ["GROUP_MESSAGE_CREATE", {content: "", attachments: [{content_type: "video/PRIVATE_TYPE"}]}, 1, null, {unsupported_type: 1}],
    ["C2C_MESSAGE_CREATE", {content: "", attachments: [{content_type: "image/png", url: "http://PRIVATE_HOST/PRIVATE_IMAGE"}]}, 1, null, {invalid_url: 1}],
    ["C2C_MESSAGE_CREATE", {content: "", attachments: [null]}, 1, null, {invalid_shape: 1}],
    ["C2C_MESSAGE_CREATE", {content: "", message_type: 103, msg_elements: [{msg_idx: "quote-1", content: "PRIVATE_QUOTE", attachments: [image]}]}, null, 1, {quoted: 1}],
  ]) {
    const frames = await fixture(t).receive(event, patch);
    assert.equal(frames.length, 1);
    const detail = frames[0].ingress;
    assert.equal(detail.reason, "normalized_text_empty");
    assert.equal(detail.last.content_type, "string");
    assert.equal(detail.last.content_bytes, Buffer.byteLength(patch.content));
    assert.equal(detail.last.normalized_text_bytes, 0);
    assert.equal(detail.last.attachments_count, attachments);
    assert.equal(detail.last.elements_count, elements);
    assert.equal(detail.last.accepted_attachment_count, 0);
    assert.deepEqual(detail.last.attachment_rejections, rejections);
    assert.equal(JSON.stringify(detail).includes("PRIVATE"), false);
  }
});

test("实际 SDK 的纯自身提及保留原话进入普通消息，重复自身提及也不丢失", async t => {
  for (const [event, patch] of [
    ["GROUP_AT_MESSAGE_CREATE", {content: " <@!test-bot> "}],
    ["GROUP_MESSAGE_CREATE", {content: "<@own-openid>", mentions: [{member_openid: "own-openid", is_you: true}]}],
    ["GROUP_MESSAGE_CREATE", {content: " \t<@test-bot> <@!own-openid> <@test-bot> \n", mentions: [{id: "own-openid", is_you: true}]}],
  ]) {
    const frames = await fixture(t).receive(event, patch);
    assert.equal(frames.some(frame => frame.type === "warning"), false);
    const message = frames.find(frame => frame.type === "message");
    assert.ok(message);
    assert.equal(message.text, patch.content.trim());
    assert.equal(message.original_text, patch.content);
    assert.equal(message.raw.content, patch.content);
    assert.equal(frames.find(frame => frame.type === "group_observation").mentions_me, true);
  }
});
