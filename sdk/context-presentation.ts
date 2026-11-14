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
    return `${indent}${labels[key] ?? key}：${readable(child, depth + 1)}`;
  }).join("\n");
}
type ObjectValue = Record<string, unknown>;
function object(value: unknown): value is ObjectValue {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
const jsonOutputs = new Set([
  "execute_script",
  "read_script",
  "write_thought",
  "inspect_context",
  "compress_context",
  "set_focus",
  "rest",
]);
/** Decode only the documented script result page, never arbitrary body/text/output fields. */
function resultPage(value: unknown): unknown {
  if (
    !object(value) || typeof value.text !== "string" ||
    typeof value.characters !== "number" || typeof value.truncated !== "boolean"
  ) return value;
  if (value.truncated || value.nextChar !== undefined) return value;
  try {
    const { text, ...page } = value;
    return { ...page, encoding: "json", value: JSON.parse(text as string) };
  } catch {
    return value;
  }
}
/** Shared presentation for model context and admin previews. Never mutates stored facts. */
export function structuredToolReceipt(data: unknown, tool?: string) {
  let receipt = data;
  if (object(data) && typeof data.output === "string" && tool && jsonOutputs.has(tool)) {
    try {
      receipt = { ...data, output: JSON.parse(data.output) };
    } catch { /* Failed validation and ordinary text remain verbatim. */ }
  }
  const output = object(receipt) ? receipt.output : undefined;
  const script = (tool === "execute_script" || tool === "read_script") &&
      object(output) && typeof output.jobId === "string" && typeof output.status === "string"
    ? output
    : undefined;
  if (script) {
    receipt = {
      ...receipt as ObjectValue,
      output: {
        ...script,
        ...(script.result === undefined ? {} : {
          result: Number(script.resultStartChar ?? 0) === 0
            ? resultPage(script.result)
            : script.result,
        }),
      },
    };
  }
  return { receipt, script };
}
/** Model-only projection; journals and RPC recovery retain the original wire receipts. */
export function presentContext(
  messages: Message[],
  rawCalls: ReadonlySet<string> = new Set(),
  records: ReadonlyMap<string, number> = new Map(),
): Message[] {
  const names = new Map<string, string>();
  for (const message of messages) {
    for (const call of message.toolCalls ?? []) names.set(call.id, call.name);
  }
  return messages.map((message) => {
    if (
      message.role !== "tool" && message.role !== "user" ||
      message.toolCallId && rawCalls.has(message.toolCallId)
    ) return message;
    let data: unknown;
    try {
      data = JSON.parse(message.content);
    } catch {
      if (message.role !== "tool") return message;
      data = message.content;
    }
    if (
      message.role === "user" &&
      (!object(data) || data.source !== "host" || !Number.isSafeInteger(data.record))
    ) return message;
    const record = message.toolCallId
      ? records.get(message.toolCallId)
      : object(data) && typeof data.record === "number"
      ? data.record
      : undefined;
    const reference = record === undefined ? {} : {
      original: {
        method: "tools.records.text",
        args: { seq: record, startChar: 0, limit: 8192 },
        pagination: "nextChar",
      },
    };
    if (message.role === "user") {
      return {
        ...message,
        content: JSON.stringify({ schema: "eve.host_notice.v1", data, ...reference }),
      };
    }
    const tool = message.toolCallId ? names.get(message.toolCallId) : undefined;
    const { receipt, script } = structuredToolReceipt(data, tool);
    return {
      ...message,
      content: JSON.stringify({
        schema: "eve.tool_receipt.v1",
        ...(message.toolCallId ? { callId: message.toolCallId } : {}),
        ...(tool ? { tool } : {}),
        callStatus: object(data) && typeof data.status === "string" ? data.status : "unreported",
        ...(script
          ? {
            task: {
              jobId: script.jobId,
              status: script.status,
              ...(script.status === "completed" ? {} : {
                inspect: {
                  tool: "read_script",
                  args: { jobId: script.jobId },
                },
              }),
              ...(object(script.result) && typeof script.result.nextChar === "number"
                ? {
                  continuation: {
                    tool: "read_script",
                    args: { jobId: script.jobId, startChar: script.result.nextChar, limit: 8192 },
                  },
                }
                : {}),
            },
          }
          : {}),
        receipt,
        ...reference,
      }),
    };
  });
}
