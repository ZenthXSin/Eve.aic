import assert from "node:assert/strict";
import { recallQuery } from "../plugins/eve/recall-query.ts";
import { MemoryStore } from "../services/runtime/memory.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import type { Json } from "../kernel/protocol.ts";

Deno.test("自动回忆正文：QQ句柄及状态不提供检索词，真正正文与工具嵌套输出仍召回对应事实", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-recall-body-" });
  const records = new FactJournal(`${directory}/records`),
    memory = new MemoryStore(`${directory}/memory`, records);
  try {
    await records.initialize();
    await memory.initialize();
    const source = await records.append("activation", { fixture: true });
    const qq = await memory.remember({
      text: "QQ平台的首次启动。",
      entities: ["QQ"],
      sources: [{ recordSeq: source.seq, digest: source.digest, kind: "experienced" }],
    }, "startup");
    const flower = await memory.remember({
      text: "窗边的花需要浇水。",
      entities: ["浇水"],
      sources: [{ recordSeq: source.seq, digest: source.digest, kind: "experienced" }],
    }, "flower");
    const metadata = [{
      kind: "notice",
      data: {
        source: "phone",
        items: [{
          handle: { conversationId: "qq:user:opaque", seq: 12 },
          senderPlatformUserId: "opaque",
        }],
      },
    }];
    assert.equal(recallQuery(metadata), "");
    // 原故障路径把平台字段当文本，真实MemoryStore的QQ实体因此满分。
    const previous = await memory.recall({
      query: JSON.stringify(metadata.map((event) => event.data)),
    });
    assert.equal(previous.items[0].id, qq.id);
    assert.equal(previous.items[0].score, 1);
    const events = [...metadata, {
      kind: "tool_result",
      data: {
        callId: "qq_read",
        result: {
          status: "completed",
          output: JSON.stringify({
            status: "QQ online",
            handle: { conversationId: "qq:user:opaque" },
            code: "QQ();",
            result: {
              messages: [{ text: "今天记得浇水。", handle: { conversationId: "qq:user:opaque" } }],
            },
          }),
        },
      },
    }];
    const original = JSON.stringify(events), query = recallQuery(events);
    assert.equal(query, "今天记得浇水。");
    assert.equal(JSON.stringify(events), original);
    const recalled = await memory.recall({ query });
    assert.deepEqual(recalled.items.map((item) => item.id), [flower.id]);
    const actualQQ = recallQuery([{
      kind: "notice",
      data: { source: "phone", items: [{ text: "我想聊QQ。" }] },
    }]);
    assert.equal((await memory.recall({ query: actualQQ })).items[0].id, qq.id);
  } finally {
    await memory.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("自动回忆正文：跳过旧memory、模型工具代码和原图，保留描述；超大与深层结果有界", () => {
  const events: { kind: string; data: Json }[] = [
    { kind: "notice", data: { source: "memory", items: [{ text: "QQ旧记忆", entities: ["QQ"] }] } },
    {
      kind: "model_response",
      data: {
        response: {
          content: "我想看一朵花。",
          toolCalls: [{ args: { code: "QQ();", text: "旧参数" } }],
        },
      },
    },
    {
      kind: "notice",
      data: {
        source: "image_description",
        observation: {
          description: "白色背景上的红色花。",
          provenance: { conversationId: "qq:user:opaque" },
        },
      },
    },
    {
      kind: "notice",
      data: {
        source: "image",
        image: {
          dataUrl: "data:image/png;base64,QQQQ",
          provenance: { conversationId: "qq:user:opaque" },
        },
      },
    },
  ];
  assert.equal(recallQuery(events), "我想看一朵花。\n白色背景上的红色花。");
  let deep: Json = { text: "QQ隐藏在过深结果" };
  for (let i = 0; i < 30; i++) deep = { nested: deep };
  assert.equal(recallQuery([{ kind: "notice", data: { source: "phone", payload: deep } }]), "");
  assert.equal(
    recallQuery([{
      kind: "tool_result",
      data: { result: { output: '{"text":"QQ", incomplete' } },
    }]),
    "",
  );
  const huge = recallQuery([{
    kind: "model_response",
    data: { response: { content: "花".repeat(50_000) } },
  }]);
  assert.equal(huge.length, 16_000);
});
