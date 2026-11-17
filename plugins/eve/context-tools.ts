import type { Message, ToolSpec } from "../../contracts/model.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { ConsciousEvent } from "./journal.ts";
import {
  CONTEXT_RETAIN_TARGET_TOKENS,
  CONTEXT_SUMMARY_RESERVE_TOKENS,
  CONTEXT_TARGET_TOKENS,
} from "./context-maintenance.ts";
import {
  appendPreviewEvent,
  blocks,
  type ContextCounter,
  contextEstimate,
  estimate,
  lastActiveEvent,
  render,
  selectedEvents,
} from "./view.ts";

export const CONTEXT_TOOLS: ToolSpec[] = [
  {
    name: "inspect_context",
    description:
      "查看当前上下文用量估算、最近一次上游真实用量，以及可直接用于compress_context的完整消息/工具回执边界。recommendation给出保留最近片段、目标约100k的建议边界；recommendedNow为false时保留稳定缓存，不因查询到候选就压缩，通常270k再整理。分页用nextAfterSeq；不写入压缩。候选removedMessageTokens为移出的消息估算，未扣除新摘要。",
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
      "按inspect_context给出的throughSeq和自己写的summary立即压缩上下文。通常270k时一次整理到约100k；200k预提醒先准备，不反复压缩。summary承接旧摘要、重要事实、最新往来和自己已说的话、承诺及未完成事项。压缩会重建历史缓存，宿主同步固定投影，随后继续复用。返回已生效的持久记录和前后用量估算；原件保留。当前未完成工具批次不能移出。真实压后inputTokens要等下一次模型请求。",
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
  for (const event of selectedEvents(events, "compression")) {
    through = Math.max(through, (event.data as { throughSeq: number }).throughSeq);
  }
  return through;
}
export function contextUsage(
  events: ConsciousEvent[],
  prompt: string,
  tools: ToolSpec[],
  counter?: ContextCounter,
  project: (messages: Message[]) => Message[] = (messages) => messages,
) {
  const latest = lastActiveEvent(
    events,
    "model_response",
    (event) => !!(event.data as { response?: { usage?: Json } }).response?.usage,
  );
  return {
    estimatedTokens: contextEstimate(project(render(events, prompt)), tools, counter),
    measurement: counter?.description ?? "本地估算，包含身份、消息及工具定义；非上游精确token",
    compressedThroughSeq: compressionThrough(events),
    targetTokens: CONTEXT_TARGET_TOKENS,
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
  project?: (messages: Message[]) => Message[],
) {
  const value = contextArgs(args, ["afterSeq", "limit"]);
  const after = value.afterSeq ?? 0, limit = value.limit ?? 50;
  if (
    !Number.isSafeInteger(after) || Number(after) < 0 ||
    !Number.isSafeInteger(limit) || Number(limit) < 1 || Number(limit) > 100
  ) {
    throw new KernelError("rejected", "afterSeq须为非负整数，limit须为1至100");
  }
  const usage = contextUsage(events, prompt, tools, counter, project);
  const complete = blocks(events, usage.compressedThroughSeq);
  let removed = 0;
  const all = complete.map((block) => {
    removed += estimate(block.messages);
    return { throughSeq: block.throughSeq, removedMessageTokens: removed };
  });
  const recommendation = recommendCompression(all, usage.estimatedTokens);
  const remaining = all.filter((block) => block.throughSeq > Number(after));
  const candidates = remaining.slice(0, Number(limit));
  return {
    ...usage,
    candidates,
    recommendation,
    totalCandidates: all.length,
    hasMore: remaining.length > candidates.length,
    nextAfterSeq: remaining.length > candidates.length ? candidates.at(-1)!.throughSeq : null,
    preview: "compress_context({throughSeq,summary,dryRun:true})返回包含实际摘要的前后估算",
  };
}

export function recommendCompression(
  candidates: { throughSeq: number; removedMessageTokens: number }[],
  estimatedTokens: number,
) {
  const needed = Math.max(
    8_000,
    estimatedTokens - CONTEXT_RETAIN_TARGET_TOKENS + CONTEXT_SUMMARY_RESERVE_TOKENS,
  );
  // Prefer retaining eight recent complete blocks; a few very large blocks may
  // need summarizing too. The caller still chooses the scope and writes its own summary.
  const eligible = candidates.slice(0, Math.max(1, candidates.length - 8));
  const fallback = candidates.slice(0, Math.max(1, candidates.length - 1));
  const candidate = eligible.find((item) => item.removedMessageTokens >= needed) ??
    fallback.find((item) => item.removedMessageTokens >= needed) ??
    eligible.findLast((item) => item.removedMessageTokens >= 8_000) ??
    fallback.findLast((item) => item.removedMessageTokens >= 8_000) ?? null;
  return {
    candidate,
    recommendedNow: estimatedTokens >= CONTEXT_TARGET_TOKENS,
    retainTargetTokens: CONTEXT_RETAIN_TARGET_TOKENS,
    summaryReserveTokens: CONTEXT_SUMMARY_RESERVE_TOKENS,
    retainedRecentBlocks: candidate
      ? candidates.length - candidates.findIndex((item) => item === candidate) - 1
      : candidates.length,
    scope:
      "建议边界是完整片段，不代表现在必须压缩；通常270k时一次整理到约100k。保留当前目标、最新往来和自己已发出的回复、承诺、失败与unknown动作编号、子代理任务、下一步及原件位置；承接旧摘要，不逐条抄日志。以compress_context的applied/compressionSeq和after核实生效，再看下一次inputTokens。",
  };
}
export function previewCompression(
  events: ConsciousEvent[],
  prompt: string,
  tools: ToolSpec[],
  args: Json,
  counter?: ContextCounter,
  project?: (messages: Message[]) => Message[],
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
  const compressedThroughSeq = compressionThrough(events);
  if (
    throughSeq <= compressedThroughSeq ||
    !blocks(events, compressedThroughSeq).some((block) => block.throughSeq === throughSeq)
  ) {
    throw new KernelError(
      "rejected",
      "指定位置不是尚未压缩的完整消息/工具回执边界；请用inspect_context查询",
    );
  }
  const before = contextUsage(events, prompt, tools, counter, project);
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
  const after = contextUsage(appendPreviewEvent(events, preview), prompt, tools, counter, project);
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
