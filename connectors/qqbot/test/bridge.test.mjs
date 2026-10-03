import test from "node:test";
import assert from "node:assert/strict";
import { Readable } from "node:stream";
import { createBridge, consume, optionsFromEnv } from "../bridge-core.mjs";
function fixture({ fail = false, limit = 128 } = {}) {
  const frames = [], sent = [], handlers = new Map();
  const bot = {
    on: (event, handler) => handlers.set(event, handler),
    stop: () => handlers.set("stopped", true),
    async sendText(target, text) {
      sent.push({ target, text });
      if (fail) throw new Error("sensitive secret and message");
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

test("群聊只接纳官方 @ 事件，拒绝普通群消息与机器人消息", async () => {
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
