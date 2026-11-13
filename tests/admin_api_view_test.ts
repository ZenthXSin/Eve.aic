import assert from "node:assert/strict";
import { panelHtml } from "../services/admin/view.ts";

function fixture(fetch: (path: string, options: RequestInit) => Promise<unknown>) {
  const html = panelHtml("test");
  const script = html.slice(
    html.indexOf("async function api(path,body)"),
    html.indexOf("\nfunction showLogin()"),
  );
  let timeout: (() => void) | undefined, cleared = 0, loggedOut = false;
  const api = new Function(
    "fetch",
    "AbortController",
    "AbortSignal",
    "setTimeout",
    "clearTimeout",
    "showLogin",
    "let csrf='session';" + script + ";return api;",
  )(
    fetch,
    AbortController,
    {},
    (callback: () => void, delay: number) => {
      assert.equal(delay, 35000);
      timeout = callback;
      return 1;
    },
    (id: number) => {
      assert.equal(id, 1);
      cleared++;
    },
    () => {
      loggedOut = true;
    },
  ) as (path: string, body?: unknown) => Promise<unknown>;
  return { api, expire: () => timeout!(), cleared: () => cleared, loggedOut: () => loggedOut };
}

Deno.test("panel login works without AbortSignal.timeout and clears its deadline", async () => {
  const f = fixture((path, options) => {
    assert.equal(path, "/api/login");
    assert.equal(options.method, "POST");
    assert.equal(options.credentials, "same-origin");
    assert.equal(options.cache, "no-store");
    assert.deepEqual(JSON.parse(options.body as string), { token: "test-password" });
    assert.equal(new Headers(options.headers).get("x-eve-csrf"), "session");
    assert.ok(options.signal instanceof AbortSignal);
    return Promise.resolve({
      status: 200,
      json: () => ({ ok: true, value: { csrf: "new-session" } }),
    });
  });
  assert.deepEqual(await f.api("/api/login", { token: "test-password" }), { csrf: "new-session" });
  assert.equal(f.cleared(), 1);
});

Deno.test("panel deadlines cancel fetch or body reads and preserve uncertain control results", async () => {
  for (const phase of ["fetch", "body"]) {
    for (const path of ["/api/login", "/api/control"]) {
      const f = fixture((_path, options) => {
        const wait = () =>
          new Promise((_resolve, reject) => {
            options.signal!.addEventListener("abort", () => reject(new Error("aborted")), {
              once: true,
            });
            queueMicrotask(() => f.expire());
          });
        return phase === "fetch" ? wait() : Promise.resolve({ json: wait });
      });
      await assert.rejects(
        f.api(path, {}),
        path === "/api/control" ? /操作结果尚未确认/ : /响应超时/,
      );
      assert.equal(f.cleared(), 1);
    }
  }
});

Deno.test("panel clears deadlines on network, malformed response and expired authentication", async () => {
  for (const scenario of ["network", "json", "unauthorized"]) {
    const f = fixture(() => {
      if (scenario === "network") return Promise.reject(new Error("offline"));
      return Promise.resolve({
        status: 401,
        json: () => {
          if (scenario === "json") throw new Error("invalid json");
          return { ok: false, error: { message: "请先登录" } };
        },
      });
    });
    const pattern = scenario === "network"
      ? /暂时无法连接/
      : scenario === "json"
      ? /有效数据/
      : /请先登录/;
    await assert.rejects(f.api("/api/status"), pattern);
    assert.equal(f.loggedOut(), scenario === "unauthorized");
    assert.equal(f.cleared(), 1);
  }
});
