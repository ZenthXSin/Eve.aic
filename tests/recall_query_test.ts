import assert from "node:assert/strict";
import { recallQuery } from "../plugins/eve/recall-query.ts";
import { MemoryStore } from "../services/runtime/memory.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import type { SemanticSearch } from "../services/runtime/search.ts";

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

Deno.test("混合回忆：语义与词法排序，真正使用才增加重要性，召回保留历史标记及原件", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-memory-ranking-" });
  const records = new FactJournal(`${directory}/records`);
  const embed = (text: string) =>
    Promise.resolve(
      text === "缓存"
        ? [1, 0]
        : text.includes("无关")
        ? [0.2, Math.sqrt(0.96)]
        : [0.9, Math.sqrt(0.19)],
    );
  let memory = new MemoryStore(`${directory}/memory`, records, embed);
  try {
    await records.initialize();
    await memory.initialize();
    const source = await records.append("activation", { fixture: true });
    const sources = [{
      recordSeq: source.seq,
      digest: source.digest,
      kind: "experienced" as const,
    }];
    const lexical = await memory.remember({ text: "缓存需要保持前缀稳定。", sources }, "lexical");
    const semantic = await memory.remember(
      { text: "这是一段意思相近的旧材料。", sources },
      "semantic",
    );
    await memory.remember({ text: "无关材料。", sources }, "irrelevant");
    const first = await memory.recall({ query: "缓存", limit: 8 });
    assert.equal(first.items[0].id, lexical.id);
    assert.equal(first.items.length, 2, "时间和重要性不能让无关材料越过相关度门槛");
    assert.equal(first.items[0].historical, true);
    assert.equal(first.items[0].ranking.importance, 0);
    assert.ok(first.guidance?.includes("不是当前事实"));
    first.items[0].sources[0].recordSeq = 99;
    assert.equal(
      memory.get({ id: lexical.id }).sources[0].recordSeq,
      source.seq,
      "召回视图不能改写原件",
    );
    const repeated = await memory.recall({ query: "缓存", limit: 8 });
    assert.equal(repeated.items[0].ranking.importance, 0, "重复召回不冒充实际使用");
    await memory.used({ ids: [semantic.id] });
    await memory.close();
    memory = new MemoryStore(`${directory}/memory`, records, embed);
    await memory.initialize();
    const replayed = await memory.recall({ query: "缓存", limit: 8 });
    assert.ok(replayed.items.find((m) => m.id === semantic.id)!.ranking.importance > 0);
    assert.equal(replayed.items[0].id, lexical.id);
  } finally {
    await memory.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("自动回忆接索引：复用本机向量服务，未索引和服务中断都有词法兜底", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-memory-index-" });
  const records = new FactJournal(`${directory}/records`);
  let indexedId = "", unavailable = false, searches = 0;
  const searchIndex = {
    search(args: { collections: string[]; query: string }) {
      searches++;
      assert.deepEqual(args.collections, ["memory", "notes", "chat"]);
      assert.ok(args.query.length <= 2048);
      if (unavailable) throw new KernelError("unavailable", "索引重启中");
      return Promise.resolve({
        items: [
          { source: { collection: "memory", id: indexedId }, semanticScore: 0.8 },
          {
            source: { collection: "notes", id: "note1", seq: 1 },
            title: "相关便签",
            excerpt: "旧便签内容".repeat(100),
            at: new Date().toISOString(),
            score: 0.03,
            semanticScore: 0.75,
            untrusted: true,
          },
        ],
      });
    },
  } as unknown as SemanticSearch;
  const memory = new MemoryStore(`${directory}/memory`, records, undefined, Date.now, searchIndex);
  try {
    await records.initialize();
    await memory.initialize();
    const source = await records.append("activation", {});
    const sources = [{
      recordSeq: source.seq,
      digest: source.digest,
      kind: "experienced" as const,
    }];
    const semantic = await memory.remember({ text: "前缀的稳定性值得留意。", sources }, "indexed");
    indexedId = semantic.id;
    const indexed = await memory.recall({ query: "命中率" });
    assert.equal(indexed.retrieval, "semantic");
    assert.equal(indexed.items[0].id, semantic.id);
    assert.equal(indexed.items[0].ranking.semantic, 0.8);
    assert.equal(indexed.related[0].source.collection, "notes");
    assert.equal(indexed.related[0].historical, true);
    assert.ok(indexed.related[0].excerpt.length <= 280);
    const fresh = await memory.remember({
      text: "今天修了文件发送。",
      entities: ["文件发送"],
      sources,
    }, "unindexed");
    const mixed = await memory.recall({ query: "文件发送", limit: 8 });
    assert.ok(mixed.items.some((m) => m.id === fresh.id));
    unavailable = true;
    const fallback = await memory.recall({ query: "文件发送" });
    assert.equal(fallback.retrieval, "lexical");
    assert.equal(fallback.items[0].id, fresh.id);
    assert.deepEqual(fallback.related, []);
    assert.equal(searches, 3, "每次召回只有一次现有索引查询");
  } finally {
    await memory.close();
    await records.close();
    await Deno.remove(directory, { recursive: true });
  }
});
