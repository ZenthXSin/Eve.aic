import assert from "node:assert/strict";
import { liveEvent, LiveReader, liveState } from "../services/admin/live.ts";
import { startAdminServer } from "../services/admin/server.ts";
import { panelHtml } from "../services/admin/view.ts";
import type { AdminBackend } from "../services/admin/contracts.ts";
import type { Json } from "../kernel/protocol.ts";

const status = {
  supervisor: { paused: false },
  runtime: { available: true, throughSeq: 0, budget: {}, model: {} },
};
const event = (seq: number, kind: string, data: Json = {}) => ({
  seq,
  kind,
  at: "2026-11-12T00:00:00Z",
  data,
});

Deno.test("live projection preserves explicit text, handles truncation and never invents outcomes", () => {
  const e = liveEvent(
    event(1, "model_response", {
      requestId: "model_1",
      response: { content: "这是我留下的文字", toolCalls: [] },
    }),
  );
  assert.equal(e.text, "这是我留下的文字");
  assert.equal(e.category, "thought");
  const noText = liveEvent(
    event(2, "model_response", {
      response: { content: "", toolCalls: [{ name: "execute_script" }] },
    }),
  );
  assert.match(noText.text, /未留下文字/);
  const truncated = liveEvent({
    ...event(3, "model_response"),
    truncated: true,
    preview: "private partial output",
  });
  assert.match(truncated.text, /未据此推断/);
  assert(!truncated.text.includes("private partial output"));
  const result = liveEvent(
    event(4, "tool_result", { result: { status: "completed", output: "not a delivered receipt" } }),
  );
  assert.match(result.text, /不等于消息已送达/);
  assert(!result.text.includes("not a delivered receipt"));
});

Deno.test("live thoughts include explicit tool notes once and preserve literal markup", () => {
  const thought = "想先看看群里的近况。<img src=x onerror=alert(1)>";
  const response: {
    content: string;
    toolCalls: { id: string; name: string; args: Record<string, Json> }[];
  } = {
    content: thought,
    toolCalls: [
      { id: "note_1", name: "write_thought", args: { thought } },
      { id: "script_1", name: "execute_script", args: { thought, code: "unused" } },
    ],
  };
  const original = structuredClone(response);
  const projected = liveEvent(event(1, "model_response", { response }));
  assert.equal(projected.text, thought);
  assert.equal(projected.hasText, true);
  assert.deepEqual(response, original);
  const toolOnly = liveEvent(event(2, "model_response", {
    response: { ...response, content: "" },
  }));
  assert.equal(toolOnly.text, thought);
  assert.equal(toolOnly.hasText, true);
  const ordinary = liveEvent(event(3, "tool_start", {
    callId: "script_1",
    call: response.toolCalls[1],
  }));
  assert.equal(ordinary.thoughtWrite, undefined);
});

Deno.test("live reader associates native note receipts across refreshes without hiding failures", async () => {
  const records = [
    event(1, "model_response", {
      response: {
        content: "",
        toolCalls: [{ id: "note_1", name: "write_thought", args: { thought: "想休息一会儿。" } }],
      },
    }),
    event(2, "tool_start", {
      callId: "note_1",
      call: { id: "note_1", name: "write_thought", args: {} },
    }),
    event(3, "tool_result", { callId: "note_1", result: { status: "failed" } }),
  ];
  let through = 2;
  const reader = new LiveReader({
    status: () =>
      Promise.resolve({ ...status, runtime: { ...status.runtime, throughSeq: through } }),
    query: ({ args }) =>
      Promise.resolve({
        items: records.slice(Number(args.afterSeq), Number(args.throughSeq)),
      }),
    control: () => {
      throw new Error("read only");
    },
  });
  const pending = await reader.read() as Record<string, Json>;
  assert.equal((pending.state as Record<string, Json>).phase, "tool");
  through = 3;
  const complete = await reader.read() as Record<string, Json>;
  const events = complete.events as Record<string, Json>[];
  assert.equal(events[1].thoughtWrite, true);
  assert.equal(events[2].thoughtWrite, true);
  assert.equal(events[2].category, "error");
  assert.equal((complete.thought as Record<string, Json>).text, "想休息一会儿。");
  assert.equal(events.length, 3);
});

Deno.test("live state pairs requests and recovery, respects pause and blocking", () => {
  const started = liveEvent(event(1, "model_start", { requestId: "model_1" }));
  assert.equal(liveState([started], status).phase, "model");
  const recovered = liveEvent(event(2, "recovery", { requestId: "model_1", result: "结果未知" }));
  assert.equal(liveState([started, recovered], status).phase, "attention");
  const tool = liveEvent(
    event(3, "tool_start", { callId: "call_1", call: { name: "execute_script" } }),
  );
  assert.equal(liveState([tool], status).phase, "tool");
  const result = liveEvent(
    event(4, "tool_result", { callId: "call_1", result: { status: "completed" } }),
  );
  assert.equal(liveState([tool, result], status).phase, "result");
  assert.equal(liveState([tool], { ...status, supervisor: { paused: true } }).phase, "paused");
  assert.equal(
    liveState([started], {
      runtime: { available: true, model: { blocked: { requestId: "model_1" } } },
    }).phase,
    "attention",
  );
  assert.equal(liveState([], status).phase, "waiting");
});

Deno.test("live reader bounds windows, tails new records, coalesces reads and resets on rollback", async () => {
  let through = 100, reads = 0;
  const backend: AdminBackend = {
    status: () =>
      Promise.resolve({ ...status, runtime: { ...status.runtime, throughSeq: through } }),
    query: ({ args }) => {
      reads++;
      const start = Number(args.afterSeq), end = Number(args.throughSeq);
      return Promise.resolve({
        items: Array.from(
          { length: Math.min(8, end - start) },
          (_, i) => event(start + i + 1, "wake", { reason: "test" }),
        ),
      });
    },
    control: () => {
      throw new Error("read only");
    },
  };
  const reader = new LiveReader(backend);
  const [a, b] = await Promise.all([reader.read(), reader.read()]) as Record<string, Json>[];
  assert.deepEqual(a, b);
  assert.equal(reads, 10);
  assert.equal((a.events as Json[]).length, 80);
  await reader.read();
  assert.equal(reads, 10);
  through = 101;
  const next = await reader.read() as Record<string, Json>;
  assert.equal(reads, 11);
  assert.equal((next.events as Json[]).length, 80);
  through = 3;
  const rolledBack = await reader.read() as Record<string, Json>;
  assert.equal((rolledBack.events as Json[]).length, 3);
  assert.equal(rolledBack.throughSeq, 3);
});

Deno.test("live endpoint requires a session, rejects gaps and never invokes controls", async () => {
  const backend: AdminBackend = {
    status: () => Promise.resolve({ ...status, runtime: { ...status.runtime, throughSeq: 3 } }),
    query: () => Promise.resolve({ items: [event(3, "wake")] }),
    control: () => {
      throw new Error("read only");
    },
  };
  await assert.rejects(() => new LiveReader(backend).read(), /顺序无效/);
  const server = await startAdminServer(backend, { token: "live_test_password" });
  try {
    const anonymous = await fetch(server.url + "/api/live");
    assert.equal(anonymous.status, 401);
    await anonymous.json();
    const login = await fetch(server.url + "/api/login", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ token: "live_test_password" }),
    });
    await login.json();
    const cookie = login.headers.get("set-cookie")!.split(";")[0];
    const response = await fetch(server.url + "/api/live", { headers: { cookie } });
    assert.equal(response.status, 503);
    const result = await response.json();
    assert.equal(result.ok, false);
  } finally {
    await server.close();
  }
});

Deno.test("live page displays a note once, retains ordinary tools and exposes note failures as text", async () => {
  class Element {
    className = "";
    dataset: Record<string, string> = {};
    children: Element[] = [];
    ownText = "";
    hidden = false;
    open = false;
    value = "";
    scrollHeight = 0;
    scrollTop = 0;
    clientHeight = 0;
    constructor(readonly tag = "div") {}
    get textContent(): string {
      return this.ownText + this.children.map((c) => c.textContent).join("");
    }
    set textContent(value: string) {
      this.ownText = value;
      this.children = [];
    }
    append(...children: Element[]) {
      this.children.push(...children);
    }
    replaceChildren(...children: Element[]) {
      this.children = children;
      this.ownText = "";
    }
    setAttribute() {}
    addEventListener() {}
    close() {}
    querySelectorAll() {
      return [];
    }
  }
  const elements = new Map<string, Element>();
  const document = {
    getElementById(id: string) {
      if (!elements.has(id)) elements.set(id, new Element());
      return elements.get(id)!;
    },
    querySelectorAll() {
      return [];
    },
    createElement(tag: string) {
      return new Element(tag);
    },
  };
  const fetch = () =>
    Promise.resolve({
      status: 401,
      json: () => Promise.resolve({ ok: false, error: { message: "请先登录" } }),
    });
  const script = panelHtml("notes").match(/<script nonce="notes">([\s\S]*?)<\/script>/)![1];
  const paint = new Function(
    "document",
    "fetch",
    `${script}\nreturn (snapshot) => { liveSnapshot=snapshot; paintLive(snapshot); };`,
  )(document, fetch);
  // 等待启动时的未登录检查，不让它清理后续的测试画面。
  await new Promise((resolve) => setTimeout(resolve, 0));
  const note = "想先整理一下。<img src=x onerror=alert(1)>";
  const events = [
    liveEvent(event(1, "model_response", {
      response: {
        content: note,
        toolCalls: [{ id: "n", name: "write_thought", args: { thought: note } }],
      },
    })),
    {
      ...liveEvent(event(2, "tool_start", { callId: "n", call: { name: "write_thought" } })),
      thoughtWrite: true,
    },
    {
      ...liveEvent(event(3, "tool_result", { callId: "n", result: { status: "completed" } })),
      thoughtWrite: true,
    },
    liveEvent(event(4, "tool_start", { callId: "s", call: { name: "execute_script" } })),
    liveEvent(event(5, "tool_result", { callId: "s", result: { status: "completed" } })),
    {
      ...liveEvent(event(6, "tool_result", { callId: "bad", result: { status: "failed" } })),
      thoughtWrite: true,
    },
  ];
  paint({ events, state: { title: "已返回", description: "", phase: "result" }, throughSeq: 6 });
  const feed = document.getElementById("liveFeed");
  assert.equal(feed.textContent.split(note).length - 1, 1);
  assert(!feed.textContent.includes("原件 #2"));
  assert(!feed.textContent.includes("原件 #3"));
  assert(feed.textContent.includes("原件 #4"));
  assert(feed.textContent.includes("原件 #5"));
  assert(feed.textContent.includes("原件 #6"));
  assert(feed.textContent.includes("回执状态：failed"));
  assert(feed.textContent.includes("调用 1 次脚本工具"));
  assert.equal(document.getElementById("liveFeedSummary").textContent, "6 条原始记录 · 正在跟随");
  const tags = (e: Element): string[] => [e.tag, ...e.children.flatMap(tags)];
  assert(!tags(feed).includes("img"));
});
