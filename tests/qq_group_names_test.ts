import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient, QqRequestError } from "../adapters/qq/client.ts";
import { QqGroupNames } from "../adapters/qq/contacts.ts";
import { QqPhoneDevice } from "../adapters/qq/device.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";

const credentials = { appId: "fixture-app", appSecret: "fixture-secret" };

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => resolve = r);
  return { promise, resolve };
}

function httpClient(
  handler: (url: URL, init: RequestInit) => Response | Promise<Response>,
) {
  let tokens = 0;
  const client = new QqClient(credentials, {
    httpsTransport: async (url, init) => {
      if (url.pathname === "/app/getAppAccessToken") {
        tokens++;
        assert.equal(url.origin, "https://bots.qq.com");
        assert.equal(init.method, "POST");
        assert.deepEqual(JSON.parse(String(init.body)), {
          appId: credentials.appId,
          clientSecret: credentials.appSecret,
        });
        return Response.json({ access_token: "fixture-token", expires_in: "4975" });
      }
      assert.equal(url.origin, "https://api.sgroup.qq.com");
      assert.equal(init.method, "GET");
      assert.equal(new Headers(init.headers).get("Authorization"), "QQBot fixture-token");
      assert.equal(new Headers(init.headers).get("X-Union-Appid"), credentials.appId);
      return await handler(url, init);
    },
  });
  return { client, tokens: () => tokens };
}

async function fixture(names: QqGroupNames, groupCount = 1) {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-group-names-" });
  const journal = new QqInboundJournal(`${directory}/inbound`);
  const actionsClient = new QqClient(credentials);
  const actions = new QqSendActions(actionsClient, `${directory}/actions`);
  const timeline = new QqTimeline(`${directory}/timeline`);
  const reading = new QqPhoneReading(`${directory}/reading`);
  const device = new QqPhoneDevice(`${directory}/device`);
  await journal.initialize();
  await actions.initialize();
  await timeline.initialize();
  await reading.initialize();
  await device.initialize();
  for (let seq = 1; seq <= groupCount; seq++) {
    const raw: QqDispatch = {
      op: 0,
      s: seq,
      t: "GROUP_AT_MESSAGE_CREATE",
      d: {
        id: `message-${seq}`,
        group_openid: `group-${seq}`,
        author: { member_openid: "member", username: "群友" },
        content: `message ${seq}`,
        timestamp: "2026-11-16T01:00:00Z",
      },
    };
    await journal.append(qqMessage(raw)!, raw);
  }
  const phone = new QqPhone(
    () => journal.records(),
    actions,
    timeline,
    { id: "fixture-bot", displayName: "Eve" },
    reading,
    Date.now,
    device,
    undefined,
    false,
    names,
  );
  return {
    phone,
    journal,
    device,
    close: async () => {
      actionsClient.close();
      await actions.close();
      await timeline.close();
      await reading.close();
      await device.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("QQ group names: SDK GET uses token/union appid and names only the matching openid", async () => {
  const openId = "group/with?#characters";
  let queries = 0;
  const api = httpClient((url) => {
    queries++;
    assert.equal(url.pathname, `/v2/groups/${encodeURIComponent(openId)}/info`);
    assert.equal(url.search, "");
    return Response.json({
      group_openid: openId,
      group_name: "真实群名",
      group_member_num: 42,
      group_tags: ["fixture"],
    });
  });
  try {
    assert.deepEqual(await api.client.groupInfo(openId), {
      groupOpenId: openId,
      groupName: "真实群名",
    });
    await api.client.groupInfo(openId);
    assert.equal(queries, 2);
    assert.equal(api.tokens(), 1);
    // Read metadata is not a send, and remains available with the private-only send scope.
    assert.equal(api.client.sendScope, "private");
  } finally {
    api.client.close();
  }
});

Deno.test("QQ group names: successful lookup enhances list/latest/open/history/message, preserving local names and raw facts", async () => {
  let queries = 0;
  const api = httpClient((url) => {
    queries++;
    assert.equal(url.pathname, "/v2/groups/group-1/info");
    return Response.json({ group_openid: "group-1", group_name: "真实群名" });
  });
  const names = new QqGroupNames((id, signal) => api.client.groupInfo(id, signal));
  const f = await fixture(names);
  try {
    const before = JSON.stringify(f.journal.records());
    await f.device.update("qq:group:group-1", { alias: "local", remark: "本机备注" });
    const list = await f.phone.conversations();
    const expected = { platformName: "真实群名", alias: "local", remark: "本机备注" };
    assert.equal(list.items[0].platformName, "真实群名");
    assert.equal(list.items[0].conversationId, "qq:group:group-1");
    assert.deepEqual(list.items[0].latest.conversation, expected);
    assert.deepEqual(
      (await f.phone.open({ conversationId: "local" })).messages[0].conversation,
      expected,
    );
    assert.deepEqual(
      (await f.phone.history({ conversationId: "qq:group:group-1" })).messages[0].conversation,
      expected,
    );
    assert.deepEqual(
      (await f.phone.message({ handle: { conversationId: "qq:group:group-1", seq: 1 } }))
        .conversation,
      expected,
    );
    assert.equal(queries, 1);
    assert.throws(() => f.device.resolve("真实群名"));
    assert.equal(f.device.resolve("local"), "qq:group:group-1");
    assert.equal(JSON.stringify(f.journal.records()), before);
  } finally {
    api.client.close();
    await f.close();
  }
});

for (const failure of ["http", "business", "mismatch", "invalid", "network", "timeout"] as const) {
  Deno.test(`QQ group names: ${failure} failure leaves name missing without breaking list/screens`, async () => {
    let queries = 0;
    const api = httpClient((_url, init) => {
      queries++;
      if (failure === "network") throw new Error("fixture network failure");
      if (failure === "timeout") {
        return new Promise<Response>((_resolve, reject) => {
          init.signal!.addEventListener("abort", () => reject(init.signal!.reason), { once: true });
        });
      }
      if (failure === "http") return Response.json({ code: 123 }, { status: 403 });
      if (failure === "business") {
        return Response.json({ code: 123, group_openid: "group-1", group_name: "不能采用" });
      }
      if (failure === "mismatch") {
        return Response.json({ group_openid: "other", group_name: "其他群" });
      }
      return Response.json({ group_openid: "group-1", group_name: 42 });
    });
    const names = new QqGroupNames((id, signal) => api.client.groupInfo(id, signal), {
      timeoutMs: 20,
    });
    const f = await fixture(names);
    try {
      await f.device.update("qq:group:group-1", { alias: "local", remark: "本机备注" });
      const list = await f.phone.conversations();
      assert.equal(list.items[0].alias, "local");
      assert.equal(list.items[0].remark, "本机备注");
      assert.deepEqual(list.items[0].latest.conversation, { alias: "local", remark: "本机备注" });
      assert.equal(list.items.length, 1);
      assert.equal(list.items[0].platformName, undefined);
      assert.equal(
        (await f.phone.open({ conversationId: "qq:group:group-1" })).messages[0].conversation
          ?.platformName,
        undefined,
      );
      assert.equal(queries, 1); // negative cache, not a request on every render
    } finally {
      api.client.close();
      await f.close();
    }
  });
}

Deno.test("QQ group names: TTL hits avoid requests, expired names refresh and failures remove stale names", async () => {
  let now = 0, queries = 0, fail = false;
  const names = new QqGroupNames((id) => {
    queries++;
    if (fail) throw new Error("fixture failure");
    return Promise.resolve({ groupOpenId: id, groupName: `群名-${queries}` });
  }, { now: () => now, ttlMs: 100, failureTtlMs: 10 });
  const f = await fixture(names);
  try {
    assert.equal((await f.phone.conversations()).items[0].platformName, "群名-1");
    now = 99;
    assert.equal((await f.phone.conversations()).items[0].platformName, "群名-1");
    assert.equal(queries, 1);
    now = 100;
    assert.equal((await f.phone.conversations()).items[0].platformName, "群名-2");
    fail = true;
    now = 200;
    assert.equal((await f.phone.conversations()).items[0].platformName, undefined);
    assert.equal(queries, 3);
    now = 209;
    await f.phone.conversations();
    assert.equal(queries, 3);
    now = 210;
    fail = false;
    assert.equal((await f.phone.conversations()).items[0].platformName, "群名-4");
  } finally {
    await f.close();
  }
});

Deno.test("QQ group names: same openid shares one in-flight HTTP query across list and open", async () => {
  const started = deferred<void>(), reply = deferred<Response>();
  let queries = 0;
  const api = httpClient(() => {
    queries++;
    started.resolve();
    return reply.promise;
  });
  const names = new QqGroupNames((id, signal) => api.client.groupInfo(id, signal));
  const f = await fixture(names);
  try {
    const list = f.phone.conversations();
    await started.promise;
    const open = f.phone.open({ conversationId: "qq:group:group-1" });
    const first = names.get("group-1");
    assert.equal(first, names.get("group-1"));
    assert.equal(queries, 1);
    reply.resolve(Response.json({ group_openid: "group-1", group_name: "同一真实群" }));
    assert.equal((await list).items[0].platformName, "同一真实群");
    assert.equal((await open).messages[0].conversation?.platformName, "同一真实群");
    assert.equal(await first, "同一真实群");
    assert.equal(queries, 1);
  } finally {
    reply.resolve(Response.json({ group_openid: "group-1" }));
    api.client.close();
    await f.close();
  }
});

Deno.test("QQ group names: 12 groups query only the visible page, bounded to three concurrent requests", async () => {
  let active = 0, maximum = 0;
  const queries: string[] = [];
  const names = new QqGroupNames(async (id) => {
    queries.push(id);
    maximum = Math.max(maximum, ++active);
    await new Promise((resolve) => setTimeout(resolve, 5));
    active--;
    return { groupOpenId: id, groupName: `平台-${id}` };
  });
  const f = await fixture(names, 12);
  try {
    await f.phone.captureNotificationBoundary();
    assert.equal(queries.length, 0); // local maintenance must not query all groups
    const first = await f.phone.conversations();
    assert.equal(first.items.length, 8);
    assert.equal(queries.length, 8);
    assert.ok(
      first.items.every((item) => item.platformName === `平台-${item.conversationId.slice(9)}`),
    );
    assert.equal(maximum, 3);
    const second = await f.phone.conversations({ cursor: first.older });
    assert.equal(second.items.length, 4);
    assert.equal(queries.length, 12);
    assert.equal(new Set(queries).size, 12);
    assert.equal(maximum, 3);
  } finally {
    await f.close();
  }
});

Deno.test("QQ group names: open/history/notifications independently load only their displayed groups", async () => {
  const queries: string[] = [];
  const names = new QqGroupNames((id) => {
    queries.push(id);
    return Promise.resolve({ groupOpenId: id, groupName: `平台-${id}` });
  });
  const f = await fixture(names, 12);
  try {
    assert.equal(
      (await f.phone.open({ conversationId: "qq:group:group-12" })).messages[0].conversation
        ?.platformName,
      "平台-group-12",
    );
    assert.deepEqual(queries, ["group-12"]);
    assert.equal(
      (await f.phone.history({ conversationId: "qq:group:group-11" })).messages[0].conversation
        ?.platformName,
      "平台-group-11",
    );
    assert.deepEqual(queries, ["group-12", "group-11"]);
    const notices = await f.phone.notifications();
    assert.equal(notices.items.length, 8);
    assert.ok(notices.items.every((item) => item.conversation?.platformName !== undefined));
    assert.equal(queries.length, 10);
  } finally {
    await f.close();
  }
});

Deno.test("QQ group names: missing/blank names are not invented, cache is bounded and timed-out injections cannot hang", async () => {
  let queries = 0;
  const names = new QqGroupNames((id) => {
    queries++;
    return Promise.resolve({ groupOpenId: id, groupName: "   " });
  }, { maxEntries: 2 });
  for (const id of ["a", "b", "c"]) assert.equal(await names.get(id), undefined);
  await names.get("c");
  assert.equal(queries, 3);
  await names.get("a");
  assert.equal(queries, 4);
  const hang = new QqGroupNames(() => new Promise(() => {}), { timeoutMs: 10 });
  assert.equal(await hang.get("a"), undefined);
  const api = httpClient(() => Response.json({ group_openid: "a" }));
  try {
    assert.deepEqual(await api.client.groupInfo("a"), { groupOpenId: "a" });
    await assert.rejects(api.client.groupInfo(""), QqRequestError);
  } finally {
    api.client.close();
  }
});

Deno.test("QQ group names: usable envelope names and private conversations do not trigger metadata queries", async () => {
  let queries = 0;
  const names = new QqGroupNames((id) => {
    queries++;
    return Promise.resolve({ groupOpenId: id, groupName: "不能替代信封名字" });
  });
  const f = await fixture(names, 0);
  try {
    const raw: QqDispatch = {
      op: 0,
      s: 1,
      t: "GROUP_AT_MESSAGE_CREATE",
      d: {
        id: "group-envelope",
        group_openid: "group-1",
        group_name: "信封真名",
        author: { member_openid: "member", username: "群友" },
        content: "message",
        timestamp: "2026-11-16T01:00:00Z",
      },
    };
    await f.journal.append(qqMessage(raw)!, raw);
    const privateRaw: QqDispatch = {
      op: 0,
      s: 2,
      t: "C2C_MESSAGE_CREATE",
      d: {
        id: "private-envelope",
        author: { user_openid: "friend", username: "私信朋友" },
        content: "private",
        timestamp: "2026-11-16T01:00:00Z",
      },
    };
    await f.journal.append(qqMessage(privateRaw)!, privateRaw);
    const list = await f.phone.conversations();
    assert.equal(list.items.find((item) => item.kind === "group")?.platformName, "信封真名");
    assert.equal(list.items.find((item) => item.kind === "user")?.platformName, "私信朋友");
    assert.equal(queries, 0);
  } finally {
    await f.close();
  }
});

Deno.test("QQ group names: expired authentication returns missing metadata without failing the phone", async () => {
  let auth = 0, reads = 0;
  const client = new QqClient(credentials, {
    httpsTransport: (url) => {
      if (url.pathname === "/app/getAppAccessToken") {
        auth++;
        return Promise.resolve(Response.json({ code: 100016 }, { status: 401 }));
      }
      reads++;
      throw new Error("group query must not run without a token");
    },
  });
  const names = new QqGroupNames((id, signal) => client.groupInfo(id, signal));
  const f = await fixture(names);
  try {
    assert.equal((await f.phone.conversations()).items[0].platformName, undefined);
    assert.equal(
      (await f.phone.open({ conversationId: "qq:group:group-1" })).messages[0].conversation
        ?.platformName,
      undefined,
    );
    assert.deepEqual([auth, reads], [1, 0]);
  } finally {
    client.close();
    await f.close();
  }
});

Deno.test("QQ group names: stalled transport cannot exceed concurrency or hold queued screens past deadline", async () => {
  const late = deferred<{ groupOpenId: string; groupName: string }>();
  const queries: string[] = [];
  const names = new QqGroupNames((id) => {
    queries.push(id);
    return id === "a" ? late.promise : Promise.resolve({ groupOpenId: id, groupName: "新名" });
  }, { concurrency: 1, timeoutMs: 20, failureTtlMs: 1 });
  const result = await Promise.all([names.get("a"), names.get("b"), names.get("c")]);
  assert.deepEqual(result, [undefined, undefined, undefined]);
  assert.deepEqual(queries, ["a"]);
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.equal(await names.get("d"), undefined);
  assert.deepEqual(queries, ["a"]); // ignored abort is still occupying the only slot
  late.resolve({ groupOpenId: "a", groupName: "迟到名字不能入缓存" });
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.equal(names.peek("a"), undefined);
  assert.equal(await names.get("e"), "新名");
  assert.deepEqual(queries, ["a", "e"]);
});

Deno.test("QQ group names: timeout during shared authentication does not start a late GET", async () => {
  const reply = deferred<Response>(), started = deferred<void>();
  let auth = 0, reads = 0;
  const client = new QqClient(credentials, {
    httpsTransport: async (url) => {
      if (url.pathname === "/app/getAppAccessToken") {
        auth++;
        started.resolve();
        return await reply.promise;
      }
      reads++;
      return Response.json({ id: "bot", username: "Eve", bot: true });
    },
  });
  try {
    const names = new QqGroupNames((id, signal) => client.groupInfo(id, signal), { timeoutMs: 20 });
    const name = names.get("a");
    await started.promise;
    const identity = client.botIdentity();
    assert.equal(await name, undefined);
    assert.deepEqual([auth, reads], [1, 0]);
    reply.resolve(Response.json({ access_token: "fixture-token", expires_in: 4975 }));
    assert.equal((await identity).id, "bot");
    assert.deepEqual([auth, reads], [1, 1]);
    assert.equal(names.peek("a"), undefined);
  } finally {
    reply.resolve(Response.json({ access_token: "fixture-token", expires_in: 4975 }));
    client.close();
  }
});

Deno.test("QQ group names: empty historical page does not query an undisplayed group", async () => {
  let queries = 0;
  const names = new QqGroupNames((id) => {
    queries++;
    return Promise.resolve({ groupOpenId: id, groupName: "真实群名" });
  });
  const f = await fixture(names);
  try {
    const screen = await f.phone.history({
      conversationId: "qq:group:group-1",
      cursor: { snapshotThroughSeq: 1, beforeSeq: 1 },
    });
    assert.equal(screen.messages.length, 0);
    assert.equal(queries, 0);
    const results = await f.phone.search({ query: "message" });
    assert.equal(results.messages[0].conversation?.platformName, "真实群名");
    assert.equal(queries, 1);
    const byName = await f.phone.search({ query: "真实群名" });
    assert.equal(byName.messages.length, 1);
    assert.equal(queries, 1);
  } finally {
    await f.close();
  }
});

Deno.test("QQ group names: same-openid deduplication survives timeout until the original transport settles", async () => {
  const late = deferred<{ groupOpenId: string; groupName: string }>();
  let queries = 0;
  const names = new QqGroupNames(() => {
    queries++;
    return late.promise;
  }, { concurrency: 3, timeoutMs: 10, failureTtlMs: 1 });
  assert.equal(await names.get("a"), undefined);
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.equal(await names.get("a"), undefined);
  assert.equal(queries, 1);
  late.resolve({ groupOpenId: "a", groupName: "过时迟到名字" });
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.equal(names.peek("a"), undefined);
  assert.equal(await names.get("a"), "过时迟到名字"); // a new read, not the late cache write
  assert.equal(queries, 2);
});
