import { type ErrorKind, type Json, KernelError } from "../../kernel/protocol.ts";
import { factProjectionAt } from "../../sdk/fact-journal.ts";
import { providerRejection } from "../../sdk/model-failure.ts";
import type { ConsciousEvent } from "./journal.ts";

type Fields = Record<string, Json>;
type FactProjection = string | [string, Fields] | [string, Fields, string];

const EMPTY: Fields = {};

function fields(value: Json | undefined): Fields {
  return value && typeof value === "object" && !Array.isArray(value) ? value : EMPTY;
}

function pick(data: Fields, ...keys: string[]): Fields {
  const result: Fields = {};
  for (const key of keys) {
    const value = data[key];
    if (value === null || ["string", "number", "boolean"].includes(typeof value)) {
      result[key] = value;
    }
  }
  return result;
}

function jobSummary(value: Json | undefined): Fields | null {
  const { jobId, status } = fields(value);
  return typeof jobId === "string" && typeof status === "string" ? { jobId, status } : null;
}

export function jobSummaryFromResult(result: Json | undefined): Fields | null {
  const { output } = fields(result);
  if (typeof output !== "string") return null;
  try {
    return jobSummary(JSON.parse(output));
  } catch {
    return null;
  }
}

function project(event: ConsciousEvent): FactProjection {
  const { kind } = event;
  const data = fields(event.data);
  switch (kind) {
    case "tool_start":
      return [kind, pick(data, "callId")];
    case "tool_result":
      return [kind, { ...pick(data, "callId"), job: jobSummaryFromResult(data.result) }];
    case "notice": {
      const items = Array.isArray(data.items) ? data.items : [];
      const jobs = data.source === "scripts" || data.source === "agents"
        ? items.flatMap((item) => {
          const job = jobSummary(fields(item).job ?? item);
          return job ? [job] : [];
        })
        : [];
      return [kind, {
        ...pick(data, "source", "throughSeq", "actionId", "anchor"),
        wakes: items.length > 0 || data.source === "reminder",
        ...(jobs.length ? { jobs } : {}),
      }];
    }
    case "mind_applied":
      return [kind, pick(data, "throughSeq", "wakeAt")];
    case "rest":
      return [kind, pick(data, "wakeAt")];
    case "channel_failure":
      return [kind, pick(data, "errorKind")];
    case "model_start":
    case "recovery":
      return [kind, pick(data, "requestId")];
    case "model_response": {
      const calls = fields(data.response).toolCalls;
      return [kind, {
        ...pick(data, "requestId"),
        toolCount: Array.isArray(calls) ? calls.length : -1,
      }];
    }
    case "model_failure": {
      const failure = fields(data.error);
      const httpStatus = typeof failure.message === "string"
        ? providerRejection(new KernelError(failure.kind as ErrorKind, failure.message))
        : undefined;
      return [kind, {
        ...pick(data, "requestId", "retryAt"),
        ...(httpStatus === undefined ? {} : { httpStatus }),
      }, event.at];
    }
    case "upgrade_rollback":
      return [kind, pick(data, "afterSeq", "throughSeq")];
    case "retain":
      return [kind, pick(data, "responseSeq", "toolIndex", "mode")];
    default:
      return kind;
  }
}

export const eveHistoryCheckpoint = { version: "eve-history-v1", project };

/** Active membership needs only headers (and rollback ranges), not recovery bodies.
 * Without a checkpoint, leave data lazy so compressed model/tool originals are
 * never projected merely to discover their kind. Full recovery still uses eveFactAt. */
export function eveEventHeaderAt(events: ConsciousEvent[], position: number) {
  const saved = factProjectionAt(events, position) as FactProjection | undefined;
  if (saved === undefined) return events[position];
  return typeof saved === "string"
    ? { kind: saved, seq: position + 1, data: EMPTY }
    : { kind: saved[0], seq: position + 1, data: saved[1] };
}

export function eveFactAt(events: ConsciousEvent[], position: number) {
  const saved = factProjectionAt(events, position) as FactProjection | undefined;
  const event = saved === undefined ? events[position] : undefined;
  const value = saved ?? project(event!);
  const seq = event?.seq ?? position + 1;
  return typeof value === "string"
    ? { kind: value, seq, data: EMPTY, at: "" }
    : { kind: value[0], seq, data: value[1], at: value[2] ?? "" };
}
