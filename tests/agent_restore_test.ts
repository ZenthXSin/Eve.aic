import assert from "node:assert/strict";
import type { AgentJob } from "../contracts/agents.ts";
import type { Message, ModelResponse, ToolCall } from "../contracts/model.ts";
import { AgentService } from "../services/runtime/agents.ts";
import { transcriptCursor } from "../services/runtime/agent-restore.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => resolve = r);
  return { promise, resolve };
}
const final = (content = "done"): ModelResponse => ({ content, toolCalls: [] });
const call = (id: string): ToolCall => ({
  id,
  name: "use_tool",
  args: { namespace: "files", method: "read", args: { path: id } },
});
async function copyPrefix(source: string, dest: string, through: number) {
  await Deno.mkdir(`${dest}/events`, { recursive: true });
  await Deno.mkdir(`${dest}/bodies`);
  for await (const file of Deno.readDir(`${source}/bodies`)) {
    await Deno.copyFile(`${source}/bodies/${file.name}`, `${dest}/bodies/${file.name}`);
  }
  for (let seq = 1; seq <= through; seq++) {
    const name = `${String(seq).padStart(16, "0")}.json`;
    await Deno.copyFile(`${source}/events/${name}`, `${dest}/events/${name}`);
  }
}

Deno.test("agent restore: every durable tool prefix resumes only missing calls with stable IDs and original context", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agent-restore-" });
  const source = new AgentService(`${directory}/source`, {
    complete(_request, _options, id) {
      return Promise.resolve(
        id.endsWith("_model_1")
          ? { content: "read two", toolCalls: [call("a"), call("b")], modelOutput: [{ id: "raw" }] }
          : final(),
      );
    },
    invoke: (_n, _m, args) => Promise.resolve({ text: args.path }),
  });
  try {
    await source.initialize();
    const job = await source.send({ agentId: "agent_coding", text: "original task" }, "restore");
    await source.close();
    const steps = source.journal.events.filter((e) => e.kind === "step");
    // Admission, started, assistant+zero receipts, one receipt, all receipts, final assistant.
    for (const through of [job.requestSeq, job.requestSeq + 1, ...steps.map((e) => e.seq)]) {
      const dest = `${directory}/prefix-${through}`;
      await copyPrefix(source.directory, dest, through);
      const invokes: string[] = [], models: string[] = [];
      const restored = new AgentService(dest, {
        complete(request, _options, id) {
          models.push(id);
          assert.equal(request.messages[1].content, "original task");
          if (id.endsWith("_model_1")) {
            return Promise.resolve({
              content: "read two",
              toolCalls: [call("a"), call("b")],
              modelOutput: [{ id: "raw" }],
            });
          }
          assert.deepEqual(request.messages.slice(2).map((m) => m.role), [
            "assistant",
            "tool",
            "tool",
          ]);
          assert.deepEqual(request.messages[2].modelOutput, [{ id: "raw" }]);
          assert.deepEqual(request.messages.slice(3).map((m) => m.content), [
            '{"text":"a"}',
            '{"text":"b"}',
          ]);
          return Promise.resolve(final());
        },
        invoke(_n, _m, args, id) {
          invokes.push(id);
          return Promise.resolve({ text: args.path });
        },
      });
      try {
        await restored.initialize();
        restored.resume(); // Repeated resume and duplicate sends share one in-flight job.
        const args = { agentId: job.agentId, text: "original task" };
        const results = await Promise.all([
          restored.send(args, job.jobId),
          restored.send(args, job.jobId),
        ]);
        assert.ok(results.every((r) => r.status === "completed"));
        const savedReceipts = steps.slice(1, 3).filter((e) => e.seq <= through).length;
        assert.deepEqual(invokes, ["restore_tool_1_0", "restore_tool_1_1"].slice(savedReceipts));
        assert.deepEqual(
          models,
          through >= steps[3].seq
            ? []
            : through >= steps[0].seq
            ? ["restore_model_2"]
            : ["restore_model_1", "restore_model_2"],
        );
        assert.equal(restored.journal.events.filter((e) => e.kind === "accepted").length, 1);
        assert.equal(restored.journal.events.filter((e) => e.kind === "reply").length, 1);
        assert.equal(restored.journal.events.filter((e) => e.kind === "started").length, 1);
        assert.equal(restored.list().items.find((s) => s.agentId === job.agentId)!.pending, 0);
      } finally {
        await restored.close();
      }
    }
  } finally {
    await source.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("agent restore: saved tool-call model response resumes without paid model replay", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agent-saved-" });
  const source = new AgentService(`${directory}/source`, {
    complete: () => Promise.resolve(final()),
    invoke: () => Promise.resolve(null),
  });
  try {
    await source.initialize();
    const job = await source.send({ agentId: "agent_review", text: "task" }, "saved");
    await source.close();
    const dest = `${directory}/interrupted`;
    await copyPrefix(source.directory, dest, job.requestSeq + 1);
    const queries: string[] = [], tools: string[] = [], models: string[] = [];
    const restored = new AgentService(dest, {
      result(id) {
        queries.push(id);
        return id === "saved_model_1" ? { content: "read", toolCalls: [call("a")] } : undefined;
      },
      complete(_r, _o, id) {
        models.push(id);
        return Promise.resolve(final());
      },
      invoke(_n, _m, _args, id) {
        tools.push(id);
        return Promise.resolve({ text: "a" });
      },
    });
    try {
      await restored.initialize();
      const done = await restored.send({ agentId: job.agentId, text: "task" }, job.jobId);
      assert.equal(done.status, "completed");
      assert.deepEqual(models, ["saved_model_2"]);
      assert.deepEqual(tools, ["saved_tool_1_0"]);
      assert.ok(queries.includes("saved_model_1"));
    } finally {
      await restored.close();
    }
  } finally {
    await source.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("agent restore: pending queue is FIFO, other sessions parallel, captured configs and read watermarks survive", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agent-fifo-" });
  const gate = deferred(), began = deferred();
  const source = new AgentService(`${directory}/source`, {
    async complete(request) {
      if (request.messages.at(-1)!.content === "first") {
        began.resolve();
        await gate.promise;
      }
      return final();
    },
    invoke: () => Promise.resolve(null),
  });
  const work: Promise<unknown>[] = [];
  try {
    await source.initialize();
    const readJob = await source.send({ agentId: "agent_coding", text: "read" }, "read");
    await source.open({ agentId: "agent_coding" });
    const closed = await source.create({ role: "review" }, "closed");
    await source.closeSession({ agentId: closed.agentId });
    work.push(source.send({ agentId: "agent_coding", text: "first" }, "first"));
    await began.promise;
    work.push(source.send({ agentId: "agent_coding", text: "second" }, "second"));
    await source.accepted("second", new AbortController().signal);
    await source.configure({ agentId: "agent_coding", reasoningEffort: "low" });
    await source.send({ agentId: "agent_review", text: "parallel" }, "parallel");
    const dest = `${directory}/interrupted`;
    // Keep the parallel admission unfinished too, while both coding jobs remain pending.
    await copyPrefix(source.directory, dest, source.status({ jobId: "parallel" }).requestSeq);
    gate.resolve();
    await Promise.all(work);
    await source.close();
    const resumedGate = deferred(), resumedBegan = deferred(), parallel = deferred();
    const order: string[] = [], efforts: string[] = [];
    const restored = new AgentService(dest, {
      async complete(request, options, id) {
        order.push(id);
        efforts.push(options.reasoningEffort!);
        if (id === "first_model_1") {
          assert.ok(request.messages.some((m) => m.content === "read"));
          resumedBegan.resolve();
          await resumedGate.promise;
        }
        if (id === "parallel_model_1") parallel.resolve();
        if (id === "second_model_1") {
          assert.ok(request.messages.some((m) => m.content === "first finished"));
        }
        return final(id === "first_model_1" ? "first finished" : "done");
      },
      invoke: () => Promise.resolve(null),
    });
    const restoredWork: Promise<unknown>[] = [];
    try {
      await restored.initialize(false);
      const coding = () => restored.list().items.find((s) => s.agentId === "agent_coding")!;
      assert.equal(coding().pending, 2);
      assert.equal(coding().unread, 0);
      assert.equal(coding().config.reasoningEffort, "low");
      assert.equal(restored.status({ jobId: "read" }).replySeq, readJob.replySeq);
      assert.equal(
        restored.list().items.find((s) => s.agentId === closed.agentId)!.state,
        "closed",
      );
      restored.resume();
      restored.resume();
      await Promise.all([resumedBegan.promise, parallel.promise]);
      restoredWork.push(restored.send({ agentId: "agent_coding", text: "first" }, "first"));
      restoredWork.push(restored.send({ agentId: "agent_coding", text: "new" }, "new"));
      await restored.accepted("new", new AbortController().signal);
      assert.deepEqual([...order].sort(), ["first_model_1", "parallel_model_1"]);
      resumedGate.resolve();
      await Promise.all(restoredWork);
      assert.deepEqual(order.filter((id) => id !== "parallel_model_1"), [
        "first_model_1",
        "second_model_1",
        "new_model_1",
      ]);
      assert.deepEqual(efforts, ["high", "high", "high", "low"]);
      assert.equal(coding().pending, 0);
      assert.equal(coding().unread, 3);
      assert.equal(restored.notifications().items.length, 4);
      await restored.close();
      const again = new AgentService(dest, {
        complete() {
          throw new Error("completed model replay");
        },
        invoke() {
          throw new Error("completed tool replay");
        },
      });
      try {
        await again.initialize();
        assert.equal(again.notifications().items.length, 4);
        assert.equal(again.notifications({ afterSeq: readJob.replySeq! }).items.length, 4);
        assert.equal(again.list().items.find((s) => s.agentId === "agent_coding")!.pending, 0);
      } finally {
        await again.close();
      }
    } finally {
      resumedGate.resolve();
      await Promise.allSettled(restoredWork);
      await restored.close();
    }
  } finally {
    gate.resolve();
    await Promise.allSettled(work);
    await source.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("agent restore: cancelled jobs with no reply finalize once and never revive", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agent-cancel-" });
  // Cover cancellation before start, during a tool turn, and after a final assistant step.
  for (const stage of ["queued", "tool", "final"]) {
    const source = new AgentService(`${directory}/${stage}`, {
      complete: () => Promise.resolve(final()),
      invoke: () => Promise.resolve(null),
    });
    try {
      await source.initialize();
      // Use committed admission metadata, then construct the crash boundary using journal facts.
      const job = await source.send({ agentId: "agent_coding", text: "cancel me" }, "cancel");
      await source.close();
      const dest = `${directory}/${stage}-crash`;
      await copyPrefix(source.directory, dest, job.requestSeq);
      const { FactJournal } = await import("../sdk/fact-journal.ts");
      const { createHash } = await import("node:crypto");
      const journal = new FactJournal(dest);
      await journal.initialize();
      if (stage !== "queued") {
        await journal.append("started", { agentId: job.agentId, jobId: job.jobId });
        const message: Message = stage === "final"
          ? { role: "assistant", content: "saved final" }
          : { role: "assistant", content: "read", toolCalls: [call("a")] };
        const hash = createHash("sha256").update(JSON.stringify(message)).digest("hex");
        await writeDurableJson(`${dest}/bodies/${hash}.json`, message);
        await journal.append("step", { agentId: job.agentId, jobId: job.jobId, body: { hash } });
      }
      await journal.append("cancelled", { agentId: job.agentId, jobId: job.jobId });
      await journal.close();
      let calls = 0, results = 0;
      for (let restart = 0; restart < 2; restart++) {
        const restored = new AgentService(dest, {
          result() {
            results++;
            return final();
          },
          complete() {
            calls++;
            return Promise.resolve(final());
          },
          invoke() {
            calls++;
            return Promise.resolve(null);
          },
        });
        try {
          await restored.initialize();
          restored.resume();
          const done = await restored.send({ agentId: job.agentId, text: "cancel me" }, job.jobId);
          assert.equal(done.status, "cancelled");
          assert.equal(restored.journal.events.filter((e) => e.kind === "accepted").length, 1);
          assert.equal(restored.journal.events.filter((e) => e.kind === "reply").length, 1);
          assert.equal(restored.list().items.find((s) => s.agentId === job.agentId)!.pending, 0);
          if (stage === "final") {
            assert.equal((await restored.message({ seq: done.replySeq! })).text, "saved final");
          }
        } finally {
          await restored.close();
        }
      }
      assert.equal(calls, 0);
      assert.equal(results, 0);
    } finally {
      await source.close();
    }
  }
  await Deno.remove(directory, { recursive: true });
});

Deno.test("agent restore: transcript cursor rejects missing, out-of-order and extra receipts", () => {
  const assistant: Message = {
    role: "assistant",
    content: "read",
    toolCalls: [call("a"), call("b")],
  };
  const a: Message = { role: "tool", toolCallId: "a", content: "{}" };
  const b: Message = { role: "tool", toolCallId: "b", content: "{}" };
  for (
    const transcript of [
      [a],
      [assistant, b],
      [assistant, a, a],
      [assistant, a, { role: "assistant", content: "bad" }],
      [assistant, a, b, b],
      [{ role: "assistant", content: "done" }, assistant],
    ]
  ) assert.throws(() => transcriptCursor(transcript as Message[]), /步骤原件无效|回执未配对/);
});

Deno.test("agent restore engine: resumes only after dispatch is ready and unfinished action receipts are reconciled", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agent-engine-resume-" });
  await writeDurableJson(`${directory}/settings`, {
    dailyTokenBudget: 1000000,
    paused: false,
    timeZone: "UTC",
  });
  const began = deferred(), gate = deferred();
  let models = 0;
  const options = {
    directory: `${directory}/source`,
    workspace: `${directory}/workspace`,
    settingsFile: `${directory}/settings`,
    maxOutputTokens: 8192,
    model: {
      async complete() {
        models++;
        began.resolve();
        await gate.promise;
        return final();
      },
      close() {},
    },
  };
  let engine = new RuntimeEngine(options);
  let work: Promise<unknown> | undefined;
  try {
    await engine.initialize();
    work = engine.dispatch(
      "agents",
      "send",
      { agentId: "agent_review", text: "task" },
      "agent_engine",
    );
    await began.promise;
    const dest = `${directory}/crash`;
    await copyPrefix(engine.agents.directory, `${dest}/agents`, engine.agents.journal.count);
    // Persist the admitted model action, but no model response: recovery must not retry it.
    const modelAction = engine.transactions.events.find((e) =>
      e.kind === "request" && (e.data as { id: string }).id === "agent_engine_model_1"
    )!;
    await Deno.mkdir(`${dest}/actions/events`, { recursive: true });
    for (let seq = 1; seq <= modelAction.seq; seq++) {
      const name = `${String(seq).padStart(16, "0")}.json`;
      await Deno.copyFile(
        `${options.directory}/actions/events/${name}`,
        `${dest}/actions/events/${name}`,
      );
    }
    gate.resolve();
    await work;
    await engine.close();
    engine = new RuntimeEngine({ ...options, directory: dest });
    await engine.initialize();
    const done = await engine.agents.send(
      { agentId: "agent_review", text: "task" },
      "agent_engine",
    );
    assert.equal(done.status, "failed");
    assert.equal(done.error?.kind, "unknown");
    assert.equal(models, 1);
    assert.ok(!done.error?.message.includes("不可用"));
    assert.equal(engine.available, true);
  } finally {
    gate.resolve();
    await work?.catch(() => {});
    await engine.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("agent restore engine: accepted-only job automatically executes after startup", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agent-engine-admit-" });
  await writeDurableJson(`${directory}/settings`, {
    dailyTokenBudget: 1000000,
    paused: false,
    timeZone: "UTC",
  });
  let models = 0;
  const resumed = deferred();
  const options = {
    directory: `${directory}/source`,
    workspace: `${directory}/workspace`,
    settingsFile: `${directory}/settings`,
    maxOutputTokens: 8192,
    model: {
      complete() {
        models++;
        if (models === 2) resumed.resolve();
        return Promise.resolve(final());
      },
      close() {},
    },
  };
  let engine = new RuntimeEngine(options);
  try {
    await engine.initialize();
    const args = { agentId: "agent_coding", text: "task" };
    const job = await engine.dispatch("agents", "send", args, "agent_admit") as unknown as AgentJob;
    const dest = `${directory}/crash`;
    await copyPrefix(engine.agents.directory, `${dest}/agents`, job.requestSeq);
    await engine.close();
    engine = new RuntimeEngine({ ...options, directory: dest });
    await engine.initialize();
    await resumed.promise;
    const done = await engine.agents.send(args, job.jobId);
    assert.equal(done.status, "completed");
    assert.equal(models, 2);
    assert.equal(engine.agents.journal.events.filter((e) => e.kind === "accepted").length, 1);
  } finally {
    await engine.close();
    await Deno.remove(directory, { recursive: true });
  }
});
