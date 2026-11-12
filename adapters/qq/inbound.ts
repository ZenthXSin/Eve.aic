// 官方事件到平台事实的投影。原始事件（含平台私有扩展）由适配器日志保留，不送给模型。
import type { PhoneAttachment, PhoneMessage } from "../../contracts/phone.ts";

export interface QqDispatch {
  op: 0;
  s: number;
  t: string;
  id?: string;
  d: unknown;
}

export interface InboundQqMessage {
  /** 一条平台消息的身份；网关补发序号不参与，避免重连制造新的触发。 */
  deduplication: {
    conversationId: string;
    messageId: string;
    messageIndex?: string;
    /** 没有 msg_idx 的旧推送按官方 msg_seq 区分，不采用网关 s。 */
    messageSequence?: number;
  };
  /** source type 明确区分 C2C 和群聊，发送出口不能从昵称推断目的地。 */
  destination: { kind: "user" | "group"; openId: string };
  message: Omit<PhoneMessage, "seq">;
  referenceIndex?: string;
  /** 不能证明 @ 全体或正文里提及名字是在 @ Eve。 */
  notify: boolean;
}

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function identifier(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= 512;
}

function legacyExtension(data: Record<string, unknown>, name: string): string | undefined {
  const scene = data.message_scene;
  if (scene === undefined) return undefined;
  if (!object(scene) || !Array.isArray(scene.ext)) throw new TypeError("QQ 消息场景结构不合法");
  const found = scene.ext.filter((v) => typeof v === "string" && v.startsWith(`${name}=`));
  if (found.length === 0) return undefined;
  if (found.length !== 1) throw new TypeError("QQ 消息索引重复");
  const value = found[0].slice(name.length + 1);
  if (!identifier(value)) throw new TypeError("QQ 消息索引不合法");
  return value;
}

/** v1 必须保持原投影，旧原件不能因适配器升级而被判为损坏。 */
export type QqProjectionVersion = 1 | 2;

function extension(data: Record<string, unknown>, name: "msg_idx" | "ref_msg_idx") {
  const scene = data.message_scene;
  if (scene === undefined) return undefined;
  if (!object(scene) || (scene.ext !== undefined && !Array.isArray(scene.ext))) {
    throw new TypeError("QQ 消息场景结构不合法");
  }
  const prefixes = name === "msg_idx" ? ["msg_idx=", "msgIdx:"] : ["ref_msg_idx=", "refMsgIdx:"];
  const values: string[] = [];
  for (const item of (scene.ext as unknown[] | undefined) ?? []) {
    if (typeof item !== "string") continue;
    const prefix = prefixes.find((prefix) => item.startsWith(prefix));
    if (prefix === undefined) continue;
    const value = item.slice(prefix.length).trim();
    if (!identifier(value)) throw new TypeError("QQ 消息索引不合法");
    values.push(value);
  }
  // 两种官方前缀同时给出相同值可以合并，不接受相互矛盾的身份。
  if (new Set(values).size > 1) throw new TypeError("QQ 消息索引重复");
  return values[0];
}

function quoteIndex(data: Record<string, unknown>): string | undefined {
  const type = data.message_type ?? data.msg_type;
  if (
    data.message_type !== undefined && data.msg_type !== undefined &&
    data.message_type !== data.msg_type
  ) throw new TypeError("QQ 消息类型相互矛盾");
  if (type !== 103) return undefined;
  if (data.msg_elements === undefined) return undefined;
  if (!Array.isArray(data.msg_elements)) throw new TypeError("QQ 引用元素结构不合法");
  const first = data.msg_elements[0];
  if (first === undefined) return undefined;
  if (!object(first)) throw new TypeError("QQ 引用元素结构不合法");
  if (first.msg_idx === undefined) return undefined;
  if (!identifier(first.msg_idx)) throw new TypeError("QQ 引用索引不合法");
  return first.msg_idx;
}

function attachments(raw: unknown, version: QqProjectionVersion): PhoneAttachment[] | undefined {
  if (raw === undefined) return undefined;
  if (!Array.isArray(raw)) throw new TypeError("QQ 附件结构不合法");
  return raw.map((item) => {
    if (!object(item)) throw new TypeError("QQ 附件结构不合法");
    const mime = typeof item.content_type === "string" ? item.content_type : "";
    const kind = mime.startsWith("image/")
      ? "image"
      : mime === "voice" || mime.startsWith("audio/")
      ? "audio"
      : mime.startsWith("video/")
      ? "video"
      : mime === "file" ||
          (version === 2 && (mime.startsWith("application/") || mime.startsWith("text/")))
      ? "file"
      : "other";
    if (item.filename !== undefined && typeof item.filename !== "string") {
      throw new TypeError("QQ 附件名称不合法");
    }
    if (
      item.size !== undefined && (!Number.isSafeInteger(item.size) || (item.size as number) < 0)
    ) {
      throw new TypeError("QQ 附件大小不合法");
    }
    return {
      kind,
      ...(item.filename === undefined ? {} : { name: item.filename as string }),
      ...(item.size === undefined ? {} : { sizeBytes: item.size as number }),
    };
  });
}

/** 未支持事件返回 undefined；支持事件若损坏则报错，不能确认网关消费成功。 */
export function qqMessage(
  dispatch: QqDispatch,
  version: QqProjectionVersion = 2,
): InboundQqMessage | undefined {
  if (
    !["C2C_MESSAGE_CREATE", "GROUP_AT_MESSAGE_CREATE", "GROUP_MESSAGE_CREATE"].includes(dispatch.t)
  ) {
    return undefined;
  }
  const data = dispatch.d;
  if (
    !object(data) || !identifier(data.id) || !object(data.author) ||
    typeof data.content !== "string"
  ) {
    throw new TypeError("QQ 消息事件不完整");
  }
  const privateMessage = dispatch.t === "C2C_MESSAGE_CREATE";
  const userId = privateMessage ? data.author.user_openid : data.author.member_openid;
  const destinationId = privateMessage ? userId : data.group_openid;
  if (!identifier(userId) || !identifier(destinationId)) {
    throw new TypeError("QQ 平台用户或会话标识缺失");
  }
  if (data.author.username !== undefined && typeof data.author.username !== "string") {
    throw new TypeError("QQ 平台昵称类型不合法");
  }
  const kind = privateMessage ? "user" : "group";
  const conversationId = `qq:${kind}:${destinationId}`;
  const messageIndex = version === 1
    ? legacyExtension(data, "msg_idx")
    : extension(data, "msg_idx");
  const referenceIndex = version === 1
    ? legacyExtension(data, "ref_msg_idx")
    : quoteIndex(data) ?? extension(data, "ref_msg_idx");
  if (
    data.msg_seq !== undefined &&
    (!Number.isSafeInteger(data.msg_seq) || (data.msg_seq as number) < 0)
  ) {
    throw new TypeError("QQ 消息序号不合法");
  }
  const cards = attachments(data.attachments, version);
  return {
    deduplication: {
      conversationId,
      messageId: data.id,
      ...(messageIndex === undefined ? {} : { messageIndex }),
      ...(messageIndex === undefined && data.msg_seq !== undefined
        ? { messageSequence: data.msg_seq as number }
        : {}),
    },
    destination: { kind, openId: destinationId },
    message: {
      conversationId,
      platformMessageId: data.id,
      sender: {
        platformUserId: userId,
        displayName: (data.author.username as string | undefined) ?? "",
      },
      direction: "incoming",
      // 时间格式的真实性由屏幕标明；缺失不补当前时间。
      ...(typeof data.timestamp === "string" ? { sentAt: data.timestamp } : {}),
      text: data.content,
      ...(cards === undefined ? {} : { attachments: cards }),
    },
    ...(referenceIndex === undefined ? {} : { referenceIndex }),
    notify: privateMessage || dispatch.t === "GROUP_AT_MESSAGE_CREATE",
  };
}
