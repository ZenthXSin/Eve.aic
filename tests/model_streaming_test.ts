import assert from "node:assert/strict";
import { ChatModelClient, ChatModelError } from "../plugins/model-chat/client.ts";
import type { ModelRequest } from "../contracts/model.ts";
import { MODEL_MAX_DURATION_MS, MODEL_RPC_TIMEOUT_MS } from "../sdk/model-timeouts.ts";
import { eveStopDeadlineMs } from "../sdk/lifecycle-timeouts.ts";

const request: ModelRequest = {
  messages: [{ role: "user", content: "test" }],
  tools: [{ name: "echo", description: "echo", parameters: { type: "object" } }],
};
const encoder = new TextEncoder();
const event = (delta: unknown, finish: string | null = null) =>
  `data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] })}\r\n\r\n`;
const done = "data: [DONE]\r\n\r\n";
function fixture(handler: () => Response | Promise<Response>) {
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, handler);
  const client = (overrides = {}) =>
    new ChatModelClient({
      baseUrl: `http://127.0.0.1:${server.addr.port}`,
      model: "fixture",
      apiKey: "fixture-secret",
      sessionId: "stream-test",
      maxOutputTokens: 8192,
      timeoutMs: 1000,
      ...overrides,
    });
  return { client, close: () => server.shutdown() };
}
function stream(parts: (string | Uint8Array)[], interval = 0, close = true): Response {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let index = 0;
  return new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        const push = () => {
          if (index === parts.length) {
            if (close) {
              try {
                controller.close();
              } catch { /* Peer canceled. */ }
            }
            return;
          }
          const part = parts[index++];
          try {
            controller.enqueue(typeof part === "string" ? encoder.encode(part) : part);
          } catch {
            return;
          } // The HTTP peer may have canceled before cancel() is delivered.
          timer = setTimeout(push, interval);
        };
        push();
      },
      cancel() {
        clearTimeout(timer);
      },
    }),
    { headers: { "content-type": "text/event-stream; charset=utf-8" } },
  );
}
const timeout = (phase: string) => (error: unknown) =>
  error instanceof ChatModelError && error.category === "transport" &&
  error.timeoutPhase === phase && !error.message.includes("fixture-secret");

Deno.test("stream: fragmented UTF-8, CRLF, multiline data, reasoning and final usage", async () => {
  const wire = ": keepalive\r\n\r\n" + event({ role: "assistant" }) +
    'data: {"choices": [\r\ndata: {"index":0,"delta":{"reasoning_content":"思考"},"finish_reason":null}]}\r\n\r\n' +
    event({ content: "你好🌏" }) + event({ content: "，完成" }, "stop") +
    `data: ${
      JSON.stringify({
        choices: [],
        usage: {
          prompt_tokens: 20,
          completion_tokens: 8,
          total_tokens: 28,
          prompt_tokens_details: { cached_tokens: 10 },
        },
      })
    }\r\n\r\n` + done;
  // Split every byte, including UTF-8 codepoints and CRLF pairs.
  const local = fixture(() => stream([...encoder.encode(wire)].map((n) => new Uint8Array([n]))));
  const client = local.client({ timeoutMs: 5000 });
  try {
    const result = await client.complete(request);
    assert.equal(result.content, "你好🌏，完成");
    assert.deepEqual(result.toolCalls, []);
    assert.deepEqual(result.usage, {
      status: "reported",
      inputTokens: 20,
      outputTokens: 8,
      totalTokens: 28,
      cachedInputTokens: 10,
    });
  } finally {
    client.close();
    await local.close();
  }
});

Deno.test("stream: parallel tool fragments assembled by index, arguments validated only on completion", async () => {
  const local = fixture(() =>
    stream([
      event({
        role: "assistant",
        tool_calls: [
          {
            index: 1,
            id: "call-",
            type: "function",
            function: { name: "ec", arguments: '{"text":' },
          },
          { index: 0, id: "first", type: "function", function: { name: "echo", arguments: "{" } },
        ],
      }),
      event({
        tool_calls: [
          { index: 0, function: { arguments: '"text":"一"}' } },
          { index: 1, id: "second", function: { name: "ho", arguments: '"二"}' } },
        ],
      }),
      event({}, "tool_calls"),
      done,
    ])
  );
  const client = local.client();
  try {
    const result = await client.complete(request);
    assert.deepEqual(result.toolCalls, [
      { id: "first", name: "echo", args: { text: "一" } },
      { id: "call-second", name: "echo", args: { text: "二" } },
    ]);
    assert.equal(result.usage, undefined);
  } finally {
    client.close();
    await local.close();
  }
});

Deno.test("stream: truncated/error/invalid streams never return partial content or tools", async () => {
  const bad = [
    event({ content: "partial" }),
    event({ content: "partial" }, "stop"), // missing [DONE]
    done, // missing finish
    event({ content: "partial" }, "length") + done,
    event({ content: "partial" }, "content_filter") + done,
    event({ content: "partial" }) + 'data: {"error":{"message":"fixture-secret"}}\n\n',
    event(
      { tool_calls: [{ index: 0, id: "x", function: { name: "echo", arguments: "{" } }] },
      "tool_calls",
    ) + done,
    event(
      { tool_calls: [{ index: 0, id: "x", function: { name: "unknown", arguments: "{}" } }] },
      "tool_calls",
    ) + done,
    event(
      { tool_calls: [{ index: 1, id: "x", function: { name: "echo", arguments: "{}" } }] },
      "tool_calls",
    ) + done,
    event({ role: "user", content: "bad role" }, "stop") + done,
    event({ content: "" }, "stop") + done,
    event({ content: "a" }, "stop") + event({ content: "late" }) + done,
    "data: {broken}\n\n",
  ];
  let wire = "";
  let calls = 0;
  const local = fixture(() => {
    calls++;
    return stream([wire]);
  });
  const client = local.client();
  try {
    for (wire of bad) {
      await assert.rejects(
        client.complete(request),
        (e) =>
          e instanceof ChatModelError && e.category === "invalid_response" &&
          !e.message.includes("fixture-secret"),
      );
    }
    assert.equal(calls, bad.length); // no automatic replay
  } finally {
    client.close();
    await local.close();
  }
});

Deno.test("stream: progress outlives legacy total deadline, heartbeat cannot extend first-token wait", async () => {
  let heartbeat = false;
  const local = fixture(() =>
    heartbeat
      ? stream([": ping\n\n", event({ role: "assistant" }), ": ping\n\n"], 30, false)
      : stream([
        event({ reasoning_content: "thinking" }),
        ...Array.from({ length: 6 }, () => event({ content: "a" })),
        event({}, "stop"),
        done,
      ], 30)
  );
  const client = local.client({ timeoutMs: 100, connectTimeoutMs: 1000 });
  try {
    const start = performance.now();
    assert.equal((await client.complete(request)).content, "aaaaaa");
    assert.ok(performance.now() - start > 200);
    heartbeat = true;
    await assert.rejects(client.complete(request), timeout("first_token"));
  } finally {
    client.close();
    await local.close();
  }
});

Deno.test("stream: idle, stalled final usage, absolute deadline and close cancel active I/O", async () => {
  let mode = "idle";
  const local = fixture(() =>
    mode === "total"
      ? stream(Array.from({ length: 50 }, () => event({ content: "x" })), 20)
      : stream([event({ content: "x" }, mode === "usage" ? "stop" : null), ": ping\n\n"], 20, false)
  );
  try {
    for (mode of ["idle", "usage", "total", "close"]) {
      const client = local.client({ timeoutMs: 100, maxDurationMs: mode === "total" ? 140 : 5000 });
      try {
        const work = client.complete(request);
        if (mode === "close") {
          const timer = setTimeout(() => client.close(), 30);
          try {
            await assert.rejects(
              work,
              (e) => e instanceof ChatModelError && e.category === "transport" && !e.timeoutPhase,
            );
          } finally {
            clearTimeout(timer);
          }
        } else await assert.rejects(work, timeout(mode === "total" ? "total" : "idle"));
      } finally {
        client.close();
      }
    }
  } finally {
    await local.close();
  }
});

Deno.test("stream: response headers have an independent connect deadline", async () => {
  const local = fixture(async () => {
    await new Promise((resolve) => setTimeout(resolve, 80));
    return stream([event({ content: "late" }, "stop"), done]);
  });
  const client = local.client({ connectTimeoutMs: 20 });
  try {
    await assert.rejects(client.complete(request), timeout("connect"));
  } finally {
    client.close();
    await local.close();
  }
});

Deno.test("stream: decoded output and unfinished event buffers remain bounded", async () => {
  let hugeEvent = false;
  const local = fixture(() =>
    stream(
      hugeEvent
        ? ["data: " + "x".repeat(2048)]
        : Array.from({ length: 20 }, () => event({ content: "x".repeat(100) })),
    )
  );
  const client = local.client({ maxResponseBytes: 1024 });
  try {
    for (hugeEvent of [false, true]) {
      await assert.rejects(
        client.complete(request),
        (e) => e instanceof ChatModelError && e.category === "invalid_response",
      );
    }
  } finally {
    client.close();
    await local.close();
  }
});

Deno.test("stream: timeout configuration is validated and outer waits allow the final receipt", () => {
  const localOptions = {
    baseUrl: "http://127.0.0.1",
    model: "fixture",
    apiKey: "secret",
    sessionId: "test",
    maxOutputTokens: 1000,
    timeoutMs: 120_000,
  };
  for (
    const name of ["connectTimeoutMs", "firstTokenTimeoutMs", "idleTimeoutMs", "maxDurationMs"]
  ) {
    for (const value of [0, -1, NaN, Infinity, 1.5, MODEL_MAX_DURATION_MS + 1]) {
      assert.throws(() => new ChatModelClient({ ...localOptions, [name]: value }), ChatModelError);
    }
  }
  assert.ok(MODEL_RPC_TIMEOUT_MS > MODEL_MAX_DURATION_MS);
  assert.ok(eveStopDeadlineMs(120_000) > MODEL_RPC_TIMEOUT_MS);
});
