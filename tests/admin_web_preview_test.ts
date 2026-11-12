import assert from "node:assert/strict";
import { type Json } from "../kernel/protocol.ts";
import { withNativeOperations, WorkspaceReader } from "../services/admin/workspace.ts";
import { TOOL_CATALOG } from "../services/admin/tool-catalog.ts";
import { workspaceScript } from "../services/admin/workspace-view.ts";

class Element {
  children: Element[] = [];
  ownText = "";
  className = "";
  dataset: Record<string, string> = {};
  attributes: Record<string, string> = {};
  value: string | number = "";
  max = 0;
  scrollTop = 0;
  scrollHeight = 0;
  clientHeight = 0;
  constructor(readonly tag: string) {}
  set textContent(value: string) {
    this.ownText = value;
    this.children = [];
  }
  get textContent(): string {
    return this.ownText + this.children.map((c) => c.textContent).join("");
  }
  append(...children: Element[]) {
    this.children.push(...children);
  }
  replaceChildren(...children: Element[]) {
    this.children = children;
    this.ownText = "";
  }
  setAttribute(name: string, value: string) {
    this.attributes[name] = value;
  }
  addEventListener() {}
  querySelectorAll() {
    return [];
  }
}
const nodes = (element: Element): Element[] => [element, ...element.children.flatMap(nodes)];
const visible = (element: Element): string =>
  element.tag === "details" ? "" : element.ownText + element.children.map(visible).join("");
function fixture() {
  const elements = new Map<string, Element>();
  let apiCalls = 0;
  const el = (id: string) => {
    if (!elements.has(id)) elements.set(id, new Element("div"));
    return elements.get(id)!;
  };
  const document = {
    querySelectorAll: () => [],
    createElementNS: (_: string, tag: string) => new Element(tag),
  };
  const node = (tag: string, css = "", text?: string) => {
    const e = new Element(tag);
    e.className = css;
    if (text !== undefined) e.textContent = text;
    return e;
  };
  const renderer = new Function(
    "document",
    "el",
    "node",
    "time",
    "num",
    "pretty",
    "api",
    `let csrf='test',liveSnapshot=null;${workspaceScript}
    return {paint(snapshot){liveSnapshot=snapshot;paintWorkspace(snapshot);}, choose(app){workspaceFollow=false;workspaceApp=app;workspaceOperation=null;workspaceKey='';paintWorkspace(liveSnapshot);},select(id){workspaceOperation=id;workspaceKey='';paintWorkspace(liveSnapshot);}};`,
  )(document, el, node, String, String, JSON.stringify, () => {
    apiCalls++;
    throw new Error("preview must not execute or fetch");
  });
  return { renderer, el, calls: () => apiCalls };
}
const at = "2026-11-12T00:00:00Z";
function operation(id: string, method: string, args: unknown, result?: unknown, extra = {}) {
  return {
    id,
    namespace: "web",
    method,
    args,
    ...(result === undefined ? {} : { result }),
    at,
    startedAt: at,
    sourceSeq: 9,
    status: "completed",
    ...extra,
  };
}
function snapshot(items: unknown[]) {
  return {
    events: [{ kind: "model_response", hasText: true, text: "右侧真实随记，只显示一次" }],
    workspace: {
      asOf: at,
      currentApp: { appId: "browser", operation: "web.fetch" },
      apps: {
        browser: { available: true, items },
        tools: { available: true, catalog: TOOL_CATALOG, items },
      },
    },
  };
}
Deno.test("web operations project into browser and remain in complete tool catalog", async () => {
  const op = operation("search-1", "search", { query: "actual query" }, {
    query: "actual query",
    results: [],
  });
  const reader = new WorkspaceReader({
    status: () => Promise.resolve(null),
    control: () => {
      throw new Error("read only");
    },
    query: (request) =>
      Promise.resolve(
        request.resource === "workspace" ? { throughSeq: 9, items: [op] } as Json : { items: [] },
      ),
  });
  const projection = await reader.read() as {
    currentApp: { appId: string };
    apps: Record<string, { items: { id: string }[] }>;
  };
  assert.equal(projection.currentApp.appId, "browser");
  assert.equal(projection.apps.browser.items[0].id, "search-1");
  assert.equal(projection.apps.tools.items[0].id, "search-1");
  assert.deepEqual(TOOL_CATALOG.find((g) => g.namespace === "web")!.methods, ["search", "fetch"]);
  const joined = withNativeOperations(projection as unknown as Json, []) as typeof projection;
  assert.equal(joined.currentApp.appId, "browser");
});
Deno.test("web search has readable source cards in browser and tools without loading or executing external content", () => {
  const f = fixture();
  const title = "<img src=x onerror=alert(1)>",
    snippet = "<script>alert(1)</script>实际摘要",
    unsafeUrl = "javascript:alert('do not execute')";
  f.renderer.paint(snapshot([operation("search", "search", { query: "Eve 实际搜索词" }, {
    query: "Eve 实际搜索词",
    sourceUrl: "https://search.example/?q=Eve",
    title: "真实搜索来源",
    fetchedAt: at,
    untrusted: true,
    results: [{ title, snippet, url: unsafeUrl }, {
      title: "第二个标题",
      snippet: "第二条摘要",
      url: "https://docs.example/a",
    }],
  })]));
  for (const app of ["browser", "tools"]) {
    f.renderer.choose(app);
    const screen = f.el("workspaceApp"), body = visible(screen);
    assert.match(body, /实际搜索词.*Eve 实际搜索词/);
    assert(body.includes(title));
    assert(body.includes(snippet));
    assert(body.includes(unsafeUrl));
    assert(body.includes("https://docs.example/a"));
    assert(body.includes(at));
    assert.match(body, /本次返回 2 条结果/);
    assert.match(body, /外部资料/);
    assert(!body.includes("右侧真实随记"));
    assert(
      !nodes(screen).some((e) => ["script", "img", "iframe", "a", "form", "input"].includes(e.tag)),
    );
    assert(nodes(screen).some((e) => e.className === "ws-web-result"));
  }
  assert.equal(f.el("workspaceThoughtCount").textContent, "1 则");
  assert.equal(f.calls(), 0);
});
Deno.test("web fetch shows literal document page, source time, digest and actual pagination without fetching next pages", () => {
  const f = fixture(), sha256 = "a".repeat(64), content = "<h1>这是正文，不是HTML</h1>\n下一行原文";
  const items = [
    operation(
      "page-1",
      "fetch",
      { url: "https://original.example/doc", startChar: 100, limit: 60 },
      {
        title: "真实网页标题",
        sourceUrl: "https://final.example/doc",
        fetchedAt: at,
        untrusted: true,
        content,
        characters: 1000,
        startChar: 100,
        nextStartChar: 160,
        sha256,
        truncated: true,
      },
    ),
    operation("page-2", "fetch", {
      url: "https://final.example/doc",
      startChar: 160,
      expectedSha256: sha256,
    }, {
      title: "真实网页标题",
      sourceUrl: "https://final.example/doc",
      fetchedAt: at,
      untrusted: true,
      content: "后页真实内容",
      characters: 1000,
      startChar: 160,
      sha256,
      truncated: false,
    }),
  ];
  f.renderer.paint(snapshot(items));
  f.renderer.select("page-1");
  const screen = f.el("workspaceApp"), body = visible(screen);
  assert(body.includes(content));
  assert(body.includes("真实网页标题"));
  assert(body.includes("https://original.example/doc"));
  assert(body.includes("https://final.example/doc"));
  assert(body.includes(sha256));
  assert(body.includes(at));
  assert.match(body, /本页起点 100 \/ 全文 1000 字符/);
  assert.match(body, /下一页起点 160/);
  assert.match(body, /正文还有后续内容/);
  const progress = nodes(screen).find((e) => e.tag === "progress")!;
  assert.equal(progress.max, 1000);
  assert.equal(progress.value, 100 + content.length);
  assert(!nodes(screen).some((e) => ["h1", "iframe", "a", "script"].includes(e.tag)));
  f.renderer.select("page-2");
  const next = visible(screen);
  assert.match(next, /后页真实内容/);
  assert.match(next, /本页起点 160/);
  assert(!next.includes(content));
  assert.match(next, /未标记后续页/);
  assert.equal(f.calls(), 0);
});
Deno.test("web previews distinguish pending, failed, empty and bounded pages without inventing results or read progress", () => {
  const f = fixture();
  f.renderer.paint(
    snapshot([
      operation("pending", "search", { query: "waiting" }, undefined, { status: "running" }),
    ]),
  );
  let body = visible(f.el("workspaceApp"));
  assert.match(body, /正在搜索，尚未取得搜索结果/);
  assert(!body.includes("取得时间"));
  assert(!body.includes("本次返回 0"));
  f.renderer.paint(snapshot([operation("empty", "search", { query: "nothing" }, { results: [] })]));
  assert.match(visible(f.el("workspaceApp")), /本次搜索没有返回结果/);
  f.renderer.paint(
    snapshot([
      operation("failed", "fetch", { url: "http://127.0.0.1/" }, undefined, {
        status: "failed",
        error: { message: "公开地址验证拒绝" },
      }),
    ]),
  );
  body = visible(f.el("workspaceApp"));
  assert.match(body, /没有取得可展示的网页正文.*公开地址验证拒绝/);
  assert(!body.includes("全文"));
  f.renderer.paint(
    snapshot([
      operation("partial", "fetch", { url: "https://example.com/" }, {
        content: "有界正文\n…",
        startChar: 20,
        characters: 9000,
        nextStartChar: 8192,
        truncated: true,
      }, { partial: true }),
    ]),
  );
  const screen = f.el("workspaceApp");
  assert.match(visible(screen), /本地有界预览.*下一页起点 8192.*有界预览/);
  assert(!nodes(screen).some((e) => e.tag === "progress"));
  assert.equal(f.calls(), 0);
});
