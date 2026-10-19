// Tencent SDK adaptation; stdout is reserved for the versioned JSONL protocol.
export const MAX_FRAME = 65536;
// The control task ID adds "qq:" and must fit its 256-byte contract.
export const MAX_MESSAGE_ID_BYTES = 253;
// QQ passive replies to one inbound message are limited; segmented replies stay within it.
export const MAX_SEGMENTS = 5;
// Proactive invitations are short host-composed texts; Rust enforces the same bound.
export const MAX_PUSH_BYTES = 2048;
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
function userText(msg, appId) {
  let text = typeof msg.content === "string" ? msg.content.trim() : "";
  if (msg.kind !== "group") return text;
  const own = new Set(validId(appId) ? [appId] : []);
  for (const mention of Array.isArray(msg.mentions) ? msg.mentions : []) {
    if (mention?.is_you !== true) continue;
    for (const id of [mention.id, mention.member_openid, mention.user_openid]) {
      if (validId(id)) own.add(id);
    }
  }
  while (true) {
    const match = /^<@!?([A-Za-z0-9_-]{1,128})>\s*/.exec(text);
    if (!match || !own.has(match[1])) break;
    text = text.slice(match[0].length).trimStart();
  }
  return text;
}
// Only the envelope's own attachments belong to this message. msg_elements can
// contain quoted images; never treat those as freshly uploaded pictures.
function imageAttachments(msg) {
  if (!Array.isArray(msg.attachments)) return [];
  return msg.attachments.filter(a => a && typeof a.content_type === "string" &&
    /^image\/[a-z0-9.+-]{1,64}$/i.test(a.content_type) &&
    typeof a.url === "string" && a.url.length <= 4096 &&
    /^https:\/\/[^/\s]+\//.test(a.url) && !/[\p{Cc}]/u.test(a.url))
    .slice(0, 4).map(a => ({ content_type: a.content_type.toLowerCase(), url: a.url }));
}
export function optionsFromEnv(env) {
  const appId = env.QQBOT_APP_ID || "1904159860";
  if (!validId(appId) || !env.QQBOT_APP_SECRET?.trim()) throw new Error("credentials_missing");
  if (!["", "true", "false"].includes(env.QQBOT_SANDBOX || "")) throw new Error("sandbox_invalid");
  // Markdown needs a separately reviewed permission; without it QQ rejects every
  // send with 40034090, so it stays opt-in and an invalid value fails closed.
  if (!["", "true", "false"].includes(env.QQBOT_MARKDOWN || "")) throw new Error("markdown_invalid");
  return {
    appId, appSecret: env.QQBOT_APP_SECRET, intents: 1 << 25,
    markdownSupport: env.QQBOT_MARKDOWN === "true",
    baseUrl: env.QQBOT_SANDBOX === "true" ? "https://sandbox.api.sgroup.qq.com" : "https://api.sgroup.qq.com",
    tokenBaseUrl: "https://bots.qq.com",
    logger: { info() {}, warn() {}, error() {}, debug() {} },
  };
}
// 群里回谁就标出谁：被动回复正文前加官方提及标签；私聊没有可 @ 的对象。
// 提及是路由信息，不写回执正文，Rust 保存的仍是模型原话。
const atUser = id => `<qqbot-at-user id="${id}" /> `;
function mentionSender(item, text, index = 0) {
  // 只有第一段带提及；模型自己写了提及时不重复追加。
  if (index !== 0 || !item.mention) return text;
  if (text.includes("<qqbot-at-user")) return text;
  const prefix = atUser(item.mention);
  // 加前缀不得让正文超出平台上限；超了就退回不带提及的原正文。
  return Buffer.byteLength(text) <= 32768 - Buffer.byteLength(prefix) ? prefix + text : text;
}
export function createBridge(bot, emit, limit = 128) {
  const pending = new Map();
  const pushing = new Set();
  const warned = new Set();
  let activeSends = 0;
  let closed = false;
  const send = frame => { if (!closed) emit({ version: 1, ...frame }); };
  const warn = code => {
    if (!warned.has(code)) { warned.add(code); send({ type: "warning", code }); }
  };
  // Share one bound across passive replies, segments and proactive messages.
  // Admission failure happens before the SDK call and is never queued/retried.
  async function sendText(target, text) {
    if (activeSends >= limit) {
      warn("outbound_limit");
      throw new Error("outbound_limit");
    }
    activeSends++;
    try { return await bot.sendText(target, text); }
    finally { activeSends--; }
  }
  // 文件走同一条被动配额与并发上限；路径已由 Rust 侧解析并校验，这里只做形状检查。
  async function sendFile(target, localPath, name, caption) {
    if (activeSends >= limit) {
      warn("outbound_limit");
      throw new Error("outbound_limit");
    }
    activeSends++;
    try {
      return await bot.sendFile(target, { localPath },
        { ...(name ? { fileName: name } : {}), ...(caption ? { content: caption } : {}) });
    } finally { activeSends--; }
  }
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
    const attachments = imageAttachments(msg);
    if (!validMessageId(msg.messageId) || !validId(msg.senderId) || !target ||
        target.scope !== msg.kind || target.msgId !== msg.messageId ||
        !validId(target.targetId) || (typeof msg.content !== "string" || Buffer.byteLength(msg.content) > 32768) ||
        (msg.kind === "c2c" && target.targetId !== msg.senderId) ||
        (msg.kind === "group" && target.targetId !== msg.groupOpenid)) {
      warn("invalid_route_or_text"); return;
    }
    if (pending.has(msg.messageId)) { warn("duplicate_pending"); return; }
    if (pending.size >= limit) { warn("pending_limit"); return; }
    const text = userText(msg, bot.appId);
    if (!validText(text) && !attachments.length) { warn("invalid_route_or_text"); return; }
    const senderName = typeof msg.senderName === "string" ? msg.senderName.trim() : "";
    const validName = senderName && Buffer.byteLength(senderName) <= 128 && [...senderName].length <= 48 && !/[\p{Cc}]/u.test(senderName);
    const frame = { type: "message", id: msg.messageId, scope: msg.kind,
      target_id: target.targetId, user_id: msg.senderId,
      text, ...(attachments.length ? { attachments } : {}), ...(validName ? { sender_name: senderName } : {}) };
    // JSON escaping can expand a legal body beyond the Rust wire-frame limit.
    if (Buffer.byteLength(JSON.stringify({ version: 1, ...frame })) > MAX_FRAME) {
      warn("frame_limit"); return;
    }
    // 群聊记下发送者，被动回复时 @ 回他；私聊没有可 @ 的对象。
    pending.set(msg.messageId, {
      target: { ...target }, sending: false,
      ...(msg.kind === "group" && validId(msg.senderId) ? { mention: msg.senderId } : {}),
    });
    send(frame);
  });
  // One part at a time, strictly in order; a failed or final part ends the reply.
  function reject(id, index) {
    warn("invalid_reply_text");
    pending.delete(id);
    send({ type: "delivery", id, ...(index === undefined ? {} : { index }), ok: false });
  }
  async function segment(frame, item) {
    const { index, count } = frame;
    if (!Number.isInteger(count) || count < 2 || count + (item.progress ?? 0) > MAX_SEGMENTS ||
        !Number.isInteger(index) || index < 0 || index >= count) {
      warn("invalid_command"); return;
    }
    if (item.sending) { warn("duplicate_reply"); return; }
    if ((item.next ?? 0) !== index || (item.count ?? count) !== count) { warn("segment_order"); return; }
    if (!validText(frame.text)) { reject(frame.id, index); return; }
    item.sending = true;
    item.count = count;
    let ok = false;
    try {
      const result = await sendText(item.target, mentionSender(item, frame.text, index));
      ok = true;
      send({ type: "delivery", id: frame.id, index, ok: true,
        ...(validMessageId(result?.id) ? { message_id: result.id } : {}) });
    } catch (err) {
      const diagnostic = {};
      if (Number.isInteger(err?.statusCode)) diagnostic.http_status = err.statusCode;
      if (Number.isInteger(err?.code)) diagnostic.biz_code = err.code;
      send({ type: "delivery", id: frame.id, index, ok: false, ...diagnostic });
    } finally {
      item.sending = false;
      item.next = index + 1;
      if (!ok || item.next === count || item.finished) pending.delete(frame.id);
    }
  }
  // Progress consumes the original passive window, but never completes its reply.
  async function progress(frame, item) {
    const index = item.progress ?? 0;
    if (item.sending || item.next !== undefined || frame.index !== index || index >= 3) {
      warn("invalid_progress"); return;
    }
    if (!validText(frame.text) || Buffer.byteLength(frame.text) > 1024) {
      warn("invalid_progress");
      send({ type: "delivery", id: frame.id, progress: true, index, ok: false });
      return;
    }
    item.progress = index + 1; // Failed/unknown attempts also consume quota; never retry.
    item.sending = true;
    try {
      const result = await sendText(item.target, frame.text);
      send({ type: "delivery", id: frame.id, progress: true, index, ok: true,
        ...(validMessageId(result?.id) ? { message_id: result.id } : {}) });
    } catch {
      send({ type: "delivery", id: frame.id, progress: true, index, ok: false });
    } finally {
      item.sending = false;
      if (item.finished) pending.delete(frame.id);
    }
  }
  // 文件走主动投递：目标由 Rust 从已确认的回执推出，路径由宿主在绑定目录内解析好，
  // 这里只校验形状。失败不重试，回执如实交回宿主账本。
  async function pushFile(frame) {
    if (!validMessageId(frame.id) || !validId(frame.target_id) ||
        !["c2c", "group"].includes(frame.scope)) {
      warn("invalid_command");
      if (validMessageId(frame.id)) send({ type: "delivery", id: frame.id, push: true, file: true, ok: false });
      return;
    }
    const key = `file:${frame.id}`;
    if (pushing.has(key)) { warn("duplicate_reply"); return; }
    const path = frame.path;
    const name = frame.name;
    const caption = frame.caption ?? "";
    if (typeof path !== "string" || !path || path.length > 4096 || /[\p{Cc}]/u.test(path) ||
        (name !== undefined && (typeof name !== "string" || !name || name.length > 255 ||
          /[\p{Cc}\\/]/u.test(name))) ||
        (caption !== "" && (!validText(caption) || Buffer.byteLength(caption) > 1024))) {
      warn("invalid_file");
      send({ type: "delivery", id: frame.id, push: true, file: true, ok: false });
      return;
    }
    pushing.add(key);
    try {
      const result = await sendFile({ scope: frame.scope, targetId: frame.target_id }, path, name, caption);
      const id = result?.message?.id ?? result?.upload?.id;
      send({ type: "delivery", id: frame.id, push: true, file: true, ok: true,
        ...(validMessageId(id) ? { message_id: id } : {}) });
    } catch (err) {
      const diagnostic = {};
      if (Number.isInteger(err?.statusCode)) diagnostic.http_status = err.statusCode;
      if (Number.isInteger(err?.code)) diagnostic.biz_code = err.code;
      send({ type: "delivery", id: frame.id, push: true, file: true, ok: false, ...diagnostic });
    } finally {
      pushing.delete(key);
    }
  }
  // A proactive private message: no msg_id, so the platform applies its own quota and
  // the user's opt-out; refusals come back as a failed receipt and are never retried here.
  async function push(frame) {
    if (!validMessageId(frame.id) || !validId(frame.target_id)) { warn("invalid_command"); return; }
    if (pushing.has(frame.id)) { warn("duplicate_reply"); return; }
    if (!validText(frame.text) || Buffer.byteLength(frame.text) > MAX_PUSH_BYTES) {
      warn("invalid_reply_text");
      send({ type: "delivery", id: frame.id, push: true, ok: false });
      return;
    }
    pushing.add(frame.id);
    try {
      const result = await sendText({ scope: "c2c", targetId: frame.target_id }, frame.text);
      send({ type: "delivery", id: frame.id, push: true, ok: true,
        ...(validMessageId(result?.id) ? { message_id: result.id } : {}) });
    } catch (err) {
      const diagnostic = {};
      if (Number.isInteger(err?.statusCode)) diagnostic.http_status = err.statusCode;
      if (Number.isInteger(err?.code)) diagnostic.biz_code = err.code;
      send({ type: "delivery", id: frame.id, push: true, ok: false, ...diagnostic });
    } finally {
      pushing.delete(frame.id);
    }
  }
  return {
    stop,
    warn,
    async command(frame) {
      if (!frame || typeof frame !== "object" || Array.isArray(frame)) { warn("invalid_command"); return; }
      if (frame.version !== 1) { warn("protocol_version"); return; }
      if (frame.type === "stop") { stop(); return; }
      if (closed) return;
      if (frame.type === "push") { await push(frame); return; }
      if (frame.type === "file") { await pushFile(frame); return; }
      const item = pending.get(frame.id);
      if (!item) { warn("unknown_reply"); return; }
      if (frame.type === "finish") {
        if (!item.sending) pending.delete(frame.id);
        else item.finished = true;
        return;
      }
      if (frame.type === "segment") { await segment(frame, item); return; }
      if (frame.type === "progress") { await progress(frame, item); return; }
      if (frame.type !== "reply") { warn("invalid_command"); return; }
      if (item.sending || item.next !== undefined) { warn("duplicate_reply"); return; }
      // Text Rust accepted but QQ cannot carry (e.g. only U+FEFF) fails this reply
      // explicitly instead of leaving Rust waiting for a receipt that never comes.
      if (!validText(frame.text)) { reject(frame.id); return; }
      item.sending = true;
      try {
        const result = await sendText(item.target, mentionSender(item, frame.text));
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
  const outstanding = new Set();
  for await (const chunk of input) {
    const incoming = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    let start = 0;
    for (let i = 0; i < incoming.length; i++) {
      if (incoming[i] !== 10) continue;
      if (!discarding && bytes.length + i - start <= MAX_FRAME) {
        const line = Buffer.concat([bytes, incoming.subarray(start, i)]).toString("utf8");
        let frame;
        try { frame = JSON.parse(line); } catch { bridge.warn("invalid_json"); }
        if (frame !== undefined) {
          // Network sends must not hold up the input reader or stop/finish.
          // createBridge admits at most `limit` actual SDK sends at once.
          const command = bridge.command(frame).catch(() => bridge.warn("invalid_command"));
          outstanding.add(command);
          void command.then(() => outstanding.delete(command));
          if (frame?.version === 1 && frame.type === "stop") {
            await command;
            return; // In-flight remote sends have unknown outcomes; do not resend.
          }
          // Release completed commands even within a large input chunk.
          await Promise.resolve();
        }
      } else bridge.warn("frame_limit");
      bytes = Buffer.alloc(0); discarding = false; start = i + 1;
    }
    if (!discarding) {
      if (bytes.length + incoming.length - start > MAX_FRAME) {
        bytes = Buffer.alloc(0); discarding = true; bridge.warn("frame_limit");
      } else bytes = Buffer.concat([bytes, incoming.subarray(start)]);
    }
  }
  await Promise.all(outstanding);
  bridge.stop();
}
