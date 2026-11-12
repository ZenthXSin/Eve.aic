import type { ToolSpec } from "../../contracts/model.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { ConsciousEvent } from "./journal.ts";
import {
  activeEvents,
  blocks,
  type ContextCounter,
  contextEstimate,
  estimate,
  render,
} from "./view.ts";

export const CONTEXT_TOOLS: ToolSpec[] = [
  {
    name: "inspect_context",
    description:
      "查看当前上下文用量估算、最近一次上游真实用量，以及可直接用于compress_context的完整消息/工具回执边界。分页用nextAfterSeq；不写入压缩。候选removedMessageTokens为移出的消息估算，未扣除新摘要。",
    parameters: {
      type: "object",
      properties: {
        afterSeq: { type: "integer", minimum: 0 },
        limit: { type: "integer", minimum: 1, maximum: 100 },
      },
      additionalProperties: false,
    },
  },
  {
    name: "compress_context",
    description:
      "按inspect_context给出的throughSeq和自己写的summary立即压缩上下文。summary需承接旧摘要、重要事实和未完成事项。返回已生效的持久记录和前后用量估算，不是入队通知；原件保留。当前未完成工具批次不能移出。真实压后inputTokens要等下一次模型请求。",
    parameters: {
      type: "object",
      properties: {
        throughSeq: { type: "integer", minimum: 1 },
        summary: { type: "string", minLength: 1, maxLength: 100000 },
        dryRun: { type: "boolean" },
      },
      required: ["throughSeq", "summary"],
      additionalProperties: false,
    },
  },
];

export function contextArgs(args: Json, keys: string[]): Record<string, Json> {
  if (
    !args || typeof args !== "object" || Array.isArray(args) ||
    Object.keys(args).some((key) => !keys.includes(key))
  ) {
    throw new KernelError("rejected", "上下文工具参数无效");
  }
  return args;
}
export function compressionThrough(events: ConsciousEvent[]): number {
  let through = 0;
  for (const event of activeEvents(events)) {
    if (event.kind === "compression") {
      through = Math.max(through, (event.data as { throughSeq: number }).throughSeq);
    }
  }
  return through;
}
export function contextUsage(
  events: ConsciousEvent[],
  prompt: string,
  tools: ToolSpec[],
  counter?: ContextCounter,
) {
  const latest = activeEvents(events).findLast((event) =>
    event.kind === "model_response" &&
    !!(event.data as { response?: { usage?: Json } }).response?.usage
  );
  return {
    estimatedTokens: contextEstimate(render(events, prompt), tools, counter),
    measurement: counter?.description ?? "本地估算，包含身份、消息及工具定义；非上游精确token",
    compressedThroughSeq: compressionThrough(events),
    targetTokens: 150000,
    latestProviderUsage: latest
      ? {
        responseSeq: latest.seq,
        usage: (latest.data as { response: { usage: Json } }).response.usage,
        scope: "该历史模型请求的实测用量，不代表当前或压缩后的输入",
      }
      : null,
  };
}
export function inspectContext(
  events: ConsciousEvent[],
  prompt: string,
  tools: ToolSpec[],
  args: Json,
  counter?: ContextCounter,
) {
  const value = contextArgs(args, ["afterSeq", "limit"]);
  const after = value.afterSeq ?? 0, limit = value.limit ?? 50;
  if (
    !Number.isSafeInteger(after) || Number(after) < 0 ||
    !Number.isSafeInteger(limit) || Number(limit) < 1 || Number(limit) > 100
  ) {
    throw new KernelError("rejected", "afterSeq须为非负整数，limit须为1至100");
  }
  const usage = contextUsage(events, prompt, tools, counter);
  const complete = blocks(events, usage.compressedThroughSeq);
  let removed = 0;
  const all = complete.map((block) => {
    removed += estimate(block.messages);
    return { throughSeq: block.throughSeq, removedMessageTokens: removed };
  });
  const remaining = all.filter((block) => block.throughSeq > Number(after));
  const candidates = remaining.slice(0, Number(limit));
  return {
    ...usage,
    candidates,
    totalCandidates: all.length,
    hasMore: remaining.length > candidates.length,
    nextAfterSeq: remaining.length > candidates.length ? candidates.at(-1)!.throughSeq : null,
    preview: "compress_context({throughSeq,summary,dryRun:true})返回包含实际摘要的前后估算",
  };
}
export function previewCompression(
  events: ConsciousEvent[],
  prompt: string,
  tools: ToolSpec[],
  args: Json,
  counter?: ContextCounter,
) {
  const value = contextArgs(args, ["throughSeq", "summary", "dryRun"]);
  if (
    !Number.isSafeInteger(value.throughSeq) || typeof value.summary !== "string" ||
    !value.summary.trim() || value.summary.length > 100000 ||
    (value.dryRun !== undefined && typeof value.dryRun !== "boolean")
  ) {
    throw new KernelError(
      "rejected",
      "需要整数throughSeq、非空summary（最多100000字符）及可选dryRun布尔值",
    );
  }
  const throughSeq = Number(value.throughSeq);
  if (
    throughSeq <= compressionThrough(events) ||
    !blocks(events).some((block) => block.throughSeq === throughSeq)
  ) {
    throw new KernelError(
      "rejected",
      "指定位置不是尚未压缩的完整消息/工具回执边界；请用inspect_context查询",
    );
  }
  const before = contextUsage(events, prompt, tools, counter);
  const last = events.at(-1);
  const preview: ConsciousEvent = {
    schema: 1,
    seq: (last?.seq ?? 0) + 1,
    at: new Date().toISOString(),
    kind: "compression",
    previous: last?.digest ?? "0".repeat(64),
    digest: "preview",
    data: { throughSeq, summary: value.summary, source: "eve" },
  };
  const after = contextUsage([...events, preview], prompt, tools, counter);
  return {
    throughSeq,
    summary: value.summary,
    dryRun: value.dryRun === true,
    before,
    after,
    estimatedTokensSaved: before.estimatedTokens - after.estimatedTokens,
    scope:
      "同一投影视图下估算，含工具定义；不含本次工具回执及后续新增消息。压后真实用量待下一次模型请求。原件保留。",
  };
}
