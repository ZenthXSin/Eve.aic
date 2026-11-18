import assert from "node:assert/strict";
import type { Json } from "../kernel/protocol.ts";
import type { ConsciousEvent } from "../plugins/eve/journal.ts";
import { continuityMessage } from "../plugins/eve/continuity.ts";
import { JobObservations } from "../plugins/eve/job-observations.ts";
import receipts from "./fixtures/continuity-agent-status.json" with { type: "json" };

const saved = receipts as ConsciousEvent[];
const jobId = "eve_70266_0_task";
const admission = JSON.parse(receipts[0].data.result.output);
const agentJob = {
  jobId,
  agentId: admission.agentId,
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
  assert.deepEqual(
    scriptReports(projection),
    [],
    "complete running fixture is accepted as nonterminal information",
  );
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
  const running = structuredClone(projection.jobs.get(jobId));
  projection.observe(returned(22, { ...agentJob, status: "unknown" }));
  assert.deepEqual(projection.jobs.get(jobId), running);
  assert.equal(scriptReports(projection).at(-1).reportedStatus, "unknown");
  projection.observe(returned(23, agentJob));
  const refreshed = structuredClone(projection.jobs.get(jobId));
  assert.equal(refreshed?.status, "running");
  projection.observe(returned(24, { ...agentJob, status: "completed" }));
  assert.deepEqual(projection.jobs.get(jobId), refreshed);
  assert.equal(scriptReports(projection).at(-1).reportedStatus, "completed");
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
          : toolReceipt(30, { ...agentJob, status }),
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

Deno.test("running rejects late queued/checking through all four ingress paths and replay", () => {
  for (const [source, receipt] of Object.entries(ingress)) {
    for (const status of ["queued", "checking"]) {
      const events = [...saved];
      const projection = replay(events);
      const running = structuredClone(projection.jobs.get(jobId));
      const before = pending(projection);
      const e = receipt(70520, { ...agentJob, status });
      events.push(e);
      projection.observe(e);
      assert.equal(
        projection.jobs.get(jobId)?.status,
        "running",
        "late admission cannot regress running",
      );
      assert.equal(projection.jobs.get(jobId)?.observation?.record, 70488);
      assert.deepEqual(projection.jobs.get(jobId), running);
      assert.deepEqual(pending(projection), before);
      assert.deepEqual(
        scriptReports(projection),
        source === "script_result"
          ? [{
            jobId,
            reportedStatus: status,
            observation: { record: e.seq, recordedAt: e.at, source: "script_result" },
          }]
          : [],
      );
      assertReplay(projection, events);
    }
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

const scriptReports = (projection: JobObservations) => {
  const message = continuityMessage([], projection.jobs);
  const payload = JSON.parse(message.content.split("\n")[1]);
  assert.match(payload.scriptReportGuidance, /脚本自述、未经服务端证明/);
  assert.match(payload.scriptReportGuidance, /不据此退休任务或更新pendingJobs状态/);
  return payload.scriptReports;
};
const replay = (events: ConsciousEvent[]) => {
  const projection = new JobObservations();
  for (const e of events) projection.observe(e);
  return projection;
};
const assertReplay = (projection: JobObservations, events: ConsciousEvent[]) => {
  const restored = replay(events);
  assert.deepEqual([...restored.jobs], [...projection.jobs]);
  assert.deepEqual(pending(restored), pending(projection));
  assert.deepEqual(scriptReports(restored), scriptReports(projection));
};

Deno.test("script terminal/unknown strings only report regardless of identity or missing fields", () => {
  const { agentId: _agent, ...missingAgent } = agentJob;
  const { requestSeq: _request, ...missingRequest } = agentJob;
  const shapes = [
    agentJob,
    { ...agentJob, agentId: "forged_agent" },
    { ...agentJob, requestSeq: 1 },
    missingAgent,
    missingRequest,
    { ...agentJob, requestSeq: 1.5 },
    { ...agentJob, requestSeq: "12152" },
    { jobId },
  ];
  for (const initial of ["queued", "checking", "running"]) {
    for (const status of ["completed", "failed", "cancelled", "unknown"]) {
      for (const shape of shapes) {
        const events = [toolReceipt(71000, { jobId, status: initial })];
        const projection = replay(events);
        const before = pending(projection);
        const observation = structuredClone(projection.jobs.get(jobId));
        const e = returned(71001, { ...shape, status });
        events.push(e);
        projection.observe(e);
        assert.deepEqual(projection.jobs.get(jobId), observation);
        assert.deepEqual(pending(projection), before);
        assert.deepEqual(scriptReports(projection), [{
          jobId,
          reportedStatus: status,
          observation: { record: e.seq, recordedAt: e.at, source: "script_result" },
        }]);
        assertReplay(projection, events);
        // Reports cannot freeze subsequent permitted nonterminal observations.
        events.push(returned(71002, agentJob));
        projection.observe(events[2]);
        assert.equal(projection.jobs.get(jobId)?.status, "running");
        assert.equal(projection.jobs.get(jobId)?.observation?.record, 71002);
        assertReplay(projection, events);
      }
    }
  }
});

Deno.test("untracked script values never create jobs while tracked values only supplement nonterminal state", () => {
  for (
    const status of ["queued", "checking", "running", "completed", "failed", "cancelled", "unknown"]
  ) {
    const events = [returned(71100, { ...agentJob, status })];
    const projection = replay(events);
    assert.equal(projection.jobs.has(jobId), false);
    assert.deepEqual(pending(projection), []);
    assert.equal(scriptReports(projection)[0].reportedStatus, status);
    assertReplay(projection, events);
    // Server tracking need not provide identity; complete nonterminal shape is a display hint.
    events.push(toolReceipt(71101, { jobId, status: "queued" }));
    events.push(returned(71102, agentJob));
    projection.observe(events[1]);
    projection.observe(events[2]);
    assert.equal(projection.jobs.get(jobId)?.status, "running");
    assert.equal(projection.jobs.get(jobId)?.observation?.record, 71102);
    assertReplay(projection, events);
  }
});

Deno.test("uncertain script nonterminal shapes report while complete shapes need no identity proof", () => {
  for (
    const value of [
      { jobId, status: "running" },
      { ...agentJob, agentId: null },
      { ...agentJob, requestSeq: 1.5 },
      { ...agentJob, childJobIds: [null] },
    ]
  ) {
    const events = [saved[0], returned(71200, value)];
    const projection = replay([events[0]]);
    const before = pending(projection);
    projection.observe(events[1]);
    assert.deepEqual(pending(projection), before);
    assert.equal(scriptReports(projection)[0].reportedStatus, "running");
    assertReplay(projection, events);
  }
  const events = [
    saved[0],
    returned(71201, {
      ...agentJob,
      agentId: "different_agent",
      requestSeq: 1,
      status: "checking",
    }),
    returned(71202, agentJob),
  ];
  const projection = replay([events[0]]);
  projection.observe(events[1]);
  assert.equal(projection.jobs.get(jobId)?.status, "checking");
  assert.equal(projection.jobs.get(jobId)?.observation?.record, 71201);
  projection.observe(events[2]);
  assert.equal(projection.jobs.get(jobId)?.status, "running");
  assert.equal(projection.jobs.get(jobId)?.observation?.record, 71202);
  assert.deepEqual(scriptReports(projection), []);
  assertReplay(projection, events);
});

Deno.test("checking rejects late queued through all four ingress paths and replay", () => {
  for (const [source, receipt] of Object.entries(ingress)) {
    const events = [saved[0], toolReceipt(71300, { ...agentJob, status: "checking" })];
    const projection = replay(events);
    const before = pending(projection);
    events.push(receipt(71301, { ...agentJob, status: "queued" }));
    projection.observe(events[2]);
    assert.equal(projection.jobs.get(jobId)?.status, "checking", source);
    assert.equal(projection.jobs.get(jobId)?.observation?.record, 71300, source);
    assert.deepEqual(pending(projection), before);
    assert.deepEqual(
      scriptReports(projection),
      source === "script_result"
        ? [{
          jobId,
          reportedStatus: "queued",
          observation: { record: events[2].seq, recordedAt: events[2].at, source: "script_result" },
        }]
        : [],
    );
    assertReplay(projection, events);
  }
});

Deno.test("four ingress × terminal/unknown policy and freeze protections match restart replay", () => {
  for (const [source, receipt] of Object.entries(ingress)) {
    for (const status of ["completed", "failed", "cancelled", "unknown"]) {
      const events = [saved[0], event(71400, saved[1].data)];
      const projection = replay(events);
      const running = structuredClone(projection.jobs.get(jobId));
      const finalEvent = receipt(71401, { ...agentJob, status });
      events.push(finalEvent);
      projection.observe(finalEvent);
      if (source === "script_result") {
        assert.deepEqual(projection.jobs.get(jobId), running);
        assert.equal(pending(projection)[0].status, "running");
        assert.deepEqual(scriptReports(projection), [{
          jobId,
          reportedStatus: status,
          observation: {
            record: finalEvent.seq,
            recordedAt: finalEvent.at,
            source: "script_result",
          },
        }]);
        assertReplay(projection, events);
        // Only an outer server receipt may freeze the task, never the script branch.
        const server = toolReceipt(71402, { ...agentJob, status });
        events.push(server);
        projection.observe(server);
        assert.equal(projection.jobs.get(jobId)?.observation?.source, "tool_receipt");
      } else {
        assert.equal(projection.jobs.get(jobId)?.observation?.source, source);
        assert.deepEqual(scriptReports(projection), []);
      }
      assert.equal(projection.jobs.get(jobId)?.status, status);
      assert.equal(
        projection.jobs.get(jobId)?.observation?.record,
        source === "script_result" ? 71402 : 71401,
      );
      assert.deepEqual(pending(projection), []);
      assertReplay(projection, events);
      const retired = structuredClone(projection.jobs.get(jobId));
      for (const [lateSource, lateReceipt] of Object.entries(ingress)) {
        for (
          const lateStatus of [
            "queued",
            "checking",
            "running",
            "unknown",
            "completed",
            "failed",
            "cancelled",
          ]
        ) {
          const e = lateReceipt(71403 + events.length, { ...agentJob, status: lateStatus });
          // Server terminal receipts may resolve unknown, so test them separately below.
          if (
            status === "unknown" && lateSource !== "script_result" &&
            ["completed", "failed", "cancelled"].includes(lateStatus)
          ) continue;
          events.push(e);
          projection.observe(e);
          assert.deepEqual(
            projection.jobs.get(jobId),
            retired,
            [source, status, lateSource, lateStatus].join("/"),
          );
          assert.deepEqual(pending(projection), []);
          if (lateSource === "script_result") {
            assert.deepEqual(scriptReports(projection), [{
              jobId,
              reportedStatus: lateStatus,
              observation: { record: e.seq, recordedAt: e.at, source: "script_result" },
            }]);
          }
          assertReplay(projection, events);
        }
      }
      if (status === "unknown") {
        const completed = toolReceipt(71500, { ...agentJob, status: "completed" });
        events.push(completed);
        projection.observe(completed);
        assert.equal(projection.jobs.get(jobId)?.status, "completed");
        assertReplay(projection, events);
      }
    }
  }
});

Deno.test("script reports are bounded latest clues and preserve replay without changing jobs", () => {
  const events = [saved[0]];
  const projection = replay(events);
  const before = pending(projection);
  for (let i = 0; i < 8; i++) {
    const e = returned(71600 + i, { ...agentJob, jobId: `unbound_${i}` });
    events.push(e);
    projection.observe(e);
  }
  assert.deepEqual(pending(projection), before);
  assert.equal(scriptReports(projection).length, 5);
  assert.deepEqual(scriptReports(projection).map((r: { jobId: string }) => r.jobId), [
    "unbound_3",
    "unbound_4",
    "unbound_5",
    "unbound_6",
    "unbound_7",
  ]);
  assertReplay(projection, events);
});
