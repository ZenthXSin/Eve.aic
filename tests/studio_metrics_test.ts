import assert from "node:assert/strict";
import type { Json } from "../kernel/protocol.ts";
import type { AdminQuery } from "../services/admin/contracts.ts";
import {
  collectStudioOverview,
  createStudioOverviewCollector,
} from "../services/admin/studio-metrics.ts";

const origin = Date.UTC(2026, 0, 2), tick = 15_000;
function event(seq: number, kind: string, data: Json, offset = seq * 100): Json {
  return { seq, at: new Date(origin + offset).toISOString(), kind, data };
}

class Backend {
  now = origin;
  head = 0;
  calls = 0;
  requests: AdminQuery[] = [];
  gate?: Promise<void>;
  collect = createStudioOverviewCollector(() => this.now);
  snapshot = (): Json => ({
    runtime: {
      available: true,
      throughSeq: this.head,
      receiptsThroughSeq: 93,
      budget: {
        paused: false,
        day: "2026-01-02",
        timeZone: "Asia/Tokyo",
        dailyTokenBudget: 100,
        knownNoncachedTokens: 30,
        unknownReservedTokens: 7,
        safetyTotal: 37,
        safeRemaining: 63,
      },
      model: { blocked: null, current: { model: "test-model", revision: 2 } },
    },
    ownership: { mode: "active", activePermits: 0, pendingRequest: null },
    supervisor: { current: "blue", paused: false, upgrade: null },
  });
  item = (seq: number) => event(seq, "notice", {});
  queue = (): Json => ({ items: [] });
  page = (afterSeq: number, throughSeq: number): Json => {
    const end = Math.min(afterSeq + 8, throughSeq);
    return {
      throughSeq,
      items: Array.from({ length: end - afterSeq }, (_, index) => this.item(afterSeq + index + 1)),
      ...(end < throughSeq ? { next: { afterSeq: end, throughSeq } } : {}),
    };
  };
  async status() {
    this.calls++;
    await this.gate;
    return this.snapshot();
  }
  query(request: AdminQuery) {
    this.requests.push(request);
    assert.equal(request.operation, "page");
    if (request.resource === "qqFailures") {
      assert.deepEqual(request.args, { limit: 3 });
      return Promise.resolve(this.queue());
    }
    assert.equal(request.resource, "timeline");
    assert.deepEqual(Object.keys(request.args).sort(), ["afterSeq", "throughSeq"]);
    return Promise.resolve(
      this.page(Number(request.args.afterSeq), Number(request.args.throughSeq)),
    );
  }
  read() {
    return this.collect(this);
  }
  next() {
    this.now += tick;
    return this.read();
  }
}

Deno.test("Studio：跨用户合并在途请求，缓存按后端隔离，只读固定水位的尾部两页", async () => {
  const source = new Backend(), other = new Backend();
  source.head = 1_000_003;
  const { promise, resolve } = Promise.withResolvers<void>();
  source.gate = promise;
  const pending = Array.from({ length: 50 }, () => source.read());
  assert.equal(source.calls, 1);
  resolve();
  const values = await Promise.all(pending), first = values[0];
  assert.ok(values.every((value) => value === first));
  assert.equal(first.timeline.count, 16);
  assert.equal(first.timeline.fromSeq, source.head - 15);
  assert.deepEqual(
    source.requests.filter((request) => request.resource === "timeline").map((request) =>
      request.args
    ),
    [
      { afterSeq: source.head - 16, throughSeq: source.head },
      { afterSeq: source.head - 8, throughSeq: source.head },
    ],
  );
  assert.equal(source.requests.length, 3);
  source.now += tick - 1;
  assert.strictEqual(await source.read(), first);
  assert.equal(source.calls, 1);
  assert.equal((await source.collect(other)).timeline.count, 0);
  assert.equal(other.calls, 1);
  source.now++;
  const second = await source.read();
  assert.equal(second.history.length, 2);
  assert.deepEqual(second.history.map((point) => point.at), [origin, origin + tick]);
  assert.equal(first.history.length, 1);
  assert.equal(source.requests.length, 3);
  assert.equal(source.calls, 2);
  assert.strictEqual(await collectStudioOverview(other), await collectStudioOverview(other));
});

Deno.test("Studio：计时只取真实字段，按请求匹配成功回执，截断记录不追原件、不泄漏正文", async () => {
  const source = new Backend();
  const secret = "SENSITIVE_FIXTURE_DO_NOT_PROJECT";
  const items: Json[] = [
    event(1, "model_start", {
      requestId: "model_1",
      runtimeTiming: { preparationMs: 0, noticesMs: 0, recallMs: 4, toolsListMs: -3 },
    }),
    event(2, "model_start", {
      requestId: "model_2",
      runtimeTiming: { preparationMs: 18.5, recallMs: 9, stateUpdateMs: "6" },
    }),
    event(3, "model_response", { requestId: "model_other", response: { content: secret } }),
    event(4, "model_failure", { requestId: "model_2", error: { message: secret } }),
    event(5, "model_response", { requestId: "model_2" }),
    event(6, "tool_result", { executionMs: 0, result: { images: [secret] } }),
    {
      seq: 7,
      kind: "model_start",
      at: new Date(origin).toISOString(),
      truncated: true,
      get data(): Json {
        throw new Error("不能读取截断原件");
      },
      get preview(): Json {
        throw new Error("不能解析节选或图片");
      },
    },
    event(8, "model_response", { requestId: "model_1", response: { modelOutput: [secret] } }),
    { seq: 9, kind: "tool_result", at: "日期未知", data: { executionMs: 99 } },
    event(10, "tool_result", { executionMs: "0" }),
  ];
  source.head = items.length;
  source.item = (seq) => items[seq - 1];
  const base = source.snapshot();
  source.snapshot = () => ({
    ...base as Record<string, Json>,
    supervisor: {
      current: "blue",
      paused: false,
      upgrade: {
        phase: "observing",
        get observation(): Json {
          throw new Error("不允许遍历升级大图");
        },
      },
    },
  });
  source.queue = () => ({
    items: [1, 2, 3].map((sequence) => ({
      sequence,
      actionId: `act_${sequence}`,
      fingerprint: secret,
    })),
    next: { afterSequence: 3, limit: 3 },
  });
  const snapshot = await source.read(), perf = snapshot.performance;
  assert.equal(snapshot.timeline.count, 10);
  assert.equal(snapshot.timeline.truncated, 1);
  assert.deepEqual(snapshot.timeline.recent.map((item) => item.seq), [10, 9, 8, 7, 6, 5, 4, 3]);
  assert.equal(snapshot.timeline.recent[1].at, null);
  assert.equal(snapshot.status.paused, "unpaused");
  assert.equal(snapshot.status.model, "test-model");
  assert.equal(snapshot.pending.modelBlocked, false);
  assert.equal(snapshot.pending.upgradePhase, "observing");
  assert.equal(snapshot.pending.ownershipRequest, false);
  assert.deepEqual(snapshot.pending.qqFailures, {
    read: "ok",
    sampledAt: origin,
    count: 3,
    more: true,
  });
  assert.deepEqual(snapshot.budget, {
    day: "2026-01-02",
    timeZone: "Asia/Tokyo",
    dailyTokenBudget: 100,
    knownNoncachedTokens: 30,
    unknownReservedTokens: 7,
    safetyTotal: 37,
    safeRemaining: 63,
  });
  assert.deepEqual(perf.model.points, [{ seq: 8, at: origin + 800, value: 700 }]);
  assert.deepEqual(perf.preparation.points.map((point) => point.value), [0, 18.5]);
  assert.equal(perf.tool.count, 1);
  assert.equal(perf.tool.p50, 0);
  assert.equal(perf.tool.p95, null);
  assert.deepEqual(perf.stages[0], { key: "recallMs", count: 2, p50: 4, p95: null });
  assert.equal(perf.stages.find((stage) => stage.key === "stateUpdateMs")?.p50, null);
  assert.ok(!JSON.stringify(snapshot).includes(secret));
});

Deno.test("Studio：状态失败和恢复不猜数不查历史；缺失字段与显式零、暂停不一致分别保留", async () => {
  const source = new Backend();
  source.snapshot = () => {
    throw new Error("private backend error");
  };
  const missing = await source.read();
  assert.equal(missing.status.read, "unavailable");
  assert.equal(missing.timeline.count, null);
  assert.equal(missing.pending.modelBlocked, null);
  assert.equal(missing.pending.ownershipRequest, null);
  assert.equal(missing.pending.upgrading, null);
  assert.equal(missing.budget.unknownReservedTokens, null);
  assert.equal((await source.read()).status.available, null);
  assert.equal(source.calls, 1);
  source.snapshot = () => ({ runtime: { available: false }, startup: { phase: "recovering" } });
  const recovery = await source.next();
  assert.equal(recovery.status.available, false);
  assert.equal(recovery.status.recovery, "recovering");
  assert.equal(recovery.status.paused, "unknown");
  assert.equal(recovery.pending.qqFailures.read, "not_read");
  assert.equal(source.requests.length, 0);
  source.snapshot = () => ({
    runtime: {
      available: true,
      throughSeq: 0,
      budget: { paused: true, dailyTokenBudget: 0, safeRemaining: -1 },
      model: { blocked: { requestId: "model_117", httpStatus: 429 } },
    },
    supervisor: { paused: false },
    ownership: { pendingRequest: "grant-3" },
  });
  const actual = await source.next();
  assert.equal(actual.timeline.count, 0);
  assert.equal(actual.budget.dailyTokenBudget, 0);
  assert.equal(actual.budget.safeRemaining, null);
  assert.equal(actual.status.paused, "inconsistent");
  assert.equal(actual.status.activePermits, null);
  assert.equal(actual.pending.modelBlocked, true);
  assert.equal(actual.pending.ownershipRequest, true);
  assert.equal(actual.pending.requestId, "model_117");
  assert.equal(actual.pending.httpStatus, 429);
  assert.equal(actual.pending.qqFailures.count, 0);
});

Deno.test("Studio：无效时间线不算作零或完整窗口，失败队列单独降级并限制重试频率", async () => {
  const source = new Backend();
  source.head = 15;
  const page = source.page;
  source.page = (after, through) =>
    after === 8 ? { throughSeq: through, items: [{ seq: 9 }] } : page(after, through);
  source.queue = () => {
    throw new Error("no queue");
  };
  const failed = await source.read();
  assert.equal(failed.timeline.read, "unavailable");
  assert.equal(failed.timeline.count, null);
  assert.equal(failed.timeline.recent.length, 0);
  assert.equal(failed.pending.qqFailures.count, null);
  assert.equal(failed.pending.qqFailures.more, null);
  assert.equal(failed.status.available, true);
  source.page = page;
  source.queue = () => ({ items: [] });
  const recovered = await source.next();
  assert.equal(recovered.timeline.count, 15);
  assert.equal(recovered.pending.qqFailures.read, "unavailable");
  assert.equal(recovered.pending.qqFailures.sampledAt, origin);
  source.now += 45_000;
  const ready = await source.read();
  assert.deepEqual(ready.pending.qqFailures, {
    read: "ok",
    sampledAt: origin + 60_000,
    count: 0,
    more: false,
  });
  assert.equal(source.requests.filter((request) => request.resource === "qqFailures").length, 2);
});

Deno.test("Studio：增量、不跨缺口配对、回退水位、P95 门槛、状态和事件窗口都有上限", async () => {
  const source = new Backend();
  source.item = (seq) => event(seq, "tool_result", { executionMs: seq });
  source.head = 16;
  const initial = await source.read();
  assert.equal(initial.performance.tool.p95, null);
  source.head = 20;
  const twenty = await source.next();
  assert.equal(twenty.performance.tool.count, 20);
  assert.equal(twenty.performance.tool.p50, 10);
  assert.equal(twenty.performance.tool.p95, 19);
  assert.deepEqual(source.requests.at(-1)?.args, { afterSeq: 16, throughSeq: 20 });
  for (let sample = 0; sample < 100; sample++) {
    source.head++;
    await source.next();
  }
  const bounded = await source.read();
  assert.equal(bounded.history.length, 48);
  assert.equal(bounded.timeline.count, 96);
  assert.equal(bounded.performance.tool.count, 96);
  assert.equal(bounded.timeline.fromSeq, 25);
  source.item = (seq) => event(seq, "model_start", { requestId: `model_${seq}` });
  source.head++;
  await source.next();
  source.head += 42;
  source.item = (seq) => event(seq, "model_response", { requestId: "model_121" });
  const gap = await source.next();
  assert.equal(gap.performance.model.count, 0);
  assert.equal(gap.timeline.count, 16);
  assert.equal(gap.timeline.fromSeq, source.head - 15);
  source.head = 3;
  const reset = await source.next();
  assert.equal(reset.history.length, 1);
  assert.equal(reset.timeline.count, 3);
  assert.equal(reset.timeline.throughSeq, 3);
  assert.equal(reset.timeline.fromSeq, 1);
});

Deno.test("Studio：不配对负间隔和回退两侧请求，不采信越界、跳序或不匹配水位的页", async () => {
  const source = new Backend();
  source.head = 5;
  const records = [
    event(1, "model_start", { requestId: "model_1" }),
    event(2, "model_response", { requestId: "model_1" }, 10),
    event(3, "model_start", { requestId: "model_3" }),
    event(4, "upgrade_rollback", {}),
    event(5, "model_response", { requestId: "model_3" }),
  ];
  source.item = (seq) => records[seq - 1];
  assert.equal((await source.read()).performance.model.count, 0);
  source.head++;
  for (
    const malformed of [
      { throughSeq: 100, items: [event(6, "notice", {})] },
      { throughSeq: 6, items: [event(8, "notice", {})] },
      { throughSeq: 6, items: Array(9).fill(event(6, "notice", {})) },
    ]
  ) {
    source.page = () => malformed;
    const bad = await source.next();
    assert.equal(bad.timeline.read, "unavailable");
    assert.equal(bad.timeline.fromSeq, null);
    assert.equal(bad.timeline.throughSeq, null);
  }
});
