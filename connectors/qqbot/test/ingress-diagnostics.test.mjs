import test from "node:test";
import assert from "node:assert/strict";
import { createBridge } from "../bridge-core.mjs";
import { createIngressDiagnostics, INGRESS_REPORT_INTERVAL_MS } from "../ingress-diagnostics.mjs";

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

test("纯附件拒收区分结构、类型和 URL，只记录实际首个过滤原因", t => {
  const cases = [
    [null, "invalid_shape"],
    ["PRIVATE_ATTACHMENT", "invalid_shape"],
    [{content_type: "audio/PRIVATE_TYPE", url: "file:///PRIVATE_PATH"}, "unsupported_type"],
    [{contentType: "video/PRIVATE_TYPE"}, "unsupported_type"],
    [{content_type: "image/png", url: "file:///PRIVATE_PATH"}, "invalid_url"],
    [{content_type: "image/png", url: "https://PRIVATE_HOST"}, "invalid_url"],
    [{content_type: "image/png", url: `https://PRIVATE_HOST/${"x".repeat(4096)}`}, "invalid_url"],
    [{content_type: "image/png", url: "https://PRIVATE_HOST/PRIVATE\u0000PATH"}, "invalid_url"],
  ];
  for (const [raw, reason] of cases) {
    const f = fixture(t);
    f.receive({content: "", attachments: [raw]});
    assert.equal(f.frames.length, 1);
    const warning = f.warnings()[0];
    assert.equal(warning.code, "invalid_route_or_text");
    assert.equal(warning.ingress.version, 1);
    assert.equal(warning.ingress.reason, "normalized_text_empty");
    assert.equal(warning.ingress.last.attachments_count, 1);
    assert.equal(warning.ingress.last.accepted_attachment_count, 0);
    assert.deepEqual(warning.ingress.last.attachment_rejections, {[reason]: 1});
    assert.equal(JSON.stringify(warning).includes("PRIVATE"), false);
    f.receive({content: "", attachments: [{content_type: "image/png", url: "https://media.example.invalid/own.png"}]});
    assert.equal(f.frames.at(-1).type, "message", "拒收不占用原消息的 pending 路由");
  }
});

test("引用与元素筛选拒收分别计数，不把附件并成本条或回显引用", t => {
  const image = {content_type: "image/png", url: "https://PRIVATE_HOST/PRIVATE_IMAGE"};
  for (const patch of [
    {msg_elements: [{msg_idx: "PRIVATE_QUOTE", attachments: [image, image]}]},
    {msgType: 103, msgElements: [{content: "PRIVATE_QUOTE", attachments: [image, image]}]},
  ]) {
    const f = fixture(t);
    f.receive({content: "", ...patch});
    const warning = f.warnings()[0];
    assert.equal(warning.ingress.reason, "normalized_text_empty");
    assert.equal(warning.ingress.last.accepted_attachment_count, 0);
    assert.deepEqual(warning.ingress.last.attachment_rejections, {quoted: 2});
    assert.equal(JSON.stringify(warning).includes("PRIVATE"), false);
  }
  const f = fixture(t);
  f.receive({content: "", msgElements: [
    {content: "PRIVATE_DISPLAY"}, {attachments: [image, image]},
  ]});
  const warning = f.warnings()[0];
  assert.equal(warning.ingress.reason, "normalized_text_empty");
  assert.equal(warning.ingress.last.accepted_attachment_count, 0);
  assert.deepEqual(warning.ingress.last.attachment_rejections, {element_not_selected: 2});
  assert.equal(JSON.stringify(warning).includes("PRIVATE"), false);
});

test("混合附件诊断只含最后一个拒收样本的计数，限频与刷新保持", t => {
  const f = fixture(t);
  f.receive({content: "", attachments: [{content_type: "audio/PRIVATE_TYPE"}]});
  f.receive({content: "", attachments: [null,
    {content_type: "image/png", url: "http://PRIVATE_HOST/one"},
    {content_type: "image/png", url: "file:///PRIVATE_PATH"}]});
  f.receive({content: "", msgElements: [{msg_idx: "PRIVATE_QUOTE", attachments: [{content_type: "image/png"}]}]});
  assert.equal(f.warnings().length, 1);
  f.advance(INGRESS_REPORT_INTERVAL_MS);
  assert.equal(f.warnings().length, 2);
  const report = f.warnings()[1].ingress;
  assert.equal(report.count, 2);
  assert.equal(report.total, 3);
  assert.deepEqual(report.last.attachment_rejections, {quoted: 1});
  f.receive({content: ""});
  f.advance(INGRESS_REPORT_INTERVAL_MS);
  assert.equal(f.warnings().at(-1).ingress.last.attachment_rejections, undefined);
  assert.equal(JSON.stringify(f.warnings()).includes("PRIVATE"), false);
});

test("附件混合计数、去重与容量维持原次序，合法附件和元素回退仍进入", t => {
  const image = index => ({content_type: "image/png", url: `https://media.example.invalid/${index}.png`});
  const f = fixture(t);
  f.receive({senderId: "PRIVATE/USER", attachments: [null,
    {content_type: "audio/PRIVATE_TYPE"}, {content_type: "image/png", url: "file:///PRIVATE_PATH"},
    image(1), image(1), image(2), image(3), image(4), image(4), image(5)]});
  const warning = f.warnings()[0];
  assert.equal(warning.ingress.reason, "sender_id");
  assert.equal(warning.ingress.last.accepted_attachment_count, 4);
  assert.deepEqual(warning.ingress.last.attachment_rejections,
    {invalid_shape: 1, unsupported_type: 1, invalid_url: 1, duplicate: 1, limit: 2});
  assert.equal(JSON.stringify(warning).includes("PRIVATE"), false);
  for (const patch of [
    {attachments: [image(1), image(1), image(2), image(3), image(4), image(5)]},
    {attachments: [{content_type: "audio/PRIVATE_TYPE"}], msgElements: [{attachments: [image(1)]}]},
    {attachments: [{contentType: "IMAGE/PNG", url: image(1).url, filename: "PRIVATE/name"}]},
    {attachments: [{}]},
  ]) {
    const accepted = fixture(t);
    accepted.receive({content: "", ...patch});
    assert.equal(accepted.warnings().length, 0);
    const message = accepted.frames.find(frame => frame.type === "message");
    assert.ok(message);
    assert.equal(message.original_text, "");
    assert.equal(message.attachment_rejections, undefined, "诊断不进入正常入站帧");
    if (patch.attachments.length === 6) assert.deepEqual(message.attachments, [image(1), image(2), image(3), image(4)]);
    else if (Object.keys(patch.attachments[0]).length === 0) assert.deepEqual(message.attachments, [{content_type: "application/octet-stream"}]);
    else assert.deepEqual(message.attachments, [image(1)]);
  }
});

test("附件诊断仅复制固定正整数计数，兼容无新增元数据的调用", () => {
  const frames = [];
  const diagnostics = createIngressDiagnostics(frame => frames.push(frame), () => 0);
  const reasons = {unsupported_type: 2, invalid_url: Number.MAX_SAFE_INTEGER,
    invalid_shape: -1, quoted: 1.5, element_not_selected: "PRIVATE", duplicate: 0,
    limit: Number.MAX_SAFE_INTEGER + 1, "audio/PRIVATE_MIME": 1, url: "https://PRIVATE_HOST/PRIVATE"};
  const msg = {content: ""};
  diagnostics.reject("normalized_text_empty", msg, [], "", reasons);
  reasons.unsupported_type = 5;
  assert.deepEqual(frames[0].ingress.last.attachment_rejections,
    {unsupported_type: 2, invalid_url: Number.MAX_SAFE_INTEGER});
  assert.equal(JSON.stringify(frames).includes("PRIVATE"), false);
  assert.ok(Buffer.byteLength(JSON.stringify(frames[0])) < 2048);
  diagnostics.reject("content_type", {content: null}, []);
  assert.equal(frames[1].ingress.last.attachment_rejections, undefined);
  diagnostics.close();
});
