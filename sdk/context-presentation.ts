import type { Message } from "../contracts/model.ts";

const labels: Record<string, string> = {
  source: "来源",
  kind: "类型",
  record: "原件记录",
  at: "时间",
  data: "内容",
  items: "条目",
  event: "事件",
  status: "状态",
  state: "状态",
  actionId: "动作编号",
  jobId: "脚本编号",
  requestId: "请求编号",
  callId: "调用编号",
  conversationId: "会话",
  messages: "消息",
  sender: "发送者",
  platformUserId: "平台身份",
  displayName: "名称",
  relationship: "可信关系",
  body: "原始正文预览",
  displayBody: "可读正文预览",
  text: "文字",
  characters: "总字符数",
  truncated: "是否节选",
  nextChar: "后续字符位置",
  throughSeq: "水位",
  output: "工具输出",
  result: "结果",
  error: "错误",
  reason: "原因",
  blockedBy: "阻塞动作",
  attemptCount: "真实尝试次数",
  resolution: "结束方式",
  complete: "全部完成",
  applied: "已持久生效",
  before: "整理前用量",
  after: "整理后用量",
  compressionSeq: "压缩记录",
  safeRemaining: "预算安全剩余",
  wakeAt: "唤醒时间",
  phase: "阶段",
  operation: "操作",
  outcome: "最终结果",
  commit: "源码版本",
  aboutMe: "关于我",
  ideas: "浮现的原话",
  alias: "短名",
  remark: "备注",
  platformName: "平台名称",
  reference: "引用",
  mentions: "艾特身份",
  resolved: "已解析名称",
  delivery: "发送回执",
  code: "代码",
  queue: "等待队列",
  encoding: "计数或编码方式",
  tokens: "token数",
  revision: "版本",
  noteId: "来源便签",
  sourceSeq: "来源记录",
  hasMore: "还有后续",
  notice: "说明",
  executionMs: "工具耗时毫秒",
  fingerprint: "原参数指纹",
  receiptDigest: "回执摘要",
  canCancel: "可取消等待",
  attempted: "已尝试提交",
  httpStatus: "HTTP状态",
  category: "错误分类",
  pendingNotifications: "通知条数",
  notificationThroughSeq: "通知水位",
  displayedSeqs: "本屏预览编号",
  earlierMessagesNotShown: "上方未展示消息数",
  attachments: "附件",
  handle: "消息位置",
  name: "名称",
  direction: "方向",
  path: "路径",
  sentAt: "平台时间",
  time: "时间信息",
};
/** Stable plain-text projection. All fields survive; raw facts remain in their journals. */
export function readable(value: unknown, depth = 0): string {
  if (value === null) return "null";
  if (typeof value !== "object") return typeof value === "string" ? value : String(value);
  if (depth >= 16) return JSON.stringify(value);
  const indent = "  ".repeat(Math.min(depth, 16));
  if (Array.isArray(value)) {
    return value.length
      ? value.map((v, i) => `${indent}[${i + 1}] ${readable(v, depth + 1)}`).join("\n")
      : "（空）";
  }
  return Object.entries(value as Record<string, unknown>).map(([key, child]) => {
    // Script output is JSON text in the transport, but should be read as its returned value.
    if ((key === "output" || key === "result") && typeof child === "string") {
      try {
        child = JSON.parse(child);
      } catch { /* Arbitrary text stays verbatim. */ }
    }
    return `${indent}${labels[key] ?? key}：${readable(child, depth + 1)}`;
  }).join("\n");
}
export function presentContext(
  messages: Message[],
  rawCalls: ReadonlySet<string> = new Set(),
  records: ReadonlyMap<string, number> = new Map(),
): Message[] {
  return messages.map((message) => {
    if (
      message.role !== "tool" && message.role !== "user" ||
      message.toolCallId && rawCalls.has(message.toolCallId)
    ) return message;
    let data: unknown;
    try {
      data = JSON.parse(message.content);
    } catch {
      return message;
    }
    const envelope = data as Record<string, unknown> | null;
    if (
      message.role === "user" &&
      (!envelope || envelope.source !== "host" || !Number.isSafeInteger(envelope.record))
    ) return message;
    const record = message.toolCallId
      ? records.get(message.toolCallId)
      : envelope && typeof envelope.record === "number"
      ? envelope.record
      : undefined;
    return {
      ...message,
      content: (message.role === "tool" ? "工具回执（仅表示所列真实状态）\n" : "主机通知\n") +
        readable(data) + (record
          ? `\n完整原件：tools.records.text({seq:${record},startChar:0,limit:8192})，按nextChar继续。`
          : ""),
    };
  });
}
