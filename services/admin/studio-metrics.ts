import type { AdminBackend } from "./contracts.ts";

const REFRESH_MS = 15_000, QUEUE_MS = 60_000;
const PAGE = 8, TAIL = 2 * PAGE, WINDOW = 96, HISTORY = 48, QUEUE_PAGE = 3;
const STAGES = [
  "pendingScanMs",
  "noticesMs",
  "toolsListMs",
  "recallMs",
  "selfContextMs",
  "contextProjectionMs",
  "stateUpdateMs",
] as const;
type Stage = typeof STAGES[number];
type Read = "ok" | "unavailable" | "not_read";
type Source = Pick<AdminBackend, "status" | "query">;

function record(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

function text(value: unknown, limit = 128) {
  return typeof value === "string" && value.length > 0 && value.length <= limit ? value : null;
}

function count(value: unknown) {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function ms(value: unknown) {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 &&
      value <= Number.MAX_SAFE_INTEGER
    ? value
    : null;
}

function time(value: unknown) {
  const iso = text(value, 40);
  if (!iso || !/^\d{4}-\d\d-\d\dT.+(?:Z|[+-]\d\d:\d\d)$/.test(iso)) return null;
  const stamp = Date.parse(iso);
  return Number.isFinite(stamp) ? stamp : null;
}

function present(value: unknown) {
  return value === null ? false : text(value) !== null ? true : null;
}

function projectStatus(raw: unknown) {
  const source = record(raw),
    supervisor = record(source.supervisor),
    runtime = record(source.runtime);
  const owner = record(source.ownership),
    budget = record(runtime.budget),
    model = record(runtime.model);
  const blocked = record(model.blocked), upgrade = record(supervisor.upgrade);
  const paused = typeof supervisor.paused !== "boolean" || typeof budget.paused !== "boolean"
    ? "unknown"
    : supervisor.paused !== budget.paused
    ? "inconsistent"
    : supervisor.paused
    ? "paused"
    : "unpaused";
  return {
    status: {
      read: typeof runtime.available === "boolean" ? "ok" : "unavailable",
      available: typeof runtime.available === "boolean" ? runtime.available : null,
      recovery: text(record(runtime.recovery).stage) ?? text(record(source.startup).phase),
      paused,
      instance: text(supervisor.current),
      ownershipMode: text(owner.mode, 24),
      activePermits: count(owner.activePermits),
      model: text(record(model.current).model),
      throughSeq: count(runtime.throughSeq),
      receiptsThroughSeq: count(runtime.receiptsThroughSeq),
    },
    budget: {
      day: text(budget.day, 10),
      timeZone: text(budget.timeZone, 80),
      dailyTokenBudget: count(budget.dailyTokenBudget),
      knownNoncachedTokens: count(budget.knownNoncachedTokens),
      unknownReservedTokens: count(budget.unknownReservedTokens),
      safetyTotal: count(budget.safetyTotal),
      safeRemaining: count(budget.safeRemaining),
    },
    pending: {
      modelBlocked: model.blocked === null ? false : text(blocked.requestId) ? true : null,
      requestId: text(blocked.requestId),
      httpStatus: count(blocked.httpStatus),
      ownershipRequest: present(owner.pendingRequest),
      upgrading: supervisor.upgrade === null ? false : text(upgrade.phase) ? true : null,
      upgradePhase: text(upgrade.phase, 40),
    },
  };
}

function observation(item: unknown, expected: number) {
  const event = record(item);
  if (event.seq !== expected) throw new Error("时间线序号不连续");
  const kind = text(event.kind, 64), truncated = event.truncated === true;
  const isModel = kind === "model_start" || kind === "model_response" || kind === "model_failure";
  const data = !truncated && (isModel || kind === "tool_result") ? record(event.data) : {};
  const timing = kind === "model_start" ? record(data.runtimeTiming) : {};
  const stages: Partial<Record<Stage, number>> = {};
  for (const key of STAGES) {
    const value = ms(timing[key]);
    if (value !== null) stages[key] = value;
  }
  return {
    seq: expected,
    kind,
    at: time(event.at),
    truncated,
    requestId: isModel ? text(data.requestId) : null,
    preparationMs: ms(timing.preparationMs),
    executionMs: kind === "tool_result" ? ms(data.executionMs) : null,
    stages,
  };
}

type Observation = ReturnType<typeof observation>;
type Window = { head: number; events: Observation[] };

async function tail(source: Source, head: number, prior?: Window): Promise<Window> {
  if (prior?.head === head) return prior;
  const after = Math.max(0, head - TAIL, prior && prior.head <= head ? prior.head : 0);
  const events = prior?.head === after ? prior.events.slice() : [];
  for (let cursor = after; cursor < head;) {
    const page = record(
      await source.query({
        resource: "timeline",
        operation: "page",
        args: { afterSeq: cursor, throughSeq: head },
      }),
    );
    if (
      page.throughSeq !== head || !Array.isArray(page.items) ||
      page.items.length !== Math.min(PAGE, head - cursor)
    ) throw new Error("时间线页未覆盖约定窗口");
    for (const item of page.items) events.push(observation(item, ++cursor));
  }
  return { head, events: events.slice(-WINDOW) };
}

type Point = { seq: number; at: number; value: number };

function summary(values: number[]) {
  const sorted = values.toSorted((left, right) => left - right), count = sorted.length;
  const percentile = (fraction: number) => sorted[Math.ceil(count * fraction) - 1] ?? null;
  return { count, p50: percentile(0.5), p95: count >= 20 ? percentile(0.95) : null };
}

function performanceSample(events: Observation[]) {
  const preparation: Point[] = [], model: Point[] = [], tool: Point[] = [];
  const starts = new Map<string, number>();
  for (const event of events) {
    const { seq, at, requestId } = event;
    if (event.kind === "upgrade_rollback") starts.clear();
    if (requestId && event.kind === "model_failure") starts.delete(requestId);
    if (at === null) continue;
    if (event.preparationMs !== null) preparation.push({ seq, at, value: event.preparationMs });
    if (event.executionMs !== null) tool.push({ seq, at, value: event.executionMs });
    if (requestId && event.kind === "model_start") starts.set(requestId, at);
    if (requestId && event.kind === "model_response") {
      const start = starts.get(requestId);
      if (start !== undefined && at >= start) model.push({ seq, at, value: at - start });
      starts.delete(requestId);
    }
  }
  const metric = (points: Point[]) => ({ ...summary(points.map((point) => point.value)), points });
  return {
    preparation: metric(preparation),
    model: metric(model),
    tool: metric(tool),
    stages: STAGES.map((key) => ({
      key,
      ...summary(events.flatMap((event) => event.at === null ? [] : event.stages[key] ?? [])),
    })).sort((left, right) => (right.p50 ?? -1) - (left.p50 ?? -1)),
  };
}

type QueueSample = {
  read: Read;
  sampledAt: number | null;
  count: number | null;
  more: boolean | null;
};
const noQueue: QueueSample = { read: "not_read", sampledAt: null, count: null, more: null };

function fresh(now: number, at: number, ttl: number) {
  return now >= at && now - at < ttl;
}

async function queueSample(
  source: Source,
  now: () => number,
  prior?: QueueSample,
): Promise<QueueSample> {
  if (prior?.sampledAt != null && fresh(now(), prior.sampledAt, QUEUE_MS)) return prior;
  try {
    const page = record(
      await source.query({
        resource: "qqFailures",
        operation: "page",
        args: { limit: QUEUE_PAGE },
      }),
    );
    if (
      !Array.isArray(page.items) || page.items.length > QUEUE_PAGE ||
      page.items.some((item) => !text(record(item).actionId) || !count(record(item).sequence)) ||
      (page.next !== undefined && (!page.items.length || record(page.next).afterSequence !==
          record(page.items.at(-1)).sequence))
    ) throw new Error("失败队首页无效");
    return {
      read: "ok",
      sampledAt: now(),
      count: page.items.length,
      more: page.next !== undefined,
    };
  } catch {
    return { read: "unavailable", sampledAt: now(), count: null, more: null };
  }
}

type StatusPoint = { at: number; throughSeq: number | null; receiptsThroughSeq: number | null };
type Memory = { history: StatusPoint[]; window?: Window; queue?: QueueSample };

async function observe(source: Source, memory: Memory, now: () => number) {
  let raw: unknown;
  try {
    raw = await source.status();
  } catch {
    raw = null;
  }
  const { status, budget, pending } = projectStatus(raw), sampledAt = now();
  const { throughSeq, receiptsThroughSeq } = status;
  if (throughSeq !== null && memory.window && throughSeq < memory.window.head) {
    memory.window = undefined;
    memory.history = [];
  }
  let read: Read = "not_read", events: Observation[] = [], qqFailures = noQueue;
  if (status.available === true) {
    if (throughSeq !== null || receiptsThroughSeq !== null) {
      memory.history = [...memory.history, { at: sampledAt, throughSeq, receiptsThroughSeq }].slice(
        -HISTORY,
      );
    }
    const [window, queue] = await Promise.all([
      throughSeq === null ? null : tail(source, throughSeq, memory.window).catch(() => null),
      queueSample(source, now, memory.queue),
    ]);
    memory.queue = qqFailures = queue;
    if (window) {
      read = "ok";
      memory.window = window;
      events = window.events;
    } else if (throughSeq !== null) read = "unavailable";
  }
  return {
    schema: 1,
    sampledAt,
    refreshAfterMs: REFRESH_MS,
    status,
    budget,
    pending: { ...pending, qqFailures },
    history: memory.history,
    timeline: {
      read,
      count: read === "ok" ? events.length : null,
      fromSeq: events[0]?.seq ?? null,
      throughSeq: read === "ok" ? throughSeq : null,
      truncated: read === "ok" ? events.filter((event) => event.truncated).length : null,
      recent: events.slice(-8).reverse().map(({ seq, at, kind, truncated }) => ({
        seq,
        at,
        kind,
        truncated,
      })),
    },
    performance: performanceSample(events),
  };
}

export type StudioOverview = Awaited<ReturnType<typeof observe>>;
type Slot = Memory & {
  cached?: { at: number; value: StudioOverview };
  flight?: Promise<StudioOverview>;
};

export function createStudioOverviewCollector(now: () => number = Date.now) {
  const slots = new WeakMap<Source, Slot>();
  return (source: Source): Promise<StudioOverview> => {
    let slot = slots.get(source);
    if (!slot) slots.set(source, slot = { history: [] });
    if (slot.cached && fresh(now(), slot.cached.at, REFRESH_MS)) {
      return Promise.resolve(slot.cached.value);
    }
    const entry = slot;
    return entry.flight ??= observe(source, entry, now).then((value) => {
      entry.cached = { at: now(), value };
      return value;
    }).finally(() => {
      entry.flight = undefined;
    });
  };
}

export const collectStudioOverview = createStudioOverviewCollector();
