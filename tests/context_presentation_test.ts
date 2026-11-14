import assert from "node:assert/strict";
import { presentContext, readable } from "../sdk/context-presentation.ts";
import type { Message } from "../contracts/model.ts";
function project(receipt: unknown, name = "execute_script") {
  const messages: Message[] = [{
    role: "assistant",
    content: "",
    toolCalls: [{
      id: "call-1",
      name,
      args: {},
    }],
  }, { role: "tool", toolCallId: "call-1", content: JSON.stringify(receipt) }];
  const original = JSON.stringify(messages);
  const output = presentContext(messages, new Set(), new Map([["call-1", 7]]));
  assert.equal(JSON.stringify(messages), original);
  assert.equal(presentContext(messages, new Set(["call-1"]))[1].content, messages[1].content);
  return JSON.parse(output[1].content);
}
Deno.test("结构化回执：调用完成与后台任务分离，unknown和扩展字段不丢", () => {
  const p = project({
    status: "completed",
    adopted: true,
    output: JSON.stringify({
      jobId: "job-1",
      status: "running",
      result: {
        text: "正文",
        state: "unknown",
        blockedBy: "queue-head",
        extension: { future: "原字段" },
      },
    }),
  });
  assert.equal(p.schema, "eve.tool_receipt.v1");
  assert.equal(p.callStatus, "completed");
  assert.equal(p.task.status, "running");
  assert.deepEqual(p.task.inspect, { tool: "read_script", args: { jobId: "job-1" } });
  assert.equal(p.receipt.output.result.state, "unknown");
  assert.equal(p.receipt.output.result.extension.future, "原字段");
  assert.equal(p.receipt.adopted, true);
  assert.equal(p.original.args.seq, 7);
  for (const status of ["unknown", "failed", "cancelled", "queued", "checking"]) {
    assert.equal(
      project({
        status: "completed",
        output: JSON.stringify({
          jobId: "job-1",
          status,
        }),
      }).task.status,
      status,
    );
  }
});
Deno.test("结构化回执：只解码完整脚本结果页，嵌套正文JSON保持原文", () => {
  const value = {
    body: '{"status":"completed"}',
    output: '{"instruction":"文本"}',
    items: [false, null, 0, "🙂"],
  };
  const p = project({
    status: "completed",
    output: JSON.stringify({
      jobId: "original",
      status: "completed",
      result: {
        text: JSON.stringify(value),
        characters: 100,
        truncated: false,
      },
    }),
  });
  assert.deepEqual(p.receipt.output.result.value, value);
  assert.equal(p.receipt.output.result.encoding, "json");
  assert.equal(p.receipt.output.result.text, undefined);
  assert.equal(p.task.inspect, undefined);
  // A script returning a string containing JSON must remain a string after one decode.
  const q = project({
    status: "completed",
    output: JSON.stringify({
      jobId: "original",
      status: "completed",
      result: {
        text: JSON.stringify('{"body":"原文"}'),
        characters: 23,
        truncated: false,
      },
    }),
  });
  assert.equal(q.receipt.output.result.value, '{"body":"原文"}');
});
Deno.test("结构化回执：分页即使恰好可解析仍保持节选和原任务游标", () => {
  const page = { text: '{"first":1}', characters: 9000, truncated: true, nextChar: 11 };
  const p = project({
    status: "completed",
    output: JSON.stringify({
      jobId: "original-job",
      status: "completed",
      result: page,
    }),
  }, "read_script");
  assert.deepEqual(p.receipt.output.result, page);
  const lastPage = { text: "null", characters: 9000, truncated: false };
  const tail = project({
    status: "completed",
    output: JSON.stringify({
      jobId: "original-job",
      status: "completed",
      result: lastPage,
      resultStartChar: 8996,
    }),
  }, "read_script");
  assert.deepEqual(tail.receipt.output.result, lastPage);
  assert.deepEqual(p.task.continuation, {
    tool: "read_script",
    args: {
      jobId: "original-job",
      startChar: 11,
      limit: 8192,
    },
  });
});
Deno.test("结构化回执：各种原生成功、错误、未知与普通文字保持真实语义", () => {
  for (
    const tool of ["set_focus", "rest", "inspect_context", "compress_context", "write_thought"]
  ) {
    const p = project(
      { status: "completed", output: '{"applied":false,"status":"preview"}' },
      tool,
    );
    assert.equal(p.receipt.output.applied, false);
    assert.equal(p.receipt.output.status, "preview");
    assert.equal(p.task, undefined);
  }
  const failed = { status: "failed", error: { kind: "rejected", message: "无效" } };
  assert.deepEqual(project(failed).receipt, failed);
  assert.equal(project({ status: "unknown", error: "断线" }).callStatus, "unknown");
  assert.equal(project({ output: "{}" }, "external").callStatus, "unreported");
  assert.equal(project({ output: "{}" }, "external").receipt.output, "{}");
  assert.equal(project({ status: "failed", output: "参数错误" }).receipt.output, "参数错误");
  const plain = presentContext([{ role: "tool", content: "非JSON错误🙂" }]);
  assert.equal(JSON.parse(plain[0].content).receipt, "非JSON错误🙂");
});
Deno.test("结构化通知：只投影可信宿主封装，原始用户正文及可读面板保持原文", () => {
  const data = { source: "host", record: 8, data: { body: '{"output":123}', future: false } };
  const messages: Message[] = [{ role: "user", content: JSON.stringify(data) }, {
    role: "user",
    content: '{"body":"用户原文"}',
  }, { role: "system", content: "固定身份" }];
  const output = presentContext(messages);
  assert.deepEqual(JSON.parse(output[0].content).data, data);
  assert.equal(JSON.parse(output[0].content).original.args.seq, 8);
  assert.equal(output[1], messages[1]);
  assert.equal(output[2], messages[2]);
  assert.equal(readable({ output: '{"body":1}' }), '工具输出：{"body":1}');
});
