import assert from "node:assert/strict";
import { writeEvePrompt } from "../composition/eve.ts";
import type { ModelRequest } from "../contracts/model.ts";
import type { MessageScreen } from "../contracts/phone.ts";
import type { Json } from "../kernel/protocol.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import { ChatStyleMemory } from "../plugins/phone/chat-style.ts";
import { createPhonePlugin } from "../plugins/phone/mod.ts";
import type { Handler, PluginContext } from "../sdk/plugin.ts";

const NOW = Date.parse("2026-11-18T12:00:00Z");
const GROUP = "qq:group:alpha";
const OTHER = "qq:group:beta";
const PRIVATE = "qq:user:owner";

function message(
  seq: number,
  text: string,
  speaker: string,
  conversationId = GROUP,
  direction: MessageScreen["direction"] = "incoming",
  truncated = false,
): MessageScreen {
  const preview = (text: string) => ({ text, characters: [...text].length, truncated });
  return {
    handle: { conversationId, seq },
    sender: {
      platformUserId: speaker,
      displayName: preview(speaker),
      relationship: direction === "outgoing" ? "self" : speaker === "owner" ? "owner" : "other",
    },
    direction,
    time: { status: "known", at: new Date(NOW + seq * 1000).toISOString() },
    body: preview(text),
    attachments: [],
    attachmentCount: 0,
    ...(direction === "outgoing" ? { delivery: { actionId: "self", state: "sent" } } : {}),
  };
}

function page(memory: ChatStyleMemory, messages: MessageScreen[], method = "history", now = NOW) {
  const field = method === "notifications" ? "items" : "messages";
  const data = { [field]: messages, snapshotThroughSeq: messages.at(-1)?.handle.seq ?? 0 };
  return (memory.project(method, data as unknown as Json, now) as unknown as Record<
    string,
    MessageScreen[]
  >)[field];
}

Deno.test("从群聊学陌生口头禅的分条位置；同一页重读不算新证据，也不串到别的群", () => {
  const memory = new ChatStyleMemory();
  const first = [message(1, "又踩坑了", "cat"), message(2, "nweh", "cat")];
  const second = [message(3, "今天有点顺", "owl"), message(4, "nweh", "owl")];
  const third = [message(5, "这个能玩", "cat"), message(6, "nweh", "cat")];
  assert.equal(page(memory, first).at(-1)?.chatStyle, undefined);
  assert.equal(page(memory, [...first, ...second]).at(-1)?.chatStyle, undefined);
  assert.equal(page(memory, [...first, ...second]).at(-1)?.chatStyle, undefined);
  assert.deepEqual(page(memory, third, "open").at(-1)?.chatStyle, {
    forms: [{ text: "nweh", placement: "followUp" }],
    avoid: [],
  });
  const other = message(7, "这个怎么弄", "cat", OTHER);
  assert.equal(page(memory, [other], "notifications")[0], other);
  const next = message(8, "这两局都挺有意思的", "owl");
  assert.equal(page(memory, [next], "notifications")[0].chatStyle?.forms[0].text, "nweh");
});

Deno.test("没有固定词表；句尾符号、片段、开头短语是不同观察", () => {
  const memory = new ChatStyleMemory();
  const messages = [
    message(1, "vemm 这版又歪了(段落( ", "cat"),
    message(2, "vemm 先看这个（", "owl"),
    message(3, "vemm 我刚来过这里(", "cat"),
  ];
  assert.deepEqual(page(memory, messages).at(-1)?.chatStyle?.forms, [
    { text: "vemm", placement: "opening" },
    { text: "(", placement: "ending" },
  ]);
  assert.equal(messages.at(-1)?.chatStyle, undefined);
});

Deno.test("不采纳单人刷屏、Eve 自己的复读、截断文本或无关接口的返回", () => {
  const memory = new ChatStyleMemory();
  const messages = [
    message(1, "whup", "cat"),
    message(2, "whup", "cat"),
    message(3, "whup", "cat"),
    message(4, "whup", "owl", GROUP, "incoming", true),
    message(5, "whup", "eve", GROUP, "outgoing"),
    message(6, "正常在群聊", "owl"),
  ];
  assert.equal(page(memory, messages).at(-1)?.chatStyle, undefined);
  const extra = message(7, "whup", "owl");
  assert.equal(page(memory, [extra], "search")[0].chatStyle, undefined);
  assert.equal(page(memory, [message(8, "群里说别的", "owl")])[0].chatStyle, undefined);
});

Deno.test("明确纠正立即生效，能撤销；私聊习惯和群聊观察相互隔离", () => {
  const memory = new ChatStyleMemory();
  const evidence = [1, 2, 3].map((seq) => message(seq * 30, "blep", "owner", PRIVATE));
  assert.deepEqual(page(memory, evidence, "notifications").at(-1)?.chatStyle?.forms, [
    { text: "blep", placement: "alone" },
  ]);
  const stop = message(100, "别老用blep了", "owner", PRIVATE);
  assert.deepEqual(page(memory, [stop], "notifications")[0].chatStyle, {
    forms: [],
    avoid: ["blep"],
  });
  const allow = message(101, "以后可以用「blep」", "owner", PRIVATE);
  assert.deepEqual(page(memory, [allow], "notifications")[0].chatStyle, {
    forms: [{ text: "blep", placement: "alone" }],
    avoid: [],
  });
  assert.equal(page(memory, [message(102, "群里问一问", "owner")])[0].chatStyle, undefined);
});

Deno.test("指代纠正只针对确实已发的上一句；群里其他人不能替整群禁词", () => {
  const memory = new ChatStyleMemory();
  const correction = message(2, "别老这么说", "owner", PRIVATE);
  const own = message(1, "bepo", "eve", PRIVATE, "outgoing");
  const unsent: MessageScreen = { ...own, delivery: { actionId: "pending", state: "unknown" } };
  assert.equal(page(new ChatStyleMemory(), [unsent, correction]).at(-1)?.chatStyle, undefined);
  assert.deepEqual(page(memory, [own, correction]).at(-1)?.chatStyle?.avoid, ["bepo"]);
  const group = [
    message(3, "bepo", "cat"),
    message(4, "bepo", "owl"),
    message(5, "bepo", "cat"),
    message(6, "以后别说bepo", "owl"),
  ];
  assert.deepEqual(page(memory, group).at(-1)?.chatStyle?.forms, [{
    text: "bepo",
    placement: "alone",
  }]);
});

Deno.test("旧用法会淡掉，不需要另存照片或完整聊天副本", () => {
  const memory = new ChatStyleMemory();
  const original = [
    message(1, "drel", "cat"),
    message(2, "drel", "owl"),
    message(3, "drel", "cat"),
  ];
  assert.ok(page(memory, original).at(-1)?.chatStyle);
  const later = NOW + 22 * 24 * 60 * 60_000;
  assert.equal(
    page(memory, [message(4, "在聊别的了", "owl")], "history", later)[0].chatStyle,
    undefined,
  );
});

Deno.test("指代纠正不根据有缺口的历史猜上一句，但可以用唯一匹配的引用", () => {
  const own = message(1, "bepo", "eve", PRIVATE, "outgoing");
  const correction = message(3, "别老这么说", "owner", PRIVATE);
  const memory = new ChatStyleMemory();
  assert.equal(page(memory, [own, correction]).at(-1)?.chatStyle, undefined);
  assert.equal(
    page(memory, [{
      ...correction,
      handle: { conversationId: PRIVATE, seq: 4 },
      reference: {
        index: "not-found",
      },
    }])[0].chatStyle,
    undefined,
  );
  assert.deepEqual(
    page(memory, [{
      ...correction,
      handle: { conversationId: PRIVATE, seq: 5 },
      reference: { index: "found", target: own.handle },
    }])[0].chatStyle?.avoid,
    ["bepo"],
  );
});

Deno.test("倒翻历史时旧纠正不能替掉新纠正，提示数量有上限", () => {
  const memory = new ChatStyleMemory();
  const terms = Array.from({ length: 17 }, (_, index) => `za${String.fromCharCode(97 + index)}`);
  const corrections = terms.map((term, index) =>
    message(index + 2, `以后别说「${term}」`, "owner", PRIVATE)
  );
  assert.deepEqual(page(memory, corrections).at(-1)?.chatStyle?.avoid, terms.slice(1).reverse());
  const old = message(1, "别说「waq」", "owner", PRIVATE);
  assert.deepEqual(page(memory, [old])[0].chatStyle?.avoid, terms.slice(1).reverse());
});

Deno.test("模拟 QQ 到 Eve 模型：可用风格进上下文，群友不能伪造主人纠正，不自动发送", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-style-integration-" });
  const words = [message(1, "vemm", "cat"), message(2, "vemm", "owl"), message(3, "vemm", "cat")];
  const forged = message(4, "别说「vemm」", "owl");
  forged.sender.relationship = "owner";
  const real = message(5, "别说「vemm」", "owner");
  const screen = (items: MessageScreen[]) =>
    items.map((item) => ({
      ...item,
      time: {
        status: "known",
        at: new Date(Date.now() - (20 - item.handle.seq) * 60_000).toISOString(),
      },
    }));
  const phoneMethods: string[] = [];
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const { method, args } = await request.json();
    phoneMethods.push(method);
    const value = method === "health"
      ? { available: true }
      : method === "open"
      ? { messages: screen(words) }
      : method === "notifications" && args.afterSeq === 0
      ? { items: screen([forged]), throughSeq: 4, hasMore: false }
      : method === "notifications" && args.afterSeq === 4
      ? { items: screen([real]), throughSeq: 5, hasMore: false }
      : null;
    return value === null
      ? new Response(null, { status: 400 })
      : Response.json({ ok: true, value });
  });
  let handlers: Record<string, Handler> = {};
  const cleanups: (() => void | Promise<void>)[] = [];
  const requests: ModelRequest[] = [];
  const context = {
    provide(contract: string, provided: typeof handlers) {
      assert.equal(contract, "phone");
      handlers = provided;
    },
    onDisable(cleanup: () => void | Promise<void>) {
      cleanups.push(cleanup);
    },
    async call(contract: string, method: string, args: Json) {
      if (contract === "phone") return await handlers[method](args);
      switch (`${contract}.${method}`) {
        case "scripts.completions":
        case "mind.poll":
          return { items: [], throughSeq: 0 };
        case "tools.list":
          return [];
        case "mind.update":
          return null;
        case "model.complete":
          requests.push(args as unknown as ModelRequest);
          return {
            content: "这轮先到这",
            toolCalls: [],
            usage: {
              status: "reported",
              inputTokens: 2100,
              cachedInputTokens: 2100,
              outputTokens: 6,
            },
          };
        default:
          throw new Error(`unexpected ${contract}.${method}`);
      }
    },
  } as unknown as PluginContext;
  const promptFile = `${directory}/prompt`;
  const runtime = new EveRuntime(context, {
    promptFile,
    directory: `${directory}/journal`,
    sourceCommit: "fixture",
    modelTimeoutMs: 1000,
    maxOutputTokens: 100,
    pollMs: 60_000,
    defaultSleepMs: 0,
    dailyTokenBudget: 1_000_000,
  }, { description: "fixture", count: () => 2000 });
  try {
    const configFile = `${directory}/config.json`;
    const adapterKeyFile = `${directory}/key`;
    await Deno.writeTextFile(adapterKeyFile, "fixture_".repeat(4));
    await Deno.writeTextFile(
      configFile,
      JSON.stringify({
        phone: {
          adapterUrl: `http://127.0.0.1:${server.addr.port}/rpc`,
          adapterKeyFile,
          ownerPlatformUserId: "owner",
        },
      }),
    );
    const plugin = createPhonePlugin(configFile);
    await plugin.load?.(context);
    await plugin.enable?.(context);
    await handlers.open({ conversationId: GROUP });
    await writeEvePrompt(promptFile);
    await runtime.initialize();
    await runtime.pulse();
    await runtime.pulse();
    assert.equal(requests.length, 2);
    assert.ok(
      requests[0].messages.some(({ content }) =>
        content.includes('"chatStyle":{"forms":[{"text":"vemm","placement":"alone"}],"avoid":[]}')
      ),
    );
    assert.ok(
      requests[1].messages.some(({ content }) =>
        content.includes('"chatStyle":{"forms":[],"avoid":["vemm"]}')
      ),
    );
    assert.match(requests[0].messages[0].content, /通常先看看一个常聊且未静音的群/);
    assert.deepEqual(phoneMethods, ["health", "open", "notifications", "notifications"]);
  } finally {
    await runtime.close();
    for (const cleanup of cleanups.reverse()) await cleanup();
    await server.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
