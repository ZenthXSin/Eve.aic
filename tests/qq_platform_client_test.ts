import assert from "node:assert/strict";
import { QqClient, QqRequestError } from "../adapters/qq/client.ts";
import {
  createQqPlatformOperator,
  createQqPlatformRead,
  type QqPlatformCatalog,
  type QqPlatformConfirm,
  type QqPlatformRequest,
} from "../adapters/qq/platform-client.ts";
import { platformDigest, readPrivatePlatformFile } from "../adapters/qq/platform-tool.ts";
import { terminalConfirmation } from "../deployment/qq-platform-operator.ts";

const routes = {
  getMe: { method: "GET", path: "/users/@me", access: "read" },
  members: {
    method: "GET",
    path: "/guilds/{guild}/members",
    access: "read",
    queryParams: ["limit"],
  },
  send: {
    method: "POST",
    path: "/v2/users/{user}/messages",
    access: "write",
    bodyParams: ["content"],
    requiredBody: ["content"],
  },
  mute: {
    method: "PATCH",
    path: "/guilds/{guild}/mute",
    access: "admin",
    bodyParams: ["mute_seconds"],
  },
} as const;

const catalog: QqPlatformCatalog = {
  lookup(operation) {
    return Object.hasOwn(routes, operation) ? routes[operation as keyof typeof routes] : undefined;
  },
  build(operation, params) {
    const route = routes[operation as keyof typeof routes];
    const slots = params.path as Record<string, unknown>;
    const path = route.path.replace(/\{(\w+)\}/, (_, key) => {
      if (typeof slots?.[key] !== "string" || !/^[\w-]+$/.test(slots[key])) {
        throw new TypeError("fixture");
      }
      return slots[key];
    });
    const search = new URLSearchParams(params.query as Record<string, string> | undefined);
    return {
      path: search.size ? `${path}?${search}` : path,
      ...(params.body === undefined ? {} : { body: params.body }),
    };
  },
};

const self = { operation: "getMe", params: {} };
const members = { operation: "members", params: { path: { guild: "guild-1" } } };
const send = {
  operation: "send",
  params: { path: { user: "user-1" }, body: { content: "private text" } },
  idempotencyKey: "intent-1",
};
const mute = {
  operation: "mute",
  params: { path: { guild: "guild-1" }, body: { mute_seconds: 10 } },
  idempotencyKey: "intent-2",
};

const approve: QqPlatformConfirm = () => true;

async function privateFixture(run: (directory: string) => Promise<void>) {
  const directory = await Deno.makeTempDir({ prefix: "qq-platform-client-" });
  const savedUid = Deno.uid;
  try {
    if (Deno.permissions.querySync({ name: "sys", kind: "uid" }).state !== "granted") {
      const { uid } = await Deno.stat(directory);
      Deno.uid = () => uid;
    }
    await run(directory);
  } finally {
    Deno.uid = savedUid;
    await Deno.remove(directory, { recursive: true });
  }
}

async function files(directory: string) {
  const entries = await Array.fromAsync(Deno.readDir(`${directory}/platform-operations`));
  return entries.filter(({ name }) => name !== ".identity").map(({ name }) =>
    `${directory}/platform-operations/${name}`
  );
}

Deno.test("worker accepts only the pinned self GET and cannot elevate itself through incoming JSON", async () => {
  const calls: string[] = [];
  const read = createQqPlatformRead({
    catalog,
    request: (method, path, body) => {
      calls.push(`${method} ${path}`);
      assert.equal(body, undefined);
      return Promise.resolve({
        httpStatus: 200,
        data: {
          id: "bot-1",
          username: "小 Eve",
          bot: true,
          members: [{ id: "private-member" }],
          file_info: "file-key",
          access_token: "bearer-key",
        },
      });
    },
  });
  for (
    const input of [
      members,
      send,
      mute,
      { ...self, confirmed: true },
      { ...self, access: "admin" },
      { ...self, method: "GET", path: "/guilds/1/members" },
      { ...self, params: { url: "https://bad.invalid" } },
      { ...self, idempotencyKey: "try" },
      { operation: "/users/@me", params: {} },
      { operation: "__proto__", params: {} },
      { operation: "getMe", params: [] },
      JSON.parse('{"operation":"getMe","params":{"__proto__":{}}}'),
    ]
  ) {
    assert.equal((await read(input)).status, "failed");
  }
  assert.deepEqual(await read(self), {
    status: "ok",
    httpStatus: 200,
    data: { id: "bot-1", username: "小 Eve", bot: true },
  });
  assert.deepEqual(calls, ["GET /users/@me"]);
});

Deno.test("operator levels, explicit consent and private configuration precede transport", () =>
  privateFixture(async (directory) => {
    const calls: string[] = [];
    const request: QqPlatformRequest = (method, path) => {
      calls.push(`${method} ${path}`);
      return Promise.resolve({
        httpStatus: method === "GET" ? 200 : method === "POST" ? 201 : 204,
        data: method === "GET" ? [{ id: "member" }] : method === "POST" ? { id: "ack" } : null,
      });
    };
    const operator = (access: "read" | "write" | "admin") =>
      createQqPlatformOperator({ directory, catalog, request, access });
    assert.deepEqual(await operator("read")(send, approve), {
      status: "failed",
      reason: "not_allowed",
    });
    assert.deepEqual(await operator("write")(mute, approve), {
      status: "failed",
      reason: "not_allowed",
    });
    assert.deepEqual(await operator("admin")(mute), {
      status: "failed",
      reason: "confirmation_required",
    });
    assert.equal((await operator("admin")({ ...mute, confirmed: true })).status, "failed");
    assert.deepEqual(
      await operator("write")({ operation: send.operation, params: send.params }, approve),
      { status: "failed", reason: "idempotency_required" },
    );
    assert.equal(
      (await operator("admin")({ ...mute, params: { directory: "/tmp" } }, approve)).status,
      "failed",
    );
    assert.throws(() =>
      createQqPlatformOperator({ catalog, request, access: "admin", directory: "relative" })
    );
    assert.deepEqual(calls, []);
    assert.deepEqual(await operator("read")(members), {
      status: "ok",
      httpStatus: 200,
      data: [{ id: "member" }],
    });
    assert.equal((await operator("write")(send, approve)).status, "ok");
    assert.equal((await operator("admin")(mute, approve)).status, "ok");
    assert.deepEqual(calls, [
      "GET /guilds/guild-1/members",
      "POST /v2/users/user-1/messages",
      "PATCH /guilds/guild-1/mute",
    ]);
    const entries = await files(directory);
    assert.equal(entries.length, 3);
    for (const entry of entries) {
      assert.equal((await Deno.stat(entry)).mode! & 0o777, 0o600);
      const text = await Deno.readTextFile(entry);
      assert.doesNotMatch(text, /private text|intent-1|guild-1|user-1/);
      assert.ok(JSON.parse(text).finishedAt);
    }
  }));

Deno.test("only a validated catalog target can reach the wire, even if a builder returns a bad plan", () =>
  privateFixture(async (directory) => {
    let calls = 0;
    const request = () => {
      calls++;
      return Promise.resolve({ httpStatus: 204, data: {} });
    };
    for (
      const path of [
        "https://bad.invalid/",
        "//bad.invalid/",
        "/users/@me",
        "/guilds/../members",
        "/guilds/./members",
        "/guilds/a%2Fb/members",
        "/guilds/%252e/members",
        "/guilds/foo\\bar/members",
        "/guilds/user/members#frag",
        "/guilds/foo/bar/members",
        "/guilds/foo/members?limit=%0d%0aX",
        "/guilds/foo/members?a=1&a=2",
      ]
    ) {
      const run = createQqPlatformOperator({
        directory,
        request,
        access: "admin",
        catalog: { ...catalog, build: () => ({ path }) },
      });
      assert.deepEqual(await run(members), { status: "failed", reason: "invalid_params" });
    }
    const redirected = createQqPlatformRead({
      request,
      catalog: { ...catalog, lookup: () => routes.members },
    });
    assert.deepEqual(await redirected(self), { status: "failed", reason: "not_allowed" });
    const mislabeled = createQqPlatformOperator({
      directory,
      request,
      access: "admin",
      catalog: { ...catalog, lookup: () => ({ ...routes.send, access: "read" }) },
    });
    assert.deepEqual(await mislabeled(send, approve), { status: "failed", reason: "not_allowed" });
    assert.equal(calls, 0);
  }));

Deno.test("bounded safe metadata only, including nested arrays, success receipts and failures", () =>
  privateFixture(async (directory) => {
    const secret = "private-media-proof";
    const item = {
      id: "safe-id",
      name: "https://cdn.invalid/?X-Amz-Signature=" + secret,
      username: "Bearer " + secret,
      user: {
        username: "成员",
        url: secret,
        access_token: secret,
        file_info: secret,
        rkey: secret,
      },
      content: secret,
      media: { file_info: secret },
      parts: [{ presigned_url: secret }],
      upload_id: secret,
      file_uuid: secret,
      trace_id: secret,
    };
    const run = createQqPlatformOperator({
      directory,
      catalog,
      access: "write",
      request: (method) =>
        Promise.resolve({
          httpStatus: 200,
          data: {
            id: method === "POST" ? "sent" : "list",
            items: Array(512).fill(item),
            authorization: secret,
          },
          traceId: secret,
        }),
    });
    const result = await run(members);
    assert.equal(result.status, "ok");
    assert.ok(result.status === "ok" && result.truncated);
    assert.ok(JSON.stringify(result).includes("成员"));
    assert.ok(new TextEncoder().encode(JSON.stringify(result)).length < 8500);
    assert.ok(!JSON.stringify(result).includes(secret));
    const ack = { status: "ok", httpStatus: 200, data: { id: "sent" } };
    assert.deepEqual(await run(send, approve), ack);
    assert.deepEqual(await run(send, approve), ack);
    for (const entry of await files(directory)) {
      assert.doesNotMatch(
        await Deno.readTextFile(entry),
        /private-media-proof|成员|safe-id|private text|authorization/,
      );
    }
  }));

Deno.test("HTTP, business codes, preflight errors and lost contact have stable no-retry outcomes", async () => {
  const matrix = [
    [403, { code: 1, err_code: 2, message: "hidden" }, "failed", "http_error"],
    [200, { err_code: 17 }, "failed", "platform_error"],
    [200, { code: "bad" }, "unknown", "invalid_response"],
    [200, { err_code: 100016 }, "failed", "authentication"],
    [202, {}, "unknown", "pending"],
    [200, { code: 304024 }, "unknown", "pending"],
    [400, { code: 40054005 }, "unknown", "duplicate"],
    [429, {}, "failed", "rate_limit"],
    [302, {}, "unknown", "http_error"],
    [503, {}, "unknown", "http_error"],
    [408, {}, "unknown", "http_error"],
    [200, null, "unknown", "invalid_response"],
  ] as const;
  for (const [httpStatus, data, status, reason] of matrix) {
    let attempts = 0;
    const read = createQqPlatformRead({
      catalog,
      request: () => {
        attempts++;
        return Promise.resolve({ httpStatus, data });
      },
    });
    const result = await read(self);
    assert.ok(result.status !== "ok" && result.status === status && result.reason === reason);
    if (httpStatus === 403) {
      assert.deepEqual(result, { status, reason, httpStatus, code: 1, errCode: 2 });
    }
    assert.equal(attempts, 1);
    assert.ok(!JSON.stringify(result).includes("hidden"));
  }
  for (
    const [error, status, reason] of [
      [
        new QqRequestError({ category: "transport", requestStarted: false }),
        "failed",
        "not_started",
      ],
      [
        new QqRequestError({ category: "platform", failureStage: "auth", message: "hidden" }),
        "failed",
        "authentication",
      ],
      [new QqRequestError({ category: "transport", phase: "body" }), "unknown", "transport"],
      [
        new QqRequestError({ category: "transport", failureStage: "auth" }),
        "failed",
        "not_started",
      ],
      [
        new QqRequestError({ category: "rate_limit", failureStage: "auth" }),
        "failed",
        "rate_limit",
      ],
      [new Error("https://bad.invalid/?signature=hidden"), "unknown", "transport"],
    ] as const
  ) {
    const read = createQqPlatformRead({ catalog, request: () => Promise.reject(error) });
    assert.deepEqual(await read(self), { status, reason });
  }
});

Deno.test("atomic write isolation across executors, restarts, crash remnants and corrupt receipts", () =>
  privateFixture(async (directory) => {
    const started = Promise.withResolvers<void>();
    const release = Promise.withResolvers<void>();
    let attempts = 0;
    const request = async () => {
      attempts++;
      started.resolve();
      await release.promise;
      return { httpStatus: 201, data: { id: "sent" } };
    };
    const next = () => createQqPlatformOperator({ directory, catalog, request, access: "write" });
    const first = next()(send, approve);
    await started.promise;
    let path: string, intent: string;
    try {
      [path] = await files(directory);
      intent = await Deno.readTextFile(path);
      assert.deepEqual(
        await Promise.all(Array.from({ length: 8 }, () => next()(send, approve))),
        Array(8).fill({ status: "unknown", reason: "incomplete" }),
      );
      assert.deepEqual(
        await next()(
          { ...send, params: { ...send.params, path: { user: "someone-else" } } },
          approve,
        ),
        { status: "failed", reason: "idempotency_conflict" },
      );
    } finally {
      release.resolve();
    }
    const done = await first;
    assert.deepEqual(done, { status: "ok", httpStatus: 201, data: { id: "sent" } });
    assert.deepEqual(await next()(send, approve), done);
    for (
      const partial of [
        intent!,
        "{",
        "",
        JSON.stringify({ ...JSON.parse(intent!), result: { status: "unsafe", secret: "fixture" } }),
      ]
    ) {
      await Deno.writeTextFile(path!, partial);
      assert.deepEqual(await next()(send, approve), { status: "unknown", reason: "incomplete" });
    }
    assert.equal(attempts, 1);
  }));

Deno.test("lost response is durable; failed audit setup or terminal save cannot cause redispatch", () =>
  privateFixture(async (directory) => {
    let attempts = 0;
    const request = () => {
      attempts++;
      return Promise.reject(new Error("secret transport detail"));
    };
    const configured = (dir: string) =>
      createQqPlatformOperator({ directory: dir, catalog, request, access: "write" });
    const unknown = { status: "unknown", reason: "transport" };
    assert.deepEqual(await configured(directory)(send, approve), unknown);
    assert.deepEqual(await configured(directory)(send, approve), unknown);
    const linked = `${directory}/linked`;
    await Deno.symlink(directory, linked);
    assert.deepEqual(await configured(linked)(send, approve), {
      status: "unknown",
      reason: "audit_unavailable",
    });
    const unsafe = `${directory}/public`;
    await Deno.mkdir(unsafe, { mode: 0o700 });
    await Deno.chmod(unsafe, 0o755);
    assert.deepEqual(await configured(unsafe)(send, approve), {
      status: "unknown",
      reason: "audit_unavailable",
    });
    assert.equal(attempts, 1);
    const store = `${directory}/platform-operations`;
    const after = createQqPlatformOperator({
      directory,
      catalog,
      access: "write",
      request: async () => {
        attempts++;
        await Deno.rename(store, `${directory}/offline`);
        return { httpStatus: 204, data: {} };
      },
    });
    const other = { ...send, idempotencyKey: "lost-terminal-save" };
    assert.deepEqual(await after(other, approve), {
      status: "unknown",
      reason: "audit_unavailable",
    });
    const unavailable = { status: "unknown", reason: "audit_unavailable" };
    assert.deepEqual(await configured(directory)(other, approve), unavailable);
    await Deno.mkdir(store, { mode: 0o700 });
    assert.deepEqual(await configured(directory)(other, approve), unavailable);
    assert.deepEqual(await Array.fromAsync(Deno.readDir(store)), []);
    await Deno.remove(store);
    await Deno.rename(`${directory}/offline`, store);
    assert.deepEqual(await configured(directory)(other, approve), {
      status: "unknown",
      reason: "incomplete",
    });
    assert.equal(attempts, 2);
  }));

Deno.test("real QqClient authorizes only the catalog path; all HTTPS is an in-memory fixture", () =>
  privateFixture(async (directory) => {
    const calls: string[] = [];
    const client = new QqClient({ appId: "fixture-app", appSecret: "fixture-secret" }, {
      httpsTransport: (url, init) => {
        calls.push(url.pathname);
        if (url.pathname === "/app/getAppAccessToken") {
          return Promise.resolve(
            Response.json({ access_token: "fixture-token", expires_in: 3600 }),
          );
        }
        assert.equal(new Headers(init.headers).get("Authorization"), "QQBot fixture-token");
        assert.equal(init.redirect, "manual");
        if (url.pathname === "/users/@me") {
          return Promise.resolve(Response.json({ id: "bot", bot: true }));
        }
        if (url.pathname === "/guilds/guild-1/members") {
          return Promise.resolve(Response.json([{ id: "member", file_info: "secret" }]));
        }
        assert.equal(url.pathname, "/v2/users/user-1/messages");
        assert.equal(JSON.parse(String(init.body)).content, "private text");
        return Promise.resolve(Response.json({ id: "message" }, { status: 201 }));
      },
    });
    try {
      const options = { directory, request: client.platformRequest.bind(client) };
      assert.equal((await createQqPlatformRead(options)(self)).status, "ok");
      const run = createQqPlatformOperator({ ...options, access: "admin" });
      const forGuild = { path: { guildId: "guild-1" } };
      assert.deepEqual(await run({ operation: "getGuildMembers", params: forGuild }, approve), {
        status: "ok",
        httpStatus: 200,
        data: [{ id: "member" }],
      });
      const text = {
        operation: "sendC2CMessage",
        idempotencyKey: send.idempotencyKey,
        params: { path: { userOpenid: "user-1" }, body: send.params.body },
      };
      assert.deepEqual(await run(text, approve), {
        status: "ok",
        httpStatus: 201,
        data: { id: "message" },
      });
      assert.deepEqual(
        await run({
          operation: "muteGuild",
          idempotencyKey: mute.idempotencyKey,
          params: { ...forGuild, body: { mute_seconds: "10" } },
        }, approve),
        { status: "failed", reason: "request_rejected" },
      );
      assert.deepEqual(calls, [
        "/app/getAppAccessToken",
        "/users/@me",
        "/guilds/guild-1/members",
        "/v2/users/user-1/messages",
      ]);
    } finally {
      client.close();
    }
  }));

Deno.test("approval covers the immutable plan and key; denial never reserves or dispatches", () =>
  privateFixture(async (directory) => {
    const waiting = Promise.withResolvers<void>();
    const release = Promise.withResolvers<void>();
    type Preview = Parameters<QqPlatformConfirm>[0];
    const previews: Preview[] = [];
    const calls: unknown[] = [];
    const run = createQqPlatformOperator({
      directory,
      catalog,
      access: "write",
      request: (method, path, body) => {
        calls.push([method, path, body]);
        return Promise.resolve({ httpStatus: 201, data: { id: "accepted" } });
      },
    });
    const denied = { status: "failed", reason: "confirmation_required" };
    for (
      const input of [send, { ...send, idempotencyKey: "intent-3" }, {
        ...send,
        params: { ...send.params, path: { user: "user-2" } },
      }, { ...send, params: { ...send.params, body: { content: "different" } } }]
    ) {
      assert.deepEqual(
        await run(input, (preview) => {
          previews.push(preview);
          return false;
        }),
        denied,
      );
    }
    assert.equal(new Set(previews.map(({ digest }) => digest)).size, 4);
    const [plan] = previews;
    assert.equal(Object.isFrozen(plan), true);
    assert.match(plan.digest, /^[a-f0-9]{64}$/);
    assert.doesNotMatch(JSON.stringify(previews), /user-[12]|private text|different|intent-[13]/);
    const fileDigest = platformDigest(JSON.stringify(send));
    let prompts = 0;
    const ask = (text: string) => {
      prompts++;
      assert.ok(text.includes(fileDigest) && text.includes(routes.send.path));
      assert.doesNotMatch(text, /private text|user-1|intent-1/);
      return `${plan.operation} ${plan.digest.slice(0, 12)}`;
    };
    assert.deepEqual(await run(send, terminalConfirmation(fileDigest, () => false, ask)), denied);
    assert.equal(prompts, 0);
    for (const answer of [null, "", plan.operation, "yes"]) {
      assert.deepEqual(
        await run(send, terminalConfirmation(fileDigest, () => true, () => answer)),
        denied,
      );
    }
    assert.deepEqual(
      await run(send, () => {
        throw new Error("cancelled");
      }),
      denied,
    );
    assert.deepEqual(await run(send, true as unknown as QqPlatformConfirm), denied);
    assert.deepEqual(await run({ ...send, params: { ...send.params, body: {} } }, approve), {
      status: "failed",
      reason: "invalid_params",
    });
    assert.deepEqual(await Array.fromAsync(Deno.readDir(directory)), []);
    assert.deepEqual(calls, []);

    const raw = structuredClone(send);
    const first = run(raw, async (preview) => {
      waiting.resolve();
      await release.promise;
      return await terminalConfirmation(fileDigest, () => true, ask)(preview);
    });
    await waiting.promise;
    raw.params.body.content = "later edit";
    raw.params.path.user = "user-2";
    release.resolve();
    assert.deepEqual(await first, { status: "ok", httpStatus: 201, data: { id: "accepted" } });
    assert.deepEqual(await run(send), denied);
    assert.deepEqual(calls, [["POST", "/v2/users/user-1/messages", { content: "private text" }]]);
    assert.equal(prompts, 1);
  }));

Deno.test("fixed catalog admin GET requires admin consent; getMe has one minimal projection", () =>
  privateFixture(async (directory) => {
    const calls: string[] = [];
    const request: QqPlatformRequest = (method, path) => {
      calls.push(`${method} ${path}`);
      return Promise.resolve({
        httpStatus: 200,
        data: {
          id: "fixture-bot",
          username: "Bearer hidden",
          bot: true,
          user: { id: "fixture-member" },
          members: [{ id: "fixture-member" }],
          access_token: "hidden",
        },
      });
    };
    const adminRead = {
      operation: "getGuildMembers",
      params: { path: { guildId: "fixture-guild" } },
    };
    const worker = createQqPlatformRead({ request });
    const operator = (access: "write" | "admin") =>
      createQqPlatformOperator({ directory, request, access });
    const denied = { status: "failed", reason: "not_allowed" };
    let approvals = 0;
    const confirm: QqPlatformConfirm = (preview) => {
      approvals++;
      assert.equal(preview.access, "admin");
      assert.equal(preview.method, "GET");
      return true;
    };
    assert.deepEqual(await worker(adminRead), denied);
    assert.deepEqual(await operator("write")(adminRead, confirm), denied);
    assert.deepEqual(await operator("admin")(adminRead), {
      status: "failed",
      reason: "confirmation_required",
    });
    assert.equal(approvals, 0);
    assert.deepEqual(calls, []);
    assert.equal((await operator("admin")(adminRead, confirm)).status, "ok");
    const expected = { status: "ok", httpStatus: 200, data: { id: "fixture-bot", bot: true } };
    assert.deepEqual(await worker(self), expected);
    assert.deepEqual(await operator("admin")(self, confirm), expected);
    assert.equal(approvals, 1);
    assert.deepEqual(calls, [
      "GET /guilds/fixture-guild/members",
      "GET /users/@me",
      "GET /users/@me",
    ]);
    for (const path of await files(directory)) {
      assert.doesNotMatch(
        await Deno.readTextFile(path),
        /fixture-bot|fixture-member|fixture-guild|hidden/,
      );
    }
  }));

Deno.test("a builder cannot omit required fields or change the approved body or query", () =>
  privateFixture(async (directory) => {
    let attempts = 0;
    let previews = 0;
    const confirm = () => {
      previews++;
      return true;
    };
    const operator = (catalog: QqPlatformCatalog) =>
      createQqPlatformOperator({
        directory,
        access: "write",
        catalog,
        request: () => {
          attempts++;
          return Promise.resolve({ httpStatus: 200, data: { id: "ack" } });
        },
      });
    const invalid = { status: "failed", reason: "invalid_params" };
    for (
      const body of [undefined, {}, { content: "different" }, { content: "private text", extra: 1 }]
    ) {
      assert.deepEqual(
        await operator({ ...catalog, build: () => ({ path: "/v2/users/user-1/messages", body }) })(
          send,
          confirm,
        ),
        invalid,
      );
    }
    const required = operator({
      ...catalog,
      lookup: () => ({ ...routes.members, requiredQuery: ["limit"] }),
    });
    assert.deepEqual(await required(members), invalid);
    const input = { ...members, params: { ...members.params, query: { limit: "2" } } };
    const wrong = operator({
      ...catalog,
      build: () => ({ path: "/guilds/guild-1/members?limit=3" }),
    });
    assert.deepEqual(await wrong(input), invalid);
    assert.deepEqual([previews, attempts], [0, 0]);
    assert.equal((await required(input)).status, "ok");
    assert.equal(attempts, 1);
  }));

Deno.test("private request files and ledgers reject unsafe ancestry, modes, links and oversized input", () =>
  privateFixture(async (directory) => {
    const uid = (await Deno.stat(directory)).uid!;
    const path = `${directory}/request.json`;
    await Deno.writeTextFile(path, "{}", { mode: 0o600 });
    assert.equal(await readPrivatePlatformFile(path, uid, 2), "{}");
    await assert.rejects(() => readPrivatePlatformFile(path, uid, 1));
    await assert.rejects(() => readPrivatePlatformFile(path, uid + 1, 8));
    await Deno.chmod(path, 0o644);
    await assert.rejects(() => readPrivatePlatformFile(path, uid, 8));
    await Deno.chmod(path, 0o600);
    await Deno.link(path, `${directory}/second-name`);
    await assert.rejects(() => readPrivatePlatformFile(path, uid, 8));
    await Deno.remove(`${directory}/second-name`);
    await Deno.symlink(path, `${directory}/shortcut`);
    await assert.rejects(() => readPrivatePlatformFile(`${directory}/shortcut`, uid, 8));
    const parent = `${directory}/shared`;
    const nested = `${parent}/private`;
    await Deno.mkdir(nested, { recursive: true, mode: 0o700 });
    await Deno.writeTextFile(`${nested}/request`, "{}", { mode: 0o600 });
    await Deno.chmod(parent, 0o770);
    await assert.rejects(() => readPrivatePlatformFile(`${nested}/request`, uid, 8));
    let attempts = 0;
    const request = () => {
      attempts++;
      return Promise.resolve({ httpStatus: 204, data: null });
    };
    const run = createQqPlatformOperator({ directory: nested, catalog, access: "admin", request });
    assert.deepEqual(await run(mute, approve), { status: "unknown", reason: "audit_unavailable" });
    assert.deepEqual(await Array.fromAsync(Deno.readDir(nested), ({ name }) => name), ["request"]);
    assert.equal(attempts, 0);
  }));

Deno.test("only a bound terminal receipt replays; legacy, incomplete and inconsistent successes stay unknown", () =>
  privateFixture(async (directory) => {
    let attempts = 0;
    const next = () =>
      createQqPlatformOperator({
        directory,
        catalog,
        access: "write",
        request: () => {
          attempts++;
          return Promise.resolve({
            httpStatus: 201,
            data: { id: "fixture-id", username: "private alias" },
          });
        },
      });
    const done = { status: "ok", httpStatus: 201, data: { id: "fixture-id" } };
    assert.deepEqual(await next()(send, approve), done);
    const [path] = await files(directory);
    assert.ok(path.endsWith(`/key-${platformDigest(send.idempotencyKey)}.json`));
    const original = JSON.parse(await Deno.readTextFile(path));
    for (
      const patch of [
        { version: 1 },
        { recordId: "another-record" },
        { startedAt: "" },
        { finishedAt: undefined },
        { result: { ...done, httpStatus: 202 } },
        { result: { ...done, httpStatus: 204 } },
        { result: { ...done, data: {} } },
        { result: { ...done, data: { id: "fixture-id", username: "private alias" } } },
        { result: { status: "failed", reason: "platform_error", code: "private detail" } },
      ]
    ) {
      await Deno.writeTextFile(path, JSON.stringify({ ...original, ...patch }));
      assert.deepEqual(await next()(send, approve), { status: "unknown", reason: "incomplete" });
    }
    await Deno.writeTextFile(path, JSON.stringify(original));
    assert.deepEqual(await next()(send, approve), done);
    assert.equal(attempts, 1);
  }));

Deno.test("simultaneous first use shares one durable reservation across executors", () =>
  privateFixture(async (directory) => {
    let attempts = 0;
    const request = () => {
      attempts++;
      return Promise.resolve({ httpStatus: 201, data: { id: "cold-start" } });
    };
    const next = () =>
      createQqPlatformOperator({ directory, catalog, access: "write", request })(send, approve);
    const replies = await Promise.all(Array.from({ length: 12 }, next));
    const done = { status: "ok", httpStatus: 201, data: { id: "cold-start" } };
    for (const reply of replies) {
      assert.deepEqual(
        reply,
        reply.status === "ok" ? done : { status: "unknown", reason: "incomplete" },
      );
    }
    assert.ok(replies.some(({ status }) => status === "ok"));
    assert.deepEqual(await next(), done);
    assert.equal((await files(directory)).length, 1);
    assert.equal(attempts, 1);
  }));

Deno.test("an ambiguous message acknowledgement is sticky and never becomes a successful resend", () =>
  privateFixture(async (directory) => {
    for (
      const [httpStatus, data, reason] of [
        [200, {}, "invalid_response"],
        [200, { id: 25 }, "invalid_response"],
        [201, {}, "pending"],
        [204, null, "invalid_response"],
        [200, { id: "ack", code: 304024, err_code: 123 }, "pending"],
        [200, { id: "ack", err_code: 40054005, code: 100016 }, "duplicate"],
      ] as const
    ) {
      let attempts = 0;
      const next = () =>
        createQqPlatformOperator({
          directory,
          catalog,
          access: "write",
          request: () => {
            attempts++;
            return Promise.resolve({ httpStatus, data });
          },
        });
      const input = {
        ...send,
        idempotencyKey: `fixture-${platformDigest(JSON.stringify([httpStatus, data]))}`,
      };
      const first = await next()(input, approve);
      assert.ok(first.status === "unknown" && first.reason === reason);
      assert.deepEqual(await next()(input, approve), first);
      assert.equal(attempts, 1);
    }
  }));
