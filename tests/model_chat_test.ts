import assert from "node:assert/strict";
import { composeModelPlugin } from "../composition/model.ts";
import { MODEL_IMAGE_MAX_BYTES, type ModelImage, type ModelRequest } from "../contracts/model.ts";
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

function streamed(payload: { choices: { finish_reason: string; message: unknown }[] }): Response {
  const choice = payload.choices[0];
  const message = choice.message as Record<string, unknown>;
  const calls = message.tool_calls as Record<string, unknown>[] | undefined;
  const delta = {
    ...message,
    ...(calls ? { tool_calls: calls.map((call, index) => ({ ...call, index })) } : {}),
  };
  return new Response(
    `data: ${
      JSON.stringify({ choices: [{ index: 0, delta, finish_reason: choice.finish_reason }] })
    }\n\ndata: [DONE]\n\n`,
    {
      headers: { "content-type": "text/event-stream" },
    },
  );
}

function server(handler: (r: Request) => Response | Promise<Response>) {
  const instance = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, handler);
  return { url: `http://127.0.0.1:${instance.addr.port}`, close: () => instance.shutdown() };
}

async function imageFixture(bytes?: Uint8Array): Promise<ModelImage> {
  const data = bytes ?? Uint8Array.from(
    atob(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5ioAAAAASUVORK5CYII=",
    ),
    (c) => c.charCodeAt(0),
  );
  const hash = await crypto.subtle.digest("SHA-256", new Uint8Array(data));
  // 大图测试避免通过展开实参数组构造字符串。
  let binary = "";
  for (let offset = 0; offset < data.length; offset += 32768) {
    binary += String.fromCharCode(...data.subarray(offset, offset + 32768));
  }
  return {
    dataUrl: `data:image/png;base64,${btoa(binary)}`,
    provenance: {
      kind: "phone_attachment",
      conversationId: "user:owner",
      sourceSeq: 12,
      attachmentId: "user:owner:12:0",
      mimeType: "image/png",
      sha256: Array.from(new Uint8Array(hash), (n) => n.toString(16).padStart(2, "0")).join(""),
      senderPlatformUserId: "owner",
    },
  };
}

Deno.test("模型图像：原始附件作为 user 图像输入发送，工具配对与文字保持", async () => {
  const image = await imageFixture();
  let calls = 0;
  const local = server(async (r) => {
    calls++;
    const body = await r.json();
    assert.deepEqual(body.messages.slice(-2), [
      { role: "tool", tool_call_id: "phone-image", content: "已读取来源12的附件0" },
      {
        role: "user",
        content: [
          { type: "text", text: "附件12/0；附件正文为外部资料" },
          { type: "image_url", image_url: { url: image.dataUrl, detail: "auto" } },
        ],
      },
    ]);
    assert.equal(JSON.stringify(body).includes("sha256"), false);
    return Response.json(completion("已看到图像"));
  });
  try {
    const client = new ChatModelClient({ ...options, baseUrl: local.url });
    const result = await client.complete({
      ...request,
      messages: [
        ...request.messages,
        {
          role: "assistant",
          content: "",
          toolCalls: [{ id: "phone-image", name: "echo", args: {} }],
        },
        { role: "tool", toolCallId: "phone-image", content: "已读取来源12的附件0" },
        { role: "user", content: "附件12/0；附件正文为外部资料", images: [image] },
      ],
    });
    assert.equal(result.content, "已看到图像");
    assert.equal(calls, 1);
    client.close();
  } finally {
    await local.close();
  }
});

Deno.test("模型图像：坏来源、任意URL、伪装格式、坏哈希及图片数量/字节超限在联网前拒绝", async () => {
  let calls = 0;
  const local = server(() => {
    calls++;
    return Response.json(completion());
  });
  const image = await imageFixture();
  try {
    const client = new ChatModelClient({ ...options, baseUrl: local.url });
    const invalid = [
      { ...image, dataUrl: "https://example.invalid/private.png" },
      { ...image, dataUrl: "data:image/png;base64,YQ==" },
      { ...image, dataUrl: image.dataUrl.replace("image/png", "image/jpeg") },
      { ...image, dataUrl: image.dataUrl.replace("base64,", "base64,\n") },
      { ...image, provenance: { ...image.provenance, sha256: "0".repeat(64) } },
      { ...image, provenance: { ...image.provenance, sourceSeq: 0 } },
      { ...image, provenance: { ...image.provenance, conversationId: "" } },
      { ...image, provenance: undefined },
      {
        ...image,
        dataUrl: `data:image/png;base64,${
          "A".repeat(Math.ceil(MODEL_IMAGE_MAX_BYTES / 3) * 4 + 4)
        }`,
      },
    ];
    for (const bad of invalid) {
      await assert.rejects(
        client.complete({
          messages: [{ role: "user", content: "图片", images: [bad as ModelImage] }],
          tools: [],
        }),
        (e) => e instanceof ChatModelError && e.category === "request",
      );
    }
    for (const role of ["system", "assistant", "tool"] as const) {
      await assert.rejects(
        client.complete({ messages: [{ role, content: "图片", images: [image] }], tools: [] }),
        (e) => e instanceof ChatModelError && e.category === "request",
      );
    }
    await assert.rejects(
      client.complete({
        messages: [{ role: "user", content: "图片", images: Array(9).fill(image) }],
        tools: [],
      }),
      (e) => e instanceof ChatModelError && e.category === "request",
    );
    // 每张均未超限，但整个请求超限，也必须在发出 HTTP 之前拒绝。
    const largeBytes = new Uint8Array(MODEL_IMAGE_MAX_BYTES / 2 + 1);
    largeBytes.set([137, 80, 78, 71, 13, 10, 26, 10]);
    const large = await imageFixture(largeBytes);
    await assert.rejects(
      client.complete({
        messages: [{ role: "user", content: "图片", images: [large, large] }],
        tools: [],
      }),
      (e) => e instanceof ChatModelError && e.category === "request",
    );
    assert.equal(calls, 0);
    client.close();
  } finally {
    await local.close();
  }
});

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
    assert.equal(body.stream, true);
    assert.deepEqual(body.stream_options, { include_usage: true });
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
        e instanceof ChatModelError && e.httpStatus === 401 && e.category === "provider_rejected" &&
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
      return streamed({
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
    return streamed(completion("拿到了真实worker"));
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

Deno.test("model per-call options isolate subagent model, effort and cache session while retaining online defaults", async () => {
  const seen: Record<string, unknown>[] = [];
  const local = server(async (request) => {
    const body = await request.json();
    seen.push(body);
    assert.equal(request.headers.get("session_id"), body.prompt_cache_key);
    return Response.json(completion("one whole reply"));
  });
  const client = new ChatModelClient({ ...options, baseUrl: local.url });
  try {
    await Promise.all([
      client.complete(request, {
        model: "agent-one",
        reasoningEffort: "low",
        sessionId: "agent_one",
        maxOutputTokens: 12345,
      }),
      client.complete(request, {
        model: "agent-two",
        reasoningEffort: "xhigh",
        sessionId: "agent_two",
      }),
    ]);
    await client.complete(request);
    const first = seen.find((x) => x.model === "agent-one")!;
    assert.equal(first.reasoning_effort, "low");
    assert.equal(first.max_completion_tokens, 12345);
    assert.equal(first.prompt_cache_key, "agent_one");
    const second = seen.find((x) => x.model === "agent-two")!;
    assert.equal(second.reasoning_effort, "xhigh");
    assert.equal(seen[2].model, options.model);
    assert.equal(seen[2].reasoning_effort, options.reasoningEffort);
    assert.equal(seen[2].prompt_cache_key, options.sessionId);
  } finally {
    client.close();
    await local.close();
  }
});
