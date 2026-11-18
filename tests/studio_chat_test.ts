import assert from "node:assert/strict";
import type { Json } from "../kernel/protocol.ts";
import { CHAT_PAGE, CHAT_TURNS } from "../services/admin/studio-chat-contract.ts";
import { StudioChatMailbox } from "../supervisor/studio-chat.ts";
import { StudioChatTurns, studioModelOptions } from "../supervisor/studio-chat-runtime.ts";
import type { RuntimeEngine } from "../services/runtime/engine.ts";

type Port = Pick<RuntimeEngine, "dispatch" | "receipt" | "modelStatus">;
const testPort = (overrides: Partial<Port> = {}): Port => ({
  dispatch: () => {
    throw Error("unexpected model call");
  },
  receipt: () => undefined,
  modelStatus: () => ({ blocked: null }),
  ...overrides,
});
const submit = (box: StudioChatMailbox, id: string, text: string) =>
  box.handle({ action: "chatSend", id, text });
const answer = (replyTo: string, text: string): Json => ({
  content: JSON.stringify({ replyTo, text }),
  toolCalls: [],
  modelOutput: [{ private: "NEVER PROJECT" }],
});
function fixture(
  run: (
    first: StudioChatMailbox,
    second: StudioChatMailbox,
    directory: string,
  ) => void | Promise<void>,
) {
  return async () => {
    const parent = Deno.makeTempDirSync({ prefix: "studio-chat-test-" }),
      directory = `${parent}/chat`;
    try {
      const first = StudioChatMailbox.open(directory);
      try {
        const second = StudioChatMailbox.open(directory);
        try {
          await run(first, second, directory);
        } finally {
          second.close();
        }
      } finally {
        first.close();
      }
    } finally {
      Deno.removeSync(parent, { recursive: true });
    }
  };
}

Deno.test(
  "Web 私聊：两连接原子准入；同编号、不同正文及并发模型收件",
  fixture(async (web, consumer, path) => {
    const id = crypto.randomUUID(), second = crypto.randomUUID();
    const first = submit(web, id, "  你好\nEve  ");
    assert.deepEqual(submit(consumer, id, "你好\nEve"), first);
    assert.throws(() => submit(consumer, id, "改动"), /编号/);
    assert.throws(() => submit(web, second, "过早发送"), /上一条/);
    assert.equal(Deno.statSync(path).mode! & 0o777, 0o700);
    assert.equal(Deno.statSync(`${path}/chat.sqlite`).mode! & 0o777, 0o600);
    const deferred = Promise.withResolvers<Json>();
    let billed = 0;
    const turns = new StudioChatTurns(
      consumer,
      testPort({
        dispatch(namespace, method, args, actionId) {
          billed++;
          assert.deepEqual([namespace, method, actionId], [
            "model",
            "complete",
            `studio_web_${id}`,
          ]);
          assert.deepEqual(Object.keys(args).sort(), ["_purpose", "messages", "tools"]);
          assert.equal(args._purpose, "isolated");
          assert.deepEqual(args.tools, []);
          const messages = args.messages as { role: string; content: string }[];
          assert.deepEqual(messages.map((item) => item.role), ["system", "user"]);
          assert.equal(messages[1].content, "你好\nEve");
          return deferred.promise;
        },
      }),
    );
    assert(turns.pending());
    assert.equal(billed, 0);
    const active = turns.offer("studio_slot_17", {});
    assert.equal(web.page().messages[0].status, "processing");
    assert.equal(await turns.offer("studio_slot_17", {}), true);
    assert.equal(await turns.offer("studio_slot_18", {}), false);
    assert.throws(() => submit(web, second, "仍未完成"), /上一条/);
    deferred.resolve(answer(id, "实际回执正文"));
    assert.equal(await active, true);
    const page = web.page();
    assert.deepEqual(page.messages.map((msg) => [msg.seq, msg.role, msg.status]), [[
      1,
      "user",
      "answered",
    ], [2, "assistant", "sent"]]);
    assert.equal(page.messages[1].text, "实际回执正文");
    assert.equal(page.messages[1].replyTo, id);
    assert(!JSON.stringify(page).includes("NEVER PROJECT"));
    assert.deepEqual(submit(web, id, "你好\nEve"), { message: page.messages[0], mode: "isolated" });
    const reopened = StudioChatMailbox.open(path);
    try {
      assert.deepEqual(reopened.page(), page);
    } finally {
      reopened.close();
    }
    submit(web, second, "下一轮");
    assert.equal(await turns.offer("studio_slot_17", {}), true);
    assert(web.queued());
    assert.equal(billed, 1);
    const claim = consumer.claim("studio_slot_19");
    assert.deepEqual(claim.history?.map((msg) => msg.content), [
      "你好\nEve",
      "实际回执正文",
      "下一轮",
    ]);
  }),
);

Deno.test(
  "私聊恢复只读原动作回执；缺失即 unknown，永不重派",
  fixture((_web, consumer, path) => {
    for (const known of [false, true]) {
      const id = crypto.randomUUID();
      submit(consumer, id, "恢复测试");
      const slot = `studio_slot_${known ? 2 : 1}`;
      consumer.claim(slot);
      const restarted = StudioChatMailbox.open(path);
      try {
        let lookups = 0;
        const turns = new StudioChatTurns(
          restarted,
          testPort({
            receipt(actionId) {
              lookups++;
              assert.equal(actionId, `studio_web_${id}`);
              return known
                ? {
                  id: actionId,
                  namespace: "model",
                  method: "complete",
                  hash: "",
                  recovery: null,
                  terminal: true,
                  value: answer(id, "已有回执"),
                }
                : undefined;
            },
          }),
        );
        turns.recover();
        turns.recover();
        assert.equal(lookups, 1);
        assert.deepEqual(restarted.claim(slot), { used: true });
        const item = restarted.page().messages.find((msg) => msg.id === id)!;
        assert.equal(item.status, known ? "answered" : "unknown");
        assert.deepEqual(submit(restarted, id, "恢复测试"), { message: item, mode: "isolated" });
      } finally {
        restarted.close();
      }
    }
  }),
);

Deno.test(
  "错误地址、调用工具、空回话、供应商封包及错误信息绝不成为 Eve 消息",
  fixture(async (box) => {
    const cases = [
      (id: string) => ({
        content: JSON.stringify({ replyTo: id, text: "不应显示" }),
        toolCalls: [{ name: "execute_script", args: { private: "SECRET" } }],
      }),
      () => ({ content: "SECRET plain unaddressed response", toolCalls: [] }),
      () => answer(crypto.randomUUID(), "SECRET other thread"),
      (id: string) => ({
        content: JSON.stringify({ replyTo: id, text: "SECRET", reasoning: "SECRET" }),
        toolCalls: [],
      }),
      (id: string) => answer(id, " "),
      (id: string) => answer(id, "猫".repeat(3000)),
      () => {
        throw Error("SECRET upstream URL and headers");
      },
    ];
    let calls = 0;
    for (const entry of cases) {
      const id = crypto.randomUUID();
      submit(box, id, "常规文本");
      const turns = new StudioChatTurns(
        box,
        testPort({
          dispatch: () => {
            calls++;
            return Promise.resolve(entry(id) as Json);
          },
        }),
      );
      assert(await turns.offer(`studio_slot_${calls + 1}`, {}));
    }
    assert.equal(calls, cases.length);
    const page = box.page();
    assert(!JSON.stringify(page).includes("SECRET"));
    assert(
      page.messages.every((msg) =>
        msg.role === "user" && ["failed", "unknown"].includes(msg.status)
      ),
    );
    const id = crypto.randomUUID();
    submit(box, id, "最新");
    assert.deepEqual(box.claim("studio_slot_98").history, [{ role: "user", content: "最新" }]);
  }),
);

Deno.test(
  "分页边界、历史输入和终身幂等表均有硬上限；满库不淘汰交易编号",
  fixture((box) => {
    const ids: string[] = [];
    const text = "文".repeat(4096), reply = "答".repeat(2600);
    for (let turn = 1; turn <= CHAT_TURNS; turn++) {
      const id = crypto.randomUUID();
      ids.push(id);
      submit(box, id, text);
      const claim = box.claim(`studio_slot_${turn}`);
      assert(claim.history && claim.history.length <= 13);
      assert(
        claim.history.reduce(
          (total, msg) => total + new TextEncoder().encode(msg.content).length,
          0,
        ) <= 24_576,
      );
      box.finish(id, "answered", reply);
    }
    const seen = new Set<number>();
    let cursor: number | undefined;
    do {
      const page = box.page(cursor);
      assert(page.messages.length <= CHAT_PAGE);
      for (const msg of page.messages) {
        assert(!seen.has(msg.seq));
        seen.add(msg.seq);
      }
      cursor = page.nextBefore ?? undefined;
    } while (cursor !== undefined);
    assert.equal(seen.size, CHAT_TURNS * 2);
    assert.deepEqual(box.page(1).messages, []);
    assert.deepEqual(box.page(2).messages.map((msg) => msg.seq), [1]);
    assert.throws(() => submit(box, crypto.randomUUID(), "新"), /存储已满/);
    assert.doesNotThrow(() => submit(box, ids[0], text));
    assert.throws(() => submit(box, ids[0], "新文本"), /编号/);
  }),
);

Deno.test("网页私聊独立会话不超过当前模型输出上限，不改变普通轮次", () => {
  const id = `studio_web_${crypto.randomUUID()}`;
  assert.deepEqual(studioModelOptions(id, "isolated", 8192), {
    sessionId: id,
    maxOutputTokens: 2048,
  });
  assert.equal(studioModelOptions(id, "isolated", 500)?.maxOutputTokens, 500);
  assert.throws(() => studioModelOptions(id, "online", 500));
  assert.equal(studioModelOptions("model_88", "online", 8192), undefined);
});
