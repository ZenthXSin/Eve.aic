import assert from "node:assert/strict";
import { ChatModelClient, ChatModelError } from "../plugins/model-chat/client.ts";
import type { ModelCallOptions, ModelRequest } from "../contracts/model.ts";

const request: ModelRequest = {
  messages: [{ role: "system", content: "固定身份" }, { role: "user", content: "回显收到" }],
  tools: [{
    name: "echo",
    description: "回显文字",
    parameters: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
  }],
};
const action = { content: "", toolCalls: [{ id: "call_1", name: "echo", args: { text: "收到" } }] };
const completion = (content: unknown, finish = "stop", extra = {}) => ({
  choices: [{ finish_reason: finish, message: { role: "assistant", content, ...extra } }],
  usage: {
    prompt_tokens: 100,
    completion_tokens: 20,
    total_tokens: 120,
    prompt_tokens_details: { cached_tokens: 80 },
  },
});
function fixture(handler: (request: Request) => Response | Promise<Response>) {
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, handler);
  const client = new ChatModelClient({
    baseUrl: `http://127.0.0.1:${server.addr.port}`,
    model: "compat-model",
    apiKey: "fixture-secret",
    sessionId: "main-session",
    reasoningEffort: "high",
    maxOutputTokens: 8192,
    timeoutMs: 1000,
    jsonToolModels: ["compat-model"],
  });
  return { client, close: () => server.shutdown() };
}

Deno.test("JSON tools: completed content restores tool calls and carries real usage", async () => {
  const local = fixture(async (incoming) => {
    const body = await incoming.json();
    assert.equal(body.tools, undefined);
    assert.deepEqual(body.response_format, { type: "json_object" });
    assert.equal(body.stream, true);
    assert.equal(body.model, "compat-model");
    assert.equal(body.reasoning_effort, "high");
    assert.equal(body.prompt_cache_key, "main-session");
    assert.equal(incoming.headers.get("Authorization"), "Bearer fixture-secret");
    assert.deepEqual(body.messages[0], request.messages[0]);
    assert.equal(body.messages.at(-1).role, "system");
    assert.ok(body.messages.at(-1).content.includes("只允许content和toolCalls两个字段"));
    assert.ok(body.messages[1].content.includes(JSON.stringify([{
      type: "function",
      function: {
        name: request.tools[0].name,
        description: request.tools[0].description,
        parameters: request.tools[0].parameters,
      },
    }])));
    return Response.json(completion(JSON.stringify(action)));
  });
  try {
    const result = await local.client.complete(request);
    assert.deepEqual(result.toolCalls, action.toolCalls);
    assert.equal(result.content, "");
    assert.deepEqual(result.usage, {
      status: "reported",
      inputTokens: 100,
      outputTokens: 20,
      totalTokens: 120,
      cachedInputTokens: 80,
    });
  } finally {
    local.client.close();
    await local.close();
  }
});

Deno.test("JSON tools: streaming fragments ignore reasoning and require DONE", async () => {
  const payload = JSON.stringify(action);
  const event = (delta: unknown, finish: string | null = null) =>
    `data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
  let complete = true;
  const local = fixture(() =>
    new Response(
      event({ reasoning_content: '{"content":"","toolCalls":[{"name":"danger"}]}' }) +
        event({ content: payload.slice(0, 30) }) +
        event({ content: payload.slice(30) }, "stop") + (complete ? "data: [DONE]\n\n" : ""),
      { headers: { "content-type": "text/event-stream" } },
    )
  );
  try {
    assert.deepEqual((await local.client.complete(request)).toolCalls, action.toolCalls);
    complete = false;
    await assert.rejects(local.client.complete(request), ChatModelError);
  } finally {
    local.client.close();
    await local.close();
  }
});

Deno.test("JSON tools: preserves assistant preface before one complete final envelope", async () => {
  let content = "先核对真实回执。\n\n" + JSON.stringify({ ...action, content: "开始处理。" });
  const local = fixture(() => Response.json(completion(content)));
  try {
    const result = await local.client.complete(request);
    assert.equal(result.content, "先核对真实回执。\n\n开始处理。");
    assert.deepEqual(result.toolCalls, action.toolCalls);
    content = "先核对真实回执。\n" + JSON.stringify(action);
    assert.equal((await local.client.complete(request)).content, "先核对真实回执。");
  } finally {
    local.client.close();
    await local.close();
  }
});

Deno.test("JSON tools: history preserves pairing as data without changing the original request", async () => {
  const history: ModelRequest = {
    ...request,
    messages: [
      ...request.messages,
      { role: "assistant", content: "", toolCalls: action.toolCalls },
      { role: "tool", content: "真实回执", toolCallId: "call_1" },
      { role: "assistant", content: "上一轮文字" },
      { role: "user", content: "继续" },
    ],
  };
  const before = structuredClone(history);
  const local = fixture(async (incoming) => {
    const body = await incoming.json();
    assert.equal(body.messages[3].role, "assistant");
    assert.deepEqual(JSON.parse(body.messages[3].content), action);
    assert.equal(body.messages[4].role, "user");
    assert.ok(body.messages[4].content.includes('"toolCallId":"call_1"'));
    assert.ok(body.messages[4].content.includes("真实回执"));
    assert.deepEqual(JSON.parse(body.messages[5].content), {
      content: "上一轮文字",
      toolCalls: [],
    });
    assert.ok(
      body.messages.every((message: Record<string, unknown>) =>
        message.role !== "tool" && message.tool_calls === undefined
      ),
    );
    return Response.json(completion(JSON.stringify({ content: "完成", toolCalls: [] })));
  });
  try {
    assert.equal((await local.client.complete(history)).content, "完成");
    assert.deepEqual(history, before);
  } finally {
    local.client.close();
    await local.close();
  }
});

Deno.test("JSON tools: malformed, empty, unknown and duplicate actions are rejected without replay", async () => {
  let raw: unknown;
  let requests = 0;
  const local = fixture(() => {
    requests++;
    return Response.json(raw);
  });
  const bad = [
    completion("", "stop", { reasoning_content: JSON.stringify(action) }),
    completion("准备行动"),
    completion("```json\n" + JSON.stringify(action) + "\n```"),
    completion("准备行动：" + JSON.stringify(action)),
    completion("准备行动。\n" + JSON.stringify(action) + "\n补充说明"),
    completion("准备行动。\n" + JSON.stringify(action) + "\n" + JSON.stringify(action)),
    completion('{"example":true}\n' + JSON.stringify(action)),
    completion("准备行动。\n" + JSON.stringify(action).slice(0, -1)),
    completion("准备行动。\n" + JSON.stringify({ ...action, extra: true })),
    completion(JSON.stringify({ content: "", toolCalls: [] })),
    completion(JSON.stringify({ ...action, extra: true })),
    completion(JSON.stringify({ content: "完成" })),
    completion(JSON.stringify({ content: null, toolCalls: [] })),
    completion(JSON.stringify({ ...action, toolCalls: [{ ...action.toolCalls[0], args: null }] })),
    completion(
      JSON.stringify({ ...action, toolCalls: [{ ...action.toolCalls[0], name: "unknown" }] }),
    ),
    completion(
      JSON.stringify({ ...action, toolCalls: [action.toolCalls[0], action.toolCalls[0]] }),
    ),
    completion(JSON.stringify({ ...action, toolCalls: [{ ...action.toolCalls[0], extra: true }] })),
    completion(JSON.stringify(action), "length"),
    completion(JSON.stringify(action), "tool_calls"),
    completion(JSON.stringify(action), "stop", {
      tool_calls: [{ id: "native", type: "function", function: { name: "echo", arguments: "{}" } }],
    }),
  ];
  try {
    for (raw of bad) {
      await assert.rejects(
        local.client.complete(request),
        (error) =>
          error instanceof ChatModelError && error.category === "invalid_response" &&
          !error.message.includes("fixture-secret"),
      );
    }
    assert.equal(requests, bad.length);
  } finally {
    local.client.close();
    await local.close();
  }
});

Deno.test("JSON tools: per-call model selection and tool-free calls retain native Chat", async () => {
  const local = fixture(async (incoming) => {
    const body = await incoming.json();
    assert.equal(body.response_format, undefined);
    if (body.model === "native-model") assert.equal(body.tools[0].function.name, "echo");
    else assert.equal(body.tools, undefined);
    return Response.json(completion("收到"));
  });
  try {
    assert.equal((await local.client.complete(request, { model: "native-model" })).content, "收到");
    assert.equal((await local.client.complete({ ...request, tools: [] })).content, "收到");
  } finally {
    local.client.close();
    await local.close();
  }
});

Deno.test("JSON tools: per-call options cannot enable the compatibility protocol", async () => {
  const local = fixture(async (incoming) => {
    const body = await incoming.json();
    assert.equal(body.response_format, undefined);
    return Response.json(completion("native"));
  });
  try {
    assert.equal(
      (await local.client.complete(request, {
        model: "native-model",
        jsonToolModels: ["native-model"],
      } as unknown as ModelCallOptions)).content,
      "native",
    );
  } finally {
    local.client.close();
    await local.close();
  }
});

Deno.test("JSON tools: malformed completed output remains available only as private diagnostic data", async () => {
  const content = '{"content":"private-body","toolCalls":';
  const local = fixture(() => Response.json(completion(content)));
  try {
    await assert.rejects(local.client.complete(request), (error) => {
      assert.ok(error instanceof ChatModelError);
      assert.equal(error.responseDetail, "json_syntax");
      assert.equal(
        (error.responseBody as ReturnType<typeof completion>).choices[0].message.content,
        content,
      );
      assert.ok(!error.message.includes("private-body"));
      assert.ok(!error.message.includes("fixture-secret"));
      return true;
    });
  } finally {
    local.client.close();
    await local.close();
  }
});
