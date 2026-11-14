import assert from "node:assert/strict";
import { ChatModelClient, ChatModelError } from "../plugins/model-chat/client.ts";
import { ResponsesStream } from "../plugins/model-chat/responses.ts";
import { render } from "../plugins/eve/view.ts";
import type { ConsciousEvent } from "../plugins/eve/journal.ts";
import type { Json } from "../kernel/protocol.ts";

Deno.test("responses: native output and opaque reasoning survive synthetic execution IDs", async () => {
  const bodies: Record<string, unknown>[] = [];
  const output: Json[] = [
    { type: "reasoning", id: "rs_1", encrypted_content: "opaque", summary: [] },
    {
      type: "function_call",
      id: "fc_1",
      call_id: "call_native",
      name: "echo",
      arguments: '{"text":"hello"}',
    },
  ];
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (req) => {
    assert.equal(new URL(req.url).pathname, "/v1/responses");
    assert.equal(req.headers.get("session_id"), "native-test");
    const body = await req.json();
    bodies.push(body);
    const completed = {
      type: "response.completed",
      response: {
        status: "completed",
        output,
        usage: {
          input_tokens: 12000,
          output_tokens: 10,
          total_tokens: 12010,
          input_tokens_details: { cached_tokens: 11520 },
        },
      },
    };
    return new Response(`data: ${JSON.stringify(completed)}\r\n\r\ndata: [DONE]\r\n\r\n`, {
      headers: { "content-type": "text/event-stream" },
    });
  });
  const client = new ChatModelClient({
    protocol: "responses",
    baseUrl: `http://127.0.0.1:${server.addr.port}`,
    apiKey: "fixture",
    model: "fixture",
    sessionId: "native-test",
    maxOutputTokens: 100,
    timeoutMs: 1000,
  });
  const tools = [{ name: "echo", description: "Echo", parameters: { type: "object" } }];
  try {
    const result = await client.complete({ messages: [{ role: "user", content: "hi" }], tools });
    assert.equal(result.toolCalls[0].id, "call_native");
    assert.deepEqual(result.modelOutput, output);
    assert.equal(result.usage?.cachedInputTokens, 11520);
    await client.complete({
      messages: [
        { role: "user", content: "hi" },
        {
          role: "assistant",
          content: "",
          toolCalls: [{ ...result.toolCalls[0], id: "eve_9_0" }],
          modelOutput: result.modelOutput,
        },
        { role: "tool", toolCallId: "eve_9_0", content: "receipt" },
      ],
      tools,
    });
    const body = bodies[1];
    assert.equal(body.store, false);
    assert.deepEqual(body.include, ["reasoning.encrypted_content"]);
    assert.deepEqual(body.input, [
      { role: "user", content: [{ type: "input_text", text: "hi" }] },
      ...output,
      { type: "function_call_output", call_id: "call_native", output: "receipt" },
    ]);
  } finally {
    client.close();
    await server.shutdown();
  }
});

Deno.test("responses: incomplete output never becomes an executable tool call; heartbeats do not reset idle", () => {
  let progressed = 0;
  const stream = new ResponsesStream(10000, () => progressed++);
  stream.feed(': ping\n\ndata: {"type":"response.created"}\n\n');
  assert.equal(progressed, 0);
  stream.feed('data: {"type":"response.function_call_arguments.delta","delta":"{\\"a\\":"}\n\n');
  assert.equal(progressed, 1);
  assert.throws(() => stream.result(), ChatModelError);
  assert.throws(() => stream.feed('data: {"type":"response.incomplete"}\n\n'), ChatModelError);
});

Deno.test("responses: context retention removes opaque replay alongside omitted tool content", () => {
  const output = [{
    type: "function_call",
    call_id: "native",
    name: "echo",
    arguments: '{"secret":"original"}',
  }];
  const events = [
    {
      seq: 1,
      kind: "model_response",
      data: {
        response: {
          content: "",
          toolCalls: [{ id: "native", name: "echo", args: { secret: "original" } }],
          modelOutput: output,
        },
      },
    },
    {
      seq: 2,
      kind: "tool_result",
      data: { callId: "eve_1_0", result: { status: "completed", output: "original" } },
    },
  ] as unknown as ConsciousEvent[];
  assert.deepEqual(render(events, "system")[1].modelOutput, output);
  events.push(
    {
      seq: 3,
      kind: "retain",
      data: { responseSeq: 1, toolIndex: 0, mode: "omit" },
    } as unknown as ConsciousEvent,
  );
  assert.ok(render(events, "system").every((message) => message.modelOutput === undefined));
});
