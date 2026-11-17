import type { ModelResponse } from "../../contracts/model.ts";
import type { ConsciousEvent } from "./journal.ts";
import { activeEvents, lastActiveEvent } from "./view.ts";

export const CONTEXT_TARGET_TOKENS = 270_000;
export const CONTEXT_REMINDER_TOKENS = 200_000;
export const CONTEXT_RETAIN_TARGET_TOKENS = 100_000;
export const CONTEXT_SUMMARY_RESERVE_TOKENS = 4_000;
export const CONTEXT_PROJECTION_EMERGENCY_TOKENS = CONTEXT_TARGET_TOKENS + 30_000;
export const CONTEXT_PROJECTION_MIN_SAVING_TOKENS = 30_000;

/** A reported input only describes the view actually sent, not a later compression or release. */
export function currentProviderInput(events: ConsciousEvent[], sourceCommit?: string) {
  const response = lastActiveEvent(events, "model_response");
  if (!response) return null;
  const data = response.data as unknown as { requestId: string; response: ModelResponse };
  const usage = data.response.usage;
  if (
    usage?.status === "invalid" || !Number.isSafeInteger(usage?.inputTokens) ||
    usage!.inputTokens! <= 0
  ) return null;
  const start = lastActiveEvent(
    events,
    "model_start",
    (event) => (event.data as { requestId?: string }).requestId === data.requestId,
  );
  const changed = Math.max(
    ...["compression", "context_projection", "retain"].map((kind) =>
      lastActiveEvent(events, kind)?.seq ?? 0
    ),
  );
  return {
    responseSeq: response.seq,
    inputTokens: usage!.inputTokens!,
    currentView: !!start && start.seq > changed &&
      (!sourceCommit || (start.data as { sourceCommit?: string }).sourceCommit === sourceCommit),
    scope: "最近一次模型请求实测输入；整理或换版后须等新请求核实",
  };
}

/** Repeat only at active model turns. Persisted notices keep the ordinary request prefix stable. */
export function contextMaintenanceState(
  events: ConsciousEvent[],
  estimatedTokens: number,
  sourceCommit?: string,
  now = Date.now(),
) {
  const providerInput = currentProviderInput(events, sourceCommit);
  const observedTokens = Math.max(
    estimatedTokens,
    providerInput?.currentView ? providerInput.inputTokens : 0,
  );
  const level = observedTokens >= CONTEXT_TARGET_TOKENS
    ? "urgent"
    : observedTokens >= CONTEXT_REMINDER_TOKENS
    ? "due"
    : "normal";
  const last = lastActiveEvent(events, "perception");
  const previous = (last?.data as {
    contextMaintenance?: { level?: string; compressedThroughSeq?: number };
  } | undefined)?.contextMaintenance;
  const compressed = lastActiveEvent(events, "compression");
  const compressedThroughSeq = (compressed?.data as { throughSeq?: number } | undefined)
    ?.throughSeq ?? 0;
  let turns = 0;
  const active = activeEvents(events);
  for (let i = active.length - 1; i >= 0 && turns < 4; i--) {
    if (active[i].seq <= (last?.seq ?? 0)) break;
    if (active[i].kind === "model_response") turns++;
  }
  const reminder = level === "normal"
    ? null
    : level === "urgent"
    ? "上下文已达到270k整理线：先完成眼前必要回复，再一次整理到约100k，保留最新往来、自己已说的话、承诺和子代理任务；宿主在同次整理中固定历史投影，随后复用缓存。"
    : "上下文已达到200k预提醒线：先保留稳定请求前缀，继续当前阶段；只准备摘要要点，不因预提醒或睡前就压缩。约270k时再一次整理到约100k；原件保留。";
  return {
    level,
    estimatedTokens,
    providerInput,
    observedTokens,
    compressedThroughSeq,
    targetTokens: CONTEXT_TARGET_TOKENS,
    retainTargetTokens: CONTEXT_RETAIN_TARGET_TOKENS,
    compressionDue: level === "urgent",
    reminder,
    due: level !== "normal" &&
      (!previous || previous.level !== level ||
        previous.compressedThroughSeq !== compressedThroughSeq || level === "urgent" &&
          (turns >= 4 || now - Date.parse(last!.at) >= 15 * 60_000)),
  };
}
