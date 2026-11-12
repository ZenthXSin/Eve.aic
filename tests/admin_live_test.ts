import assert from "node:assert/strict";
import { liveEvent, LiveReader, liveState } from "../services/admin/live.ts";
import { startAdminServer } from "../services/admin/server.ts";
import type { AdminBackend } from "../services/admin/contracts.ts";
import type { Json } from "../kernel/protocol.ts";

const status = {
  supervisor: { paused: false },
  runtime: { available: true, throughSeq: 0, budget: {}, model: {} },
};
const event = (seq: number, kind: string, data: Json = {}) => ({
  seq,
  kind,
  at: "2026-11-12T00:00:00Z",
  data,
});

Deno.test("live projection preserves explicit text, handles truncation and never invents outcomes", () => {
  const e = liveEvent(
    event(1, "model_response", {
      requestId: "model_1",
      response: { content: "这是我留下的文字", toolCalls: [] },
    }),
  );
  assert.equal(e.text, "这是我留下的文字");
  assert.equal(e.category, "thought");
  const noText = liveEvent(
    event(2, "model_response", {
      response: { content: "", toolCalls: [{ name: "execute_script" }] },
    }),
  );
  assert.match(noText.text, /未留下文字/);
  const truncated = liveEvent({
    ...event(3, "model_response"),
    truncated: true,
    preview: "private partial output",
  });
  assert.match(truncated.text, /未据此推断/);
  assert(!truncated.text.includes("private partial output"));
  const result = liveEvent(
    event(4, "tool_result", { result: { status: "completed", output: "not a delivered receipt" } }),
  );
  assert.match(result.text, /不等于消息已送达/);
  assert(!result.text.includes("not a delivered receipt"));
});

Deno.test("live state pairs requests and recovery, respects pause and blocking", () => {
  const started = liveEvent(event(1, "model_start", { requestId: "model_1" }));
  assert.equal(liveState([started], status).phase, "model");
  const recovered = liveEvent(event(2, "recovery", { requestId: "model_1", result: "结果未知" }));
  assert.equal(liveState([started, recovered], status).phase, "attention");
  const tool = liveEvent(
    event(3, "tool_start", { callId: "call_1", call: { name: "execute_script" } }),
  );
  assert.equal(liveState([tool], status).phase, "tool");
  const result = liveEvent(
    event(4, "tool_result", { callId: "call_1", result: { status: "completed" } }),
  );
  assert.equal(liveState([tool, result], status).phase, "result");
  assert.equal(liveState([tool], { ...status, supervisor: { paused: true } }).phase, "paused");
  assert.equal(
    liveState([started], {
      runtime: { available: true, model: { blocked: { requestId: "model_1" } } },
    }).phase,
    "attention",
  );
  assert.equal(liveState([], status).phase, "waiting");
});

Deno.test("live reader bounds windows, tails new records, coalesces reads and resets on rollback", async () => {
  let through = 100, reads = 0;
  const backend: AdminBackend = {
    status: () =>
      Promise.resolve({ ...status, runtime: { ...status.runtime, throughSeq: through } }),
    query: ({ args }) => {
      reads++;
      const start = Number(args.afterSeq), end = Number(args.throughSeq);
      return Promise.resolve({
        items: Array.from(
          { length: Math.min(8, end - start) },
          (_, i) => event(start + i + 1, "wake", { reason: "test" }),
        ),
      });
    },
    control: () => {
      throw new Error("read only");
    },
  };
  const reader = new LiveReader(backend);
  const [a, b] = await Promise.all([reader.read(), reader.read()]) as Record<string, Json>[];
  assert.deepEqual(a, b);
  assert.equal(reads, 10);
  assert.equal((a.events as Json[]).length, 80);
  await reader.read();
  assert.equal(reads, 10);
  through = 101;
  const next = await reader.read() as Record<string, Json>;
  assert.equal(reads, 11);
  assert.equal((next.events as Json[]).length, 80);
  through = 3;
  const rolledBack = await reader.read() as Record<string, Json>;
  assert.equal((rolledBack.events as Json[]).length, 3);
  assert.equal(rolledBack.throughSeq, 3);
});

Deno.test("live endpoint requires a session, rejects gaps and never invokes controls", async () => {
  const backend: AdminBackend = {
    status: () => Promise.resolve({ ...status, runtime: { ...status.runtime, throughSeq: 3 } }),
    query: () => Promise.resolve({ items: [event(3, "wake")] }),
    control: () => {
      throw new Error("read only");
    },
  };
  await assert.rejects(() => new LiveReader(backend).read(), /顺序无效/);
  const server = await startAdminServer(backend, { token: "live_test_password" });
  try {
    const anonymous = await fetch(server.url + "/api/live");
    assert.equal(anonymous.status, 401);
    await anonymous.json();
    const login = await fetch(server.url + "/api/login", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ token: "live_test_password" }),
    });
    await login.json();
    const cookie = login.headers.get("set-cookie")!.split(";")[0];
    const response = await fetch(server.url + "/api/live", { headers: { cookie } });
    assert.equal(response.status, 503);
    const result = await response.json();
    assert.equal(result.ok, false);
  } finally {
    await server.close();
  }
});
