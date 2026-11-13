// 平台事实与手机屏幕契约。阅读状态另存于独立适配器，不随手机 worker 换版回滚。
// 此处不认证调用方，也不从昵称、正文、引用或附件推断主人身份。

export interface PhoneMessage {
  /** 适配器日志内单调递增的编号；不等于平台消息编号。 */
  seq: number;
  conversationId: string;
  platformMessageId?: string;
  sender: { platformUserId: string; displayName: string };
  direction: "incoming" | "outgoing";
  /** 平台原始时间；缺失或非法时不能伪装成刚刚。 */
  sentAt?: string;
  text: string;
  quote?: { platformMessageId: string };
  /** 本消息的平台引用索引；仅用于 referenceIndex，不等于 platformMessageId。 */
  referenceIndex?: string;
  /** 平台明确带有引用；target 缺失表示本机无法唯一定位，不能猜测对象或内容。 */
  reference?: PhoneReference;
  /** 资源定位信息留在适配器，屏幕仅返回本消息中的附件序号。 */
  attachments?: PhoneAttachment[];
  /** 出站动作的真实状态；看见正文不等于平台已送达。 */
  delivery?: PhoneActionReceipt;
}

export interface PhoneReference {
  index: string;
  /** 仅在同会话已有事实唯一匹配时提供，可用 phone.message 点开。 */
  target?: { conversationId: string; seq: number };
}

export interface PhoneAttachment {
  kind: "image" | "file" | "audio" | "video" | "other";
  name?: string;
  sizeBytes?: number;
  durationMs?: number;
  /** 适配器投影的临时文件状态；远端资源 URL 不进入屏幕。 */
  download?: AttachmentDownload;
}

export type AttachmentDownload =
  | { state: "ready"; path: string }
  | { state: "pending" | "failed" | "cleared" };

export interface TextPreview {
  text: string;
  /** Unicode 码点数，不按 UTF-16 半个代理项截断。 */
  characters: number;
  truncated: boolean;
}

export interface MessageScreen {
  handle: { conversationId: string; seq: number };
  platformMessageId?: string;
  sender: {
    platformUserId: string;
    displayName: TextPreview;
    /** 仅由手机插件使用可信配置映射，适配器不认定主人。 */
    relationship?: "owner" | "other" | "self";
  };
  direction: "incoming" | "outgoing";
  time: {
    status: "known" | "missing" | "invalid" | "future";
    at?: string;
    age?: string;
  };
  body: TextPreview;
  quote?: { platformMessageId: string };
  /** 本消息的平台引用索引；仅用于 referenceIndex，不等于 platformMessageId。 */
  referenceIndex?: string;
  /** 平台明确带有引用；target 缺失表示本机无法唯一定位，不能猜测对象或内容。 */
  reference?: PhoneReference;
  attachments: {
    index: number;
    kind: PhoneAttachment["kind"];
    name?: TextPreview;
    sizeBytes?: number;
    durationMs?: number;
    download?: AttachmentDownload;
  }[];
  attachmentCount: number;
  delivery?: PhoneActionReceipt;
}

/** 由手机实现提供显示策略；这里没有已读或唤醒策略。 */
export interface ScreenPolicy {
  pageSize: number;
  previewCharacters: number;
  attachmentPageSize: number;
}

export interface HistoryCursor {
  /** 后续页使用同一日志快照水位，后来新到的消息不会挤动历史页。 */
  snapshotThroughSeq: number;
  beforeSeq: number;
}

export interface HistoryScreen {
  messages: MessageScreen[];
  snapshotThroughSeq: number;
  /** 最高已展示编号；不证明此前所有消息都已看过，不自动确认已读。空页无此字段。 */
  readThroughSeq?: number;
  older?: HistoryCursor;
}

/** 本机通知水位与最近一次打开的屏幕。水位不代表此前所有消息都实际看过。 */
export interface PhoneReadingState {
  notificationThroughSeq: number;
  lastOpened: { snapshotThroughSeq: number; displayedSeqs: number[] };
}
export interface OpenedConversation extends HistoryScreen {
  notificationThroughSeq: number;
  /** 本次仅展示这些消息的预览，不表示长文或附件已完整读取。 */
  displayedSeqs: number[];
  /** 在本次屏幕上方的消息条数，不是累计未读条数。 */
  earlierMessagesNotShown: number;
  notice: string;
}

export const PHONE = "phone";

export interface PhoneActionReceipt {
  actionId: string;
  state: "running" | "blocked" | "sent" | "failed" | "unknown" | "cancelled";
  /** 逻辑结束保留原始失败/未尝试记录；绝不表示已发送。 */
  resolution?: {
    kind: "failed_retained" | "unknown_retained" | "queued_cancelled";
    actor: "admin" | "eve";
    at: string;
  };
  /** 同会话未成功的队首动作；不得用新动作越过。 */
  blockedBy?: string;
  attempted?: boolean;
  /** 仅多次真实尝试时提供；仅在确认尚未提交或明确拒绝时自动重试原主动消息。 */
  attemptCount?: number;
  platformMessageId?: string;
  reason?: "interrupted";
  error?: {
    category: string;
    httpStatus?: number;
    code?: number;
    traceId?: string;
    /** 受控传输诊断，不含资源地址、凭据或原始异常文本。 */
    transportReason?: "timeout" | "aborted" | "network";
    /** 包括建连/等待响应头，或读取响应正文；不证明请求是否已经送达。 */
    phase?: "headers" | "body";
    elapsedMs?: number;
    retryAfterMs?: number;
    failureStage?: "auth" | "upload" | "message";
  };
}

/** QQ 文字统一主动发送；引用展示不改变发送模式。 */
export interface PhoneSend {
  actionId: string;
  conversationId: string;
  text: string;
  /** 平台已提供的引用索引；不是平台消息编号。 */
  referenceIndex?: string;
  wakeup?: boolean;
}

/** 显式一次发一至四条文字；不自动拆分代码、附件或普通send。 */
export interface PhoneSendBurst {
  actionId: string;
  conversationId: string;
  messages: string[];
  /** 显式选择按约30字切分；总共超过四条会拒绝，绝不截断。默认false。 */
  splitLongText?: boolean;
  /** 新脚本继续原父请求：仅允许已存在父编号且全部原参数一致；不会重发已结束子条。 */
  resumeActionId?: string;
}
export interface PhoneBurstReceipt {
  actionId: string;
  conversationId: string;
  /** 每条均为独立真实动作；unknown/blocked可用该子编号检查和取消。 */
  items: { index: number; actionId: string; receipt: PhoneActionReceipt }[];
  /** 只有所有子消息实际sent才为true；取消不算送达。 */
  complete: boolean;
}
/** 只读父请求；子receipt为null代表尚未准入，没有虚构发送状态。 */
export interface PhoneBurstStatus {
  actionId: string;
  conversationId: string;
  complete: boolean;
  items: { index: number; actionId: string; receipt: PhoneActionReceipt | null }[];
}
export interface PhoneWaitForMessages {
  conversationId: string;
  purpose: "reply";
  /** 默认10秒；新真实入站延长本会话静默窗口。范围1..60000。 */
  quietMs?: number;
  /** 默认60秒；有界等待，范围1..120000。到期由Eve决定下一步。 */
  maxWaitMs?: number;
}
export interface PhoneMessagesWaitResult {
  conversationId: string;
  reason: "quiet" | "max_wait" | "interrupted";
  waitedMs: number;
  /** 当前真实手机时间线水位；应重新打开/读取会话，不代表自动确认已读。 */
  throughSeq: number;
  /** 本会话最近真实入站的手机编号；没有入站时省略。 */
  lastIncomingSeq?: number;
}

export interface PhoneConversationSettings {
  muted: boolean;
  pinned: boolean;
  /** 仅存在本机，不自动发送。空串表示没有草稿。 */
  draft: string;
}
export interface PhoneDownload {
  path: string;
  sizeBytes: number;
  sha256: string;
  mimeType: string;
  source: {
    kind: "phone_attachment";
    conversationId: string;
    sourceSeq: number;
    attachmentId: string;
    senderPlatformUserId: string;
  };
}
/** QQ 附件统一主动发送。 */
export interface PhoneSendFile {
  actionId: string;
  conversationId: string;
  path: string;
  kind: "image" | "file";
  text?: string;
}

/** 两种看图方式由 Eve 明确选择；默认描述，任何失败都不自动切换。 */
export type PhoneImage = {
  selectedMode: "describe";
  description: string;
  provenance: import("./model.ts").ModelImage["provenance"];
  download: PhoneDownload;
} | {
  selectedMode: "original";
  image: import("./model.ts").ModelImage;
  provenance: import("./model.ts").ModelImage["provenance"];
  download: PhoneDownload;
};

/** 脚本可用的手机接口。只有 open 更新本机会话通知水位，历史/搜索只浏览。 */
export interface PhoneApi {
  conversations(args?: {
    cursor?: { snapshotThroughSeq: number; beforeLastSeq: number; beforePinned?: boolean };
  }): Promise<{
    items: {
      conversationId: string;
      kind: "user" | "group";
      latest: MessageScreen;
      pendingNotifications: number;
      reading: PhoneReadingState | null;
      settings: { muted: boolean; pinned: boolean; hasDraft: boolean };
    }[];
    snapshotThroughSeq: number;
    older?: { snapshotThroughSeq: number; beforeLastSeq: number; beforePinned?: boolean };
  }>;
  /** 打开最新一屏，先持久保存本会话通知水位；上方未展示内容不算实际读过。 */
  open(args: { conversationId: string }): Promise<OpenedConversation>;
  history(args: { conversationId: string; cursor?: HistoryCursor }): Promise<HistoryScreen>;
  message(args: {
    handle: { conversationId: string; seq: number };
    attachmentOffset?: number;
  }): Promise<MessageScreen>;
  text(args: {
    handle: { conversationId: string; seq: number };
    startChar: number;
    limit: number;
  }): Promise<TextPreview & { startChar: number; nextChar?: number }>;
  search(args: {
    query: string;
    conversationId?: string;
    cursor?: HistoryCursor;
  }): Promise<HistoryScreen>;
  notifications(args?: { afterSeq?: number }): Promise<{
    items: MessageScreen[];
    throughSeq: number;
    hasMore: boolean;
  }>;
  send(args: PhoneSend): Promise<PhoneActionReceipt>;
  sendBurst(args: PhoneSendBurst): Promise<PhoneBurstReceipt>;
  burstStatus(args: { actionId: string }): Promise<PhoneBurstStatus | null>;
  waitForMessages(args: PhoneWaitForMessages): Promise<PhoneMessagesWaitResult>;
  action(args: { actionId: string }): Promise<PhoneActionReceipt | null>;
  /** 队列分页不发送任何消息。发送中的请求不可取消；只有canCancel且有receiptDigest可提交取消。 */
  listSendQueue(
    args?: { conversationId?: string; afterSequence?: number; limit?: number },
  ): Promise<{
    items: {
      actionId: string;
      fingerprint: string;
      receiptDigest?: string;
      sequence: number;
      conversationId: string;
      receipt: PhoneActionReceipt;
      canCancel: boolean;
      cancelBlockedReason?: "running" | "unknown" | "not_waiting";
    }[];
    next?: { conversationId?: string; afterSequence: number; limit?: number };
  }>;
  /** 取消等待并保留原件：允许失败/送达未知的队首及零尝试blocked项。不会撤回已可能送达的消息；陈旧回执拒绝。 */
  cancelSend(
    args: { actionId: string; expectedFingerprint: string; expectedReceiptDigest: string },
  ): Promise<PhoneActionReceipt>;
  /** 只改变明确提供的字段；省略所有字段时只读取。本机设置随重启保留。 */
  settings(args: {
    conversationId: string;
    muted?: boolean;
    pinned?: boolean;
    draft?: string;
  }): Promise<PhoneConversationSettings>;
  /** 显式点开才下载，返回原始来源与校验值；不会自动读入上下文。 */
  download(args: {
    handle: { conversationId: string; seq: number };
    /** 使用屏幕显示的附件index，从1开始。 */
    index: number;
  }): Promise<PhoneDownload>;
  /** 默认由相同模型客观描述；mode:original 明确选择直接看原图。两路费用均计入共享预算。 */
  image(args: {
    handle: { conversationId: string; seq: number };
    /** 使用屏幕显示的附件index，从1开始。 */
    index: number;
    mode?: "describe" | "original";
  }): Promise<PhoneImage>;
  sendFile(args: PhoneSendFile): Promise<PhoneActionReceipt>;
  /** 手机内的附件分页读取，无需把下载目录开放给普通工作空间。 */
  file(args: {
    handle: { conversationId: string; seq: number };
    /** 使用屏幕显示的附件index，从1开始。 */
    index: number;
    offsetBytes?: number;
    limitBytes?: number;
    encoding?: "utf8" | "base64";
  }): Promise<
    { download: PhoneDownload; offsetBytes: number; text: string; nextOffsetBytes?: number }
  >;
}
