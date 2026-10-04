// Tencent SDK adaptation; stdout is reserved for the versioned JSONL protocol.
export const MAX_FRAME = 65536;
// The control task ID adds "qq:" and must fit its 256-byte contract.
export const MAX_MESSAGE_ID_BYTES = 253;
const validId = v => typeof v === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(v);
const validMessageId = v => typeof v === "string" && v.trim() === v && v.length > 0 &&
  Buffer.byteLength(v) <= MAX_MESSAGE_ID_BYTES && !/[\p{Cc}]/u.test(v);
const validText = v => typeof v === "string" && v.trim() && Buffer.byteLength(v) <= 32768;
function isGroupAt(msg, appId) {
  if (msg.rawEventType === "GROUP_AT_MESSAGE_CREATE") return true;
  if (msg.rawEventType !== "GROUP_MESSAGE_CREATE") return false;
  if (Array.isArray(msg.mentions) && msg.mentions.some(m => m?.is_you === true)) return true;
  return validId(appId) && typeof msg.content === "string" &&
    new RegExp(`<@!?${appId}>`).test(msg.content);
}
export function optionsFromEnv(env) {
  const appId = env.QQBOT_APP_ID || "1904159860";
  if (!validId(appId) || !env.QQBOT_APP_SECRET?.trim()) throw new Error("credentials_missing");
  if (!["", "true", "false"].includes(env.QQBOT_SANDBOX || "")) throw new Error("sandbox_invalid");
  return {
    appId, appSecret: env.QQBOT_APP_SECRET, intents: 1 << 25,
    baseUrl: env.QQBOT_SANDBOX === "true" ? "https://sandbox.api.sgroup.qq.com" : "https://api.sgroup.qq.com",
    tokenBaseUrl: "https://bots.qq.com",
    logger: { info() {}, warn() {}, error() {}, debug() {} },
  };
}
export function createBridge(bot, emit, limit = 128) {
  const pending = new Map();
  const warned = new Set();
  let closed = false;
  const send = frame => { if (!closed) emit({ version: 1, ...frame }); };
  const warn = code => {
    if (!warned.has(code)) { warned.add(code); send({ type: "warning", code }); }
  };
  const stop = () => {
    if (closed) return;
    closed = true;
    pending.clear();
    bot.stop();
  };
  bot.on("ready", () => send({ type: "ready" }));
  bot.on("resumed", () => warn("transport_resumed"));
  bot.on("error", () => warn("sdk_error"));
  bot.on("message", (_ctx, msg) => {
    if (closed) return;
    if (!["c2c", "group"].includes(msg.kind) || msg.senderIsBot ||
        (msg.kind === "group" && !isGroupAt(msg, bot.appId))) {
      warn("unsupported_message"); return;
    }
    const target = msg.replyTarget;
    if (!validMessageId(msg.messageId) || !validId(msg.senderId) || !target ||
        target.scope !== msg.kind || target.msgId !== msg.messageId ||
        !validId(target.targetId) || !validText(msg.content) ||
        (msg.kind === "c2c" && target.targetId !== msg.senderId) ||
        (msg.kind === "group" && target.targetId !== msg.groupOpenid)) {
      warn("invalid_route_or_text"); return;
    }
    if (pending.has(msg.messageId)) { warn("duplicate_pending"); return; }
    if (pending.size >= limit) { warn("pending_limit"); return; }
    pending.set(msg.messageId, { target: { ...target }, sending: false });
    send({ type: "message", id: msg.messageId, scope: msg.kind,
      target_id: target.targetId, user_id: msg.senderId,
      text: msg.content.trim() });
  });
  return {
    stop,
    warn,
    async command(frame) {
      if (!frame || typeof frame !== "object" || Array.isArray(frame)) { warn("invalid_command"); return; }
      if (frame.version !== 1) warn("protocol_version");
      if (frame.type === "stop") { stop(); return; }
      if (closed) return;
      const item = pending.get(frame.id);
      if (!item) { warn("unknown_reply"); return; }
      if (frame.type === "finish") {
        if (!item.sending) pending.delete(frame.id);
        return;
      }
      if (frame.type !== "reply" || !validText(frame.text)) { warn("invalid_command"); return; }
      if (item.sending) { warn("duplicate_reply"); return; }
      item.sending = true;
      try {
        const result = await bot.sendText(item.target, frame.text);
        send({ type: "delivery", id: frame.id, ok: true,
          ...(validMessageId(result?.id) ? { message_id: result.id } : {}) });
      } catch (err) {
        const diagnostic = {};
        if (Number.isInteger(err?.statusCode)) diagnostic.http_status = err.statusCode;
        if (Number.isInteger(err?.code)) diagnostic.biz_code = err.code;
        send({ type: "delivery", id: frame.id, ok: false, ...diagnostic });
      } finally {
        pending.delete(frame.id);
      }
    },
  };
}
export async function consume(input, bridge) {
  let bytes = Buffer.alloc(0), discarding = false;
  for await (const chunk of input) {
    const incoming = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    let start = 0;
    for (let i = 0; i < incoming.length; i++) {
      if (incoming[i] !== 10) continue;
      if (!discarding && bytes.length + i - start <= MAX_FRAME) {
        const line = Buffer.concat([bytes, incoming.subarray(start, i)]).toString("utf8");
        try { await bridge.command(JSON.parse(line)); } catch { bridge.warn("invalid_json"); }
      } else bridge.warn("frame_limit");
      bytes = Buffer.alloc(0); discarding = false; start = i + 1;
    }
    if (!discarding) {
      if (bytes.length + incoming.length - start > MAX_FRAME) {
        bytes = Buffer.alloc(0); discarding = true; bridge.warn("frame_limit");
      } else bytes = Buffer.concat([bytes, incoming.subarray(start)]);
    }
  }
  bridge.stop();
}
