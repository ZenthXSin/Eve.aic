import assert from "node:assert/strict";
import { composeModelPlugin } from "../composition/model.ts";
import type { ModelRequest } from "../contracts/model.ts";
import { Kernel } from "../kernel/kernel.ts";
import {
  ChatModelClient,
  ChatModelError,
  type ChatModelOptions,
} from "../plugins/model-chat/client.ts";

const options: ChatModelOptions = {
  baseUrl: "http://127.0.0.1",
  model: "fixture-model",
  apiKey: "fixture-secret",
  sessionId: "eve-consciousness-test",
  reasoningEffort: "high",
  maxOutputTokens: 8192,
  timeoutMs: 1000,
};
const request: ModelRequest = {
  messages: [{ role: "system", content: "固定身份" }, { role: "user", content: "检验回声" }],
  tools: [{
    name: "echo",
    description: "原样返回文字",
    parameters: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
  }],
};
const completion = (content = "结果", usage?: unknown) => ({
  choices: [{ finish_reason: "stop", message: { role: "assistant", content } }],
  ...(usage === undefined ? {} : { usage }),
});

function server(handler: (r: Request) => Response | Promise<Response>) {
  const instance = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, handler);
  return { url: `http://127.0.0.1:${instance.addr.port}`, close: () => instance.shutdown() };
}

Deno.test("模型传输：真实 HTTP 参数、固定会话、无 temperature，工具回传配对", async () => {
  const bodies: Record<string, unknown>[] = [];
  const local = server(async (r) => {
    assert.equal(new URL(r.url).pathname, "/v1/chat/completions");
    assert.equal(r.headers.get("Authorization"), "Bearer fixture-secret");
    assert.equal(r.headers.get("session_id"), options.sessionId);
    assert.equal(r.headers.get("X-Session-ID"), options.sessionId);
    const body = await r.json();
    bodies.push(body);
    assert.equal(body.temperature, undefined);
    assert.equal(body.reasoning_effort, "high");
    assert.equal(body.prompt_cache_key, options.sessionId);
    assert.equal(body.max_completion_tokens, 8192);
    assert.equal(body.stream, false);
    if (bodies.length === 1) {
      return Response.json({
        choices: [{
          finish_reason: "tool_calls",
          message: {
            role: "assistant",
            tool_calls: [{
              id: "call-real-http",
              type: "function",
              function: { name: "echo", arguments: '{"text":"海风"}' },
            }],
          },
        }],
        usage: {
          prompt_tokens: 100,
          completion_tokens: 20,
          total_tokens: 120,
          prompt_tokens_details: { cached_tokens: 70 },
        },
      });
    }
    return Response.json(completion("海风"));
  });
  try {
    const client = new ChatModelClient({ ...options, baseUrl: `${local.url}/v1` });
    const first = await client.complete(request);
    assert.deepEqual(first.toolCalls, [{
      id: "call-real-http",
      name: "echo",
      args: { text: "海风" },
    }]);
    assert.deepEqual(first.usage, {
      status: "reported",
      inputTokens: 100,
      outputTokens: 20,
      totalTokens: 120,
      cachedInputTokens: 70,
    });
    const second = await client.complete({
      ...request,
      messages: [...request.messages, {
        role: "assistant",
        content: first.content,
        toolCalls: first.toolCalls,
      }, { role: "tool", toolCallId: first.toolCalls[0].id, content: "海风" }],
    });
    assert.equal(second.content, "海风");
    assert.equal(second.usage, undefined);
    const messages = bodies[1].messages as Record<string, unknown>[];
    assert.deepEqual(messages.slice(0, 2), bodies[0].messages);
    assert.deepEqual(messages.at(-1), {
      role: "tool",
      tool_call_id: "call-real-http",
      content: "海风",
    });
    client.close();
  } finally {
    await local.close();
  }
});

Deno.test("模型传输：缓存缺失、明确零值和非法值分别保留", async () => {
  let raw: unknown;
  const local = server(() => Response.json(completion("有效回答", raw)));
  try {
    const client = new ChatModelClient({ ...options, baseUrl: local.url });
    assert.equal((await client.complete(request)).usage, undefined);
    raw = { prompt_tokens: 10, completion_tokens: 2 };
    assert.deepEqual((await client.complete(request)).usage, {
      status: "partial",
      inputTokens: 10,
      outputTokens: 2,
    });
    raw = { prompt_tokens: 10, completion_tokens: 2, prompt_tokens_details: { cached_tokens: 0 } };
    assert.equal((await client.complete(request)).usage?.cachedInputTokens, 0);
    raw = { prompt_tokens: 10, completion_tokens: 2, prompt_tokens_details: { cached_tokens: 11 } };
    const invalid = (await client.complete(request)).usage;
    assert.equal(invalid?.status, "invalid");
    assert.equal(invalid?.cachedInputTokens, undefined);
    raw = null;
    assert.deepEqual((await client.complete(request)).usage, { status: "invalid" });
    client.close();
  } finally {
    await local.close();
  }
});

Deno.test("模型传输：拒绝坏 JSON、未完成响应、未知工具与不配对工具参数，不重试", async () => {
  let payload = completion() as unknown;
  let attempts = 0;
  const local = server(() => {
    attempts++;
    return payload === "bad-json" ? new Response("fixture-secret {") : Response.json(payload);
  });
  try {
    const client = new ChatModelClient({ ...options, baseUrl: local.url });
    const malformed = [
      "bad-json",
      { choices: [] },
      { choices: [{ finish_reason: "length", message: { role: "assistant", content: "截断" } }] },
      completion(""),
      { choices: [{ finish_reason: "tool_calls", message: { role: "assistant", content: null } }] },
      ...["missing", "echo"].map((name) => ({
        choices: [{
          finish_reason: "tool_calls",
          message: {
            role: "assistant",
            content: null,
            tool_calls: [{
              id: "call",
              type: "function",
              function: { name, arguments: name === "echo" ? "broken{" : "{}" },
            }],
          },
        }],
      })),
    ];
    for (const bad of malformed) {
      payload = bad;
      await assert.rejects(
        client.complete(request),
        (error) =>
          error instanceof ChatModelError && error.category === "invalid_response" &&
          !error.message.includes("fixture-secret"),
      );
    }
    assert.equal(attempts, malformed.length);
    await assert.rejects(
      client.complete({ ...request, tools: [{ name: "echo", description: "没有真实参数" }] }),
      (e) => e instanceof ChatModelError && e.category === "request",
    );
    await assert.rejects(
      client.complete({
        ...request,
        messages: [...request.messages, {
          role: "tool",
          content: "孤立结果",
          toolCallId: "missing",
        }],
      }),
      (e) => e instanceof ChatModelError && e.category === "request",
    );
    assert.equal(attempts, malformed.length);
    client.close();
  } finally {
    await local.close();
  }
});

Deno.test("模型传输：HTTP 拒绝、响应过大与超时不泄露错误正文、不自动重试", async () => {
  let status = 401;
  let attempts = 0;
  const local = server(() => {
    attempts++;
    return new Response(JSON.stringify(completion("fixture-secret".repeat(100))), { status });
  });
  try {
    const client = new ChatModelClient({ ...options, baseUrl: local.url, maxResponseBytes: 100 });
    await assert.rejects(
      client.complete(request),
      (e) =>
        e instanceof ChatModelError && e.httpStatus === 401 &&
        !e.message.includes("fixture-secret"),
    );
    status = 200;
    await assert.rejects(
      client.complete(request),
      (e) => e instanceof ChatModelError && e.category === "invalid_response",
    );
    assert.equal(attempts, 2);
    client.close();
    await assert.rejects(
      client.complete(request),
      (e) => e instanceof ChatModelError && e.category === "transport",
    );
    assert.equal(attempts, 2);
  } finally {
    await local.close();
  }
  let timeoutAttempts = 0;
  const slow = server(async () => {
    timeoutAttempts++;
    await new Promise((resolve) => setTimeout(resolve, 50));
    return Response.json(completion());
  });
  try {
    const client = new ChatModelClient({ ...options, baseUrl: slow.url, timeoutMs: 10 });
    await assert.rejects(
      client.complete(request),
      (e) => e instanceof ChatModelError && e.category === "transport",
    );
    client.close();
    assert.equal(timeoutAttempts, 1);
  } finally {
    await slow.close();
  }
});

Deno.test("模型插件：真实 worker 经内核工具回传，不把凭据放入插件入口或回执", async () => {
  let calls = 0;
  const local = server(async (r) => {
    const body = await r.json();
    calls++;
    if (calls === 1) {
      return Response.json({
        choices: [{
          finish_reason: "tool_calls",
          message: {
            role: "assistant",
            content: null,
            tool_calls: [{
              id: "tool-1",
              type: "function",
              function: { name: "echo", arguments: '{"text":"真实worker"}' },
            }],
          },
        }],
      });
    }
    assert.equal(body.messages.at(-1).content, "真实worker");
    return Response.json(completion("拿到了真实worker"));
  });
  const directory = await Deno.makeTempDir({ prefix: "eve-model-test-" });
  const root = new URL("..", import.meta.url).pathname;
  let kernel: Kernel | undefined;
  try {
    await Deno.writeTextFile(`${directory}/key`, options.apiKey);
    await Deno.writeTextFile(
      `${directory}/config.json`,
      JSON.stringify({
        model: {
          ...options,
          baseUrl: local.url,
          protocol: "chat",
          apiKey: undefined,
          apiKeyFile: `${directory}/key`,
        },
      }),
    );
    await composeModelPlugin(`${directory}/plugin`, `${directory}/config.json`);
    assert.equal(
      (await Deno.readTextFile(`${directory}/plugin/mod.ts`)).includes(options.apiKey),
      false,
    );
    kernel = new Kernel({ dataDir: `${directory}/data`, typecheck: false });
    for (
      const path of [`${directory}/plugin`, `${root}plugins/tools-echo`, `${root}plugins/turn-loop`]
    ) await kernel.register(path);
    await kernel.start();
    const response = await kernel.call("turn", "run", { input: "回声测试" });
    assert.deepEqual(response, { reply: "拿到了真实worker", modelRequests: 2, toolCalls: 1 });
    const history = await kernel.call<{ content: string }[]>("turn", "history");
    assert.equal(history.length, 4);
    assert.equal(JSON.stringify(history).includes(options.apiKey), false);
    assert.equal(calls, 2);
    assert.deepEqual(await kernel.stop(), []);
    kernel = undefined;
  } finally {
    await kernel?.stop();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});
