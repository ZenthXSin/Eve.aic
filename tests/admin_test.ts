import assert from "node:assert/strict";
import { type Json } from "../kernel/protocol.ts";
import { AdminControlClient } from "../services/admin/client.ts";
import type { AdminBackend, AdminControl, AdminQuery } from "../services/admin/contracts.ts";
import { startAdminControlServer, startAdminServer } from "../services/admin/server.ts";
import { panelHtml } from "../services/admin/view.ts";

const token = "admin_test_operator_01234567890123456789";
const key = "admin_test_backend_012345678901234567890";

function fixtureBackend() {
  const queries: AdminQuery[] = [], controls: AdminControl[] = [];
  let paused = false, dailyTokenBudget = 200_000_000, statuses = 0;
  const backend: AdminBackend = {
    status() {
      statuses++;
      return Promise.resolve({ paused, budget: { dailyTokenBudget }, version: "a".repeat(40) });
    },
    query(request) {
      queries.push(structuredClone(request));
      return Promise.resolve({
        items: [{
          seq: 1,
          at: "2026-11-12T00:00:00Z",
          kind: "thought",
          sourceCommit: "a".repeat(40),
        }],
        next: { afterSeq: 1 },
      });
    },
    control(request) {
      controls.push(structuredClone(request));
      if (request.action === "budget") dailyTokenBudget = request.args.dailyTokenBudget;
      else paused = request.action === "pause";
      return Promise.resolve({ paused, dailyTokenBudget });
    },
  };
  return { backend, queries, controls, statuses: () => statuses };
}

async function login(url: string) {
  const response = await fetch(`${url}/api/login`, {
    method: "POST",
    headers: { "content-type": "application/json", origin: url },
    body: JSON.stringify({ token }),
  });
  assert.equal(response.status, 200);
  const body = await response.json();
  const cookie = response.headers.get("set-cookie")!;
  assert.match(cookie, /HttpOnly; SameSite=Strict/);
  return { cookie: cookie.split(";")[0], csrf: body.value.csrf };
}

async function post(
  url: string,
  path: string,
  body: Json,
  session?: { cookie: string; csrf: string },
  extra: Record<string, string> = {},
) {
  const response = await fetch(`${url}${path}`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(session ? { cookie: session.cookie, "x-eve-csrf": session.csrf } : {}),
      ...extra,
    },
    body: JSON.stringify(body),
  });
  return { status: response.status, body: await response.json() };
}

Deno.test("admin browser authentication, CSRF, bounded queries and owner controls", async () => {
  const f = fixtureBackend(), server = await startAdminServer(f.backend, { token });
  try {
    const anonymous = await fetch(`${server.url}/api/status`);
    assert.equal(anonymous.status, 401);
    await anonymous.json();
    assert.equal(f.statuses(), 0);
    const publicPage = await fetch(server.url);
    assert.equal(publicPage.status, 200);
    assert.match(publicPage.headers.get("content-security-policy")!, /frame-ancestors 'none'/);
    const page = await publicPage.text();
    assert(!page.includes(token));
    assert(!page.includes(key));
    assert.equal(publicPage.headers.get("cache-control"), "no-store");
    const session = await login(server.url);
    const status = await fetch(`${server.url}/api/status`, { headers: { cookie: session.cookie } });
    assert.equal((await status.json()).value.paused, false);
    let result = await post(server.url, "/api/control", { action: "pause", args: {} }, {
      ...session,
      csrf: "wrong",
    });
    assert.equal(result.status, 403);
    assert.equal(f.controls.length, 0);
    result = await post(server.url, "/api/control", { action: "pause", args: {} }, session, {
      origin: "https://evil.invalid",
    });
    assert.equal(result.status, 403);
    assert.equal(f.controls.length, 0);
    result = await post(server.url, "/api/control", { action: "pause", args: {} }, session, {
      "sec-fetch-site": "cross-site",
    });
    assert.equal(result.status, 403);
    result = await post(server.url, "/api/control", { action: "pause", args: {} }, session);
    assert.equal(result.body.value.paused, true);
    result = await post(server.url, "/api/control", {
      action: "budget",
      args: { dailyTokenBudget: 12345 },
    }, session);
    assert.equal(result.body.value.dailyTokenBudget, 12345);
    result = await post(server.url, "/api/control", { action: "resume", args: {} }, session);
    assert.equal(result.body.value.paused, false);
    for (
      const args of [{ dailyTokenBudget: -1 }, { dailyTokenBudget: 1.5 }, {
        dailyTokenBudget: 12,
        credential: key,
      }] as Record<string, Json>[]
    ) {
      result = await post(server.url, "/api/control", { action: "budget", args }, session);
      assert.equal(result.status, 400);
    }
    assert.equal(f.controls.length, 3);
    result = await post(server.url, "/api/control", {
      action: "resumeModel",
      args: { expectedRequestId: "model_299" },
    }, session);
    assert.equal(result.body.ok, true);
    assert.deepEqual(f.controls.at(-1), {
      action: "resumeModel",
      args: { expectedRequestId: "model_299" },
    });
    for (
      const args of [{}, { expectedRequestId: "observer_1" }, {
        expectedRequestId: "model_299",
        scope: "all",
      }] as Record<string, Json>[]
    ) {
      result = await post(server.url, "/api/control", { action: "resumeModel", args }, session);
      assert.equal(result.status, 400);
    }
    for (const resource of ["timeline", "notes", "memory", "receipts"]) {
      result = await post(
        server.url,
        "/api/query",
        { resource, operation: "page", args: {} },
        session,
      );
      assert.equal(result.body.value.items[0].at, "2026-11-12T00:00:00Z");
      result = await post(server.url, "/api/query", {
        resource,
        operation: "page",
        args: result.body.value.next,
      }, session);
      assert.equal(result.body.ok, true);
      assert.deepEqual(f.queries.at(-1)!.args, { afterSeq: 1 });
    }
    for (const resource of ["files", "commands", "model", "__proto__"]) {
      result = await post(
        server.url,
        "/api/query",
        { resource, operation: "page", args: {} },
        session,
      );
      assert.equal(result.status, 400);
    }
    result = await post(server.url, "/api/query", {
      resource: "notes",
      operation: "create",
      args: {},
    }, session);
    assert.equal(result.status, 400);
    result = await post(server.url, "/api/query", {
      resource: "notes",
      operation: "search",
      args: { query: "x".repeat(65_536) },
    }, session);
    assert.equal(result.status, 400);
    const before = f.statuses();
    // fetch 会自行覆盖 Host，使用真实 HTTP 报文覆盖 DNS rebinding 边界。
    const connection = await Deno.connect({
      hostname: "127.0.0.1",
      port: Number(new URL(server.url).port),
    });
    try {
      await connection.write(
        new TextEncoder().encode(
          `GET /api/status HTTP/1.1\r\nHost: evil.invalid\r\nCookie: ${session.cookie}\r\nConnection: close\r\n\r\n`,
        ),
      );
      const bytes = new Uint8Array(4096), count = await connection.read(bytes);
      assert.match(new TextDecoder().decode(bytes.subarray(0, count ?? 0)), /^HTTP\/1\.1 403 /);
    } finally {
      connection.close();
    }
    assert.equal(f.statuses(), before);
    result = await post(server.url, "/api/logout", {}, session);
    assert.equal(result.body.ok, true);
    const loggedOut = await fetch(`${server.url}/api/status`, {
      headers: { cookie: session.cookie },
    });
    assert.equal(loggedOut.status, 401);
    await loggedOut.json();
  } finally {
    await server.close();
  }
});

Deno.test("admin sessions expire and login attempts are throttled", async () => {
  let now = 1;
  const f = fixtureBackend(), server = await startAdminServer(f.backend, { token, now: () => now });
  try {
    const session = await login(server.url);
    now += 8 * 60 * 60 * 1000;
    const expired = await fetch(`${server.url}/api/session`, {
      headers: { cookie: session.cookie },
    });
    assert.equal(expired.status, 401);
    await expired.json();
    for (let i = 0; i < 5; i++) {
      assert.equal((await post(server.url, "/api/login", { token: "incorrect" })).status, 401);
    }
    assert.equal((await post(server.url, "/api/login", { token })).status, 429);
    now += 60_000;
    const session2 = await login(server.url);
    assert.notEqual(session2.cookie, session.cookie);
  } finally {
    await server.close();
  }
});

Deno.test("admin IPC separates operator sessions from private backend credentials", async () => {
  const f = fixtureBackend(), backend = await startAdminControlServer(f.backend, key);
  const directory = await Deno.makeTempDir({ prefix: "eve-admin-test-" });
  let browser: Awaited<ReturnType<typeof startAdminServer>> | undefined;
  try {
    await Deno.writeTextFile(`${directory}/connection.json`, JSON.stringify({ url: backend.url }));
    await Deno.writeTextFile(`${directory}/key`, key, { mode: 0o600 });
    const client = new AdminControlClient(`${directory}/connection.json`, `${directory}/key`);
    browser = await startAdminServer(client, { token });
    const session = await login(browser.url);
    const result = await post(browser.url, "/api/control", { action: "pause", args: {} }, session);
    assert.equal(result.body.value.paused, true);
    const query = await client.query({
      resource: "receipts",
      operation: "get",
      args: { id: "action-1" },
    });
    assert.equal((query as { items: Json[] }).items.length, 1);
    assert.equal((await client.status() as { paused: boolean }).paused, true);
    const reject = await fetch(backend.url, {
      method: "POST",
      headers: {
        authorization: `Bearer ${key}`,
        origin: browser.url,
        "content-type": "application/json",
      },
      body: JSON.stringify({ method: "status" }),
    });
    assert.equal(reject.status, 403);
    await reject.json();
    await Deno.writeTextFile(
      `${directory}/connection.json`,
      JSON.stringify({ url: "https://example.com/admin-control" }),
    );
    await assert.rejects(() => client.status(), /本机管理出口/);
  } finally {
    if (browser) await browser.close();
    await backend.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("admin inline interface compiles and only renders recorded text", () => {
  const html = panelHtml("nonce-test");
  const script = html.match(/<script nonce="nonce-test">([\s\S]*?)<\/script>/)![1];
  assert.doesNotThrow(() => new Function(script));
  assert(!script.includes("innerHTML"));
  assert(!script.includes("localStorage"));
  assert(!script.includes("sessionStorage"));
  assert(html.includes("语义检索模型接入待办"));
});

Deno.test("admin status fills budget from the actual supervisor envelope", async () => {
  const script = panelHtml("test").match(/<script nonce="test">([\s\S]*?)<\/script>/)![1];
  const elements = new Map<string, Record<string, unknown>>();
  const document = {
    getElementById(id: string) {
      if (!elements.has(id)) {
        elements.set(id, {
          value: "",
          textContent: "",
          open: false,
          addEventListener() {},
          replaceChildren() {},
          close() {},
        });
      }
      return elements.get(id)!;
    },
    querySelectorAll() {
      return [];
    },
  };
  const fetch = (path: string) =>
    Promise.resolve({
      status: path === "/api/session" ? 401 : 200,
      json: () =>
        Promise.resolve(
          path === "/api/session" ? { ok: false, error: { message: "请先登录" } } : {
            ok: true,
            value: {
              supervisor: { paused: true },
              runtime: { budget: { dailyTokenBudget: 123456789 } },
            },
          },
        ),
    });
  const refresh = new Function("document", "fetch", `${script}\nreturn status;`)(document, fetch);
  await refresh();
  assert.equal(document.getElementById("budget").value, "123456789");
});
