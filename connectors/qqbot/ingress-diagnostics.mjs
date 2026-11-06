// 入站拒绝只保存固定原因和形状；计数器不保留正文、身份或原始 SDK 对象。
export const INGRESS_REPORT_INTERVAL_MS = 60000;
export const INGRESS_REJECTION_REASONS = Object.freeze([
  "message_id", "sender_id", "reply_target", "target_scope", "target_message_id",
  "target_id", "content_type", "content_bytes", "c2c_target", "group_target",
  "normalized_text_empty", "normalized_text_bytes",
]);
const increment = value => Math.min(Number.MAX_SAFE_INTEGER, value + 1);
const valueType = value => value === null ? "null" : Array.isArray(value) ? "array" :
  ["undefined", "string", "number", "boolean", "object"].includes(typeof value) ? typeof value : "other";
const bytes = value => typeof value === "string" ? Buffer.byteLength(value) : null;
const count = value => Array.isArray(value) ? value.length : null;

function shape(msg, attachments, text) {
  const target = msg.replyTarget;
  const elements = msg.msg_elements ?? msg.msgElements;
  const sample = {
    kind: ["c2c", "group"].includes(msg.kind) ? msg.kind : "other",
    event: msg.rawEventType === "C2C_MESSAGE_CREATE" ? "c2c_message" :
      msg.rawEventType === "GROUP_AT_MESSAGE_CREATE" ? "group_at_message" :
      msg.rawEventType === "GROUP_MESSAGE_CREATE" ? "group_message" : "other",
    reply_target_type: valueType(target),
    target_scope: ["c2c", "group"].includes(target?.scope) ? target.scope : "other",
    attachments_type: valueType(msg.attachments),
    attachments_count: count(msg.attachments),
    elements_type: valueType(elements),
    elements_count: count(elements),
    accepted_attachment_count: attachments.length,
    normalized_text_bytes: bytes(text),
  };
  for (const [name, value] of [
    ["message_id", msg.messageId], ["sender_id", msg.senderId],
    ["target_message_id", target?.msgId], ["target_id", target?.targetId],
    ["group_id", msg.groupOpenid], ["content", msg.content],
  ]) {
    sample[`${name}_type`] = valueType(value);
    sample[`${name}_bytes`] = bytes(value);
  }
  return sample;
}

export function createIngressDiagnostics(emit, now, timers = {}) {
  const schedule = timers.setTimeout ?? setTimeout;
  const unschedule = timers.clearTimeout ?? clearTimeout;
  const reasons = new Map();
  let closed = false;
  function report(reason, state) {
    if (closed || !state.count) return;
    const at = now();
    emit({ type: "warning", code: "invalid_route_or_text", ingress: {
      version: 1, reason, count: state.count, total: state.total,
      counts_saturated: state.saturated,
      elapsed_ms: Math.min(Number.MAX_SAFE_INTEGER, Math.max(0, Math.floor(at - state.reportedAt))),
      last: state.last,
    } });
    state.count = 0;
    state.reportedAt = at;
  }
  return {
    reject(reason, msg, attachments, text) {
      if (closed || !INGRESS_REJECTION_REASONS.includes(reason)) return;
      let state = reasons.get(reason);
      if (!state) {
        state = { count: 0, total: 0, saturated: false, reportedAt: now(), timer: null };
        reasons.set(reason, state);
      }
      state.saturated ||= state.total === Number.MAX_SAFE_INTEGER;
      state.count = increment(state.count);
      state.total = increment(state.total);
      state.last = shape(msg, attachments, text);
      const elapsed = Math.max(0, now() - state.reportedAt);
      if (state.total === 1 || elapsed >= INGRESS_REPORT_INTERVAL_MS) {
        if (state.timer !== null) unschedule(state.timer);
        state.timer = null;
        report(reason, state);
      } else if (state.timer === null) {
        state.timer = schedule(() => {
          state.timer = null;
          report(reason, state);
        }, INGRESS_REPORT_INTERVAL_MS - elapsed);
        state.timer?.unref?.();
      }
    },
    close() {
      if (closed) return;
      for (const [reason, state] of reasons) {
        if (state.timer !== null) unschedule(state.timer);
        report(reason, state);
      }
      closed = true;
      reasons.clear();
    },
  };
}
