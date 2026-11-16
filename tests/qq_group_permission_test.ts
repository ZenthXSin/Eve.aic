import assert from "node:assert/strict";
import { QqClient } from "../adapters/qq/client.ts";
import { QqSendActions } from "../adapters/qq/actions.ts";

const credentials = { appId: "fixture", appSecret: "fixture-secret" };
const input = { destination: { kind: "group" as const, openId: "actual-group" }, text: "全文" };
const now = Date.parse("2026-11-16T01:00:00Z");

function platform(handler: (r: Request) => Response | Promise<Response>) {
  const server = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    (r) =>
      new URL(r.url).pathname === "/app/getAppAccessToken"
        ? Response.json({ access_token: "fixture-token", expires_in: 7200 })
        : handler(r),
  );
  return { url: `http://127.0.0.1:${server.addr.port}`, close: () => server.shutdown() };
}

for (const allowed of [false, true]) {
  Deno.test(`QQ permission: actual group state ${allowed} survives restart without replay`, async () => {
    let sends = 0, queries = 0;
    const api = platform(async (r) => {
      assert.equal(r.headers.get("authorization"), "QQBot fixture-token");
      const path = new URL(r.url).pathname;
      if (path === "/v2/groups/actual-group/bot_state") {
        queries++;
        assert.equal(r.method, "GET");
        return Response.json({ allow_proactive_msg: allowed, recv_msg_setting: "all" });
      }
      assert.equal(path, "/v2/groups/actual-group/messages");
      assert.equal(r.method, "POST");
      assert.deepEqual(await r.json(), {
        msg_type: 2,
        markdown: { content: input.text },
        msg_seq: 2 * ++sends,
      });
      return Response.json({ code: 40034105 }, {
        status: 400,
        headers: { "X-Tps-trace-ID": "send-trace" },
      });
    });
    const directory = await Deno.makeTempDir();
    const client = new QqClient(credentials, {
      baseUrl: api.url,
      sendScope: "all",
      now: () => now,
    });
    let actions = new QqSendActions(client, directory, "all");
    try {
      await actions.initialize();
      const outcome = await actions.send("denied", input);
      assert.ok(outcome.status === "finished" && outcome.result.status === "failed");
      assert.equal(outcome.result.error.code, 40034105);
      assert.equal(outcome.result.error.traceId, "send-trace");
      assert.deepEqual(outcome.result.error.groupPermission, {
        source: "qq_bot_state",
        checkedAt: new Date(now).toISOString(),
        state: "observed",
        allowProactiveMsg: allowed,
      });
      // Later messages remain independent actions, sharing only the read observation.
      await actions.send("next", input);
      assert.deepEqual([sends, queries], [2, 1]);
      await actions.close();
      actions = new QqSendActions(client, directory, "all");
      await actions.initialize();
      assert.deepEqual(await actions.send("denied", input), outcome);
      assert.deepEqual([sends, queries], [2, 1]);
    } finally {
      await actions.close();
      await api.close();
      await Deno.remove(directory, { recursive: true });
    }
  });
}

for (const state of ["forbidden", "invalid", "timeout"]) {
  Deno.test(`QQ permission: ${state} lookup preserves the definite send refusal`, async () => {
    const paths: string[] = [];
    const api = platform((r) => {
      const path = new URL(r.url).pathname;
      paths.push(path);
      if (path.endsWith("/messages")) return Response.json({ code: 40034105 }, { status: 400 });
      if (state === "forbidden") return Response.json({ code: 11253 }, { status: 403 });
      if (state === "invalid") return Response.json({ allow_proactive_msg: "false" });
      return new Response(
        new ReadableStream({
          start(controller) {
            controller.enqueue(new TextEncoder().encode('{"allow_proactive_msg":'));
          },
        }),
      );
    });
    try {
      const result = await new QqClient(credentials, {
        baseUrl: api.url,
        sendScope: "all",
        timeoutMs: 100,
        now: () => now,
      }).sendText(input, 2);
      assert.ok(result.status === "failed" && result.attempted);
      assert.equal(result.error.code, 40034105);
      assert.equal(result.error.reason, "active_permission_denied");
      assert.deepEqual(result.error.groupPermission, {
        source: "qq_bot_state",
        checkedAt: new Date(now).toISOString(),
        state: "unavailable",
      });
      assert.deepEqual(paths, [
        "/v2/groups/actual-group/messages",
        "/v2/groups/actual-group/bot_state",
      ]);
    } finally {
      await api.close();
    }
  });
}

Deno.test("QQ permission: ordinary success and private refusal add no group queries", async () => {
  const api = platform((r) => {
    assert.ok(new URL(r.url).pathname.endsWith("/messages"));
    return new URL(r.url).pathname.includes("/users/")
      ? Response.json({ code: 40034105 }, { status: 400 })
      : Response.json({ id: "sent" });
  });
  try {
    const client = new QqClient(credentials, { baseUrl: api.url, sendScope: "all" });
    assert.equal((await client.sendText(input, 2)).status, "sent");
    const refused = await client.sendText({
      ...input,
      destination: { kind: "user", openId: "user" },
    });
    assert.ok(refused.status === "failed" && refused.error.groupPermission === undefined);
  } finally {
    await api.close();
  }
});

Deno.test("QQ permission: fresh failures renew expired state and bound cross-group query rate", async () => {
  let time = now, allowed = false, queries = 0;
  const api = platform((r) => {
    if (new URL(r.url).pathname.endsWith("/messages")) {
      return Response.json({ code: 40034105 }, { status: 400 });
    }
    queries++;
    return Response.json({ allow_proactive_msg: allowed });
  });
  try {
    const client = new QqClient(credentials, {
      baseUrl: api.url,
      sendScope: "all",
      now: () => time,
    });
    await client.sendText(input, 2);
    const other = await client.sendText({
      ...input,
      destination: { kind: "group", openId: "other" },
    }, 4);
    assert.ok(other.status === "failed" && other.error.groupPermission?.state === "unavailable");
    assert.equal(queries, 1);
    time += 60_000;
    allowed = true;
    const renewed = await client.sendText(input, 6);
    assert.ok(renewed.status === "failed");
    assert.deepEqual(renewed.error.groupPermission, {
      source: "qq_bot_state",
      checkedAt: new Date(time).toISOString(),
      state: "observed",
      allowProactiveMsg: true,
    });
    assert.equal(queries, 2);
  } finally {
    await api.close();
  }
});
