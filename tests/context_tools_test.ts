import assert from "node:assert/strict";
import type { Json } from "../kernel/protocol.ts";
import { CONTEXT_TOOLS, inspectContext, previewCompression } from "../plugins/eve/context-tools.ts";
import { ConsciousJournal } from "../plugins/eve/journal.ts";
import { render } from "../plugins/eve/view.ts";

Deno.test("上下文工具：分页只列完整边界，预览与生效估算一致，旧摘要不能被倒退或切断工具对", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-context-tools-" });
  const journal = new ConsciousJournal(directory);
  try {
    await journal.initialize();
    const notice = await journal.append("notice", {
      source: "phone",
      items: [{ text: "历史".repeat(5000) }],
    });
    const response = await journal.append("model_response", {
      response: {
        content: "",
        toolCalls: [
          { id: "a", name: "x", args: {} },
          { id: "b", name: "y", args: {} },
        ],
        usage: { status: "reported", inputTokens: 12345, outputTokens: 10 },
      },
    });
    const first = await journal.append("tool_result", {
      callId: `eve_${response.seq}_0`,
      result: "a",
    });
    let inspection = inspectContext(journal.events, "身份", CONTEXT_TOOLS, { limit: 1 });
    assert.deepEqual(inspection.candidates.map((item) => item.throughSeq), [notice.seq]);
    assert.equal(inspection.hasMore, false);
    assert.throws(() =>
      previewCompression(journal.events, "身份", CONTEXT_TOOLS, {
        throughSeq: first.seq,
        summary: "无效",
      })
    );
    const second = await journal.append("tool_result", {
      callId: `eve_${response.seq}_1`,
      result: "b",
    });
    inspection = inspectContext(journal.events, "身份", CONTEXT_TOOLS, { limit: 1 });
    assert.equal(inspection.nextAfterSeq, notice.seq);
    const page = inspectContext(journal.events, "身份", CONTEXT_TOOLS, {
      afterSeq: inspection.nextAfterSeq!,
      limit: 1,
    });
    assert.deepEqual(page.candidates.map((item) => item.throughSeq), [second.seq]);
    assert.equal(page.hasMore, false);
    const preview = previewCompression(journal.events, "身份", CONTEXT_TOOLS, {
      throughSeq: notice.seq,
      summary: "重要事实与未完成事项",
      dryRun: true,
    });
    assert.ok(preview.estimatedTokensSaved > 9000);
    assert.equal(journal.events.some((event) => event.kind === "compression"), false);
    const before = render(journal.events, "身份");
    await journal.append("compression", { throughSeq: notice.seq, summary: preview.summary });
    const after = inspectContext(journal.events, "身份", CONTEXT_TOOLS, {});
    assert.equal(after.estimatedTokens, preview.after.estimatedTokens);
    assert.equal(after.latestProviderUsage?.responseSeq, response.seq);
    assert.deepEqual(render(journal.events, "身份").slice(-3), before.slice(-3));
    assert.equal(journal.events[0].seq, notice.seq);
    assert.throws(() =>
      previewCompression(journal.events, "身份", CONTEXT_TOOLS, {
        throughSeq: notice.seq,
        summary: "不能覆盖同一边界",
      })
    );
    for (const args of [{ limit: 0 }, { afterSeq: -1 }, { unknown: true }] as Json[]) {
      assert.throws(() => inspectContext(journal.events, "身份", CONTEXT_TOOLS, args));
    }
  } finally {
    await journal.close();
    await Deno.remove(directory, { recursive: true });
  }
});
