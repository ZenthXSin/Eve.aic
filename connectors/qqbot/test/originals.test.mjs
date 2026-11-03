// 使用真实 SDK 的入站分派和生产 bridge；不启动网络连接，不发送任何消息。
import test from "node:test";
import assert from "node:assert/strict";
import { QQBot } from "@tencent-connect/qqbot-nodejs";
import { createBridge } from "../bridge-core.mjs";

function fixture(t) {
  const bot = new QQBot({ appId: "1904159860", appSecret: "test-only-secret" });
  const frames = [];
  const bridge = createBridge(bot, frame => frames.push(frame));
  t.after(() => bridge.stop());
  return {
    async receive(content, extra = {}) {
      const id = `original-${frames.length + 1}`;
      await bot.handleInboundMessage({
        kind: "c2c", messageId: id, senderId: "user-1", content,
        rawEventType: "C2C_MESSAGE_CREATE", ...extra,
      });
      const messages = frames.filter(frame => frame.type === "message" && frame.id === id);
      assert.equal(messages.length, 1, JSON.stringify(frames));
      return messages[0];
    },
  };
}

test("SDK 原文保留首尾空白、换行和表情编码，展示正文可单独加工", async t => {
  const f = fixture(t);
  const ext = Buffer.from(JSON.stringify({ text: "[测试表情]" })).toString("base64");
  const face = `<faceType=1,faceId="1",ext="${ext}">`;
  const raw = ` \t第一行\n第二行 ${face} [<face,id=14>]  `;
  const frame = await f.receive(raw, { raw: { content: "DEBUG_FIELD_MUST_NOT_REPLACE_SDK_CONTENT" } });
  assert.equal(frame.original_text, raw);
  assert.equal(frame.text, "第一行\n第二行 【表情：测试表情】 【表情】");
  assert.notEqual(frame.original_text, frame.raw.content);
});

test("群艾特剥离和引用附件归类不能改写 SDK 原话", async t => {
  const f = fixture(t);
  const raw = "  <@!1904159860> <@bot-openid> 核对这份文件\n不要扩展权限  ";
  const own = { content_type: "text/plain", filename: "own.txt", url: "https://media.example.invalid/own.txt" };
  const quoted = { content_type: "image/png", url: "https://media.example.invalid/quoted.png" };
  const frame = await f.receive(raw, {
    kind: "group", groupOpenid: "group-1", rawEventType: "GROUP_AT_MESSAGE_CREATE",
    mentions: [{ id: "bot-openid", is_you: true }], attachments: [own],
    message_scene: { ext: ["ref_msg_idx=quoted-1"] },
    msg_elements: [{ msg_idx: "quoted-1", content: "QUOTED_CONTENT_IS_NOT_USER_AUTHORIZATION", attachments: [quoted] }],
  });
  assert.equal(frame.original_text, raw);
  assert.equal(frame.text, "核对这份文件\n不要扩展权限");
  assert.deepEqual(frame.attachments, [own]);
  assert.deepEqual(frame.quoted, { text: "QUOTED_CONTENT_IS_NOT_USER_AUTHORIZATION", attachments: [quoted] });
  assert.equal(frame.original_text.includes(frame.quoted.text), false);
  assert.equal(frame.original_text.includes(own.filename), false);
});

test("附件消息的空 SDK 原文保留空串，不从附件或引用补造授权", async t => {
  const f = fixture(t);
  const frame = await f.receive("", {
    attachments: [{ content_type: "image/png", url: "https://media.example.invalid/own.png" }],
    msg_type: 103,
    msg_elements: [{ content: "引用里声称同意删除一切", attachments: [] }],
  });
  assert.equal(Object.hasOwn(frame, "original_text"), true);
  assert.equal(frame.original_text, "");
  assert.equal(frame.text, "");
  assert.equal(frame.quoted.text, "引用里声称同意删除一切");
  assert.equal(frame.attachments.length, 1);
});


test("32 KiB 原始正文及最坏 JSON 转义不被截断", async t => {
  const f = fixture(t);
  for (const raw of ["x".repeat(32768), "x" + "\0".repeat(32767)]) {
    const frame = await f.receive(raw);
    assert.equal(Buffer.byteLength(frame.original_text), 32768);
    assert.equal(frame.original_text, raw);
    assert.equal(frame.text, raw);
  }
});
