import assert from "node:assert/strict";
import type { Json } from "../kernel/protocol.ts";
import type { ConsciousEvent } from "../plugins/eve/journal.ts";
import { continuityMessage } from "../plugins/eve/continuity.ts";
import { JobObservations } from "../plugins/eve/job-observations.ts";
import receipts from "./fixtures/continuity-agent-status.json" with { type: "json" };

const saved = receipts as ConsciousEvent[];
const jobId = "eve_70266_0_task";
const agentJob = {
  jobId,
  agentId: "agent_fixture",
  status: "running",
  requestSeq: 12152,
  childJobIds: ["child_a", "child_b", "child_c"],
};
const event = (seq: number, data: Json, kind = "tool_result"): ConsciousEvent => ({
  schema: 1,
  seq,
  at: new Date(Date.UTC(2026, 10, 18, 12, 0, 0, seq)).toISOString(),
  kind,
  data,
  previous: "0".repeat(64),
  digest: "fixture",
});
const returned = (seq: number, value: Json, patch: Record<string, Json> = {}) => {
  const text = JSON.stringify(value);
  return event(seq, {
    callId: `call_`,
    result: {
      status: "completed",
      output: JSON.stringify({
        jobId: "script_fixture",
        status: "completed",
        scriptStarted: true,
        result: { text, characters: Array.from(text).length, truncated: false },
        ...patch,
      }),
    },
  });
};
const pending = (projection: JobObservations) => {
  const message = continuityMessage([], projection.jobs);
  assert.match(message.content, /不是实时状态/);
  return JSON.parse(message.content.split("\n")[1]).pendingJobs;
};

Deno.test("saved 70268 → 70488: complete returned AgentJob advances queued, without polling children", () => {
  const projection = new JobObservations();
  projection.observe(saved[0]);
  assert.deepEqual(pending(projection), [{
    jobId,
    status: "queued",
    observation: {
      record: 70268,
      recordedAt: "2026-11-18T11:01:04.508Z",
      source: "tool_receipt",
    },
  }]);
  projection.observe(saved[1]);
  assert.deepEqual(pending(projection), [{
    jobId,
    status: "running",
    observation: {
      record: 70488,
      recordedAt: "2026-11-18T11:41:52.987Z",
      source: "script_result",
    },
  }]);
  assert.equal(projection.jobs.size, 2, "children are not inferred or indexed");
  projection.observe(event(70500, saved[0].data));
  assert.equal(
    projection.jobs.get(jobId)?.observation?.record,
    70488,
    "late queued cannot regress running",
  );
  const restored = new JobObservations();
  for (const receipt of saved) restored.observe(receipt);
  assert.deepEqual(
    pending(restored),
    pending(projection),
    "offline replay preserves latest observation",
  );
});

Deno.test("missing, unknown, partial or unrelated script progress is not guessed", () => {
  const projection = new JobObservations();
  projection.observe(saved[0]);
  const text = JSON.stringify(agentJob);
  const invalid: Json[] = [
    null,
    { jobId },
    { ...agentJob, status: "maybe" },
    { ...agentJob, requestSeq: null },
    { ...agentJob, agentId: null },
    { ...agentJob, jobId: "untracked" },
    { ...agentJob, childJobIds: [null] },
    [agentJob],
    { jobs: [agentJob] },
  ];
  for (const [i, value] of invalid.entries()) projection.observe(returned(i + 1, value));
  const patches: Record<string, Json>[] = [
    { status: "unknown" },
    { status: "running" },
    { scriptStarted: false },
    { resultStartChar: 4 },
    { result: { text, characters: Array.from(text).length, truncated: true } },
    { result: { text, characters: Array.from(text).length, truncated: false, nextChar: 100 } },
    { result: { text, characters: 1, truncated: false } },
    { result: { text } },
  ];
  for (const patch of patches) projection.observe(returned(20, agentJob, patch));
  assert.equal(projection.jobs.get(jobId)?.status, "queued");
  assert.equal(projection.jobs.get(jobId)?.observation?.record, 70268);
  // Running itself is known even when no child count/progress has been supplied.
  const { childJobIds: _children, ...withoutChildren } = agentJob;
  projection.observe(returned(21, withoutChildren));
  assert.equal(projection.jobs.get(jobId)?.status, "running");
  assert.ok(!("childJobIds" in pending(projection)[0]));
  projection.observe(returned(22, { ...agentJob, status: "unknown" }));
  projection.observe(returned(23, agentJob));
  assert.equal(projection.jobs.get(jobId)?.status, "unknown");
  assert.ok(!pending(projection).some((j: { jobId: string }) => j.jobId === jobId));
  projection.observe(returned(24, { ...agentJob, status: "completed" }));
  assert.equal(projection.jobs.get(jobId)?.status, "completed");
});

Deno.test("terminal status retires pending jobs and cannot be revived by late receipts", () => {
  for (const status of ["completed", "failed", "cancelled"]) {
    for (const fromNotice of [false, true]) {
      const projection = new JobObservations();
      projection.observe(saved[0]);
      projection.observe(saved[1]);
      projection.observe(
        fromNotice
          ? event(30, { source: "agents", items: [{ jobId, status }] }, "notice")
          : returned(30, { ...agentJob, status }),
      );
      projection.observe(event(31, saved[0].data));
      projection.observe(event(32, saved[1].data));
      assert.equal(projection.jobs.get(jobId)?.status, status);
      assert.deepEqual(pending(projection), []);
    }
  }
});

const toolReceipt = (seq: number, value: Json) =>
  event(seq, {
    callId: "receipt_fixture",
    result: { status: "completed", output: JSON.stringify(value) },
  });
const ingress: Record<string, (seq: number, value: Json) => ConsciousEvent> = {
  tool_receipt: toolReceipt,
  script_result: returned,
  agents_notification: (seq, value) => event(seq, { source: "agents", items: [value] }, "notice"),
  scripts_notification: (seq, value) =>
    event(seq, { source: "scripts", items: [{ job: value }] }, "notice"),
};

Deno.test("non-string statuses cannot advance, retire or replace an observation through any ingress", () => {
  const invalidStatuses: Json[] = [
    ["running"],
    ["completed"],
    ["unknown"],
    ["failed"],
    ["cancelled"],
    ["queued"],
    ["checking"],
    0,
    1,
    true,
    false,
    null,
    {},
    [],
    ["running", "completed"],
  ];
  for (const [source, receipt] of Object.entries(ingress)) {
    for (const status of invalidStatuses) {
      const projection = new JobObservations();
      projection.observe(saved[0]);
      const admission = structuredClone(projection.jobs.get(jobId));
      const before = pending(projection);
      projection.observe(receipt(70510, { ...agentJob, status }));
      assert.deepEqual(
        projection.jobs.get(jobId),
        admission,
        `${source}: ${JSON.stringify(status)}`,
      );
      assert.deepEqual(
        pending(projection),
        before,
        "invalid status must not hide or advance the job",
      );
      assert.equal(projection.jobs.get(jobId)?.observation?.record, 70268);
      // A malformed terminal/unknown status must not poison future legitimate receipts.
      projection.observe(toolReceipt(70511, { jobId, status: "queued" }));
      assert.equal(projection.jobs.get(jobId)?.status, "queued");
      assert.equal(projection.jobs.get(jobId)?.observation?.record, 70511);
      projection.observe(event(70512, saved[1].data));
      assert.equal(projection.jobs.get(jobId)?.status, "running");
      assert.equal(projection.jobs.get(jobId)?.observation?.record, 70512);
      assert.equal(pending(projection).length, 1);
      const running = structuredClone(projection.jobs.get(jobId));
      const runningPending = pending(projection);
      projection.observe(receipt(70513, { ...agentJob, status }));
      assert.deepEqual(projection.jobs.get(jobId), running, "invalid status must preserve running");
      assert.deepEqual(pending(projection), runningPending);
      // Nor may a malformed status create a new indexed job.
      projection.observe(receipt(70514, { ...agentJob, jobId: "untracked", status }));
      assert.equal(projection.jobs.has("untracked"), false);
    }
  }
  // Invalid completion envelopes must not authorize otherwise legal nested progress.
  for (const status of invalidStatuses) {
    const projection = new JobObservations();
    projection.observe(saved[0]);
    const before = structuredClone([...projection.jobs]);
    const summary = JSON.parse(JSON.stringify(saved[1].data)).result.output;
    projection.observe(event(70515, {
      result: { status, output: summary },
    }));
    assert.deepEqual(
      [...projection.jobs],
      before,
      "tool_result status must be the completed string",
    );
    projection.observe(returned(70516, agentJob, { status }));
    assert.deepEqual(
      [...projection.jobs],
      before,
      "script envelope status must be the completed string",
    );
  }
});

Deno.test("running rejects late checking without changing its observation", () => {
  for (const receipt of Object.values(ingress)) {
    const projection = new JobObservations();
    for (const e of saved) projection.observe(e);
    const running = structuredClone(projection.jobs.get(jobId));
    const before = pending(projection);
    projection.observe(receipt(70520, { ...agentJob, status: "checking" }));
    assert.equal(
      projection.jobs.get(jobId)?.status,
      "running",
      "late checking cannot regress running",
    );
    assert.equal(projection.jobs.get(jobId)?.observation?.record, 70488);
    assert.deepEqual(projection.jobs.get(jobId), running);
    assert.deepEqual(pending(projection), before);
  }
});

Deno.test("qualified same-state observations refresh record/time and distinguish notification sources", () => {
  const projection = new JobObservations();
  for (const e of saved) projection.observe(e);
  for (const [i, [source, receipt]] of Object.entries(ingress).entries()) {
    const e = receipt(70600 + i, agentJob);
    const previousAt = projection.jobs.get(jobId)?.observation?.recordedAt;
    projection.observe(e);
    assert.deepEqual(pending(projection), [{
      jobId,
      status: "running",
      observation: { record: e.seq, recordedAt: e.at, source },
    }]);
    assert.notEqual(e.at, previousAt);
  }
  // A coerced notification source is not a qualified receipt either.
  const before = pending(projection);
  projection.observe(event(70610, {
    source: ["agents"],
    items: [{ ...agentJob, status: "completed" }],
  }, "notice"));
  projection.observe(event(70611, {
    source: ["scripts"],
    items: [{ job: { ...agentJob, status: "unknown" } }],
  }, "notice"));
  assert.deepEqual(pending(projection), before);
});

Deno.test("legacy caller with no observation does not invent a timestamp", () => {
  const message = continuityMessage([], new Map([[jobId, { status: "queued" }]]));
  assert.equal(JSON.parse(message.content.split("\n")[1]).pendingJobs[0].observation, null);
});
