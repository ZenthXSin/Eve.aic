import assert from "node:assert/strict";
import type { ModelResponse, ToolCall } from "../contracts/model.ts";
import type { Json } from "../kernel/protocol.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import { SCRIPT_TOOL } from "../plugins/script-runtime/mod.ts";
import { projectThoughts, THOUGHT_MAX_LENGTH } from "../plugins/eve/thoughts.ts";
import { pendingTools, render } from "../plugins/eve/view.ts";
import { recallQuery } from "../plugins/eve/recall-query.ts";

const call = (name: string, thought: Json, code?: string): ToolCall => ({
  id: "provider-id",
  name,
  args: { thought, ...(code === undefined ? {} : { code }) },
});

Deno.test("随记投影：普通文字与两工具同轮只表达一次，保留原件与其他参数", () => {
  const response: ModelResponse = {
    content: "先看看花。",
    toolCalls: [
      call("execute_script", "先看看花。", "return 1"),
      call("write_thought", "我喜欢今天的安静。"),
      call("execute_script", "我喜欢今天的安静。", "return 2"),
    ],
  };
  const original = JSON.stringify(response);
  const projected = projectThoughts(response);
  assert.equal(projected.content, "先看看花。\n\n我喜欢今天的安静。");
  assert.deepEqual(projected.thoughts, ["先看看花。", "我喜欢今天的安静。"]);
  assert.deepEqual(projected.toolCalls.map((c) => c.args), [{ code: "return 1" }, {}, {
    code: "return 2",
  }]);
  assert.equal(JSON.stringify(response), original);
  assert.equal(projectThoughts(response).content, projected.content);
  // 不把其他工具的业务字段、空值或过长参数伪称想法。
  const invalid: ModelResponse = {
    content: "旧正文",
    toolCalls: [
      call("unrelated", "业务参数"),
      call("execute_script", " "),
      call("write_thought", "x".repeat(THOUGHT_MAX_LENGTH + 1)),
      { id: "null", name: "execute_script", args: null },
    ],
  };
  assert.equal(projectThoughts(invalid).content, "旧正文");
  assert.deepEqual(projectThoughts(invalid).thoughts, []);
  assert.deepEqual(projectThoughts({ content: "旧\n\n正文", toolCalls: [] }), {
    content: "旧\n\n正文",
    thoughts: [],
    toolCalls: [],
  });
});

async function fixture(response: ModelResponse) {
  const directory = await Deno.makeTempDir({ prefix: "eve-thought-tools-" });
  await Deno.writeTextFile(`${directory}/identity`, "你是 Eve。");
  const dispatched: ToolCall[] = [];
  let modelCalls = 0;
  const ctx = {
    call: (contract: string, method: string, args: Json) => {
      const address = `${contract}.${method}`;
      let result: unknown;
      if (address === "tools.list") result = [SCRIPT_TOOL];
      else if (address === "model.complete") {
        const request = args as { tools: { name: string }[] };
        assert.deepEqual(request.tools.map((t) => t.name), ["execute_script", "write_thought"]);
        result = ++modelCalls === 1 ? response : { content: "先休息。", toolCalls: [] };
      } else if (address === "tools.execute") {
        dispatched.push(args as unknown as ToolCall);
        result = { status: "completed", output: "1" };
      } else if (["scripts.observations", "scripts.images"].includes(address)) {
        result = { items: [], throughSeq: 0, hasMore: false };
      } else if (["phone.notifications", "scripts.completions", "mind.poll"].includes(address)) {
        result = { items: [], throughSeq: 0, hasMore: false };
      } else if (address === "mind.update") result = null;
      else throw new Error(`未授权的测试调用 ${address}`);
      return Promise.resolve(result);
    },
  } as unknown as PluginContext;
  const config = {
    directory: `${directory}/journal`,
    promptFile: `${directory}/identity`,
    sourceCommit: "a".repeat(40),
    modelTimeoutMs: 1000,
    maxOutputTokens: 1000,
    pollMs: 10,
    defaultSleepMs: 86400000,
    dailyTokenBudget: 200_000_000,
  };
  let runtime = new EveRuntime(ctx, config);
  await runtime.initialize();
  return {
    current: () => runtime,
    dispatched,
    modelCalls: () => modelCalls,
    restart: async () => {
      await runtime.close();
      runtime = new EveRuntime(ctx, config);
      await runtime.initialize();
    },
    close: async () => {
      await runtime.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("随记原生工具：重启补齐调用与回执，正文只有一个事实来源且进入上下文与回忆", async () => {
  const thought = "想安静地看看最近群友怎么聊天。";
  const f = await fixture({ content: "", toolCalls: [call("write_thought", thought)] });
  try {
    await f.current().pulse();
    const response = f.current().journal.events.find((e) => e.kind === "model_response")!;
    assert.equal(pendingTools(f.current().journal.events).length, 1);
    // 模拟正文落盘后、执行前中断；恢复不要求新模型再输出一次。
    await f.restart();
    await f.current().pulse();
    assert.equal(f.modelCalls(), 1);
    assert.equal(f.dispatched.length, 0);
    assert.equal(pendingTools(f.current().journal.events).length, 0);
    const events = f.current().journal.events;
    assert.equal(JSON.stringify(events).split(thought).length - 1, 1);
    const messages = render(events, "身份");
    const assistant = messages.find((m) => m.role === "assistant")!;
    const receipt = messages.find((m) => m.role === "tool")!;
    assert.equal(assistant.content, thought);
    assert.equal(assistant.toolCalls![0].id, receipt.toolCallId);
    assert.deepEqual(assistant.toolCalls![0].args, {});
    assert.equal(receipt.content.includes(thought), false);
    const result = events.find((e) => e.kind === "tool_result")!.data as {
      result: { status: string; output: string };
    };
    assert.equal(result.result.status, "completed");
    assert.deepEqual(JSON.parse(result.result.output), {
      recorded: true,
      responseSeq: response.seq,
      toolIndex: 0,
    });
    assert.equal(recallQuery([response]), thought);
    await f.restart();
    await f.current().pulse();
    assert.equal(f.current().journal.events.filter((e) => e.kind === "tool_result").length, 1);
    assert.equal(f.dispatched.length, 0);
  } finally {
    await f.close();
  }
});

Deno.test("随记脚本参数：剥离元数据后派发一次，跨轮同文仍保留，回滚移除未接受想法", async () => {
  const thought = "我想整理今天的便签。";
  const f = await fixture({
    content: thought,
    toolCalls: [call("execute_script", thought, "return 1")],
  });
  try {
    await f.current().pulse();
    const response = f.current().journal.events.find((e) => e.kind === "model_response")!;
    await f.current().pulse();
    assert.equal(f.dispatched.length, 1);
    assert.deepEqual(f.dispatched[0].args, { code: "return 1" });
    const messages = render(f.current().journal.events, "身份");
    assert.equal(JSON.stringify(messages).split(thought).length - 1, 1);
    await f.current().journal.append("model_response", {
      response: { content: thought, toolCalls: [] },
    });
    assert.equal(
      JSON.stringify(render(f.current().journal.events, "身份")).split(thought).length - 1,
      2,
    );
    const throughSeq = f.current().journal.events.at(-1)!.seq;
    await f.current().journal.append("upgrade_rollback", {
      afterSeq: response.seq - 1,
      throughSeq,
    });
    assert.equal(
      render(f.current().journal.events, "身份").some((m) => m.role === "assistant"),
      false,
    );
    assert.ok(JSON.stringify(f.current().journal.events).includes(thought));
  } finally {
    await f.close();
  }
});

Deno.test("随记原生工具：已开始未完成的恢复不再写正文，无效参数有失败回执", async () => {
  const f = await fixture({
    content: "",
    toolCalls: [call("write_thought", "只有这一次随记。"), {
      ...call("write_thought", " "),
      id: "second-provider-id",
    }],
  });
  try {
    await f.current().pulse();
    const [pending] = pendingTools(f.current().journal.events);
    await f.current().journal.append("tool_start", {
      responseSeq: pending.responseSeq,
      index: pending.index,
      callId: pending.call.id,
      call: { ...pending.call, args: {} } as unknown as Json,
    });
    await f.restart();
    await f.current().pulse();
    assert.equal(f.dispatched.length, 0);
    const results = f.current().journal.events.filter((e) => e.kind === "tool_result");
    assert.deepEqual(results.map((e) => (e.data as { result: { status: string } }).result.status), [
      "completed",
      "failed",
    ]);
    assert.equal(pendingTools(f.current().journal.events).length, 0);
    assert.equal(
      JSON.stringify(f.current().journal.events).split("只有这一次随记。").length - 1,
      1,
    );
  } finally {
    await f.close();
  }
});

Deno.test("随记脚本参数：非法正文不静默丢弃或执行脚本，保留原件和明确失败回执", async () => {
  const invalid = [null, 12, " ", "x".repeat(THOUGHT_MAX_LENGTH + 1)];
  const f = await fixture({
    content: "",
    toolCalls: invalid.map((thought, index) => ({
      ...call("execute_script", thought, "return 1"),
      id: `invalid-${index}`,
    })),
  });
  try {
    await f.current().pulse();
    const response = f.current().journal.events.find((e) => e.kind === "model_response")!;
    const original = JSON.stringify(response);
    await f.current().pulse();
    assert.equal(f.dispatched.length, 0);
    assert.equal(pendingTools(f.current().journal.events).length, 0);
    const results = f.current().journal.events.filter((e) => e.kind === "tool_result");
    assert.equal(results.length, invalid.length);
    for (const event of results) {
      const result = (event.data as { result: { status: string; output: string } }).result;
      assert.equal(result.status, "failed");
      assert.ok(result.output.includes("thought参数无效"));
    }
    assert.equal(JSON.stringify(response), original);
    assert.equal(
      render(f.current().journal.events, "身份").filter((m) => m.role === "tool").length,
      invalid.length,
    );
  } finally {
    await f.close();
  }
});
