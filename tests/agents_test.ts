import assert from "node:assert/strict";
import { type AgentHooks, AgentService } from "../services/runtime/agents.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import type { ModelCallOptions, ModelRequest, ModelResponse } from "../contracts/model.ts";
import { startRuntimeRpc } from "../services/runtime/rpc.ts";
import { RuntimeClient } from "../services/runtime/client.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";
import type { AgentJob } from "../contracts/agents.ts";
import { KernelError } from "../kernel/protocol.ts";

function deferred<T = void>() {
  let resolve!: (v: T | PromiseLike<T>) => void;
  const promise = new Promise<T>((r) => resolve = r);
  return { promise, resolve };
}
const response = (content: string): ModelResponse => ({
  content,
  toolCalls: [],
  usage: { status: "reported", inputTokens: 100, cachedInputTokens: 80, outputTokens: 10 },
});
async function fixture(hooks: Partial<AgentHooks> = {}) {
  const directory = await Deno.makeTempDir({ prefix: "eve-agents-test-" });
  const api = new AgentService(directory, {
    complete: () => Promise.resolve(response("完整回复")),
    invoke: () => Promise.resolve({ done: true }),
    ...hooks,
  });
  await api.initialize();
  return {
    api,
    directory,
    async close() {
      await api.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}
Deno.test("agents: three high defaults, duplicate roles, isolated configuration and persistent conversations", async () => {
  const calls: { request: ModelRequest; options: ModelCallOptions; id: string }[] = [];
  const f = await fixture({
    complete(request, options, id) {
      calls.push({ request: structuredClone(request), options: structuredClone(options), id });
      return Promise.resolve(response("单条完整回复"));
    },
  });
  try {
    const defaults = f.api.list().items;
    assert.equal(defaults.length, 3);
    assert.deepEqual(defaults.map((x) => x.config.reasoningEffort), ["high", "high", "high"]);
    const first = await f.api.create({
      role: "coding",
      name: "编码甲",
      model: "custom-model",
      reasoningEffort: "low",
      instructions: "检查原件",
      maxOutputTokens: 12345,
    }, "create_one");
    const second = await f.api.create(
      { role: "coding", name: "编码乙", reasoningEffort: "xhigh" },
      "create_two",
    );
    assert.notEqual(first.agentId, second.agentId);
    await f.api.send({ agentId: first.agentId, text: "只属于甲的第一条" }, "agent_first");
    await f.api.send({ agentId: first.agentId, text: "甲的追问" }, "agent_followup");
    await f.api.send({ agentId: second.agentId, text: "只属于乙" }, "agent_other");
    assert.equal(calls[0].options.reasoningEffort, "low");
    assert.equal(calls[0].options.model, "custom-model");
    assert.equal(calls[0].options.maxOutputTokens, 12345);
    assert.equal(calls[2].options.reasoningEffort, "xhigh");
    assert.ok(calls[1].request.messages.some((m) => m.content === "只属于甲的第一条"));
    assert.ok(!calls[2].request.messages.some((m) => m.content.includes("只属于甲")));
    assert.equal(f.api.list().items.find((x) => x.agentId === first.agentId)!.unread, 2);
    const before = f.api.notifications();
    assert.equal(before.items.length, 3);
    assert.equal(f.api.history({ agentId: first.agentId }).messages.length, 4);
    assert.equal(
      f.api.list().items.find((x) => x.agentId === first.agentId)!.unread,
      2,
      "history does not mark read",
    );
    await f.api.open({ agentId: first.agentId });
    assert.equal(f.api.notifications().items.length, 1);
    assert.equal(f.api.notifications({ afterSeq: before.throughSeq }).items.length, 0);
    await f.api.configure({
      agentId: second.agentId,
      model: "different",
      reasoningEffort: "medium",
    });
    await f.api.close();
    const restored = new AgentService(f.directory, {
      complete: () => {
        throw new Error("must not run idle model");
      },
      invoke: () => Promise.resolve(null),
    });
    await restored.initialize();
    try {
      assert.equal(restored.list().items.length, 5);
      assert.equal(
        restored.list().items.find((x) => x.agentId === second.agentId)!.config.reasoningEffort,
        "medium",
      );
      assert.equal(restored.notifications().items.length, 1);
      assert.equal(restored.list().items.find((x) => x.agentId === first.agentId)!.unread, 0);
    } finally {
      await restored.close();
    }
  } finally {
    await f.close();
  }
});
Deno.test("agents: same instance is FIFO, different instances parallel; queued config is captured and cancellation does not execute", async () => {
  const gate = deferred(), parallel = deferred(), began = deferred();
  let calls = 0;
  const efforts: string[] = [];
  const f = await fixture({
    async complete(request, options) {
      calls++;
      efforts.push(options.reasoningEffort!);
      const text = request.messages.at(-1)!.content;
      if (text === "first") {
        began.resolve();
        await gate.promise;
      }
      if (text === "parallel") parallel.resolve();
      return response(text);
    },
  });
  const works: Promise<unknown>[] = [];
  try {
    const a = f.api.send({ agentId: "agent_coding", text: "first" }, "agent_a");
    works.push(a);
    await began.promise;
    const b = f.api.send({ agentId: "agent_coding", text: "second" }, "agent_b");
    works.push(b);
    await f.api.accepted("agent_b", new AbortController().signal);
    await f.api.configure({ agentId: "agent_coding", reasoningEffort: "low" });
    const c = f.api.send({ agentId: "agent_review", text: "parallel" }, "agent_c");
    works.push(c);
    await parallel.promise;
    assert.equal(calls, 2);
    await f.api.cancel({ jobId: "agent_b" });
    gate.resolve();
    await Promise.all(works);
    assert.equal(calls, 2);
    assert.equal(f.api.status({ jobId: "agent_b" }).status, "cancelled");
    await f.api.send({ agentId: "agent_coding", text: "after" }, "agent_d");
    assert.equal(efforts.at(-1), "low");
    assert.equal(f.api.list().items.find((x) => x.agentId === "agent_coding")!.pending, 0);
  } finally {
    gate.resolve();
    await Promise.allSettled(works);
    await f.close();
  }
});
Deno.test("agents: full long messages remain one original, Unicode pages and search work beyond previews", async () => {
  const long = "😀完整正文\n".repeat(40000) + "TAIL-UNIQUE";
  const f = await fixture({ complete: () => Promise.resolve(response(long)) });
  try {
    const job = await f.api.send({ agentId: "agent_investigation", text: long }, "agent_long");
    const history = f.api.history({ agentId: job.agentId });
    assert.equal(history.messages.length, 2);
    assert.equal(history.messages[1].truncated, true);
    const full = await f.api.message({ seq: job.replySeq! });
    assert.equal(full.text, long);
    assert.equal(full.characters, [...long].length);
    const page = await f.api.message({ seq: job.replySeq!, startChar: 1, limit: 8 });
    assert.equal(page.text, [...long].slice(1, 9).join(""));
    assert.equal(page.nextChar, 9);
    assert.equal(
      (await f.api.search({ agentId: job.agentId, query: "TAIL-UNIQUE" })).messages.length,
      2,
    );
    assert.equal(
      f.api.notifications().items.length,
      1,
      "one reply wakes once regardless of text length",
    );
    const next = f.api.notifications().throughSeq;
    await f.api.configure({ agentId: job.agentId, name: "调查改名" });
    assert.equal(
      f.api.notifications({ afterSeq: next }).throughSeq,
      next,
      "metadata changes do not produce empty wake traffic",
    );
  } finally {
    await f.close();
  }
});
Deno.test("agents: tool loop preserves pairing, receipts, final one-message reply and errors", async () => {
  let count = 0, invoked = 0;
  const requests: ModelRequest[] = [];
  const f = await fixture({
    complete(request) {
      requests.push(structuredClone(request));
      return Promise.resolve(
        ++count === 1
          ? {
            content: "先读文件",
            toolCalls: [{
              id: "read_1",
              name: "use_tool",
              args: { namespace: "files", method: "read", args: { path: "main.ts" } },
            }],
          }
          : response("审查已完成"),
      );
    },
    invoke(namespace, method, args) {
      assert.equal(namespace, "files");
      assert.equal(method, "read");
      assert.equal(args.path, "main.ts");
      invoked++;
      return Promise.resolve({ text: "source" });
    },
  });
  try {
    const job = await f.api.send({ agentId: "agent_review", text: "审查实现" }, "agent_tools");
    assert.equal(invoked, 1);
    assert.equal(requests.length, 2);
    assert.equal(requests[1].messages.at(-1)!.role, "tool");
    assert.equal(requests[1].messages.at(-1)!.toolCallId, "read_1");
    assert.equal(f.api.history({ agentId: job.agentId }).messages.length, 2);
    assert.equal((await f.api.message({ seq: job.replySeq! })).text, "审查已完成");
  } finally {
    await f.close();
  }
});
Deno.test("agents: interrupted accepted job becomes one unknown reply after restart, never replays; tampered body fails closed", async () => {
  const f = await fixture();
  try {
    const job = await f.api.send({ agentId: "agent_review", text: "recover" }, "agent_original");
    await f.api.close();
    // Copy a valid prefix ending immediately after admission to simulate power loss.
    const interrupted = `${f.directory}/interrupted`;
    await Deno.mkdir(`${interrupted}/events`, { recursive: true });
    await Deno.mkdir(`${interrupted}/bodies`);
    for await (const file of Deno.readDir(`${f.directory}/bodies`)) {
      await Deno.copyFile(
        `${f.directory}/bodies/${file.name}`,
        `${interrupted}/bodies/${file.name}`,
      );
    }
    for (let seq = 1; seq <= job.requestSeq; seq++) {
      const name = `${String(seq).padStart(16, "0")}.json`;
      await Deno.copyFile(`${f.directory}/events/${name}`, `${interrupted}/events/${name}`);
    }
    let models = 0;
    const recovered = new AgentService(interrupted, {
      complete() {
        models++;
        return Promise.resolve(response("must not replay"));
      },
      invoke: () => Promise.resolve(null),
    });
    await recovered.initialize();
    assert.equal(models, 0);
    assert.equal(recovered.status({ jobId: job.jobId }).status, "unknown");
    assert.equal(recovered.notifications().items.length, 1);
    await recovered.close();
    const again = new AgentService(interrupted, {
      complete() {
        throw new Error("replay");
      },
      invoke: () => Promise.resolve(null),
    });
    await again.initialize();
    assert.equal(again.notifications().items.length, 1);
    await again.close();
    const accepted = JSON.parse(
      await Deno.readTextFile(
        `${interrupted}/events/${String(job.requestSeq).padStart(16, "0")}.json`,
      ),
    );
    const body = { name: accepted.data.message.body.hash + ".json" };
    await Deno.writeTextFile(`${interrupted}/bodies/${body.name}`, '"tampered"\n');
    const corrupt = new AgentService(interrupted, {
      complete: () => Promise.resolve(response("")),
      invoke: () => Promise.resolve(null),
    });
    try {
      await assert.rejects(corrupt.initialize(), /原件缺失或变化/);
    } finally {
      await corrupt.close();
    }
  } finally {
    await f.close();
  }
});

Deno.test("agents RPC: send returns after durable admission, holds ownership until actual work ends, deduplicates and shares budget", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agents-rpc-" });
  const authority = new OwnershipAuthority(`${directory}/guardian`, 60000);
  await authority.initialize();
  const credential = "fixture_agent_credential_012345678901234567890";
  await authority.register({ instanceId: "test", credential, sourceCommit: "a".repeat(40) });
  const granted = await authority.grant({
    instanceId: "test",
    requestId: "initial",
    expectedEpoch: authority.status().epoch,
  });
  assert.equal(granted.status, "granted");
  if (granted.status !== "granted") throw new Error("grant");
  const gate = deferred(), began = deferred();
  let paid = 0;
  const configs: ModelCallOptions[] = [];
  await writeDurableJson(`${directory}/settings`, {
    dailyTokenBudget: 10000000,
    paused: false,
    timeZone: "UTC",
  });
  const engine = new RuntimeEngine({
    directory: `${directory}/data`,
    workspace: `${directory}/workspace`,
    settingsFile: `${directory}/settings`,
    maxOutputTokens: 8192,
    model: {
      async complete(_request, config) {
        configs.push(config!);
        paid++;
        began.resolve();
        await gate.promise;
        return response("真实异步回复");
      },
      close() {},
    },
  });
  await engine.initialize();
  const fence = new OwnershipFence(`${directory}/fence`, "runtime", authority);
  await fence.initialize();
  const key = "fixture_agent_runtime_01234567890123456789";
  const rpc = await startRuntimeRpc(engine, key, fence);
  await writeDurableJson(`${directory}/connection`, { url: rpc.url });
  await Deno.writeTextFile(`${directory}/key`, key);
  await writeDurableJson(`${directory}/proof`, {
    instanceId: "test",
    credential,
    epoch: granted.owner.epoch,
  });
  const client = new RuntimeClient({
    connectionFile: `${directory}/connection`,
    keyFile: `${directory}/key`,
    proofFile: `${directory}/proof`,
  });
  try {
    const text = "普通消息";
    const sent = await client.call(
      "agents",
      "send",
      { agentId: "agent_coding", text },
      "agent_rpc_send",
    ) as unknown as AgentJob;
    await began.promise;
    assert.ok(["queued", "running"].includes(sent.status));
    assert.equal(authority.status().activePermits, 1);
    assert.equal(paid, 1);
    assert.equal(configs[0].reasoningEffort, "high");
    // A repeated action keeps the same queue item; it cannot become a second model request.
    const duplicate = client.call(
      "agents",
      "send",
      { agentId: "agent_coding", text },
      "agent_rpc_send",
    );
    const transactions = engine.transactions.events.length;
    await client.call("agents", "notifications");
    assert.equal(
      engine.transactions.events.length,
      transactions,
      "idle reads do not fsync duplicate ledgers",
    );
    gate.resolve();
    await engine.dispatch("agents", "send", { agentId: "agent_coding", text }, "agent_rpc_send");
    await duplicate;
    await rpc.quiesce();
    assert.equal(paid, 1);
    assert.equal(authority.status().activePermits, 0);
    const job = await client.call("agents", "status", { jobId: sent.jobId }) as unknown as AgentJob;
    assert.equal(job.status, "completed");
    assert.equal(engine.agents.notifications().items.length, 1);
    assert.equal((await engine.budget.status()).knownNoncachedTokens, 30);
    // A completed task cannot bypass ownership validation on a repeated send.
    await writeDurableJson(`${directory}/proof`, {
      instanceId: "test",
      credential,
      epoch: granted.owner.epoch + 1,
    });
    await assert.rejects(
      client.call("agents", "send", { agentId: "agent_coding", text }, "agent_rpc_send"),
    );
    await writeDurableJson(`${directory}/proof`, {
      instanceId: "test",
      credential,
      epoch: granted.owner.epoch,
    });
    const notifications = engine.agents.notifications().throughSeq;
    await client.call("agents", "send", { agentId: "agent_coding", text }, "agent_rpc_send");
    assert.equal(engine.agents.notifications().throughSeq, notifications);
    await assert.rejects(
      client.call("agents", "send", { agentId: "agent_coding", text: "changed" }, "agent_rpc_send"),
      (e) => e instanceof KernelError && e.kind === "rejected",
    );
    // Cross the former RPC and journal caps without splitting the original message.
    const huge = "z".repeat(17 * 1024 * 1024) + "END";
    const large = await client.call(
      "agents",
      "send",
      { agentId: "agent_review", text: huge },
      "agent_rpc_large",
    ) as unknown as AgentJob;
    await engine.dispatch(
      "agents",
      "send",
      { agentId: "agent_review", text: huge },
      "agent_rpc_large",
    );
    const original = await client.call("agents", "message", { seq: large.requestSeq }) as {
      text: string;
    };
    assert.equal(original.text, huge);
    assert.equal(engine.agents.history({ agentId: "agent_review" }).messages.length, 2);
  } finally {
    gate.resolve();
    client.close();
    await rpc.close();
    await authority.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("agents: restart recovers persisted final response before transcript or reply commit without model replay", async () => {
  const f = await fixture();
  try {
    const job = await f.api.send(
      { agentId: "agent_review", text: "recover final" },
      "recover_final",
    );
    await f.api.close();
    for (const withStep of [false, true]) {
      const dest = `${f.directory}/final-${withStep}`;
      await Deno.mkdir(`${dest}/events`, { recursive: true });
      await Deno.mkdir(`${dest}/bodies`);
      for await (const file of Deno.readDir(`${f.directory}/bodies`)) {
        await Deno.copyFile(`${f.directory}/bodies/${file.name}`, `${dest}/bodies/${file.name}`);
      }
      for (const event of f.api.journal.events) {
        if (event.seq >= job.replySeq! || (!withStep && event.kind === "step")) break;
        const name = `${String(event.seq).padStart(16, "0")}.json`;
        await Deno.copyFile(`${f.directory}/events/${name}`, `${dest}/events/${name}`);
      }
      let queried = 0;
      const restored = new AgentService(dest, {
        result(id) {
          queried++;
          return id === "recover_final_model_1" ? response("完整回复") : undefined;
        },
        complete() {
          throw new Error("paid replay");
        },
        invoke() {
          throw new Error("tool replay");
        },
      });
      await restored.initialize();
      try {
        const done = restored.status({ jobId: job.jobId });
        assert.equal(done.status, "completed");
        assert.equal((await restored.message({ seq: done.replySeq! })).text, "完整回复");
        assert.equal(restored.notifications().items.length, 1);
        assert.equal(queried, 1);
      } finally {
        await restored.close();
      }
    }
  } finally {
    await f.close();
  }
});

Deno.test("agents engine: committed reply restores the outer send receipt after interrupted result commit", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-agents-engine-recovery-" });
  await writeDurableJson(`${directory}/settings`, {
    dailyTokenBudget: 1000000,
    paused: false,
    timeZone: "UTC",
  });
  let calls = 0;
  const options = {
    directory: `${directory}/data`,
    workspace: `${directory}/workspace`,
    settingsFile: `${directory}/settings`,
    maxOutputTokens: 8192,
    model: {
      complete() {
        calls++;
        return Promise.resolve(response("已完成原回复"));
      },
      close() {},
    },
  };
  let engine = new RuntimeEngine(options);
  await engine.initialize();
  try {
    const args = { agentId: "agent_review", text: "review" };
    const job = await engine.dispatch(
      "agents",
      "send",
      args,
      "agent_recover_receipt",
    ) as unknown as AgentJob;
    const last = engine.transactions.events.at(-1)!;
    assert.equal(last.kind, "result");
    assert.equal((last.data as { id: string }).id, job.jobId);
    await engine.close();
    await Deno.remove(
      `${directory}/data/actions/events/${String(last.seq).padStart(16, "0")}.json`,
    );
    engine = new RuntimeEngine(options);
    await engine.initialize();
    assert.equal(
      (await engine.dispatch("agents", "send", args, job.jobId) as unknown as AgentJob).status,
      "completed",
    );
    assert.equal(calls, 1);
    assert.equal(engine.agents.notifications().items.length, 1);
  } finally {
    await engine.close();
    await Deno.remove(directory, { recursive: true });
  }
});
