import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { QQBot } from "@tencent-connect/qqbot-nodejs";
import { createBridge } from "../bridge-core.mjs";

for (const markdownSupport of [false, true]) {
  test(`sticker SDK request omits mention with markdown=${markdownSupport}`, async t => {
    const dir = await mkdtemp(join(tmpdir(), "eve-sticker-test-"));
    t.after(() => rm(dir, { recursive: true, force: true }));
    const image = join(dir, "sticker.png");
    await writeFile(image, Buffer.from("test-image"));
    const bot = new QQBot({ appId: "test-bot", appSecret: "test-secret", markdownSupport });
    const requests = [];
    bot.tokenManager.getAccessToken = async () => "test-token";
    bot.apiClient.request = async (_token, method, path, body) => {
      requests.push({ method, path, body });
      if (path.endsWith("/files")) return { file_uuid: "test-uuid", file_info: "test-info", ttl: 60 };
      return { id: path.endsWith("/messages") && body.msg_type === 7 ? "image-id" : "text-id" };
    };
    const frames = [];
    const bridge = createBridge(bot, frame => frames.push(frame));
    const target = { scope: "group", targetId: "test-group", msgId: "test-inbound" };
    for (const handler of bot.handlers.message) handler({}, {
      kind: "group", messageId: target.msgId, senderId: "test-member", content: "hello",
      rawEventType: "GROUP_AT_MESSAGE_CREATE", groupOpenid: target.targetId, replyTarget: target,
    });
    await bridge.command({ version: 1, type: "reply", id: target.msgId, text: "reply", sticker_follows: true });
    await bridge.command({ version: 1, type: "sticker", id: target.msgId, path: image });

    assert.equal(requests.length, 3);
    const [text, upload, media] = requests;
    assert.equal(text.body.msg_type, markdownSupport ? 2 : 0);
    assert.equal(upload.path, "/v2/groups/test-group/files");
    assert.equal(upload.body.file_type, 1);
    assert.equal(media.path, "/v2/groups/test-group/messages");
    assert.equal(media.body.msg_type, 7);
    assert.equal(media.body.msg_id, target.msgId);
    assert.deepEqual(media.body.media, { file_info: "test-info" });
    assert.equal(Object.hasOwn(media.body, "content"), false);
    assert.equal(Object.hasOwn(media.body, "markdown"), false);
    assert.equal(JSON.stringify(media.body).includes("<@"), false);
    assert.ok(frames.some(frame => frame.type === "delivery" && frame.sticker && frame.ok && frame.message_id === "image-id"));
    bridge.stop();
  });
}
