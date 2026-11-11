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
  /** 资源定位信息留在适配器，屏幕仅返回本消息中的附件序号。 */
  attachments?: PhoneAttachment[];
  /** 出站动作的真实状态；看见正文不等于平台已送达。 */
  delivery?: PhoneActionReceipt;
}

export interface PhoneAttachment {
  kind: "image" | "file" | "audio" | "video" | "other";
  name?: string;
  sizeBytes?: number;
  durationMs?: number;
}

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
  attachments: {
    index: number;
    kind: PhoneAttachment["kind"];
    name?: TextPreview;
    sizeBytes?: number;
    durationMs?: number;
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
  state: "running" | "sent" | "failed" | "unknown";
  attempted?: boolean;
  platformMessageId?: string;
  reason?: "interrupted";
  error?: { category: string; httpStatus?: number; code?: number; traceId?: string };
}

export interface PhoneSend {
  actionId: string;
  conversationId: string;
  text: string;
  /** 被动回复明确指定本会话入站编号和固定发送序号，不自动猜最近一条。 */
  replyTo?: { seq: number; sequence: number };
  /** 平台已提供的引用索引；不是平台消息编号。 */
  referenceIndex?: string;
  wakeup?: boolean;
}

/** 脚本可用的手机接口。只有 open 更新本机会话通知水位，历史/搜索只浏览。 */
export interface PhoneApi {
  conversations(args?: {
    cursor?: { snapshotThroughSeq: number; beforeLastSeq: number };
  }): Promise<{
    items: {
      conversationId: string;
      kind: "user" | "group";
      latest: MessageScreen;
      pendingNotifications: number;
      reading: PhoneReadingState | null;
    }[];
    snapshotThroughSeq: number;
    older?: { snapshotThroughSeq: number; beforeLastSeq: number };
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
  action(args: { actionId: string }): Promise<PhoneActionReceipt | null>;
}
