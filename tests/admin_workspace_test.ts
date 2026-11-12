import assert from "node:assert/strict";
import { ScriptObserver } from "../sdk/script-observer.ts";
import { type JobRecord, ScriptStore } from "../plugins/script-runtime/store.ts";
import {
  boundLiveSnapshot,
  publicLiveEvents,
  withNativeOperations,
  WorkspaceReader,
} from "../services/admin/workspace.ts";
import type { Json } from "../kernel/protocol.ts";
import { liveEvent } from "../services/admin/live.ts";
import { SCRIPT_TOOL_METHODS, TOOL_CATALOG } from "../services/admin/tool-catalog.ts";
import { validateQuery } from "../services/admin/server.ts";

const at = "2026-11-12T01:00:00Z";
function fixtureJob(actions = 1): JobRecord {
  const jobId = "a".repeat(64);
  return {
    jobId,
    fingerprint: "b".repeat(64),
    request: { requestId: "call_1", code: "private script source" },
    status: "completed",
    scriptStarted: true,
    startedAt: at,
    finishedAt: at,
    result: "unused",
    actions: Array.from({ length: actions }, (_, i) => ({
      seq: i + 1,
      actionId: `script_${jobId}_${i + 1}`,
      namespace: "phone",
      method: "history",
      args: { conversationId: "qq:group:real", token: "not-for-browser" },
      status: "completed",
      startedAt: at,
      finishedAt: at,
      value: { messages: [{ direction: "incoming", body: { text: "群里的原消息" } }] },
    })),
  };
}
Deno.test("workspace observer reads content-addressed actions while writer holds lock, bounds chunks and tails commits", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-workspace-observer-" });
  const store = new ScriptStore(directory);
  try {
    await store.initialize();
    await store.change((state) => {
      state.jobs.push(fixtureJob(300));
    });
    const observer = new ScriptObserver(directory);
    const first = await observer.read() as Record<string, Json>;
    assert.equal(first.throughSeq, 1);
    assert.equal((first.items as Json[]).length, 32);
    const item = (first.items as Record<string, Json>[]).at(-1)!;
    assert.equal(item.seq, 300);
    assert.equal((item.args as Record<string, Json>).token, "[凭据字段不展示]");
    assert(!JSON.stringify(first).includes("private script source"));
    assert.equal(first.partial, true);
    assert.equal(await observer.read(), first);
    assert.equal(store.available, true);
    await store.change((state) => {
      state.jobs[0].actions[299].status = "unknown";
      state.jobs[0].actions[299].error = { kind: "unknown", message: "真实未知" };
    });
    const second = await observer.read() as Record<string, Json>;
    assert.equal(second.throughSeq, 2);
    assert.equal((second.items as Record<string, Json>[]).at(-1)!.status, "unknown");
    const filenames = [];
    for await (const file of Deno.readDir(directory)) filenames.push(file.name);
    assert.deepEqual(filenames.sort(), ["commits", "index.json", "objects", "writer.lock"]);
    const commit = `${directory}/commits/events/0000000000000002.json`;
    const raw = await Deno.readTextFile(commit);
    await Deno.writeTextFile(commit, raw.replace('"kind":"script_commit"', '"kind":"fake_commit"'));
    await assert.rejects(() => new ScriptObserver(directory).read(), /摘要链不符/);
  } finally {
    await store.close();
    await Deno.remove(directory, { recursive: true });
  }
});
Deno.test("workspace projection refreshes read-only QQ history while preserving Eve actual captured screen", async () => {
  let reads = 0;
  const message = {
    handle: { conversationId: "qq:group:real", seq: 12 },
    direction: "outgoing",
    body: { text: "失败原文" },
    delivery: { state: "unknown" },
  };
  const reader = new WorkspaceReader({
    status: () => Promise.resolve(null),
    control: () => {
      throw new Error("must not execute");
    },
    query: ({ resource }) => {
      reads++;
      if (resource === "workspace") {
        return Promise.resolve({
          throughSeq: 12,
          items: [{
            id: "real",
            namespace: "phone",
            method: "open",
            status: "completed",
            startedAt: at,
            at,
            sourceSeq: 12,
            args: { conversationId: "qq:group:real" },
            result: { messages: [message] },
          }],
        });
      }
      if (resource === "qqHistory") {
        return Promise.resolve({
          messages: [message, {
            handle: { conversationId: "qq:group:real", seq: 15 },
            body: { text: "后来新到，Eve未看" },
          }],
          snapshotThroughSeq: 15,
        });
      }
      assert.equal(resource, "qqConversations");
      return Promise.resolve({
        items: [{
          conversationId: "qq:group:real",
          kind: "group",
          latest: { body: { text: "后来新到，Eve未看" } },
        }],
        snapshotThroughSeq: 15,
      });
    },
  });
  const a = await reader.read() as Record<string, Json>;
  const qq = (a.apps as Record<string, Record<string, Json>>).qq;
  assert.equal((qq.messages as Json[]).length, 2);
  assert.deepEqual(qq.capturedMessages, [message]);
  assert.deepEqual(qq.capturedDisplayedSeqs, [12]);
  assert.equal(qq.messageSource, "qq_readonly_history");
  assert.equal(qq.messageFreshness, "fresh");
  assert.equal(qq.activeConversationId, "qq:group:real");
  assert.equal((a.currentApp as Record<string, Json>).appId, "qq");
  assert.equal(await reader.read(), a);
  assert.equal(reads, 3);
});
Deno.test("QQ latest page refreshes inbound, delivery and attachments without Eve ledger progress or marking read", async () => {
  let now = Date.parse(at), historyReads = 0;
  let newest = 12, delivery = "unknown", downloaded = false, ledgerWindowRolled = false;
  const captured = [{
    handle: { conversationId: "qq:group:real", seq: 12 },
    body: { text: "Eve 当时看到的正文" },
  }];
  const reading = {
    notificationThroughSeq: 12,
    lastOpened: { snapshotThroughSeq: 12, displayedSeqs: [12] },
  };
  const reader = new WorkspaceReader({
    status: () => Promise.resolve(null),
    control: () => {
      throw new Error("never execute");
    },
    query: ({ resource, operation, args }): Promise<Json> => {
      assert.equal(operation, "page");
      if (resource === "workspace") {
        if (ledgerWindowRolled) return Promise.resolve({ throughSeq: 42, items: [] });
        return Promise.resolve<Json>({
          throughSeq: 42,
          items: [{
            id: "actual-open",
            namespace: "phone",
            method: "open",
            status: "completed",
            startedAt: at,
            args: { conversationId: "qq:group:real" },
            result: {
              messages: captured,
              snapshotThroughSeq: 12,
              displayedSeqs: [12],
              notificationThroughSeq: 12,
            },
          }, {
            id: "failed-open",
            namespace: "phone",
            method: "open",
            status: "failed",
            startedAt: at,
            args: { conversationId: "qq:group:not-actually-opened" },
          }],
        });
      }
      if (resource === "qqConversations") {
        return Promise.resolve<Json>({
          items: [{
            conversationId: "qq:group:another",
            latest: { body: { text: "不要猜最新会话" } },
          }, { conversationId: "qq:group:real", reading }],
          snapshotThroughSeq: newest,
        });
      }
      assert.equal(resource, "qqHistory");
      assert.deepEqual(args, { conversationId: "qq:group:real" });
      historyReads++;
      return Promise.resolve<Json>({
        snapshotThroughSeq: newest,
        messages: [
          {
            handle: { conversationId: "qq:group:real", seq: 12 },
            body: { text: "出站" },
            delivery: { state: delivery },
            attachments: downloaded
              ? [{ index: 1, download: { state: "ready", path: "/tmp/attachment" } }]
              : [],
          },
          ...(newest > 12
            ? [{
              handle: { conversationId: "qq:group:real", seq: newest },
              body: { text: "真实新入站" },
            }]
            : []),
        ],
      });
    },
  }, () => now);
  const screen = async () => {
    const value = await reader.read() as Record<string, Json>;
    assert.equal(value.throughSeq, 42);
    const qq = (value.apps as Record<string, Record<string, Json>>).qq;
    assert.equal(qq.activeConversationId, "qq:group:real");
    assert.deepEqual(qq.capturedMessages, captured);
    assert.equal(qq.capturedSnapshotThroughSeq, 12);
    assert.equal(qq.capturedNotificationThroughSeq, 12);
    assert.deepEqual(qq.readingState, reading);
    return qq;
  };
  assert.equal(((await screen()).messages as Json[]).length, 1);
  now += 2500;
  newest = 15;
  assert.equal(((await screen()).messages as Json[]).length, 2);
  now += 2500;
  delivery = "sent";
  const sent = await screen();
  assert.equal(
    ((sent.messages as Record<string, Json>[])[0].delivery as Record<string, Json>).state,
    "sent",
  );
  now += 2500;
  downloaded = true;
  const ready = await screen();
  assert.equal(((ready.messages as Record<string, Json>[])[0].attachments as Json[]).length, 1);
  assert.equal(ready.historySnapshotThroughSeq, 15);
  assert.equal(historyReads, 4);
  now += 2500;
  ledgerWindowRolled = true;
  const cachedFocus = await screen();
  assert.equal(cachedFocus.activeSourceFreshness, "cached");
  assert.deepEqual(cachedFocus.capturedMessages, captured);
  assert.equal(cachedFocus.messageFreshness, "fresh");
  assert.equal(historyReads, 5);
});

Deno.test("QQ unavailable reads retain labelled confirmed data without leaking another conversation", async () => {
  let now = Date.parse(at), failing = false, ledgerFailing = false, id = "qq:group:real";
  const reader = new WorkspaceReader({
    status: () => Promise.resolve(null),
    control: () => {
      throw new Error("never");
    },
    query: ({ resource }): Promise<Json> => {
      if (resource === "workspace") {
        if (ledgerFailing) return Promise.reject(new Error("ledger down"));
        return Promise.resolve({
          throughSeq: 1,
          items: [{
            id: "open",
            namespace: "phone",
            method: "open",
            status: "completed",
            startedAt: at,
            args: { conversationId: id },
            result: { messages: [] },
          }],
        });
      }
      if (failing) return Promise.reject(new Error("QQ temporarily down"));
      if (resource === "qqConversations") {
        return Promise.resolve({ items: [{ conversationId: id }] });
      }
      return Promise.resolve({
        messages: [{ body: { text: "已确认的消息" } }],
        snapshotThroughSeq: 2,
      });
    },
  }, () => now);
  const screen = async () => {
    const value = await reader.read() as Record<string, Json>;
    return (value.apps as Record<string, Record<string, Json>>).qq;
  };
  const fresh = await screen();
  now += 2500;
  failing = true;
  ledgerFailing = true;
  const stale = await screen();
  assert.equal(stale.available, true);
  assert.deepEqual(stale.messages, fresh.messages);
  assert.deepEqual(stale.conversations, fresh.conversations);
  assert.equal(stale.messagesAsOf, fresh.messagesAsOf);
  assert.equal(stale.messageFreshness, "stale");
  assert.equal(stale.conversationsFreshness, "stale");
  assert.equal(stale.activeSourceFreshness, "stale");
  assert.equal(stale.activeConversationId, "qq:group:real");
  assert.equal(typeof stale.historyError, "string");
  now += 2500;
  ledgerFailing = false;
  id = "qq:group:different";
  const switched = await screen();
  assert.deepEqual(switched.messages, []);
  assert.equal(switched.messageFreshness, "unavailable");
  assert(!JSON.stringify(switched.messages).includes("已确认的消息"));
});

Deno.test("QQ remains viewable from confirmed Eve receipts when admin QQ reads are unavailable", async () => {
  let includeReceipt = true;
  const backend = {
    status: () => Promise.resolve(null),
    control: () => {
      throw new Error("never");
    },
    query: ({ resource }: { resource: string }): Promise<Json> => {
      if (resource !== "workspace") return Promise.reject(new Error("QQ admin down"));
      return Promise.resolve({
        items: includeReceipt
          ? [{
            id: "read",
            namespace: "phone",
            method: "history",
            status: "completed",
            startedAt: at,
            args: { conversationId: "qq:group:real" },
            result: {
              messages: [{
                handle: { conversationId: "qq:group:real", seq: 1 },
                body: { text: "真实回执保留" },
              }],
            },
          }]
          : [],
      });
    },
  };
  const value = await new WorkspaceReader(backend).read() as Record<string, Json>;
  const qq = (value.apps as Record<string, Record<string, Json>>).qq;
  assert.equal(qq.available, true);
  assert.equal(qq.messageFreshness, "stale");
  assert.equal(qq.messageSource, "eve_script_receipt");
  assert.equal(qq.conversationsFreshness, "unavailable");
  assert.equal((qq.messages as Json[]).length, 1);
  includeReceipt = false;
  const absent = await new WorkspaceReader(backend).read() as Record<string, Json>;
  assert.equal((absent.apps as Record<string, Record<string, Json>>).qq.available, false);
});

Deno.test("QQ conversation list alone never invents an active screen or queries its history", async () => {
  const calls: string[] = [];
  const reader = new WorkspaceReader({
    status: () => Promise.resolve(null),
    control: () => {
      throw new Error("never");
    },
    query: ({ resource }): Promise<Json> => {
      calls.push(resource);
      assert.notEqual(resource, "qqHistory");
      return Promise.resolve({
        items: resource === "workspace" ? [] : [{ conversationId: "qq:group:newest" }],
      });
    },
  });
  const value = await reader.read() as Record<string, Json>;
  const qq = (value.apps as Record<string, Record<string, Json>>).qq;
  assert.equal(qq.activeConversationId, null);
  assert.equal(qq.messageFreshness, "unavailable");
  assert.equal(calls.length, 2);
});

Deno.test("workspace queries reject file paths, phone open and invalid cursors", () => {
  assert.throws(() =>
    validateQuery({ resource: "workspace", operation: "page", args: { path: "/root" } })
  );
  assert.throws(() =>
    validateQuery({
      resource: "qqHistory",
      operation: "page",
      args: { conversationId: "qq:group:a", markRead: true },
    })
  );
  assert.throws(() =>
    validateQuery({
      resource: "qqHistory",
      operation: "open",
      args: { conversationId: "qq:group:a" },
    })
  );
  assert.throws(() =>
    validateQuery({
      resource: "qqConversations",
      operation: "page",
      args: { cursor: { snapshotThroughSeq: 4, beforeLastSeq: 5 } },
    })
  );
  assert.equal(
    validateQuery({
      resource: "qqHistory",
      operation: "page",
      args: { conversationId: "qq:group:a" },
    }).resource,
    "qqHistory",
  );
});

Deno.test("workspace covers every ScriptApi method, native tool, and future namespace without thought duplication", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-all-tools-observer-" });
  const store = new ScriptStore(directory);
  try {
    await store.initialize();
    const methods: { namespace: string; method: string }[] = Object.entries(SCRIPT_TOOL_METHODS)
      .flatMap(([namespace, names]) => names.map((method) => ({ namespace, method })));
    assert(methods.length >= 54);
    assert.deepEqual(SCRIPT_TOOL_METHODS.web, ["search", "fetch"]);
    assert.equal(
      new Set(methods.map(({ namespace, method }) => `${namespace}.${method}`)).size,
      methods.length,
    );
    assert.equal(
      TOOL_CATALOG.flatMap((group) => group.methods).length,
      methods.length + TOOL_CATALOG.find((group) => group.namespace === "native")!.methods.length,
    );
    methods.push({ namespace: "future_plugin", method: "newRead" });
    await store.change((state) => {
      for (let offset = 0; offset < methods.length; offset += 24) {
        const job = fixtureJob(0);
        job.jobId = String(offset + 1).padStart(64, "0");
        job.actions = methods.slice(offset, offset + 24).map(({ namespace, method }, index) => ({
          seq: index + 1,
          actionId: `script_${job.jobId}_${index + 1}`,
          namespace,
          method,
          args: { sample: true },
          status: "completed",
          startedAt: new Date(Date.parse(at) + (offset + index) * 1000).toISOString(),
          finishedAt: new Date(Date.parse(at) + (offset + index) * 1000).toISOString(),
          value: { actual: true },
        }));
        state.jobs.push(job);
      }
    });
    const original = await new ScriptObserver(directory).read() as Record<string, Json>;
    assert.equal((original.items as Json[]).length, methods.length);
    const reader = new WorkspaceReader({
      status: () => Promise.resolve(null),
      control: () => {
        throw new Error("never execute previews");
      },
      query: ({ resource }): Promise<Json> =>
        Promise.resolve(resource === "workspace" ? original : { items: [] }),
    });
    const base = await reader.read();
    const thought = "独有随记正文，不在工具详情里再写一次";
    const events = [
      liveEvent({
        seq: 1,
        at: "2026-11-12T01:02:00Z",
        kind: "model_response",
        data: {
          response: {
            content: "",
            toolCalls: [
              {
                id: "native1",
                name: "execute_script",
                args: {
                  code:
                    'return await tools.commands.start({cmd:"date"}); const apiKey = "sensitive_test_abc";',
                  thought,
                },
              },
              { id: "native2", name: "write_thought", args: { thought } },
            ],
          },
        },
      }),
      liveEvent({
        seq: 2,
        at: "2026-11-12T01:02:01Z",
        kind: "tool_start",
        data: {
          callId: "eve_1_0",
          call: {
            id: "eve_1_0",
            name: "execute_script",
            args: {
              code:
                'return await tools.commands.start({cmd:"date"}); const apiKey = "sensitive_test_abc";',
            },
          },
        },
      }),
      liveEvent({
        seq: 3,
        at: "2026-11-12T01:02:02Z",
        kind: "tool_result",
        data: { callId: "eve_1_0", result: { status: "completed", output: "真实回执" } },
      }),
      liveEvent({
        seq: 4,
        at: "2026-11-12T01:02:03Z",
        kind: "tool_start",
        data: { callId: "eve_1_1", call: { id: "eve_1_1", name: "write_thought", args: {} } },
      }),
      liveEvent({
        seq: 5,
        at: "2026-11-12T01:02:04Z",
        kind: "tool_result",
        data: { callId: "eve_1_1", result: { status: "failed", output: "真实失败" } },
      }),
    ];
    const merged = withNativeOperations(base, events) as Record<string, Json>;
    const operations = merged.operations as Record<string, Json>[];
    assert.equal(operations.length, methods.length + 2);
    for (const { namespace, method } of methods) {
      assert(
        operations.some((op) => op.namespace === namespace && op.method === method),
        `${namespace}.${method}`,
      );
    }
    assert.equal(operations.find((op) => op.namespace === "future_plugin")!.appId, "tools");
    const note = operations.find((op) => op.method === "write_thought")!;
    assert.equal(note.status, "failed");
    assert.equal(note.hasThought, true);
    assert.equal(note.thoughtRecordSeq, 1);
    assert.equal(note.sourceResource, "timeline");
    const script = operations.find((op) =>
      op.namespace === "native" && op.method === "execute_script"
    )!;
    assert.equal(script.status, "completed");
    assert.equal(script.hasThought, true);
    assert.match(String((script.args as Record<string, Json>).code), /tools.commands.start/);
    assert(!JSON.stringify(merged).includes(thought));
    assert(!JSON.stringify(merged).includes("sensitive_test_abc"));
    assert.equal(events[0].text, thought);
    assert.equal((merged.currentApp as Record<string, Json>).appId, "tools");
    const tools = (merged.apps as Record<string, Record<string, Json>>).tools;
    assert.equal((tools.items as Json[]).length, methods.length + 2);
  } finally {
    await store.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("native preview uses core execution identity across provider id reuse and marks bounded and orphan receipts", () => {
  const make = (seq: number) =>
    liveEvent({
      seq,
      at: `2026-11-12T01:0${seq}:00Z`,
      kind: "model_response",
      data: {
        response: {
          content: "",
          toolCalls: [{
            id: "same_provider_id",
            name: "execute_script",
            args: { code: "x".repeat(5000), thought: "一次文字" },
          }],
        },
      },
    });
  const events = [
    make(1),
    liveEvent({
      seq: 2,
      at: "2026-11-12T01:02:00Z",
      kind: "tool_start",
      data: {
        callId: "eve_1_0",
        call: { id: "eve_1_0", name: "execute_script", args: { code: "x".repeat(5000) } },
      },
    }),
    liveEvent({
      seq: 3,
      at: "2026-11-12T01:03:00Z",
      kind: "tool_result",
      data: { callId: "eve_1_0", result: { status: "completed", output: "yes" } },
    }),
    make(4),
    liveEvent({
      seq: 5,
      at: "2026-11-12T01:05:00Z",
      kind: "tool_result",
      data: { callId: "eve_before_window", result: { status: "unknown", output: "真实孤立回执" } },
    }),
  ];
  const merged = withNativeOperations({ apps: {}, operations: [] }, events) as Record<string, Json>;
  const operations = merged.operations as Record<string, Json>[];
  assert.equal(operations.length, 3);
  assert.equal(operations.find((op) => op.id === "eve_1_0")!.status, "completed");
  assert.equal(operations.find((op) => op.id === "eve_1_0")!.thoughtRecordSeq, 1);
  assert.equal(operations.find((op) => op.id === "eve_4_0")!.status, "requested");
  assert.equal(operations.find((op) => op.id === "eve_1_0")!.partial, true);
  assert.equal(operations.find((op) => op.id === "eve_before_window")!.missingStart, true);
  assert.equal(operations.find((op) => op.id === "eve_before_window")!.status, "unknown");
  const publicEvents = publicLiveEvents(events);
  assert(!JSON.stringify(publicEvents).includes('"code"'));
  assert(!JSON.stringify(merged).includes("一次文字"));
  const large = {
    events: Array.from({ length: 80 }, () => ({ text: "中".repeat(4000) })),
    workspace: {
      ...merged,
      operations: Array.from(
        { length: 64 },
        () => ({
          id: "x",
          appId: "tools",
          args: { text: "中".repeat(12000) },
          result: { text: "中".repeat(12000) },
        }),
      ),
    },
  };
  const bounded = boundLiveSnapshot(large as Json) as Record<string, Json>;
  assert.equal(bounded.partial, true);
  assert(new TextEncoder().encode(JSON.stringify(bounded)).length < 1_500_000);
});
Deno.test("QQ text and image actions keep the last real captured screen and text stays a labeled fragment", async () => {
  const messages = [{
    handle: { conversationId: "qq:group:real", seq: 10 },
    body: { text: "真实预览" },
  }];
  const actions: Json[] = [{
    id: "read",
    namespace: "phone",
    method: "history",
    status: "completed",
    startedAt: at,
    args: { conversationId: "qq:group:real" },
    result: { messages },
  }, {
    id: "text",
    namespace: "phone",
    method: "text",
    status: "completed",
    startedAt: at,
    args: { handle: { conversationId: "qq:group:real", seq: 10 }, startChar: 160 },
    result: { text: "后续长文片段", characters: 900, truncated: true, nextChar: 320 },
  }];
  const reader = new WorkspaceReader({
    status: () => Promise.resolve(null),
    control: () => {
      throw new Error("never");
    },
    query: ({ resource }): Promise<Json> =>
      Promise.resolve(resource === "workspace" ? { items: actions } : { items: [] }),
  });
  const value = await reader.read() as Record<string, Json>,
    qq = (value.apps as Record<string, Record<string, Json>>).qq;
  assert.deepEqual(qq.messages, messages);
  assert.equal((qq.focusedText as Record<string, Json>).startChar, 160);
  assert.equal(qq.viewOperationId, "read");
  assert.equal(qq.activeConversationId, "qq:group:real");
});

Deno.test("workspace observer bounds UTF-8 and escaped-control IPC bytes without losing action identities", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-workspace-byte-budget-" });
  const store = new ScriptStore(directory);
  try {
    await store.initialize();
    const jobs = ["c", "d", "e"].map((letter) => {
      const job = fixtureJob(24);
      job.jobId = letter.repeat(64);
      job.actions = job.actions.map((action) => ({
        ...action,
        actionId: `script_${job.jobId}_${action.seq}`,
        args: {
          first: "中\u0001".repeat(2000),
          second: "文\u0002".repeat(2000),
          third: "字\u0003".repeat(2000),
        },
      }));
      return job;
    });
    await store.change((state) => state.jobs.push(...jobs));
    const snapshot = await new ScriptObserver(directory).read() as Record<string, Json>;
    const items = snapshot.items as Record<string, Json>[];
    assert.equal(items.length, 64);
    assert.equal(new Set(items.map((item) => item.id)).size, 64);
    assert.equal(snapshot.partial, true);
    assert(new TextEncoder().encode(JSON.stringify(snapshot)).length < 750_000);
    for (const item of items) {
      assert.equal(item.status, "completed");
      assert.equal(item.sourceSeq, 1);
      assert.equal(item.partial, true);
      assert(jobs.some((job) => job.actions.some((action) => action.actionId === item.id)));
    }
  } finally {
    await store.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("workspace observer marks the twelve-job window partial even when action and commit windows fit", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-workspace-observer-" });
  const store = new ScriptStore(directory);
  try {
    await store.initialize();
    await store.change((state) => {
      for (let i = 0; i < 13; i++) {
        const job = fixtureJob();
        job.jobId = i.toString(16).repeat(64);
        job.actions[0].actionId = `script_${job.jobId}_1`;
        state.jobs.push(job);
      }
    });
    const snapshot = await new ScriptObserver(directory).read() as Record<string, Json>;
    const items = snapshot.items as Record<string, Json>[];
    assert.equal(snapshot.throughSeq, 1);
    assert.equal(items.length, 12);
    assert.equal(snapshot.partial, true);
    assert(!items.some((item) => item.jobId === "0".repeat(64)));
    assert(items.some((item) => item.jobId === "c".repeat(64)));
  } finally {
    await store.close();
    await Deno.remove(directory, { recursive: true });
  }
});
