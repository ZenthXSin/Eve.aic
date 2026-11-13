import assert from "node:assert/strict";
import type { Json } from "../kernel/protocol.ts";
import { TOOL_CATALOG } from "../services/admin/tool-catalog.ts";
import { workspaceScript, workspaceSocketScript } from "../services/admin/workspace-view.ts";

class Element {
  ownText = "";
  className = "";
  dataset: Record<string, string> = {};
  children: Element[] = [];
  attributes: Record<string, string> = {};
  hidden = false;
  scrollTop = 0;
  scrollHeight = 0;
  clientHeight = 0;
  title = "";
  constructor(readonly tag = "div") {}
  get textContent(): string {
    return this.ownText + this.children.map((c) => c.textContent).join("");
  }
  set textContent(value: string) {
    this.ownText = value;
    this.children = [];
  }
  append(...items: Element[]) {
    this.children.push(...items);
  }
  replaceChildren(...items: Element[]) {
    this.children = items;
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
type HistoryQuery = {
  resource: "qqHistory";
  operation: "page";
  args: {
    conversationId: string;
    cursor?: { snapshotThroughSeq: number; beforeSeq: number };
  };
};
function viewFixture(
  api: (path: string, body: HistoryQuery) => Promise<Json> = () => Promise.resolve({}),
) {
  const elements = new Map<string, Element>();
  const el = (id: string) => {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id)!;
  };
  const document = {
    querySelectorAll: () => [],
    createElementNS: (_ns: string, tag: string) => new Element(tag),
  };
  const node = (tag: string, className = "", text?: string) => {
    const n = new Element(tag);
    n.className = className;
    if (text !== undefined) n.textContent = text;
    return n;
  };
  const renderer = new Function(
    "document",
    "el",
    "node",
    "time",
    "num",
    "pretty",
    "api",
    `
    let liveSnapshot=null,csrf='session';
    ${workspaceScript}
    return {paint(snapshot){liveSnapshot=snapshot;paintWorkspace(snapshot);},
      choose(app){workspaceFollow=false;workspaceApp=app;workspaceOperation=null;workspaceKey='';paintWorkspace(liveSnapshot);},
      load:loadWorkspaceConversation,refresh:refreshWorkspaceSelected,older:refreshWorkspaceConversation,
      logout(){clearWorkspace();csrf='';},
      clear:clearWorkspace};
  `,
  )(document, el, node, String, String, JSON.stringify, api);
  return { renderer, el };
}
const at = "2026-11-12T00:00:00Z";
Deno.test("workspace renders captured messages, delivery uncertainty and literal text without guessing active conversation", () => {
  const { renderer, el } = viewFixture();
  const app = {
    available: true,
    source: "qq_phone",
    partial: true,
    activeConversationId: "group_1",
    viewedAt: at,
    conversations: [{ conversationId: "group_1", kind: "group", pendingNotifications: 2 }],
    messages: [{
      direction: "outgoing",
      sender: { displayName: { text: "Eve" } },
      time: { at },
      body: { text: "<img src=x onerror=alert(1)>" },
      delivery: { state: "unknown" },
    }],
  };
  const snapshot = {
    events: [],
    workspace: {
      asOf: at,
      currentApp: { appId: "qq", operation: "phone.history" },
      apps: { qq: app },
    },
  };
  renderer.paint(snapshot);
  assert.match(el("workspaceApp").textContent, /<img src=x onerror=alert\(1\)>/);
  assert.match(el("workspaceApp").textContent, /结果未知/);
  assert.match(el("workspaceApp").textContent, /Eve 最近操作的会话/);
  renderer.paint({
    ...snapshot,
    workspace: {
      ...snapshot.workspace,
      apps: { qq: { ...app, activeConversationId: null, messages: [] } },
    },
  });
  assert.match(el("workspaceApp").textContent, /尚未确认查看的会话/);
  assert.match(el("workspaceApp").textContent, /不会从最新来信猜测/);
  renderer.clear();
  assert.equal(el("workspaceApp").textContent, "");
});
Deno.test("workspace shows attachment download states and literal local paths without opening files", () => {
  const { renderer, el } = viewFixture();
  const path = "/tmp/附件/<img src=x onerror=alert(1)>.png";
  const app = {
    available: true,
    source: "qq_phone",
    activeConversationId: "group_1",
    conversations: [{ conversationId: "group_1", kind: "group" }],
    messages: [{
      direction: "incoming",
      attachments: [
        { kind: "image", name: "<script>图片</script>", download: { state: "ready", path } },
        { kind: "file", name: "等待.txt", download: { state: "pending" } },
        { kind: "audio", download: { state: "failed" } },
        { kind: "video", download: { state: "cleared" } },
        { kind: "file", name: "旧消息无状态.txt" },
      ],
    }],
  };
  const snapshot = {
    events: [],
    workspace: { asOf: at, currentApp: { appId: "qq" }, apps: { qq: app } },
  };
  renderer.paint(snapshot);
  const screen = el("workspaceApp");
  assert.match(screen.textContent, /已保存 · Eve 可使用/);
  assert(screen.textContent.includes(path));
  assert(screen.textContent.includes("<script>图片</script>"));
  assert.match(screen.textContent, /下载中/);
  assert.match(screen.textContent, /下载失败 · Eve 可重试下载/);
  assert.match(screen.textContent, /临时文件已清理 · Eve 可重新下载/);
  assert.match(screen.textContent, /旧消息无状态.txt/);
  const nodes = (element: Element): Element[] => [element, ...element.children.flatMap(nodes)];
  assert(!nodes(screen).some((element) => ["img", "script", "a"].includes(element.tag)));
  const states = nodes(screen).filter((element) => element.className === "qq-attachment-status")
    .map((element) => element.dataset.state);
  assert.deepEqual(states, ["ready", "pending", "failed", "cleared"]);
  renderer.paint({
    ...snapshot,
    workspace: {
      ...snapshot.workspace,
      apps: {
        qq: {
          ...app,
          messages: [{
            direction: "incoming",
            attachments: [{ kind: "image", download: { state: "cleared" } }],
          }],
        },
      },
    },
  });
  assert(!screen.textContent.includes(path));
  assert.match(screen.textContent, /临时文件已清理/);
});
Deno.test("workspace previews file contents and tool results while keeping proposed writes distinct", () => {
  const { renderer, el } = viewFixture();
  const operation = (id: string, method: string, args: unknown, result: unknown) => ({
    id,
    namespace: "files",
    method,
    args,
    result,
    status: "completed",
    at,
    sourceSeq: 7,
  });
  const snapshot = {
    events: [],
    workspace: {
      asOf: at,
      currentApp: { appId: "files", operation: "files.read" },
      apps: {
        files: {
          available: true,
          items: [
            operation("read", "read", { path: "/notes.txt" }, {
              path: "/notes.txt",
              text: "文本文档的真实内容",
            }),
          ],
        },
        terminal: {
          available: true,
          items: [{
            ...operation("cmd", "start", { cmd: "printf '%s' '<tag>'" }, {
              state: "completed",
              exitCode: 3,
              stdout: { text: "真实输出" },
              stderr: { text: "真实错误" },
            }),
            namespace: "commands",
          }],
        },
        notes: {
          available: true,
          items: [{
            ...operation("note", "get", { alias: "日常" }, {
              note: { title: "今日", alias: "日常", group: "生活", revision: 4 },
              text: "便签正文",
            }),
            namespace: "notes",
          }],
        },
      },
    },
  };
  renderer.paint(snapshot);
  assert.match(el("workspaceApp").textContent, /文本文档的真实内容/);
  assert.match(el("workspaceApp").textContent, /脚本记录 #7/);
  renderer.choose("terminal");
  assert.match(el("workspaceApp").textContent, /printf '%s' '<tag>'/);
  assert.match(el("workspaceApp").textContent, /真实错误/);
  assert.match(el("workspaceApp").textContent, /退出码 3/);
  assert.equal(el("workspaceViewing").textContent, "你正在手动查看");
  renderer.choose("notes");
  assert.match(el("workspaceApp").textContent, /分组 生活.*@日常.*版本 4/);
  assert.match(el("workspaceApp").textContent, /便签正文/);
  renderer.paint({
    ...snapshot,
    workspace: {
      ...snapshot.workspace,
      apps: { ...snapshot.workspace.apps, notes: { available: false, emptyReason: "当前不可读" } },
    },
  });
  assert.match(el("workspaceApp").textContent, /当前不可读/);
});
Deno.test("workspace websocket applies snapshots, preserves errors, reconnects and ends expired sessions", () => {
  class Socket {
    static instances: Socket[] = [];
    readyState = 0;
    onopen?: () => void;
    onmessage?: (event: { data: string }) => void;
    onerror?: () => void;
    onclose?: ((event: { code: number }) => void) | null;
    constructor(readonly url: URL) {
      Socket.instances.push(this);
    }
    close() {
      this.readyState = 3;
    }
  }
  const elements = new Map<string, Element>();
  const el = (id: string) => {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id)!;
  };
  let applied = 0, failed = 0, expired = 0, retry: (() => void) | undefined;
  const transport = new Function(
    "WebSocket",
    "window",
    "document",
    "el",
    "applyLiveSnapshot",
    "markLiveFailure",
    "showLogin",
    "message",
    "setTimeout",
    "clearTimeout",
    `
    let csrf='session',activeView='live',liveFollow=true;
    ${workspaceSocketScript}
    return {open:openLiveSocket,close:closeLiveSocket,stalled:liveSocketStalled,age(){liveSocketFrameAt=Date.now()-13000;}};
  `,
  )(
    Socket,
    { location: { href: "https://eve.example.com/" } },
    { hidden: false },
    el,
    () => applied++,
    () => failed++,
    () => expired++,
    () => {},
    (callback: () => void) => {
      retry = callback;
      return 1;
    },
    () => {},
  );
  transport.open();
  const socket = Socket.instances[0];
  assert.equal(socket.url.href, "wss://eve.example.com/api/live/ws");
  socket.readyState = 1;
  socket.onopen!();
  assert.equal(transport.stalled(), false);
  transport.age();
  assert.equal(transport.stalled(), true);
  socket.onmessage!({ data: JSON.stringify({ type: "snapshot", value: { throughSeq: 4 } }) });
  assert.equal(applied, 1);
  assert.equal(transport.stalled(), false);
  socket.onmessage!({ data: JSON.stringify({ type: "error", error: { message: "不可读" } }) });
  assert.equal(failed, 1);
  socket.onclose!({ code: 1011 });
  assert.equal(failed, 2);
  retry!();
  assert.equal(Socket.instances.length, 2);
  Socket.instances[1].onclose!({ code: 4001 });
  assert.equal(expired, 1);
  transport.close();
});

Deno.test("every catalog method and unknown future tool has a readable parameter/result preview", () => {
  const { renderer, el } = viewFixture();
  const catalog = [...TOOL_CATALOG, { namespace: "future", methods: ["unrecognized"] }];
  let index = 0;
  for (const group of catalog) {
    for (const method of group.methods) {
      const name = group.namespace + "." + method;
      const operation = {
        id: name,
        namespace: group.namespace,
        method,
        status: index++ % 2 ? "unknown" : "failed",
        at,
        source: group.namespace === "native" ? "timeline" : "script_ledger",
        sourceResource: group.namespace === "native" ? "timeline" : undefined,
        sourceSeq: 7,
        args: { input: "参数-" + name },
        result: { answer: "结果-" + name },
        error: { message: "真实异常-" + name },
        partial: true,
        hasThought: method === "write_thought",
        thoughtRecordSeq: 5,
      };
      renderer.paint({
        events: [],
        workspace: {
          asOf: at,
          currentApp: { appId: "tools", operation: name },
          apps: { tools: { available: true, catalog, items: [operation] } },
        },
      });
      const body = el("workspaceApp").textContent;
      assert(body.includes(name), name);
      assert(body.includes("参数-" + name), name);
      assert(body.includes("结果-" + name), name);
      assert(body.includes("真实异常-" + name), name);
      assert(body.includes("有界预览"), name);
      if (method === "write_thought") assert(body.includes("正文已在随记栏呈现"));
    }
  }
  assert(index >= 55);
});

Deno.test("tools use readable cards for known APIs and preserve non-object inputs", () => {
  const { renderer, el } = viewFixture();
  const visibleText = (element: Element): string =>
    element.tag === "details" ? "" : element.ownText + element.children.map(visibleText).join("");
  const paint = (
    namespace: string,
    method: string,
    args: unknown,
    result: unknown,
    status = "completed",
  ) => {
    renderer.paint({
      events: [],
      workspace: {
        asOf: at,
        currentApp: { appId: "tools", operation: namespace + "." + method },
        apps: {
          tools: {
            available: true,
            items: [{ id: namespace + method, namespace, method, args, result, status, at }],
          },
        },
      },
    });
    return visibleText(el("workspaceApp"));
  };
  assert.match(
    paint("files", "read", { path: "/eve.txt" }, { text: "实际文件正文", sizeBytes: 23 }),
    /文件阅读.*实际文件正文/,
  );
  assert.match(
    paint("commands", "completions", {}, {
      items: [{ sessionId: "cmd-9", state: "completed", exitCode: 7 }],
      throughSeq: 8,
    }),
    /终端完成记录.*cmd-9.*退出码 7/,
  );
  assert.match(
    paint("notes", "get", {}, { note: { alias: "daily", revision: 2 }, text: "便签真实正文" }),
    /@daily.*版本 2.*便签真实正文/,
  );
  assert.match(paint("memory", "used", {}, { reinforced: ["m-1"] }), /记忆强化回执.*m-1/);
  assert.match(
    paint("phone", "settings", {}, { muted: true, pinned: false, draft: "尚未发出的草稿" }),
    /已静音.*未置顶.*尚未发送.*尚未发出的草稿/,
  );
  assert.match(
    paint("phone", "send", { text: "请求正文" }, { actionId: "a-1", state: "unknown" }),
    /拟发送内容.*请求正文.*结果未知/,
  );
  assert.match(
    paint("phone", "listSendQueue", {}, {
      items: [{
        sequence: 1,
        actionId: "a-2",
        receipt: { actionId: "a-2", state: "blocked", blockedBy: "a-1" },
        canCancel: true,
      }],
    }),
    /等待队列.*等待前序.*a-1.*是否可取消/,
  );
  assert.match(
    paint("phone", "history", {}, {
      messages: [{
        attachments: [{
          kind: "file",
          name: "附件.txt",
          download: { state: "ready", path: "/tmp/附件.txt" },
        }],
      }],
    }),
    /附件.txt.*已保存 · Eve 可使用.*\/tmp\/附件.txt/,
  );
  assert.match(
    paint("phone", "image", {}, {
      selectedMode: "describe",
      description: "返回的图片描述",
      download: { path: "/image.png" },
    }),
    /描述模式.*返回的图片描述.*下载路径/,
  );
  const original = paint("phone", "image", {}, {
    selectedMode: "original",
    image: { dataUrl: "data:image/png;base64,秘密原图" },
  });
  assert.match(original, /原图模式.*仅展示附件元数据/);
  assert(!original.includes("秘密原图"));
  assert.match(
    paint("phone", "file", { offsetBytes: 64 }, {
      text: "附件实际片段",
      offsetBytes: 64,
      nextOffsetBytes: 96,
    }),
    /附件实际片段.*字节偏移 64.*后续字节位置 96/,
  );
  assert.match(
    paint("mind", "remind", { text: "提醒正文", at }, { requestId: "r-1", queued: true }),
    /提醒申请.*提醒正文.*已排队.*不表示请求已经应用/,
  );
  assert.match(paint("mind", "status", {}, [{ at, text: "实际提醒项目" }]), /实际提醒项目/);
  assert.match(
    paint("upgrades", "status", {}, {
      state: "submitted",
      outcome: "rolled_back",
      phase: "candidate",
      reason: "真实回退原因",
    }),
    /已提交.*真实回退原因.*当前阶段.*candidate.*最终结果.*rolled_back/,
  );
  assert.match(
    paint("scripts", "status", {}, { jobId: "j-1", status: "running", actions: 4 }),
    /脚本任务.*进行中.*j-1.*实际动作数4/,
  );
  assert.match(
    paint("scripts", "result", {}, { text: '{"answer":"实际脚本输出"}', truncated: false }),
    /answer实际脚本输出/,
  );
  assert.match(
    paint("scripts", "result", {}, {
      text: '{"jobId":"business-job","status":"custom","answer":"真实用户结果"}',
      truncated: false,
    }),
    /真实用户结果/,
  );
  assert.match(
    paint("records", "page", {}, {
      items: [{ seq: 99, kind: "model_response", digest: "实际摘要" }],
    }),
    /意识记录目录.*记录 #99.*实际摘要/,
  );
  assert.match(
    paint("records", "text", { startChar: 20 }, { text: "原文片段", nextChar: 40 }),
    /实际返回的记录原文.*原文片段.*字符偏移 20.*后续位置 40/,
  );
  paint("native", "execute_script", "非法原始参数", false);
  assert(el("workspaceApp").textContent.includes("非法原始参数"));
  assert.match(paint("future", "method", ["原始数组"], 0), /原始数组.*实际回执0/);
  assert.match(
    paint("phone", "futureMethod", { query: "未来参数" }, { answer: "未来方法回执" }),
    /未来参数.*未来方法回执/,
  );
  assert.match(
    paint("native", "write_thought", {}, undefined, "failed"),
    /随记动作.*失败.*未取得有效随记正文/,
  );
  const pending = paint("future", "method", "原始字符串", undefined, "started");
  assert.match(pending, /调用尚待结果/);
});

Deno.test("manual QQ refreshes even Eve's active conversation and retains good messages on failure", async () => {
  let text = "新的来信", fail = false, calls = 0;
  const { renderer, el } = viewFixture((_path, body) => {
    calls++;
    assert.equal(body.resource, "qqHistory");
    assert.deepEqual(body.args, { conversationId: "qq:group:one" });
    if (fail) return Promise.reject(new Error("临时断线"));
    return Promise.resolve({
      messages: [{ handle: { seq: 1 }, body: { text }, direction: "incoming" }],
    });
  });
  renderer.paint({
    events: [],
    workspace: {
      asOf: at,
      currentApp: { appId: "qq" },
      apps: {
        qq: {
          available: true,
          activeConversationId: "qq:group:one",
          conversations: [{ conversationId: "qq:group:one" }],
          messages: [{ body: { text: "旧屏幕" } }],
        },
      },
    },
  });
  await renderer.load("qq:group:one");
  assert.equal(calls, 1);
  assert.match(el("workspaceApp").textContent, /新的来信/);
  assert.doesNotMatch(el("workspaceApp").textContent, /旧屏幕/);
  text = "同一水位下的新状态";
  await renderer.refresh();
  assert.match(el("workspaceApp").textContent, /同一水位下的新状态/);
  fail = true;
  await renderer.refresh();
  assert.match(el("workspaceApp").textContent, /同一水位下的新状态.*临时断线/);
});
Deno.test("late QQ request after logout cannot restore private content", async () => {
  let finish!: (value: Json) => void;
  const { renderer, el } = viewFixture(() =>
    new Promise((resolve) => {
      finish = resolve;
    })
  );
  renderer.paint({
    events: [],
    workspace: { apps: { qq: { available: true, conversations: [], messages: [] } } },
  });
  const pending = renderer.load("qq:group:one");
  renderer.logout();
  finish({ messages: [{ body: { text: "过期的私密正文" } }] });
  await pending;
  assert.equal(el("workspaceApp").textContent, "");
});

Deno.test("QQ older pages retain the requested older content at the bounded limit and can return latest", async () => {
  const make = (start: number, end: number) =>
    Array.from(
      { length: end - start + 1 },
      (_, i) => ({
        handle: { seq: start + i },
        body: { text: "消息[" + (start + i) + "]" },
        direction: "incoming",
      }),
    );
  const { renderer, el } = viewFixture((_path, body) => {
    const before = body.args.cursor?.beforeSeq ?? 441;
    return Promise.resolve({
      messages: make(Math.max(1, before - 40), before - 1),
      ...(before > 41 ? { older: { snapshotThroughSeq: 440, beforeSeq: before - 40 } } : {}),
    });
  });
  renderer.paint({
    events: [],
    workspace: {
      apps: {
        qq: {
          available: true,
          activeConversationId: "qq:group:one",
          conversations: [],
          messages: [],
        },
      },
    },
  });
  await renderer.load("qq:group:one");
  for (let before = 401; before >= 41; before -= 40) {
    await renderer.older("qq:group:one", { snapshotThroughSeq: 440, beforeSeq: before });
  }
  assert.match(el("workspaceApp").textContent, /消息\[1\]/);
  assert.match(el("workspaceApp").textContent, /正在回看历史/);
  await renderer.refresh();
  assert.match(el("workspaceApp").textContent, /消息\[1\]/);
  await renderer.load("qq:group:one");
  assert.match(el("workspaceApp").textContent, /消息\[440\]/);
  assert.doesNotMatch(el("workspaceApp").textContent, /消息\[1\]/);
});
