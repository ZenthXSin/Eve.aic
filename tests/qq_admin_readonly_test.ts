import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqAdminClient, startQqAdminServer } from "../adapters/qq/admin.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";

Deno.test("QQ admin readonly shares bounded phone history without moving Eve reading state", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-admin-readonly-" });
  const key = "fixture_qq_admin_readonly_".repeat(2);
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }),
    `${directory}/actions`,
  );
  const journal = new QqInboundJournal(`${directory}/incoming`);
  const timeline = new QqTimeline(`${directory}/timeline`);
  const reading = new QqPhoneReading(`${directory}/reading`);
  let admin: ReturnType<typeof startQqAdminServer> | undefined;
  const calls: unknown[] = [];
  try {
    await actions.initialize();
    await journal.initialize();
    await timeline.initialize();
    await reading.initialize();
    for (let seq = 1; seq <= 13; seq++) {
      const group = seq < 13;
      const event: QqDispatch = {
        op: 0,
        s: seq,
        t: group ? "GROUP_MESSAGE_CREATE" : "C2C_MESSAGE_CREATE",
        d: {
          id: `fixture-${seq}`,
          content: `message-${seq} ${"x".repeat(250)}`,
          timestamp: "2026-11-12T01:00:00Z",
          author: group
            ? { member_openid: "member", username: "群友" }
            : { user_openid: "owner", username: "主人" },
          ...(group ? { group_openid: "group" } : {}),
        },
      };
      await journal.append(qqMessage(event)!, event);
    }
    const phone = new QqPhone(
      () => journal.records(),
      actions,
      timeline,
      {
        id: "fixture-bot",
        displayName: "Eve",
      },
      reading,
      () => 1794445200000,
    );
    await phone.open({ conversationId: "qq:user:owner" });
    const originalReading = await Deno.readTextFile(`${directory}/reading/index.json`);
    admin = startQqAdminServer(actions, key, {
      conversations: (args) => {
        calls.push({ method: "conversations", args });
        return phone.conversations(args);
      },
      history: (args) => {
        calls.push({ method: "history", args });
        return phone.history(args);
      },
    });
    await Deno.writeTextFile(`${directory}/connection.json`, JSON.stringify({ url: admin.url }));
    await Deno.writeTextFile(`${directory}/key`, key);
    const client = new QqAdminClient({
      connectionFile: `${directory}/connection.json`,
      keyFile: `${directory}/key`,
    });
    const conversations = await phone.conversations();
    assert.deepEqual(await client.conversations(), conversations);
    assert.equal(conversations.items.length, 2);
    assert.equal(conversations.items.find((v) => v.kind === "group")!.reading, null);
    const args = { conversationId: "qq:group:group" };
    const latest = await phone.history(args);
    assert.deepEqual(await client.history(args), latest);
    assert.equal(latest.messages.length, 8);
    assert.ok(latest.older);
    assert.deepEqual(
      await client.history({ ...args, cursor: latest.older }),
      await phone.history({ ...args, cursor: latest.older }),
    );
    const conversationArgs = { cursor: { snapshotThroughSeq: 13, beforeLastSeq: 13 } };
    assert.deepEqual(
      await client.conversations(conversationArgs),
      await phone.conversations(conversationArgs),
    );
    assert.deepEqual(calls, [
      { method: "conversations", args: {} },
      { method: "history", args },
      { method: "history", args: { ...args, cursor: latest.older } },
      { method: "conversations", args: conversationArgs },
    ]);
    assert.equal(await Deno.readTextFile(`${directory}/reading/index.json`), originalReading);
    assert.equal(reading.get("qq:group:group"), null);

    const post = async (method: string, args: unknown, headers: Record<string, string> = {}) => {
      const response = await fetch(admin!.url, {
        method: "POST",
        headers: { "content-type": "application/json", authorization: `Bearer ${key}`, ...headers },
        body: JSON.stringify({ method, args }),
      });
      const text = await response.text();
      return { status: response.status, wire: text ? JSON.parse(text) : null };
    };
    assert.equal((await post("history", args, { authorization: "Bearer wrong" })).status, 403);
    assert.equal((await post("history", args, { origin: "https://foreign.invalid" })).status, 403);
    const invalid = [
      ["open", args],
      ["history", { ...args, limit: 100 }],
      ["history", { ...args, cursor: null }],
      ["history", { ...args, cursor: { snapshotThroughSeq: 13, beforeSeq: 14 } }],
      ["history", { ...args, cursor: { snapshotThroughSeq: 13, beforeSeq: 1, extra: true } }],
      ["history", { ...args, cursor: { snapshotThroughSeq: 13, beforeSeq: 1.5 } }],
      ["history", { conversationId: "qq:group:bad\nvalue" }],
      ["history", { conversationId: "qq:group:" + "x".repeat(513) }],
      ["history", {}],
      ["conversations", { limit: 9 }],
      ["conversations", { cursor: { snapshotThroughSeq: 13, beforeLastSeq: 1, beforePinned: 1 } }],
      ["conversations", {
        cursor: { snapshotThroughSeq: Number.MAX_SAFE_INTEGER + 1, beforeLastSeq: 1 },
      }],
    ] as const;
    for (const [method, invalidArgs] of invalid) {
      const result = await post(method, invalidArgs);
      assert.equal(result.wire.ok, false, JSON.stringify([method, invalidArgs]));
      assert.equal(result.wire.error.kind, "rejected");
    }
    assert.equal(calls.length, 4);
    assert.equal(await Deno.readTextFile(`${directory}/reading/index.json`), originalReading);
  } finally {
    await admin?.close();
    await actions.close();
    await timeline.close();
    await reading.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ readonly IPC rejects oversized UTF-8 pages before sending beyond its unchanged byte ceiling", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-admin-byte-budget-" });
  const key = "fixture_qq_admin_byte_budget_".repeat(2);
  const huge = { messages: [{ text: "中\u0001".repeat(20_000) }] };
  const admin = startQqAdminServer({} as QqSendActions, key, {
    conversations: () =>
      Promise.resolve(huge as unknown as Awaited<ReturnType<QqPhone["conversations"]>>),
    history: () => Promise.resolve(huge as unknown as Awaited<ReturnType<QqPhone["history"]>>),
  });
  try {
    await Deno.writeTextFile(`${directory}/connection.json`, JSON.stringify({ url: admin.url }));
    await Deno.writeTextFile(`${directory}/key`, key);
    const client = new QqAdminClient({
      connectionFile: `${directory}/connection.json`,
      keyFile: `${directory}/key`,
    });
    await assert.rejects(() => client.conversations(), /分页超过字节上限/);
    await assert.rejects(
      () => client.history({ conversationId: "qq:group:real" }),
      /分页超过字节上限/,
    );
    const response = await fetch(admin.url, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${key}` },
      body: JSON.stringify({ method: "history", args: { conversationId: "qq:group:real" } }),
    });
    const bytes = new Uint8Array(await response.arrayBuffer());
    assert(bytes.length < 131_072);
    const wire = JSON.parse(new TextDecoder().decode(bytes));
    assert.equal(wire.ok, false);
    assert.equal(wire.error.kind, "unavailable");
  } finally {
    await admin.close();
    await Deno.remove(directory, { recursive: true });
  }
});
