import test from "node:test";
import assert from "node:assert/strict";
import { createBridge } from "../bridge-core.mjs";
import { INGRESS_REPORT_INTERVAL_MS } from "../ingress-diagnostics.mjs";

function fixture(t) {
  let now = 0;
  const frames = [], tasks = new Map(), handlers = new Map();
  const timers = {
    setTimeout(callback, delay) {
      const timer = { unref() {} };
      tasks.set(timer, { at: now + delay, callback });
      return timer;
    },
    clearTimeout(timer) { tasks.delete(timer); },
  };
  const bot = { appId: "test-bot", on: (event, handler) => handlers.set(event, handler), stop() {} };
  const bridge = createBridge(bot, frame => frames.push(frame), 128, () => now, {}, timers);
  t.after(() => bridge.stop());
  return {
    bridge, frames, tasks,
    receive(patch = {}) {
      const message = { kind: "c2c", rawEventType: "C2C_MESSAGE_CREATE", messageId: "test-message",
        senderId: "test-user", content: "PRIVATE_CONTENT", groupOpenid: "test-group",
        replyTarget: {scope: "c2c", targetId: "test-user", msgId: "test-message"}, ...patch };
      handlers.get("message")({}, message);
    },
    advance(milliseconds) {
      now += milliseconds;
      for (const [timer, task] of [...tasks]) {
        if (task.at <= now) { tasks.delete(timer); task.callback(); }
      }
    },
    warnings() { return frames.filter(frame => frame.type === "warning"); },
  };
}

test("入口各拒绝分支保持校验并报告固定原因，合法下一条仍进入", t => {
  const cases = [
    ["message_id", { messageId: "bad\nmessage" }],
    ["sender_id", { senderId: "bad/sender" }],
    ["reply_target", { replyTarget: null }],
    ["target_scope", { replyTarget: {scope: "group", targetId: "test-user", msgId: "test-message"} }],
    ["target_message_id", { replyTarget: {scope: "c2c", targetId: "test-user", msgId: "other-message"} }],
    ["target_id", { replyTarget: {scope: "c2c", targetId: "bad/target", msgId: "test-message"} }],
    ["content_type", { content: null }],
    ["content_bytes", { content: "界".repeat(10923) }],
    ["c2c_target", { replyTarget: {scope: "c2c", targetId: "other-user", msgId: "test-message"} }],
    ["group_target", {kind: "group", rawEventType: "GROUP_AT_MESSAGE_CREATE",
      replyTarget: {scope: "group", targetId: "other-group", msgId: "test-message"}}],
    ["normalized_text_empty", { content: " \t\uFEFF " }],
  ];
  for (const [reason, patch] of cases) {
    const f = fixture(t);
    f.receive(patch);
    assert.equal(f.frames.length, 1, reason);
    assert.equal(f.warnings()[0].code, "invalid_route_or_text");
    assert.equal(f.warnings()[0].ingress.reason, reason);
    assert.equal(f.warnings()[0].ingress.count, 1);
    assert.equal(f.warnings()[0].ingress.total, 1);
    f.receive();
    assert.equal(f.frames.at(-1).type, "message", reason);
  }
});

test("相同原因首次立即报告，突发只计数，一分钟后无新消息也汇总", t => {
  const f = fixture(t);
  f.receive({ content: undefined });
  assert.equal(f.warnings().length, 1);
  assert.equal(f.tasks.size, 0);
  for (let i = 0; i < 250; i++) f.receive({ content: i ? undefined : null });
  assert.equal(f.warnings().length, 1);
  assert.equal(f.tasks.size, 1);
  f.advance(INGRESS_REPORT_INTERVAL_MS - 1);
  assert.equal(f.warnings().length, 1);
  f.advance(1);
  assert.equal(f.warnings().length, 2);
  const report = f.warnings().at(-1).ingress;
  assert.equal(report.count, 250);
  assert.equal(report.total, 251);
  assert.equal(report.counts_saturated, false);
  assert.equal(report.elapsed_ms, INGRESS_REPORT_INTERVAL_MS);
  assert.equal(report.last.content_type, "undefined");
  assert.equal(report.last.content_bytes, null);
  assert.equal(f.tasks.size, 0);
  f.advance(INGRESS_REPORT_INTERVAL_MS * 2);
  assert.equal(f.warnings().length, 2, "空闲不产生零计数日志");
  f.receive({ content: false });
  assert.equal(f.warnings().at(-1).ingress.count, 1);
  assert.equal(f.warnings().at(-1).ingress.total, 252);
  assert.equal(f.warnings().at(-1).ingress.last.content_type, "boolean");
});

test("不同拒绝原因不互相消音，停止只冲刷未报计数并撤销计时器", t => {
  const f = fixture(t);
  f.receive({ content: undefined });
  f.receive({ content: undefined });
  f.receive({ senderId: "PRIVATE/USER" });
  f.receive({ senderId: "PRIVATE/OTHER_USER" });
  assert.equal(f.warnings().length, 2);
  assert.equal(f.tasks.size, 2);
  f.bridge.stop();
  assert.equal(f.tasks.size, 0);
  const warnings = f.warnings();
  assert.equal(warnings.length, 4);
  assert.deepEqual(warnings.slice(2).map(frame => [frame.ingress.reason, frame.ingress.count, frame.ingress.total]),
    [["content_type", 1, 2], ["sender_id", 1, 2]]);
  f.bridge.stop();
  f.advance(INGRESS_REPORT_INTERVAL_MS);
  f.receive({ content: undefined });
  assert.equal(f.warnings().length, 4);
});

test("日志只含白名单形状，正文、身份、附件 URL 和未知事件值都不回显", t => {
  const f = fixture(t);
  f.receive({ senderId: "PRIVATE/USER", rawEventType: "__proto__",
    messageId: "PRIVATE_MESSAGE", content: "PRIVATE_CONTENT",
    groupOpenid: "PRIVATE_GROUP", replyTarget: {scope: "PRIVATE_SCOPE", targetId: "PRIVATE_TARGET", msgId: "PRIVATE_MESSAGE"},
    attachments: [{content_type: "image/png", filename: "PRIVATE_FILENAME", url: "https://PRIVATE_HOST/PRIVATE_TOKEN"}],
    msgElements: [{content: "PRIVATE_QUOTE"}] });
  const warning = f.warnings()[0];
  assert.equal(warning.ingress.last.event, "other");
  assert.equal(warning.ingress.last.target_scope, "other");
  assert.equal(warning.ingress.last.sender_id_type, "string");
  assert.equal(warning.ingress.last.sender_id_bytes, Buffer.byteLength("PRIVATE/USER"));
  assert.equal(warning.ingress.last.attachments_count, 1);
  assert.equal(warning.ingress.last.elements_count, 1);
  assert.equal(warning.ingress.last.accepted_attachment_count, 1);
  assert.equal(JSON.stringify(warning).includes("PRIVATE"), false);
  assert.ok(Buffer.byteLength(JSON.stringify(warning)) < 2048);
});
