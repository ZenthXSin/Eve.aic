import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqAdminClient, startQqAdminServer } from "../adapters/qq/admin.ts";
import { QqClient } from "../adapters/qq/client.ts";
import type { SendQueueItem } from "../adapters/qq/actions.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { AdminControlClient } from "../services/admin/client.ts";
import { startAdminControlServer, startAdminServer } from "../services/admin/server.ts";
import { startRuntimeAdmin } from "../services/runtime/admin.ts";
import type { RuntimeEngine } from "../services/runtime/engine.ts";

Deno.test("admin QQ failure resolution travels browser/CSRF/private bridges and preserves original failure", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-admin-qq-resolution-" });
  const qqKey = "q".repeat(48), backendKey = "b".repeat(48), password = "operator!password";
  let sends = 0;
  const platform = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture", expires_in: 7200 });
    }
    await r.json();
    sends++;
    return Response.json({ code: 40034031 }, { status: 400 });
  });
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    }),
    `${directory}/actions`,
  );
  let qq: ReturnType<typeof startQqAdminServer> | undefined;
  let backend: Awaited<ReturnType<typeof startAdminControlServer>> | undefined;
  let browser: Awaited<ReturnType<typeof startAdminServer>> | undefined;
  try {
    await actions.initialize();
    const request = {
      destination: { kind: "user" as const, openId: "owner" },
      text: "original",
      reply: { messageId: "input", sequence: 1 },
    };
    const failed = await actions.send("failed-head", request);
    assert.equal(failed.status, "finished");
    assert.equal(
      await actions.send("next", {
        ...request,
        text: "next",
        reply: { messageId: "input", sequence: 2 },
      }).then((v) => v.status),
      "blocked",
    );
    const original = await Deno.readFile(`${directory}/actions/records/failed-head.json`);
    qq = startQqAdminServer(actions, qqKey);
    await Deno.writeTextFile(`${directory}/qq.json`, JSON.stringify({ url: qq.url }));
    await Deno.writeTextFile(`${directory}/qq.key`, qqKey);
    const qqClient = new QqAdminClient({
      connectionFile: `${directory}/qq.json`,
      keyFile: `${directory}/qq.key`,
    });
    backend = await startAdminControlServer({
      status: () => Promise.resolve({ paused: true }),
      query: (request) => {
        assert.equal(request.resource, "qqFailures");
        return qqClient.failedHeads(request.args);
      },
      control: (request) => {
        if (request.action !== "resolveQqFailure") {
          throw new KernelError("rejected", "unexpected control");
        }
        return qqClient.resolveFailed(request.args);
      },
    }, backendKey);
    await Deno.writeTextFile(`${directory}/backend.json`, JSON.stringify({ url: backend.url }));
    await Deno.writeTextFile(`${directory}/backend.key`, backendKey);
    browser = await startAdminServer(
      new AdminControlClient(`${directory}/backend.json`, `${directory}/backend.key`),
      { token: password },
    );
    const login = await fetch(`${browser.url}/api/login`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ token: password }),
    });
    const cookie = login.headers.get("set-cookie")!.split(";")[0];
    const csrf = (await login.json()).value.csrf;
    const post = async (path: string, body: Json, csrfValue = csrf) => {
      const r = await fetch(`${browser!.url}${path}`, {
        method: "POST",
        headers: { "content-type": "application/json", cookie, "x-eve-csrf": csrfValue },
        body: JSON.stringify(body),
      });
      return { status: r.status, wire: await r.json() };
    };
    const page = await post("/api/query", { resource: "qqFailures", operation: "page", args: {} });
    const item = page.wire.value.items[0] as SendQueueItem;
    assert.equal(page.wire.value.items.length, 1);
    assert.equal(item.actionId, "failed-head");
    assert.equal(item.canCancel, true);
    const control = {
      action: "resolveQqFailure",
      args: {
        expectedActionId: item.actionId,
        expectedFingerprint: item.fingerprint,
        expectedReceiptDigest: item.receiptDigest!,
      },
    };
    assert.equal((await post("/api/control", control, "wrong")).status, 403);
    assert.equal(
      (await post("/api/control", {
        ...control,
        args: { ...control.args, expectedReceiptDigest: "0".repeat(64) },
      })).wire.ok,
      false,
    );
    assert.equal(
      (await post("/api/control", { ...control, args: { ...control.args, extra: true } })).status,
      400,
    );
    assert.equal(
      (await post("/api/query", {
        resource: "qqFailures",
        operation: "search",
        args: { query: "x" },
      })).status,
      400,
    );
    assert.equal(
      (await post("/api/query", { resource: "qqFailures", operation: "page", args: { limit: 33 } }))
        .status,
      400,
    );
    assert.equal(
      (await post("/api/query", {
        resource: "qqFailures",
        operation: "page",
        args: { failedHeadsOnly: false },
      })).status,
      400,
    );
    const resolved = await post("/api/control", control);
    assert.equal(resolved.wire.value.status, "cancelled");
    assert.equal(resolved.wire.value.originalResult.status, "failed");
    assert.equal(resolved.wire.value.resolution.actor, "admin");
    assert.deepEqual(
      await Deno.readFile(`${directory}/actions/records/failed-head.json`),
      original,
    );
    assert.equal(sends, 1);
    const unknownPage = await post("/api/query", {
      resource: "qqFailures",
      operation: "page",
      args: {},
    });
    const unknown = unknownPage.wire.value.items[0] as SendQueueItem;
    assert.equal(unknown.actionId, "next");
    assert.equal(unknown.canCancel, true);
    const unknownOriginal = await Deno.readFile(`${directory}/actions/records/next.json`);
    const cancelledUnknown = await post("/api/control", {
      action: "resolveQqFailure",
      args: {
        expectedActionId: unknown.actionId,
        expectedFingerprint: unknown.fingerprint,
        expectedReceiptDigest: unknown.receiptDigest!,
      },
    });
    assert.equal(cancelledUnknown.wire.value.resolution.kind, "unknown_retained");
    assert.deepEqual(
      await Deno.readFile(`${directory}/actions/records/next.json`),
      unknownOriginal,
    );
    assert.equal(sends, 1);
    assert.equal(
      (await post("/api/query", {
        resource: "qqFailures",
        operation: "page",
        args: {},
      })).wire.value.items.length,
      0,
    );
    for (
      const [suffix, extra] of [["", { origin: browser.url }], ["?x=1", {}], ["", {
        "sec-fetch-site": "same-site",
      }], ["", { authorization: "Bearer invalid" }]] as [string, Record<string, string>][]
    ) {
      const r: Response = await fetch(qq.url + suffix, {
        method: "POST",
        headers: { authorization: `Bearer ${qqKey}`, "content-type": "application/json", ...extra },
        body: JSON.stringify({ method: "failedHeads", args: {} }),
      });
      assert.equal(r.status, 403);
      await r.text();
    }
    await Deno.writeTextFile(
      `${directory}/qq.json`,
      JSON.stringify({ url: "https://example.com/qq-admin-control" }),
    );
    await assert.rejects(() => qqClient.failedHeads(), /本机独立连接/);
  } finally {
    await browser?.close();
    await backend?.close();
    await qq?.close();
    await actions.close();
    await platform.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("runtime admin refuses external QQ and workspace resources instead of falling back to memory", async () => {
  const key = "r".repeat(48);
  // A QQ query must be rejected before any runtime or memory journal access.
  const engine = new Proxy({}, {
    get() {
      throw new Error("runtime data must not be read");
    },
  }) as RuntimeEngine;
  const server = startRuntimeAdmin(engine, key, () => Promise.resolve());
  try {
    for (const resource of ["qqFailures", "qqConversations", "qqHistory", "workspace"]) {
      const response = await fetch(server.url, {
        method: "POST",
        headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
        body: JSON.stringify({ method: "query", args: { resource, operation: "page", args: {} } }),
      });
      const wire = await response.json();
      assert.equal(wire.ok, false);
      assert.equal(wire.error.kind, "rejected");
    }
  } finally {
    await server.close();
  }
});
