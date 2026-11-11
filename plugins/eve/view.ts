import type { Message, ModelResponse, ToolCall, ToolSpec } from "../../contracts/model.ts";
import type { Json } from "../../kernel/protocol.ts";
import type { ConsciousEvent } from "./journal.ts";

export interface PendingTool {
  responseSeq: number;
  index: number;
  call: ToolCall;
  started: boolean;
}
export function callFor(event: ConsciousEvent, index: number): ToolCall {
  const raw = (event.data as unknown as { response: ModelResponse }).response.toolCalls[index];
  return { ...raw, id: `eve_${event.seq}_${index}` };
}
export function pendingTools(events: ConsciousEvent[]): PendingTool[] {
  const started = new Set<string>(), finished = new Set<string>();
  for (const event of events) {
    if (event.kind === "tool_start" || event.kind === "tool_result") {
      const data = event.data as { callId: string };
      (event.kind === "tool_start" ? started : finished).add(data.callId);
    }
  }
  const pending: PendingTool[] = [];
  for (const event of events) {
    if (event.kind === "model_response") {
      const response = (event.data as unknown as { response: ModelResponse }).response;
      for (let i = 0; i < response.toolCalls.length; i++) {
        const call = callFor(event, i);
        if (!finished.has(call.id)) {
          pending.push({ responseSeq: event.seq, index: i, call, started: started.has(call.id) });
        }
      }
    }
  }
  return pending;
}
export function blocks(events: ConsciousEvent[]): { throughSeq: number; messages: Message[] }[] {
  const result: { throughSeq: number; messages: Message[] }[] = [];
  let current: {
    throughSeq: number;
    messages: Message[];
    deferred: Message[];
    expected: Set<string>;
  } | undefined;
  const retention = new Map<string, { mode: string; summary?: string }>();
  for (const event of events) {
    if (event.kind === "retain") {
      const data = event.data as {
        responseSeq: number;
        toolIndex: number;
        mode: string;
        summary?: string;
      };
      retention.set(`eve_${data.responseSeq}_${data.toolIndex}`, data);
    }
  }
  for (const event of events) {
    if (
      event.kind === "notice" || event.kind === "activation" || event.kind === "perception" ||
      event.kind === "recovery" || event.kind === "model_failure" ||
      event.kind === "channel_failure" || event.kind === "channel_recovered"
    ) {
      const message: Message = {
        role: "user",
        content: JSON.stringify({
          source: "host",
          kind: event.kind,
          at: event.at,
          record: event.seq,
          data: event.data,
        }),
      };
      // 恢复时产生的真实通知也不能插到 assistant 工具调用与 tool 回执之间。
      if (current) current.deferred.push(message);
      else result.push({ throughSeq: event.seq, messages: [message] });
    } else if (event.kind === "model_response") {
      const response = (event.data as unknown as { response: ModelResponse }).response;
      const calls = response.toolCalls.map((_, i) => callFor(event, i));
      const message: Message = {
        role: "assistant",
        content: response.content,
        ...(calls.length ? { toolCalls: calls } : {}),
      };
      if (calls.length) {
        current = {
          throughSeq: event.seq,
          messages: [message],
          deferred: [],
          expected: new Set(calls.map((c) => c.id)),
        };
      } else result.push({ throughSeq: event.seq, messages: [message] });
    } else if (event.kind === "tool_result") {
      const data = event.data as { callId: string; result: Json };
      if (!current || !current.expected.has(data.callId)) continue;
      const policy = retention.get(data.callId);
      const content = policy?.mode === "omit"
        ? `此处清除过一次工具调用结果，原文在意识记录 ${event.seq}`
        : policy?.mode === "summary"
        ? `${policy.summary}\n原始结果在意识记录 ${event.seq}`
        : JSON.stringify(data.result);
      current.messages.push({ role: "tool", toolCallId: data.callId, content });
      current.expected.delete(data.callId);
      current.throughSeq = event.seq;
      if (current.expected.size === 0) {
        result.push({
          throughSeq: current.throughSeq,
          messages: [...current.messages, ...current.deferred],
        });
        current = undefined;
      }
    }
  }
  return result;
}
export function render(events: ConsciousEvent[], prompt: string): Message[] {
  let through = 0;
  let memory = "";
  for (const event of events) {
    if (event.kind === "compression") {
      const data = event.data as { throughSeq: number; summary: string };
      if (data.throughSeq >= through) {
        through = data.throughSeq;
        memory = data.summary;
      }
    }
  }
  return [
    { role: "system", content: prompt },
    ...(through
      ? [{
        role: "user" as const,
        content: `保留的时间线（原文截至意识记录 ${through}）：\n${memory}`,
      }]
      : []),
    ...blocks(events).filter((block) => block.throughSeq > through).flatMap((block) =>
      block.messages
    ),
  ];
}
/** 上下文是估算量，非上游精确token统计；预算预留另用保守字节上界。 */
export function estimate(messages: Message[]): number {
  let total = 0;
  for (const message of messages) {
    let ascii = 0;
    for (const char of JSON.stringify(message)) {
      if (char.codePointAt(0)! < 128) ascii++;
      else total++;
    }
    total += Math.ceil(ascii / 4) + 8;
  }
  return total;
}
export function contextEstimate(messages: Message[], tools: ToolSpec[]): number {
  return estimate(messages) + estimate([{ role: "system", content: JSON.stringify(tools) }]);
}
/** 只移动完整片段；即使最后一个片段很大也可移出，锚定身份/契约始终保留。 */
export function mechanicalCompression(events: ConsciousEvent[], prompt: string, tools: ToolSpec[]) {
  if (contextEstimate(render(events, prompt), tools) < 500_000) return;
  const previous = events.filter((event) => event.kind === "compression").at(-1);
  let throughSeq = Number((previous?.data as { throughSeq?: number } | undefined)?.throughSeq ?? 0);
  const candidates = blocks(events).filter((block) => block.throughSeq > throughSeq);
  let tailEstimate = estimate(candidates.flatMap((block) => block.messages));
  const summary = (through: number) =>
    `系统机械压缩，非 Eve 自己写；原始经历至意识记录 ${through} 及先前自写摘要仍在硬盘。`;
  const prefix = (through: number) =>
    contextEstimate([
      { role: "system", content: prompt },
      {
        role: "user",
        content: `保留的时间线（原文截至意识记录 ${through}）：\n${summary(through)}`,
      },
    ], tools);
  // 历史按提交顺序排列：今天以前的最早片段自然排在今天片段之前。
  for (const block of candidates) {
    if (throughSeq && prefix(throughSeq) + tailEstimate < 450_000) break;
    throughSeq = block.throughSeq;
    tailEstimate -= estimate(block.messages);
  }
  if (!throughSeq) return;
  const nextSummary = summary(throughSeq);
  if (
    previous &&
    (previous.data as { throughSeq: number; summary: string }).throughSeq === throughSeq &&
    (previous.data as { summary: string }).summary === nextSummary
  ) return;
  return { throughSeq, summary: nextSummary };
}
export function budget(events: ConsciousEvent[], day: string) {
  const reserved = new Map<string, number>();
  let known = 0;
  let unknown = 0;
  for (const event of events) {
    const data = event.data as unknown as {
      requestId: string;
      day: string;
      reservation: number;
      response?: ModelResponse;
    };
    if (event.kind === "model_start" && data.day === day) {
      reserved.set(data.requestId, data.reservation);
    }
    if (event.kind === "model_response" && reserved.has(data.requestId)) {
      const usage = data.response?.usage;
      if (
        usage?.status !== "invalid" && usage?.inputTokens !== undefined &&
        usage.outputTokens !== undefined
      ) {
        if (usage.cachedInputTokens !== undefined && usage.cachedInputTokens <= usage.inputTokens) {
          known += usage.inputTokens - usage.cachedInputTokens + usage.outputTokens;
          reserved.delete(data.requestId);
        } else if (usage.cachedInputTokens === undefined) {
          // 输入缓存未知：输出确定计费，输入只作为非缓存成本的上界，不写成缓存0。
          known += usage.outputTokens;
          reserved.set(data.requestId, usage.inputTokens);
        }
      }
    }
  }
  for (const amount of reserved.values()) unknown += amount;
  return {
    knownNoncachedTokens: known,
    unknownReservedTokens: unknown,
    safetyTotal: known + unknown,
  };
}
export function accountingDay(timeZone = "Asia/Shanghai"): string {
  return new Intl.DateTimeFormat("en-CA", {
    timeZone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  }).format(new Date());
}
