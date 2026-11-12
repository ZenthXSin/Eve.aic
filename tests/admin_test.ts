import assert from "node:assert/strict";
import { type Json } from "../kernel/protocol.ts";
import { AdminControlClient } from "../services/admin/client.ts";
import type { AdminBackend, AdminControl, AdminQuery } from "../services/admin/contracts.ts";
import { startAdminControlServer, startAdminServer } from "../services/admin/server.ts";
import { panelHtml } from "../services/admin/view.ts";
import { runAdminService } from "../services/admin/main.ts";

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

/** 模拟 TLS 代理保留公开 Host；网络连接仍只发往本机 HTTP 监听器。 */
async function proxyRequest(
  url: string,
  path: string,
  headers: Record<string, string>,
  body?: Json,
) {
  const connection = await Deno.connect({
    hostname: "127.0.0.1",
    port: Number(new URL(url).port),
  });
  try {
    const payload = body === undefined ? "" : JSON.stringify(body);
    const bytes = new TextEncoder().encode(
      `${body === undefined ? "GET" : "POST"} ${path} HTTP/1.1\r\n` +
        Object.entries({
          "content-type": "application/json",
          "content-length": String(new TextEncoder().encode(payload).length),
          connection: "close",
          ...headers,
        }).map(([key, value]) => `${key}: ${value}\r\n`).join("") + `\r\n${payload}`,
    );
    let written = 0;
    while (written < bytes.length) written += await connection.write(bytes.subarray(written));
    let raw = "";
    const buffer = new Uint8Array(8192), decoder = new TextDecoder();
    for (;;) {
      const count = await connection.read(buffer);
      if (count === null) break;
      raw += decoder.decode(buffer.subarray(0, count), { stream: true });
    }
    raw += decoder.decode();
    const split = raw.indexOf("\r\n\r\n");
    const [statusLine, ...lines] = raw.slice(0, split).split("\r\n");
    const responseHeaders = new Headers(lines.map((line) => {
      const colon = line.indexOf(":");
      return [line.slice(0, colon), line.slice(colon + 1).trim()] as [string, string];
    }));
    let text = raw.slice(split + 4);
    if (responseHeaders.get("transfer-encoding") === "chunked") {
      let offset = 0, decoded = "";
      while (offset < text.length) {
        const end = text.indexOf("\r\n", offset), count = parseInt(text.slice(offset, end), 16);
        if (!count) break;
        decoded += text.slice(end + 2, end + 2 + count);
        offset = end + 2 + count + 2;
      }
      text = decoded;
    }
    return {
      status: Number(statusLine.split(" ")[1]),
      headers: responseHeaders,
      body: JSON.parse(text),
    };
  } finally {
    connection.close();
  }
}

Deno.test("admin HTTPS proxy accepts an ordinary password and preserves Host Origin CSRF boundaries", async () => {
  const publicOrigin = "https://eve.example.com", password = "ordinary!密碼 9";
  const f = fixtureBackend();
  const server = await startAdminServer(f.backend, { token: password, publicOrigin });
  const headers = { host: "eve.example.com", origin: publicOrigin };
  try {
    const local = await fetch(`${server.url}/api/status`);
    assert.equal(local.status, 403);
    await local.json();
    let result = await proxyRequest(server.url, "/api/login", headers, { token: password });
    assert.equal(result.status, 200);
    const cookieHeader = result.headers.get("set-cookie")!;
    assert.match(cookieHeader, /HttpOnly; SameSite=Strict; Path=\/; Secure;/);
    const session = { cookie: cookieHeader.split(";")[0], "x-eve-csrf": result.body.value.csrf };
    for (
      const extra of [
        { host: "evil.example.com" },
        { origin: "http://eve.example.com" },
        { origin: "https://other.example.com" },
        { "sec-fetch-site": "same-site" },
        { "x-eve-csrf": "incorrect" },
      ] as Record<string, string>[]
    ) {
      result = await proxyRequest(
        server.url,
        "/api/control",
        { ...headers, ...session, ...extra },
        {
          action: "pause",
          args: {},
        },
      );
      assert.equal(result.status, 403);
    }
    assert.equal(f.controls.length, 0);
    result = await proxyRequest(server.url, "/api/status", {
      ...headers,
      ...session,
      forwarded: "host=evil.invalid;proto=http",
      "x-forwarded-host": "evil.invalid",
      "x-forwarded-proto": "http",
    });
    assert.equal(result.status, 200);
    assert.equal(f.statuses(), 1);
    result = await proxyRequest(server.url, "/api/control", { ...headers, ...session }, {
      action: "pause",
      args: {},
    });
    assert.equal(result.status, 200);
    assert.equal(f.controls.length, 1);
    result = await proxyRequest(server.url, "/api/logout", { ...headers, ...session }, {});
    assert.equal(result.status, 200);
    assert.match(result.headers.get("set-cookie")!, /Secure; Max-Age=0/);
    result = await proxyRequest(server.url, "/api/status", { ...headers, ...session });
    assert.equal(result.status, 401);
    result = await proxyRequest(server.url, "/api/login", {
      ...headers,
      host: new URL(server.url).host,
      "x-forwarded-host": "eve.example.com",
      "x-forwarded-proto": "https",
    }, { token: password });
    assert.equal(result.status, 403);
  } finally {
    await server.close();
  }
});

Deno.test("admin validates explicit HTTPS origins and bounded passwords before listening", async () => {
  const { backend } = fixtureBackend();
  for (
    const publicOrigin of [
      "http://eve.example.com",
      "https://eve.example.com/",
      "https://eve.example.com/path",
      "https://eve.example.com?x=1",
      "https://eve.example.com#x",
      "https://user:password@eve.example.com",
      "https://eve.example.com:443",
      "invalid",
    ]
  ) {
    await assert.rejects(() => startAdminServer(backend, { token, publicOrigin }), /HTTPS origin/);
  }
  for (const password of ["", "short", "a".repeat(1025), "control\npassword"]) {
    await assert.rejects(() => startAdminServer(backend, { token: password }), /管理员密码/);
  }
  const server = await startAdminServer(backend, { token: "a".repeat(1024) });
  try {
    assert.equal((await post(server.url, "/api/login", { token: "a".repeat(1024) })).status, 200);
    assert.equal((await post(server.url, "/api/login", { token: "a".repeat(1025) })).status, 401);
  } finally {
    await server.close();
  }
  const directory = await Deno.makeTempDir({ prefix: "eve-admin-config-" });
  try {
    await Deno.writeTextFile(`${directory}/password`, "normal!password\n");
    const configPath = `${directory}/config.json`;
    await Deno.writeTextFile(
      configPath,
      JSON.stringify({
        admin: {
          directory: `${directory}/service`,
          tokenFile: `${directory}/password`,
          backendConnectionFile: `${directory}/backend.json`,
          backendKeyFile: `${directory}/backend.key`,
          publicOrigin: "http://eve.example.com",
        },
      }),
    );
    await assert.rejects(() => runAdminService(configPath), /HTTPS origin/);
  } finally {
    await Deno.remove(directory, { recursive: true });
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
