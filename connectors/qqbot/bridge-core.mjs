// Tencent SDK adaptation; stdout is reserved for the versioned JSONL protocol.
import { createIngressDiagnostics } from "./ingress-diagnostics.mjs";
export const MAX_FRAME = 65536;
// Raw SDK body and display text are separate; JSON escaping can expand both by 6x.
export const MAX_INBOUND_FRAME = 512 * 1024;
// The control task ID adds "qq:" and must fit its 256-byte contract.
export const MAX_MESSAGE_ID_BYTES = 253;
// QQ passive replies to one inbound message are limited; segmented replies stay within it.
export const MAX_SEGMENTS = 5;
// Proactive invitations are short host-composed texts; Rust enforces the same bound.
export const MAX_PUSH_BYTES = 2048;
// Leave a margin before QQ's five-minute passive reply window expires.
export const PASSIVE_REPLY_WINDOW_MS = 240000;
// Bound the whole SDK operation, including response-body reads after headers.
// Rust's independently owned receipt deadlines leave a margin for this result.
export const SEND_TIMEOUT_MS = 30000;
export const FILE_TIMEOUT_MS = 110000;
const passiveLimit = scope => scope === "c2c" ? 4 : MAX_SEGMENTS;
function diagnostic(err) {
  const out = {};
  const status = err?.httpStatus ?? err?.statusCode;
  const code = err?.bizCode ?? err?.code;
  if (Number.isInteger(status) && status >= 100 && status <= 599) out.http_status = status;
  if (Number.isSafeInteger(code)) out.biz_code = code;
  // 仅分类标准网络错误码，不记录原始 error、请求体或凭据；未知投递仍不重试。
  const transport = typeof err?.cause?.code === "string" ? err.cause.code : err?.code;
  out.failure_kind = ["stale_decision", "outbound_limit", "reply_limit"].includes(err?.message) ? err.message : out.http_status ? "http" :
    ["AbortError", "TimeoutError"].includes(err?.name) ||
      ["ETIMEDOUT", "UND_ERR_CONNECT_TIMEOUT", "UND_ERR_HEADERS_TIMEOUT", "UND_ERR_BODY_TIMEOUT"].includes(transport) ? "timeout" :
    ["ENOTFOUND", "EAI_AGAIN"].includes(transport) ? "dns" :
    ["ECONNRESET", "ECONNREFUSED", "EPIPE", "ENETUNREACH", "EHOSTUNREACH", "UND_ERR_SOCKET"].includes(transport) ? "connection" :
    ["CERT_HAS_EXPIRED", "DEPTH_ZERO_SELF_SIGNED_CERT", "UNABLE_TO_VERIFY_LEAF_SIGNATURE", "ERR_TLS_CERT_ALTNAME_INVALID"].includes(transport) ? "tls" : "transport_unknown";
  return out;
}
const validId = v => typeof v === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(v);
const validMessageId = v => typeof v === "string" && v.trim() === v && v.length > 0 &&
  Buffer.byteLength(v) <= MAX_MESSAGE_ID_BYTES && !/[\p{Cc}]/u.test(v);
const validText = v => typeof v === "string" && v.trim() && Buffer.byteLength(v) <= 32768;
// 顺序与原来的联合校验相同，只拆出固定拒绝原因，不推断或修补可信路由。
function routeOrTextRejection(msg) {
  const target = msg.replyTarget;
  if (!validMessageId(msg.messageId)) return "message_id";
  if (!validId(msg.senderId)) return "sender_id";
  if (!target) return "reply_target";
  if (target.scope !== msg.kind) return "target_scope";
  if (target.msgId !== msg.messageId) return "target_message_id";
  if (!validId(target.targetId)) return "target_id";
  if (typeof msg.content !== "string") return "content_type";
  if (Buffer.byteLength(msg.content) > 32768) return "content_bytes";
  if (msg.kind === "c2c" && target.targetId !== msg.senderId) return "c2c_target";
  if (msg.kind === "group" && target.targetId !== msg.groupOpenid) return "group_target";
  return null;
}
function isGroupAt(msg, appId) {
  if (msg.rawEventType === "GROUP_AT_MESSAGE_CREATE") return true;
  if (msg.rawEventType !== "GROUP_MESSAGE_CREATE") return false;
  if (Array.isArray(msg.mentions) && msg.mentions.some(m => m?.is_you === true)) return true;
  return validId(appId) && typeof msg.content === "string" &&
    new RegExp(`<@!?${appId}>`).test(msg.content);
}
// 名字前缀是明确呼叫，走普通会话；不冒充平台 @，也不交给插话判断。
function isGroupNameWake(msg) {
  return msg.kind === "group" &&
    ["GROUP_MESSAGE_CREATE", "GROUP_AT_MESSAGE_CREATE"].includes(msg.rawEventType) &&
    typeof msg.content === "string" && /^eve/i.test(msg.content.trimStart());
}
// QQ 表情在正文里是 <faceType=..,faceId="..",ext="base64(JSON)"> 或旧式 [<face,id=..>]；
// 转成模型读得懂的【表情：名字】，名字取自 ext 的 text，解不出时只标出这是表情。
function faceLabel(name) {
  const clean = typeof name === "string" ? name.replace(/^\[|\]$/g, "").trim() : "";
  return clean && clean.length <= 32 && !/[\p{Cc}【】]/u.test(clean) ? `【表情：${clean}】` : "【表情】";
}
export function readableFaces(text) {
  return text
    .replace(/<faceType=\d+,faceId="[^"]{0,64}",ext="([A-Za-z0-9+/=]{0,1024})">/g, (_m, ext) => {
      try { return faceLabel(JSON.parse(Buffer.from(ext, "base64").toString("utf8")).text); }
      catch { return faceLabel(""); }
    })
    .replace(/\[<face,id=\d{1,6}\/?>\]/g, faceLabel(""));
}
function userText(msg, appId) {
  let text = typeof msg.content === "string" ? readableFaces(msg.content).trim() : "";
  if (msg.kind !== "group") return text;
  const own = new Set(validId(appId) ? [appId] : []);
  for (const mention of Array.isArray(msg.mentions) ? msg.mentions : []) {
    if (mention?.is_you !== true) continue;
    for (const id of [mention.id, mention.member_openid, mention.user_openid]) {
      if (validId(id)) own.add(id);
    }
  }
  let removedOwnMention = false;
  while (true) {
    const match = /^<@!?([A-Za-z0-9_-]{1,128})>\s*/.exec(text);
    if (!match || !own.has(match[1])) break;
    removedOwnMention = true;
    text = text.slice(match[0].length).trimStart();
  }
  // 纯自身 @ 也是明确呼叫。保留本人实际发出的提及文本供 Eve 判断，
  // 不把空白、引用或附件说明补成用户的话；original_text 仍在信封上原样保存。
  return !text && removedOwnMention && isGroupAt(msg, appId) ? msg.content.trim() : text;
}
function rawMessage(msg) {
  const source = msg.raw && typeof msg.raw === "object" ? msg.raw : msg;
  const keep = ["id", "content", "timestamp", "message_type", "attachments", "msg_elements",
    "message_scene", "mentions"];
  const out = {};
  for (const key of keep) if (source[key] !== undefined) out[key] = source[key];
  let text;
  try { text = JSON.stringify(out); } catch { return null; }
  if (Buffer.byteLength(text) > 24576) return { truncated: true, bytes: Buffer.byteLength(text) };
  return out;
}
// 只接平台事件的明确时间字段；不按接收时间补猜，不从正文或调试格式推断。
function sourceTime(msg) {
  const value = msg.timestamp ?? msg.raw?.timestamp;
  if (typeof value !== "string" || !/^\d{4}-\d\d-\d\dT.*(?:Z|[+-]\d\d:\d\d)$/.test(value)) return undefined;
  const parsed = Date.parse(value);
  return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : undefined;
}
// Only the envelope's own attachments belong to this message. msg_elements can
// contain quoted images; never treat those as freshly uploaded pictures.
const FILE_TYPES = new Set(["text/plain", "application/pdf", "application/zip",
  "application/json", "application/octet-stream", "file"]);
function allowedType(type) {
  return /^image\/[a-z0-9.+-]{1,64}$/i.test(type) || FILE_TYPES.has(type.toLowerCase());
}
function attachment(raw, reject) {
  if (!raw || typeof raw !== "object") { reject?.("invalid_shape"); return null; }
  const type = typeof raw.content_type === "string" && raw.content_type
    ? raw.content_type
    : typeof raw.contentType === "string" && raw.contentType ? raw.contentType : "application/octet-stream";
  if (!allowedType(type)) { reject?.("unsupported_type"); return null; }
  const url = typeof raw.url === "string" ? raw.url : "";
  if (url && (url.length > 4096 || !/^https:\/\/[^/\s]+\//.test(url) || /[\p{Cc}]/u.test(url))) {
    reject?.("invalid_url"); return null;
  }
  const out = { content_type: type.toLowerCase() };
  if (url) out.url = url;
  const name = raw.filename ?? raw.file_name ?? raw.fileName;
  if (typeof name === "string" && name.trim() === name && name && name.length <= 255 &&
      !/[\p{Cc}\/\\]/u.test(name)) out.filename = name;
  return out;
}
// 本条自己的附件。信封上没有时，文件经常只出现在 msg_elements 里，
// 引用消息的元素带 msg_idx，不并进本条。
function ownAttachments(msg, rejections) {
  const found = [];
  // 只保留本次实际筛选分支的固定计数，不保留 MIME、地址或原附件对象。
  const reject = (reason, amount = 1) => {
    if (amount > 0) rejections[reason] = Math.min(Number.MAX_SAFE_INTEGER, (rejections[reason] ?? 0) + amount);
  };
  const push = raw => {
    const item = attachment(raw, reject);
    if (!item) return;
    if (found.length >= 4) { reject("limit"); return; }
    if (found.some(saved =>
        saved.content_type === item.content_type && saved.url === item.url && saved.filename === item.filename)) {
      reject("duplicate"); return;
    }
    found.push(item);
  };
  for (const raw of Array.isArray(msg.attachments) ? msg.attachments : []) push(raw);
  const elements = field(msg, "msg_elements", "msgElements");
  const quoted = Number(field(msg, "msg_type", "msgType")) === 103;
  if (!found.length && Array.isArray(elements)) {
    const own = elements.filter((element, index) => {
      if (!element || typeof element !== "object") return false;
      if (field(element, "msg_idx", "msgIdx") != null || (quoted && index === 0)) {
        reject("quoted", Array.isArray(element.attachments) ? element.attachments.length : 0);
        return false;
      }
      return true;
    });
    const current = (typeof msg.content === "string" ? msg.content : "").trim();
    const textual = own.filter(element => typeof element.content === "string" && element.content.trim());
    const selected = textual.length
      ? textual.filter(element => !current || element.content.trim() === current)
      : own;
    if (textual.length) {
      const selectedSet = new Set(selected);
      for (const element of own) {
        if (!selectedSet.has(element)) {
          reject("element_not_selected", Array.isArray(element.attachments) ? element.attachments.length : 0);
        }
      }
    }
    for (const element of selected) {
      for (const raw of Array.isArray(element.attachments) ? element.attachments : []) push(raw);
    }
  }
  return found;
}
// 被引用消息的文字与附件单独抽出，不和本条新发的附件混在一起。
function field(object, snake, camel) {
  return object?.[snake] ?? object?.[camel];
}
function quotedMessage(msg) {
  const elements = field(msg, "msg_elements", "msgElements");
  if (!Array.isArray(elements)) return null;
  const scene = field(msg, "message_scene", "messageScene") ?? {};
  const ext = field(scene, "ext", "ext");
  const refs = new Set();
  for (const item of Array.isArray(ext) ? ext : []) {
    const match = typeof item === "string" && item.match(/^ref_msg_idx=(.+)$/);
    if (match) refs.add(match[1]);
  }
  const quoted = Number(field(msg, "msg_type", "msgType")) === 103;
  const selected = elements.filter((element, index) => element && typeof element === "object" &&
    (refs.size ? refs.has(field(element, "msg_idx", "msgIdx")) : quoted && index === 0));
  const texts = [];
  const attachments = [];
  for (const element of selected) {
    const content = typeof element.content === "string" ? readableFaces(element.content) : element.content;
    if (typeof content === "string" && content.trim() &&
        Buffer.byteLength(content) <= 1024 && !/[\p{Cc}]/u.test(content)) texts.push(content.trim());
    for (const raw of Array.isArray(element.attachments) ? element.attachments : []) {
      const item = attachment(raw);
      if (item && attachments.length < 4) attachments.push(item);
    }
  }
  if (!texts.length && !attachments.length) return null;
  const text = texts.join("\n").slice(0, 1024);
  return { ...(text ? { text } : {}), ...(attachments.length ? { attachments } : {}) };
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
export function createBridge(bot, emit, limit = 128, now = () => performance.now(), group = {}, timers = {}) {
  const pending = new Map();
  const passiveWindows = new Map();
  // 群内合入长轮次的新输入保留原始窗口；账本仍属于父输入。
  const liveReplies = new Map();
  const retiredLiveInputs = new Map();
  const latestInput = new Map();
  const routeKey = (scope, target, sender) => JSON.stringify([scope,target,sender]);
  const pushing = new Set();
  const warned = new Set();
  let activeSends = 0;
  let closed = false;
  const schedule = timers.setTimeout ?? setTimeout;
  const unschedule = timers.clearTimeout ?? clearTimeout;
  // A timed-out transport may still be inside the SDK. Keep it in activeSends
  // until it settles, so repeated timeouts cannot create unbounded requests.
  async function boundedSend(operation, timeout) {
    if (activeSends >= limit) {
      warn("outbound_limit");
      throw new Error("outbound_limit");
    }
    activeSends++;
    const pending = Promise.resolve().then(operation).finally(() => { activeSends--; });
    let timer;
    try {
      return await Promise.race([pending, new Promise((_, reject) => {
        timer = schedule(() => reject(Object.assign(new Error("send_timeout"), {name:"TimeoutError"})), timeout);
      })]);
    } finally { unschedule(timer); }
  }
  const send = frame => { if (!closed) emit({ version: 1, ...frame }); };
  const warn = code => {
    if (!warned.has(code)) { warned.add(code); send({ type: "warning", code }); }
  };
  const ingress = createIngressDiagnostics(send, now, timers);
  function clearLiveReplies(parentId) {
    liveReplies.delete(parentId);
    for (const [id, parent] of retiredLiveInputs) if (parent === parentId) retiredLiveInputs.delete(id);
  }
  function forgetPending(id) {
    clearLiveReplies(id);
    if (passiveWindows.get(id)?.parent && passiveWindows.get(id).parent === pending.get(id)) passiveWindows.delete(id);
    pending.delete(id);
  }
  const replyMention = item => group.mention !== false && item.target.scope === "group" ? item.senderId : undefined;
  function mentionedText(target, text, mention) {
    if (mention === undefined || mention === null) return text;
    if (target.scope !== "group" || !validId(mention)) throw new Error("invalid_mention");
    const prefix = `<@${mention}>`;
    return text.startsWith(prefix) ? text : `${prefix} ${text}`.trimEnd();
  }
  // Share one bound across passive replies, segments and proactive messages.
  // Admission failure happens before the SDK call and is never queued/retried.
  async function sendText(target, text, mention, timeout = SEND_TIMEOUT_MS, guard) {
    text = mentionedText(target, text, mention);
    if (Buffer.byteLength(text) > 32768) throw new Error("reply_limit");
    const result = await boundedSend(() => {
      if (guard && !guard()) throw new Error("stale_decision");
      return bot.sendText(target, text);
    }, timeout);
    if (!validMessageId(result?.id)) throw new Error("platform_message_id_unknown");
    return result;
  }
  // 文件走同一条并发上限；路径已由 Rust 侧解析并校验，这里只做形状检查。
  async function sendFile(target, localPath, name, caption) {
    const result = await boundedSend(() => bot.sendFile(target, { localPath },
      { ...(name ? { fileName: name } : {}), ...(caption ? { content: caption } : {}) }), FILE_TIMEOUT_MS);
    if (!validMessageId(result?.message?.id)) throw new Error("platform_message_id_unknown");
    return result;
  }
  // 表情包作为原消息的被动回复图片发出，同样计入并发上限。
  async function sendImage(target, localPath, guard) {
    const result = await boundedSend(() => {
      if(guard && !guard()) throw new Error("stale_decision");
      return bot.sendImage(target, { localPath });
    }, SEND_TIMEOUT_MS);
    if (!validMessageId(result?.message?.id)) throw new Error("platform_message_id_unknown");
    return result;
  }
  const stop = () => {
    if (closed) return;
    ingress.close();
    closed = true;
    pending.clear();
    passiveWindows.clear();
    liveReplies.clear();
    retiredLiveInputs.clear();
    latestInput.clear();
    bot.stop();
  };
  bot.on("ready", () => send({ type: "ready" }));
  bot.on("resumed", () => warn("transport_resumed"));
  bot.on("error", () => warn("sdk_error"));
  bot.on("message", (_ctx, msg) => {
    if (closed) return;
    if (!["c2c", "group"].includes(msg.kind) || msg.senderIsBot ||
        (msg.kind === "group" && !isGroupAt(msg, bot.appId) && !isGroupNameWake(msg) &&
          !(group.observe && msg.rawEventType === "GROUP_MESSAGE_CREATE"))) {
      warn("unsupported_message"); return;
    }
    const target = msg.replyTarget;
    const attachmentRejections = {};
    const attachments = ownAttachments(msg, attachmentRejections);
    const quoted = quotedMessage(msg);
    const rejected = routeOrTextRejection(msg);
    if (rejected) { ingress.reject(rejected, msg, attachments, undefined, attachmentRejections); return; }
    if (pending.has(msg.messageId) || retiredLiveInputs.has(msg.messageId)) { warn("duplicate_pending"); return; }
    const restrictionOnly = pending.size >= limit;
    if (restrictionOnly) warn("pending_limit");
    const text = userText(msg, bot.appId);
    if (!validText(text) && !attachments.length) {
      ingress.reject(text.trim() ? "normalized_text_bytes" : "normalized_text_empty", msg, attachments, text, attachmentRejections);
      return;
    }
    const senderName = typeof msg.senderName === "string" ? msg.senderName.trim() : "";
    const validName = senderName && Buffer.byteLength(senderName) <= 128 && [...senderName].length <= 48 && !/[\p{Cc}]/u.test(senderName);
    const frame = { type: restrictionOnly ? "restriction_only" : "message", id: msg.messageId, scope: msg.kind,
      target_id: target.targetId, user_id: msg.senderId,
      text, original_text: msg.content, ...(attachments.length ? { attachments } : {}),
      ...(quoted ? { quoted } : {}), ...(validName ? { sender_name: senderName } : {}) };
    const sourceAt = sourceTime(msg);
    if (sourceAt !== undefined) frame.source_at_ms = sourceAt;
    // 调试用的原始事件，和规范化帧分开；只保留有界的结构字段，不进回执。
    const raw = rawMessage(msg);
    if (raw) frame.raw = raw;
    // JSON escaping can expand a legal body beyond the Rust wire-frame limit.
    if (Buffer.byteLength(JSON.stringify({ version: 1, ...frame })) > MAX_INBOUND_FRAME) {
      warn("frame_limit"); return;
    }
    // 群插话面向全群最新事实，旁观消息也会使尚未发送的插话过期。
    if (msg.kind === "group") {
      const groupKey=routeKey("group",target.targetId,"*");
      latestInput.delete(groupKey);
      latestInput.set(groupKey,msg.messageId);
    }
    while(latestInput.size>8192) latestInput.delete(latestInput.keys().next().value);
    if (msg.kind === "group" && group.observe) {
      // 容量满时只保留本条可信缩权入口，不因此新增群观察或待回复路由。
      if (!restrictionOnly) {
        const message = { id:frame.id, scope:frame.scope,target_id:frame.target_id,user_id:frame.user_id,text:frame.text,
          ...(frame.attachments ? {attachments:frame.attachments}:{}), ...(frame.quoted ? {quoted:frame.quoted}:{}) };
        const observation={version:1,type:"group_observation",message,mentions_me:isGroupAt(msg,bot.appId),
          original_text:frame.original_text, ...(sourceAt !== undefined ? {source_at_ms:sourceAt} : {}),
          ...(validName?{sender_name:senderName}:{})};
        if (Buffer.byteLength(JSON.stringify(observation)) <= MAX_INBOUND_FRAME) emit(observation);
        else { warn("frame_limit"); return; }
      }
      if (!isGroupAt(msg,bot.appId) && !isGroupNameWake(msg)) return;
    }
    // 仅前缀模式仍记录群原话；艾特、引用和附件本身都不能绕过前缀唤醒。
    if (msg.kind === "group" && group.prefixOnly && !/^eve/i.test(text.trimStart())) return;
    // 主轮只消费通过唤醒规则的输入。未进入主轮的群旁观不能撤销其
    // 回复或已合入窗口；新唤醒输入仍先使旧决定过期，再由宿主交接。
    const inputKey=routeKey(msg.kind,target.targetId,msg.senderId);
    latestInput.delete(inputKey);
    latestInput.set(inputKey,msg.messageId);
    while(latestInput.size>8192) latestInput.delete(latestInput.keys().next().value);
    for (const [id, item] of passiveWindows) {
      if (now() - item.receivedAt >= PASSIVE_REPLY_WINDOW_MS || routeKey(item.target.scope,item.target.targetId,item.senderId) === inputKey) passiveWindows.delete(id);
    }
    for (const [parentId, window] of liveReplies) {
      const item = window.item;
      if (now() - item.receivedAt >= PASSIVE_REPLY_WINDOW_MS || routeKey(item.target.scope,item.target.targetId,item.senderId) === inputKey) liveReplies.delete(parentId);
    }
    // 不在 SDK 猜测业务句意。Rust 核对真实身份、白名单和完整原话后只允许缩权，
    // 此帧没有 SDK pending 路由，不启动模型任务，也不能获得被动回复额度。
    if (restrictionOnly) { send(frame); return; }
    pending.set(msg.messageId, { target: { ...target }, senderId:msg.senderId, sending: false, receivedAt: now() });
    send(frame);
  });
  // One part at a time, strictly in order; a failed or final part ends the reply.
  function reject(id, index) {
    warn("invalid_reply_text");
    forgetPending(id);
    send({ type: "delivery", id, ...(index === undefined ? {} : { index }), ok: false });
  }
  // A late final answer uses the original conversation's proactive route once.
  // Never replay an ambiguous failed send or change its destination.
  async function finalText(item, text) {
    const elapsed = Math.max(0, Math.floor(now() - item.receivedAt));
    item.proactive ||= elapsed >= PASSIVE_REPLY_WINDOW_MS;
    const target = item.proactive
      ? { scope: item.target.scope, targetId: item.target.targetId } : item.target;
    const meta = { delivery_mode: item.proactive ? "proactive" : "passive", elapsed_ms: elapsed };
    if (!item.proactive && (item.passiveProgress ?? item.progress ?? 0) >= passiveLimit(item.target.scope)) {
      return {ok:false,...meta,failure_kind:"passive_quota_exhausted"};
    }
    try {
      const mention=replyMention(item);
      const result = await sendText(target, text, mention);
      return { ok: true, ...meta,
        ...(validMessageId(result?.id) ? { message_id: result.id } : {}) };
    } catch (err) {
      // 传输只回报事实；是否解释失败以及怎么说，留给 Eve。
      return { ok: false, ...meta, ...diagnostic(err) };
    }
  }
  async function segment(frame, item) {
    const { index, count } = frame;
    if (!Number.isInteger(count) || count < 2 || count + (item.passiveProgress ?? item.progress ?? 0) > passiveLimit(item.target.scope) ||
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
      const result = await finalText(item, frame.text);
      ok = result.ok;
      send({ type: "delivery", id: frame.id, index, ...result });
    } finally {
      item.sending = false;
      item.next = index + 1;
      // 最后一段之后还有表情包时保留本条，供随后的 sticker 帧使用同一个被动目标。
      if (ok && item.next === count && frame.sticker_follows === true && !item.finished) item.awaitSticker = true;
      else if (!ok || item.next === count || item.finished) forgetPending(frame.id);
    }
  }
  // 正文发完后的一张表情包：只走原消息的被动窗口，过期或失败都不重试、不改走主动消息。
  async function sticker(frame, item) {
    if (!item.awaitSticker || item.sending) { warn("invalid_sticker"); return; }
    const path = frame.path;
    item.awaitSticker = false;
    if (typeof path !== "string" || !path || path.length > 4096 || /[\p{Cc}]/u.test(path)) {
      warn("invalid_sticker");
      forgetPending(frame.id);
      send({ type: "delivery", id: frame.id, sticker: true, ok: false });
      return;
    }
    item.sending = true;
    try {
      if (now() - item.receivedAt >= PASSIVE_REPLY_WINDOW_MS) {
        send({ type: "delivery", id: frame.id, sticker: true, ok: false, failure_kind: "passive_window_elapsed" });
        return;
      }
      const result = await sendImage(item.target, path);
      const id = result?.message?.id;
      send({ type: "delivery", id: frame.id, sticker: true, ok: true,
        ...(validMessageId(id) ? { message_id: id } : {}) });
    } catch (err) {
      send({ type: "delivery", id: frame.id, sticker: true, ok: false, ...diagnostic(err) });
    } finally {
      item.sending = false;
      forgetPending(frame.id);
    }
  }
  // 新窗口只替换实际平台路由；父输入的序号继续递增，原窗口额度不跟着增长。
  async function liveReplyProgress(frame, parent) {
    const index = parent.progress ?? 0;
    if (parent.sending || parent.next !== undefined || frame.index !== index || index >= 64) {
      warn("invalid_progress"); return;
    }
    parent.passiveProgress ??= index;
    parent.progress = index + 1;
    const window = liveReplies.get(frame.id);
    const item = window?.item;
    const elapsed = item ? Math.max(0, Math.floor(now() - item.receivedAt)) : undefined;
    const reject = failure_kind => send({type:"delivery",id:frame.id,progress:true,index,ok:false,
      delivery_mode:"passive",...(elapsed === undefined ? {} : {elapsed_ms:elapsed}),failure_kind});
    if (!validMessageId(frame.based_on_message_id)) {reject("stale_decision");return;}
    if (!window || window.anchor !== frame.based_on_message_id) {reject("passive_window_missing");return;}
    if (window.parent !== parent || parent.finished || frame.proactive === true || frame.host_activity === true ||
        item.target.scope !== "group" || item.target.targetId !== parent.target.targetId || item.senderId !== parent.senderId ||
        latestInput.get(routeKey(item.target.scope,item.target.targetId,item.senderId)) !== window.anchor) {
      reject("stale_decision");return;
    }
    const used = item.passiveProgress ?? item.progress ?? 0;
    if (used >= 3) {reject("passive_quota_exhausted");return;}
    // 与宿主持久回执一致：确认失败和未知也占一次尝试，不退回额度。
    item.passiveProgress = used + 1;
    if (elapsed >= PASSIVE_REPLY_WINDOW_MS) {reject("passive_window_elapsed");return;}
    const image = frame.image_path;
    if (!validText(frame.text) || Buffer.byteLength(frame.text) > 32768 ||
        (image !== undefined && (typeof image !== "string" || !image || image.length > 4096 || /[\p{Cc}]/u.test(image)))) {
      reject("reply_limit");return;
    }
    parent.sending = true;
    item.sending = true;
    try {
      const guard = () => !closed && pending.get(frame.id) === parent && !parent.finished && parent.next === undefined &&
        liveReplies.get(frame.id) === window && now() - item.receivedAt < PASSIVE_REPLY_WINDOW_MS &&
        latestInput.get(routeKey(item.target.scope,item.target.targetId,item.senderId)) === window.anchor;
      const result = image === undefined
        ? await sendText(item.target,frame.text,replyMention(item),SEND_TIMEOUT_MS,guard)
        : await sendImage(item.target,image,guard);
      if (image === undefined) item.explicitTextSent = true;
      send({type:"delivery",id:frame.id,progress:true,index,ok:true,delivery_mode:"passive",elapsed_ms:elapsed,
        message_id:image === undefined ? result.id : result.message.id});
    } catch (err) {
      send({type:"delivery",id:frame.id,progress:true,index,ok:false,delivery_mode:"passive",elapsed_ms:elapsed,...diagnostic(err)});
    } finally {
      parent.sending = false;
      item.sending = false;
      if (parent.finished) forgetPending(frame.id);
    }
  }
  // Progress consumes the original passive window, but never completes its reply.
  async function progress(frame, item) {
    if (frame.explicit === true && item.target.scope === "group" &&
        frame.based_on_message_id !== undefined && frame.based_on_message_id !== frame.id) {
      await liveReplyProgress(frame,item);return;
    }
    const hostActivity = frame.host_activity === true;
    const index = item.progress ?? 0;
    if (item.sending || item.next !== undefined || frame.index !== index || index >= 64) {
      warn("invalid_progress"); return;
    }
    if (!validText(frame.text) || Buffer.byteLength(frame.text) > (frame.explicit === true ? 32768 : 1024)) {
      warn("invalid_progress");
      send({ type: "delivery", id: frame.id, progress: true, index, ok: false });
      return;
    }
    if (hostActivity && (frame.explicit === true || frame.proactive === true ||
        frame.image_path !== undefined || !validMessageId(frame.based_on_message_id) ||
        frame.based_on_message_id !== frame.id || !frame.text.startsWith("系统进度："))) {
      warn("invalid_progress");
      send({type:"delivery",id:frame.id,progress:true,index,ok:false});
      return;
    }
    const proactive = frame.proactive === true;
    const elapsed = Math.max(0, Math.floor(now() - item.receivedAt));
    // 自然发言不占父轮 progress 序号，但共用平台被动额度；最后一格
    // 留给主轮的明确 send_message，它在实时模式下也使用 progress 帧。
    const passiveQuota = frame.explicit === true && item.target.scope === "c2c" ? passiveLimit("c2c") : 3;
    if (!proactive && (item.passiveProgress ?? index) >= passiveQuota) {
      item.progress = index + 1;
      send({type:"delivery",id:frame.id,progress:true,index,ok:false,delivery_mode:"passive",
        elapsed_ms:elapsed,failure_kind:"passive_quota_exhausted"});
      return;
    }
    if (proactive && item.lastProgress !== undefined && now() - item.lastProgress < 1000) {
      send({ type: "delivery", id: frame.id, progress: true, index, ok: false, failure_kind: "progress_rate_limit" });
      item.progress = index + 1;
      return;
    }
    item.progress = index + 1; // Failed/unknown attempts also consume quota; never retry.
    item.passiveProgress = (item.passiveProgress ?? index) + (proactive ? 0 : 1);
    item.lastProgress = now();
    item.sending = true;
    try {
      if (hostActivity) {
        const previous = item.hostActivities ?? (item.hostActivities = new Set());
        const quota = item.target.scope === "c2c" ? 2 : 1;
        if (previous.size >= quota || previous.has(frame.text) || item.passiveProgress > 2) {
          send({type:"delivery",id:frame.id,progress:true,index,ok:false,failure_kind:"progress_rate_limit"});
          return;
        }
        // 宿主提示是独立事实；失败或未知也不在本窗口重放同一文本。
        previous.add(frame.text);
      }
      if (!proactive && elapsed >= PASSIVE_REPLY_WINDOW_MS) {
        send({ type: "delivery", id: frame.id, progress: true, index, ok: false,
          failure_kind: "passive_window_elapsed" });
        return;
      }
      const target = proactive ? {scope: item.target.scope, targetId: item.target.targetId} : item.target;
      const guard=frame.explicit===true || hostActivity ? () => !item.finished && latestInput.get(routeKey(item.target.scope,item.target.targetId,item.senderId)) === (frame.based_on_message_id ?? frame.id) : undefined;
      const image=frame.image_path;
      if(image!==undefined && (frame.explicit!==true || typeof image!=="string" || !image || image.length>4096 || /[\p{Cc}]/u.test(image))) throw new Error("invalid_image_path");
      const result = image===undefined ? await sendText(target, frame.text, replyMention(item), SEND_TIMEOUT_MS, guard) : await sendImage(target,image,guard);
      const messageId=image===undefined ? result?.id : result?.message?.id;
      if (frame.explicit === true && image === undefined) item.explicitTextSent = true;
      send({ type: "delivery", id: frame.id, progress: true, index, ok: true, delivery_mode: proactive ? "proactive" : "passive", elapsed_ms: elapsed,
        ...(validMessageId(messageId) ? { message_id: messageId } : {}) });
    } catch (err) {
      send({ type: "delivery", id: frame.id, progress: true, index, ok: false, ...diagnostic(err) });
    } finally {
      item.sending = false;
      if (item.finished) forgetPending(frame.id);
    }
  }
  // 文件走主动投递：目标由 Rust 从已确认的回执推出，路径由宿主解析好，这里只校验形状。
  // 失败不重试，回执如实交回宿主账本。
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
        (name !== undefined && name !== null && (typeof name !== "string" || !name || name.length > 255 ||
          /[\p{Cc}\/]/u.test(name))) ||
        (caption !== "" && (!validText(caption) || Buffer.byteLength(caption) > 1024))) {
      warn("invalid_file");
      send({ type: "delivery", id: frame.id, push: true, file: true, ok: false });
      return;
    }
    pushing.add(key);
    try {
      const target = { scope: frame.scope, targetId: frame.target_id };
      const content = mentionedText(target, caption, frame.mention_user_id);
      const result = await sendFile(target, path, name, content);
      const id = result?.message?.id;
      send({ type: "delivery", id: frame.id, push: true, file: true, ok: true,
        ...(validMessageId(id) ? { message_id: id } : {}) });
    } catch (err) {
      send({ type: "delivery", id: frame.id, push: true, file: true, ok: false, ...diagnostic(err) });
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
      const scope = frame.scope ?? "c2c";
      if (!["c2c", "group"].includes(scope)) throw new Error("invalid_scope");
      const mention=frame.mention_user_id;
      if (mention !== undefined && (scope !== "group" || !validId(mention))) throw new Error("invalid_mention");
      const parts=frame.parts ?? [[frame.text,0]];
      if (!Array.isArray(parts) || parts.length < 1 || parts.length > 3 || parts.some(p=>
        !Array.isArray(p)||p.length!==2||!validText(p[0])||!Number.isInteger(p[1])||p[1]<0||p[1]>2500) ||
        parts.map(p=>p[0].replace(/\s/gu,"")).join("") !== frame.text.replace(/\s/gu,"")) throw new Error("invalid_parts");
      let result;
      const deadline = now() + SEND_TIMEOUT_MS;
      let previousStarted;
      for (let index=0;index<parts.length;index++) {
        if (closed) throw new Error("closed");
        const elapsed = previousStarted === undefined ? 0 : Math.max(0, now() - previousStarted);
        const pause = Math.max(0, parts[index][1] - elapsed);
        if (pause) await new Promise(resolve=>schedule(resolve,pause));
        if (closed) throw new Error("closed");
        const remaining = deadline - now();
        if (remaining <= 0) throw Object.assign(new Error("push_timeout"), {name:"TimeoutError"});
        previousStarted = now();
        const guard=frame.based_on_message_id === undefined ? undefined : () => validMessageId(frame.based_on_message_id) && validId(frame.sender_id) && latestInput.get(routeKey(scope,frame.target_id,frame.group_chime === true ? "*" : frame.sender_id)) === frame.based_on_message_id;
        result = await sendText({ scope, targetId: frame.target_id }, parts[index][0], mention, remaining, guard);
      }
      send({ type: "delivery", id: frame.id, push: true, ok: true,
        ...(validMessageId(result?.id) ? { message_id: result.id } : {}) });
    } catch (err) {
      send({ type: "delivery", id: frame.id, push: true, ok: false, ...diagnostic(err) });
    } finally {
      pushing.delete(frame.id);
    }
  }
  // 活跃主轮和收尾后的补充都只使用原消息额度；没有窗口时不退回主动消息。
  async function passivePush(frame) {
    if (!validMessageId(frame.id)) {warn("invalid_command");return;}
    const reject = kind => send({type:"delivery",id:frame.id,push:true,ok:false,failure_kind:kind});
    const item = passiveWindows.get(frame.based_on_message_id);
    if (!item) {reject("passive_window_missing");return;}
    const parent = item.parent;
    if ((frame.foreground === true) !== (item.foreground === true)) {reject("passive_window_missing");return;}
    if (frame.scope !== "c2c" || item.target.scope !== frame.scope || item.target.targetId !== frame.target_id || item.senderId !== frame.sender_id ||
        latestInput.get(routeKey(frame.scope,frame.target_id,frame.sender_id)) !== frame.based_on_message_id) {reject("stale_decision");return;}
    if (now() - item.receivedAt >= PASSIVE_REPLY_WINDOW_MS) {passiveWindows.delete(frame.based_on_message_id);reject("passive_window_elapsed");return;}
    if (parent && (pending.get(frame.based_on_message_id) !== parent || parent.finished || parent.next !== undefined)) {reject("stale_decision");return;}
    if (item.sending || parent?.sending || item.attempts.has(frame.id)) {warn("duplicate_reply");return;}
    // 活跃主轮保留最后一个被动名额；额度取父对象实时值，不能被重复开窗恢复。
    const remaining = parent ? passiveLimit(parent.target.scope) - 1 - (parent.passiveProgress ?? parent.progress ?? 0) : item.remaining;
    if (remaining <= 0) {reject("passive_quota_exhausted");return;}
    if (!validText(frame.text) || Buffer.byteLength(frame.text) > MAX_PUSH_BYTES) {reject("reply_limit");return;}
    item.attempts.add(frame.id);
    if (parent) {
      parent.passiveProgress = (parent.passiveProgress ?? parent.progress ?? 0) + 1;
      parent.sending = true;
    } else item.remaining--;
    // 失败或未知也占额度，桥接层不重发。
    item.sending = true;
    try {
      const guard = () => !closed && passiveWindows.get(frame.based_on_message_id) === item && now() - item.receivedAt < PASSIVE_REPLY_WINDOW_MS &&
        (!parent || (pending.get(frame.based_on_message_id) === parent && !parent.finished && parent.next === undefined)) &&
        latestInput.get(routeKey(frame.scope,frame.target_id,frame.sender_id)) === frame.based_on_message_id;
      const result = await sendText(item.target,frame.text,undefined,SEND_TIMEOUT_MS,guard);
      send({type:"delivery",id:frame.id,push:true,ok:true,delivery_mode:"passive",message_id:result.id});
    } catch (err) {send({type:"delivery",id:frame.id,push:true,ok:false,...diagnostic(err)});}
    finally {
      item.sending=false;
      if (parent) {
        parent.sending=false;
        if (parent.finished) forgetPending(frame.based_on_message_id);
      }
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
      if (frame.type === "close_foreground") {
        if (passiveWindows.get(frame.id)?.foreground === true) passiveWindows.delete(frame.id);
        return;
      }
      if (frame.type === "open_foreground") {
        if (!validMessageId(frame.id)) {warn("invalid_command");return;}
        const item = pending.get(frame.id);
        if (!item || item.target.scope !== "c2c" || item.finished || item.sending || item.next !== undefined ||
            latestInput.get(routeKey(item.target.scope,item.target.targetId,item.senderId)) !== frame.id) return;
        const remaining = passiveLimit(item.target.scope) - 1 - (item.passiveProgress ?? item.progress ?? 0);
        const remainingMs = Math.floor(PASSIVE_REPLY_WINDOW_MS - Math.max(0, now() - item.receivedAt));
        if (remaining <= 0 || remainingMs <= 0) return;
        const window = passiveWindows.get(frame.id);
        if (window && (window.parent !== item || window.foreground !== true)) return;
        for (const [id, saved] of passiveWindows) if (now() - saved.receivedAt >= PASSIVE_REPLY_WINDOW_MS) passiveWindows.delete(id);
        if (!window && passiveWindows.size >= limit) return;
        item.foregroundAttempts ??= new Set();
        if (!window) passiveWindows.set(frame.id,{...item,parent:item,attempts:item.foregroundAttempts,foreground:true});
        send({type:"foreground_window",id:frame.id,remaining_ms:remainingMs,remaining});
        return;
      }
      // 只恢复宿主持久接收的锚点；绝不覆盖本进程已经看见的新消息。
      if (frame.type === "restore_anchor") {
        if (!["c2c","group"].includes(frame.scope) || !validId(frame.target_id) || !validId(frame.sender_id) || !validMessageId(frame.message_id)) {warn("invalid_command");return;}
        const key=routeKey(frame.scope,frame.target_id,frame.group_chime === true ? "*" : frame.sender_id);
        if (!latestInput.has(key) && latestInput.size < 8192) latestInput.set(key,frame.message_id);
        return;
      }
      if (frame.type === "passive_push") { await passivePush(frame); return; }
      if (frame.type === "push") { await push(frame); return; }
      if (frame.type === "file") { await pushFile(frame); return; }
      if (frame.type === "finish") {
        if (passiveWindows.get(frame.id)?.parent) passiveWindows.delete(frame.id);
        const parentId = retiredLiveInputs.get(frame.id);
        if (parentId !== undefined) {
          if (frame.keep_live_reply !== true && liveReplies.get(parentId)?.anchor === frame.id) liveReplies.delete(parentId);
          return; // 已合并的输入不能靠重复 finish 恢复窗口、时间或额度。
        }
        clearLiveReplies(frame.id);
      }
      const item = pending.get(frame.id);
      if (!item) { warn("unknown_reply"); return; }
      if (frame.type === "finish") {
        if (frame.keep_live_reply === true) {
          const parent = validMessageId(frame.parent_id) && frame.parent_id !== frame.id ? pending.get(frame.parent_id) : undefined;
          if (parent && parent.target.scope === "group" && item.target.scope === "group" &&
              parent.target.targetId === item.target.targetId && parent.senderId === item.senderId &&
              !parent.finished && parent.next === undefined && !item.sending && !item.finished && item.next === undefined &&
              latestInput.get(routeKey(item.target.scope,item.target.targetId,item.senderId)) === frame.id &&
              now() - item.receivedAt < PASSIVE_REPLY_WINDOW_MS && (item.passiveProgress ?? item.progress ?? 0) < 3 &&
              (liveReplies.has(frame.parent_id) || liveReplies.size < limit) && retiredLiveInputs.size < 8192) {
            liveReplies.set(frame.parent_id,{anchor:frame.id,parent,item});
            retiredLiveInputs.set(frame.id,frame.parent_id);
            pending.delete(frame.id); // 转移同一对象，保留真实接收时间及已消费额度。
          } else if (!item.sending) forgetPending(frame.id);
          else item.finished = true;
          return;
        }
        // 已并入长工具轮次的新输入保留自己的原被动额度，不复用父轮或重置接收时刻。
        const foreground = frame.keep_foreground === true;
        if ((foreground || (frame.keep_passive === true && item.explicitTextSent === true)) && !item.sending && !item.finished && item.next === undefined && item.target.scope === "c2c") {
          const remaining = passiveLimit(item.target.scope) - (item.passiveProgress ?? item.progress ?? 0);
          const remainingMs = Math.floor(PASSIVE_REPLY_WINDOW_MS - Math.max(0, now() - item.receivedAt));
          if (remaining > 0 && remainingMs > 0 && latestInput.get(routeKey(item.target.scope,item.target.targetId,item.senderId)) === frame.id) {
            for (const [id, saved] of passiveWindows) if (now() - saved.receivedAt >= PASSIVE_REPLY_WINDOW_MS) passiveWindows.delete(id);
            if (passiveWindows.size < limit) {
              passiveWindows.set(frame.id,{...item,remaining,attempts:new Set(),foreground});
              send({type:foreground ? "foreground_window" : "passive_window",id:frame.id,remaining_ms:remainingMs,remaining});
            }
          }
        }
        if (!item.sending) forgetPending(frame.id);
        else item.finished = true;
        return;
      }
      if (frame.type === "capacity") {
        if (item.sending || item.next !== undefined) { warn("duplicate_reply"); return; }
        forgetPending(frame.id);
        send({ type: "delivery", id: frame.id, capacity_notice: true, ok: false, failure_kind: "receipt_capacity" });
        return;
      }
      if (frame.type === "segment") { await segment(frame, item); return; }
      if (frame.type === "progress") { await progress(frame, item); return; }
      if (frame.type === "sticker") { await sticker(frame, item); return; }
      if (frame.type !== "reply") { warn("invalid_command"); return; }
      if (item.sending || item.next !== undefined) { warn("duplicate_reply"); return; }
      // Text Rust accepted but QQ cannot carry (e.g. only U+FEFF) fails this reply
      // explicitly instead of leaving Rust waiting for a receipt that never comes.
      if (!validText(frame.text)) { reject(frame.id); return; }
      item.sending = true;
      let ok = false;
      try {
        const result = await finalText(item, frame.text);
        ok = result.ok;
        send({ type: "delivery", id: frame.id, ...result });
      } finally {
        item.sending = false;
        if (ok && frame.sticker_follows === true && !item.finished) item.awaitSticker = true;
        else forgetPending(frame.id);
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
