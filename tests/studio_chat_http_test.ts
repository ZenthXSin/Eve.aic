import assert from "node:assert/strict";
import type { Json } from "../kernel/protocol.ts";
import { studioChatRoutes } from "../services/admin/studio-chat.ts";
import { StudioChatMailbox } from "../supervisor/studio-chat.ts";

const origin = "https://studio.test", session = { csrf: "fixture-csrf" };
const get = (query = "") => new Request(`${origin}/api/chat${query}`);
const post = (body: unknown, csrf = session.csrf, from = origin) =>
  new Request(`${origin}/api/chat/send`, {
    method: "POST",
    headers: { origin: from, "content-type": "application/json", "x-eve-csrf": csrf },
    body: JSON.stringify(body),
  });

Deno.test("HTTP 使用裸契约，登录、同源和 CSRF 均不可替代；不跨 IPC 发送额外字段", async () => {
  const parent = Deno.makeTempDirSync({ prefix: "studio-chat-http-" });
  try {
    const box = StudioChatMailbox.open(`${parent}/private`);
    try {
      let requests = 0;
      const route = studioChatRoutes((request) => {
        requests++;
        const source = box.handle(request);
        const privateFields = {
          modelOutput: ["SECRET reasoning"],
          toolCalls: [{ args: "SECRET function" }],
          system: "SECRET prompt",
          token: "SECRET key",
        };
        const value = "message" in source
          ? { ...source, message: { ...source.message, ...privateFields } }
          : {
            ...source,
            description: "SECRET description",
            messages: source.messages.map((msg) => ({ ...msg, ...privateFields })),
          };
        return Promise.resolve({ ...value, ...privateFields });
      });
      const id = crypto.randomUUID(), body = { id, text: "第一条" };
      assert.equal((await route(post(body), undefined, origin))?.status, 401);
      assert.equal((await route(post(body, "wrong"), session, origin))?.status, 403);
      assert.equal(
        (await route(post(body, session.csrf, "https://other.test"), session, origin))?.status,
        403,
      );
      for (
        const query of [
          "?before=0",
          "?before=1&before=2",
          "?before=-2",
          "?before=1.5",
          "?before=9007199254740992",
          "?identity=admin",
        ]
      ) {
        assert.equal((await route(get(query), session, origin))?.status, 400);
      }
      for (
        const invalid of [
          { ...body, role: "system" },
          { ...body, action: "control" },
          { id, text: "\ud800" },
          { id, text: "\0" },
          { id, text: "" },
          { ...body, id: "not-a-uuid" },
          { id, text: "a".repeat(4097) },
        ]
      ) {
        assert.equal((await route(post(invalid), session, origin))?.status, 400);
      }
      assert.equal(
        (await route(post({ id, text: "a".repeat(40_000) }), session, origin))?.status,
        413,
      );
      assert.equal(
        (await route(
          new Request(`${origin}/api/chat/send`, {
            method: "POST",
            headers: { "x-eve-csrf": session.csrf },
            body: "form",
          }),
          session,
          origin,
        ))?.status,
        415,
      );
      assert.equal(
        (await route(new Request(`${origin}/api/chat/send`), session, origin))?.status,
        405,
      );
      assert.equal(await route(new Request(`${origin}/api/status`), session, origin), undefined);
      assert.equal(requests, 0);
      const response = (await route(post(body), session, origin))!;
      assert.equal(response.status, 202);
      assert.equal(response.headers.get("cache-control"), "no-store");
      const admitted = await response.json();
      assert.deepEqual(Object.keys(admitted).sort(), ["message", "mode"]);
      assert.equal(admitted.message.text, body.text);
      assert(!JSON.stringify(admitted).includes("SECRET"));
      assert.equal((await route(post({ id, text: "变更" }), session, origin))?.status, 409);
      assert.equal(
        (await route(post({ id: crypto.randomUUID(), text: "另一条" }), session, origin))?.status,
        429,
      );
      const history = await (await route(get(), session, origin))!.json();
      assert.deepEqual(Object.keys(history).sort(), [
        "description",
        "messages",
        "mode",
        "nextBefore",
      ]);
      assert.deepEqual(history.messages, [admitted.message]);
      assert.equal(history.nextBefore, null);
      assert(!JSON.stringify(history).includes("SECRET"));
    } finally {
      box.close();
    }
  } finally {
    Deno.removeSync(parent, { recursive: true });
  }
});

Deno.test("HTTP 在读慢正文前就限制并发；错误与畸形后端结果不透传", async () => {
  const waiting = Promise.withResolvers<Json>();
  let requests = 0;
  const route = studioChatRoutes(() => {
    requests++;
    return waiting.promise;
  });
  const busy = Array.from({ length: 8 }, () => route(get(), session, origin));
  assert.equal((await route(get(), session, origin))?.status, 429);
  assert.equal(requests, 8);
  waiting.resolve({ messages: [{ role: "tool", text: "SECRET" }], nextBefore: null });
  for (const response of await Promise.all(busy)) {
    assert.equal(response?.status, 503);
    assert(!(await response!.text()).includes("SECRET"));
  }
  const unavailable = studioChatRoutes(() => Promise.reject(Error("SECRET exception")));
  const failure = (await unavailable(get(), session, origin))!;
  assert.equal(failure.status, 503);
  assert(!(await failure.text()).includes("SECRET"));
  const cancelled: boolean[] = [];
  const oversized = new Request(`${origin}/api/chat/send`, {
    method: "POST",
    headers: { "x-eve-csrf": session.csrf, "content-type": "application/json" },
    body: new ReadableStream({
      pull(controller) {
        controller.enqueue(new Uint8Array(12_000));
      },
      cancel() {
        cancelled.push(true);
      },
    }),
  });
  assert.equal((await route(oversized, session, origin))?.status, 413);
  assert.deepEqual(cancelled, [true]);
});

Deno.test("损坏或非私有原件拒绝打开而不是覆盖或暴露路径", () => {
  const parent = Deno.makeTempDirSync({ prefix: "studio-chat-invalid-" });
  try {
    const path = `${parent}/private`;
    Deno.mkdirSync(path, { mode: 0o700 });
    Deno.writeTextFileSync(`${path}/chat.sqlite`, "private original", { mode: 0o600 });
    assert.throws(() => StudioChatMailbox.open(path), /私聊暂不可用/);
    assert.equal(Deno.readTextFileSync(`${path}/chat.sqlite`), "private original");
    Deno.symlinkSync(path, `${parent}/alias`);
    assert.throws(() => StudioChatMailbox.open(`${parent}/alias`), /私聊暂不可用/);
    Deno.chmodSync(path, 0o755);
    assert.throws(() => StudioChatMailbox.open(path), /私聊暂不可用/);
  } finally {
    Deno.removeSync(parent, { recursive: true });
  }
});
