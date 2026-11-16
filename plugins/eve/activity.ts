import type { ModelResponse } from "../../contracts/model.ts";
import type { Json } from "../../kernel/protocol.ts";
import type { ConsciousEvent } from "./journal.ts";
import { lastActiveEvent } from "./view.ts";

/** Waiting for a helper does not occupy Eve's own next action. */
export function selfActivityWindow(events: ConsciousEvent[], fresh: boolean): boolean {
  if (fresh) return false;
  const response = lastActiveEvent(events, "model_response");
  if (!response) return true;
  const calls = (response.data as unknown as { response: ModelResponse }).response.toolCalls;
  return !calls.length || calls.every((call) => call.name === "rest") ||
    (lastActiveEvent(events, "rest")?.seq ?? 0) > response.seq;
}

/** Unspecified rests revisit interests soon; explicit wake times remain Eve's choice. */
export function defaultRestWake(now: number, sleepMs: number, enabled: boolean): number {
  return now + (enabled ? Math.min(sleepMs, 10 * 60_000) : sleepMs);
}

export function eveCommandCompletions(items: Json[]): Json[] {
  // AgentSessions.#run assigns this suffix to every helper tool invocation.
  // Its command result already belongs to the helper's durable transcript; the
  // incoming agent reply is the wake event for Eve, including task failures.
  return items.filter((item) => {
    const id = (item as { requestId?: unknown } | null)?.requestId;
    return typeof id !== "string" || !/^.+_tool_\d+_\d+$/.test(id);
  });
}
