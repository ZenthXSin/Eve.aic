import assert from "node:assert/strict";
import { QqClient, type QqSendText } from "../adapters/qq/client.ts";
import { QqSendActions } from "../adapters/qq/actions.ts";
const request: QqSendText = {
  destination: { kind: "user", openId: "fixture" },
  text: "# 标题\n<@user> **正文**",
  reply: { messageId: "incoming", sequence: 4 },
  referenceIndex: "ref",
};
const fileSource = "# literal code\nconst snake_case = '**not formatting**';\n";
function fixture() {
  const bodies: Record<string, unknown>[] = [];
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture", expires_in: 7200 });
    }
    if (new URL(r.url).pathname.endsWith("/files")) {
      const upload = await r.json();
      assert.equal(atob(upload.file_data), fileSource);
      return Response.json({ file_info: "fixture" });
    }
    bodies.push(await r.json());
    return Response.json({ id: "sent-" + bodies.length });
  });
  return {
    bodies,
    close: () => server.shutdown(),
    client: () =>
      new QqClient({ appId: "fixture", appSecret: "fixture" }, {
        baseUrl: `http://127.0.0.1:${server.addr.port}`,
        sendScope: "all",
        attachmentReader: () => Promise.resolve(new TextEncoder().encode(fileSource)),
      }),
  };
}

Deno.test("Plain QQ wire converts Markdown for private/group/media and preserves display references", async () => {
  const f = fixture();
  const c = f.client();
  try {
    for (
      const args of [
        { ...request, format: "markdown" as const },
        { ...request, format: undefined },
        {
          ...request,
          destination: { kind: "group" as const, openId: "group" },
          format: "markdown" as const,
          reply: { eventId: "event" },
        },
        { ...request, format: "markdown" as const, reply: undefined, wakeup: true },
      ]
    ) {
      assert.equal((await c.sendText(args)).status, "sent");
    }
    for (const body of f.bodies) {
      assert.equal(body.msg_type, 0);
      assert.equal(body.content, "标题\n<@user> 正文");
      assert.deepEqual(body.message_reference, { message_id: "ref" });
      for (const field of ["markdown", "msg_id", "msg_seq", "event_id"]) {
        assert.equal(body[field], undefined);
      }
    }
    assert.equal(f.bodies[3].is_wakeup, true);
    await assert.rejects(c.sendText({ ...request, format: "bogus" as "markdown" }));
    await assert.rejects(c.sendText({ ...request, text: "```" }));
    assert.equal(
      (await c.sendText({
        ...request,
        format: "markdown",
        attachment: { kind: "file", path: "/tmp/fixture.txt" },
      })).status,
      "sent",
    );
    assert.deepEqual(f.bodies[4], {
      msg_type: 7,
      content: "标题\n<@user> 正文",
      media: { file_info: "fixture" },
      message_reference: { message_id: "ref" },
    });
  } finally {
    await f.close();
  }
});
Deno.test("Plain admission pins default format across restart and rejects a format change", async () => {
  const f = fixture();
  const dir = await Deno.makeTempDir();
  let a = new QqSendActions(f.client(), dir);
  try {
    await a.initialize();
    await a.send("new", request);
    const path = dir + "/records/new.json";
    const raw = await Deno.readTextFile(path);
    assert.equal(JSON.parse(raw).request.format, "plain");
    assert.equal(f.bodies[0].msg_type, 0);
    await a.close();
    a = new QqSendActions(f.client(), dir);
    await a.initialize();
    await a.send("new", request);
    assert.equal(f.bodies.length, 1);
    assert.equal(await Deno.readTextFile(path), raw);
    await assert.rejects(a.send("new", { ...request, format: "markdown" }), /conflict/);
    await a.send("file", {
      ...request,
      reply: { messageId: "file-in", sequence: 1 },
      attachment: { kind: "file", path: "/tmp/fixture.txt" },
    });
    assert.equal(f.bodies[1].msg_type, 7);
    assert.equal(f.bodies[1].markdown, undefined);
  } finally {
    await a.close();
    await f.close();
    await Deno.remove(dir, { recursive: true });
  }
});
function canonical(v: unknown): string {
  if (v === null || typeof v !== "object") return JSON.stringify(v);
  if (Array.isArray(v)) return "[" + v.map(canonical).join(",") + "]";
  const r = v as Record<string, unknown>;
  return "{" +
    Object.keys(r).filter((k) => r[k] !== undefined).sort().map((k) =>
      JSON.stringify(k) + ":" + canonical(r[k])
    ).join(",") + "}";
}
Deno.test("Legacy ledger without format retains fingerprint/body and does not resend", async () => {
  const f = fixture();
  const dir = await Deno.makeTempDir();
  await Deno.mkdir(dir + "/records");
  const a = new QqSendActions(f.client(), dir);
  const originals = new Map<string, string>();
  for (
    const [id, result] of Object.entries({
      old: { status: "sent", messageId: "legacy-sent" },
      marked: { status: "sent", messageId: "legacy-markdown-sent" },
      rejected: {
        status: "failed",
        attempted: true,
        error: { category: "platform", httpStatus: 400, code: 40034031 },
      },
      uncertain: { status: "unknown", attempted: true, error: { category: "invalid_response" } },
    })
  ) {
    const savedRequest = id === "marked" ? { ...request, format: "markdown" } : request;
    const bytes = await crypto.subtle.digest(
      "SHA-256",
      new TextEncoder().encode(canonical(savedRequest)),
    );
    const hash = Array.from(new Uint8Array(bytes), (b) => b.toString(16).padStart(2, "0")).join("");
    const record = {
      schema: 1,
      actionId: id,
      fingerprint: hash,
      request: savedRequest,
      startedAt: "2026-11-12T00:00:00.000Z",
      finishedAt: "2026-11-12T00:00:01.000Z",
      status: "finished",
      result,
    };
    const raw = JSON.stringify(record);
    originals.set(id, raw);
    await Deno.writeTextFile(dir + "/records/" + id + ".json", raw);
  }
  try {
    await a.initialize();
    for (const [id, raw] of originals) {
      const result = await a.send(id, request);
      assert.equal(result.status, "finished");
      assert.equal(await Deno.readTextFile(dir + "/records/" + id + ".json"), raw);
    }
    assert.equal(f.bodies.length, 0);
    await assert.rejects(a.send("marked", { ...request, format: "plain" }), /conflict/);
  } finally {
    await a.close();
    await f.close();
    await Deno.remove(dir, { recursive: true });
  }
});
Deno.test("Plain platform refusal is retained without extra sends or format fallback", async () => {
  const bodies: unknown[] = [];
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture", expires_in: 7200 });
    }
    bodies.push(await r.json());
    return Response.json({ code: 40034031 }, { status: 400 });
  });
  const dir = await Deno.makeTempDir();
  const c = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
    baseUrl: "http://127.0.0.1:" + server.addr.port,
  });
  const a = new QqSendActions(c, dir);
  try {
    await a.initialize();
    const s = await a.send("refused", request);
    assert.equal(s.status, "finished");
    if (s.status === "finished") assert.equal(s.result.status, "failed");
    assert.equal(bodies.length, 1);
    assert.equal((bodies[0] as { msg_type: number }).msg_type, 0);
  } finally {
    await a.close();
    await server.shutdown();
    await Deno.remove(dir, { recursive: true });
  }
});
