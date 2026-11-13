import assert from "node:assert/strict";
import { presentContext, readable } from "../sdk/context-presentation.ts";
import type { Message } from "../contracts/model.ts";
Deno.test("可读上下文：嵌套JSON输出、失败与unknown原义不丢，字段保留，原件和显式raw可核对", () => {
  const data = {
    status: "completed",
    output: JSON.stringify({
      jobId: "job-1",
      status: "running",
      result: {
        text: "🌟<script>not executed</script>",
        state: "unknown",
        blockedBy: "queue-head",
        extension: { future: "完整原字段" },
      },
    }),
  };
  const messages: Message[] = [{ role: "system", content: "固定身份" }, {
    role: "tool",
    toolCallId: "eve_2_0",
    content: JSON.stringify(data),
  }, {
    role: "user",
    content: JSON.stringify({
      source: "host",
      record: 8,
      kind: "notice",
      data: { source: "phone", items: [{ text: "原始来信", relationship: "other" }] },
    }),
  }];
  const originals = JSON.stringify(messages);
  const output = presentContext(messages, new Set(), new Map([["eve_2_0", 7]]));
  assert.equal(output[0], messages[0]);
  for (
    const word of [
      "脚本编号：job-1",
      "状态：running",
      "状态：unknown",
      "阻塞动作：queue-head",
      "future：完整原字段",
      "🌟<script>",
      "seq:7",
    ]
  ) assert.ok(output[1].content.includes(word), word);
  assert.ok(output[2].content.includes("原件记录：8"));
  assert.ok(output[2].content.includes("可信关系：other"));
  assert.equal(JSON.stringify(messages), originals);
  assert.equal(presentContext(messages, new Set(["eve_2_0"]))[1].content, messages[1].content);
  assert.equal(
    readable({ futureKey: { bool: false, n: 0, empty: null } }),
    "futureKey：  bool：false\n  n：0\n  empty：null",
  );
});
