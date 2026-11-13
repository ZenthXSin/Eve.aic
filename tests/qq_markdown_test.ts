import assert from "node:assert/strict";
import { QqClient, type QqSendText } from "../adapters/qq/client.ts";
import { QqSendActions } from "../adapters/qq/actions.ts";
const request: QqSendText = {
  destination: { kind: "user", openId: "fixture" },
  text: "# 标题\n<@user> **正文**",
  reply: { messageId: "incoming", sequence: 4 },
  referenceIndex: "ref",
};
function fixture() {
  const bodies: Record<string, unknown>[] = [];
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture", expires_in: 7200 });
    }
    if (new URL(r.url).pathname.endsWith("/files")) return Response.json({ file_info: "fixture" });
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
        attachmentReader: () => Promise.resolve(new Uint8Array([1, 2, 3])),
      }),
  };
}

Deno.test("Markdown: client wire body, legacy plain, proactive/group/event and file remain distinct", async () => {
  const f = fixture();
  const c = f.client();
  try {
    assert.equal((await c.sendText({ ...request, format: "markdown" })).status, "sent");
    assert.deepEqual(f.bodies[0], {
      msg_type: 2,
      markdown: { content: request.text },
    });
    await c.sendText({ ...request, format: undefined });
    assert.deepEqual(f.bodies[1], {
      msg_type: 0,
      content: request.text,
      message_reference: { message_id: "ref" },
    });
    await c.sendText({
      ...request,
      destination: { kind: "group", openId: "group" },
      format: "markdown",
      reply: { eventId: "event" },
    });
    assert.deepEqual(f.bodies[2], {
      msg_type: 2,
      markdown: { content: request.text },
    });
    await c.sendText({ ...request, format: "markdown", reply: undefined, wakeup: true });
    assert.deepEqual(f.bodies[3], {
      msg_type: 2,
      markdown: { content: request.text },
      is_wakeup: true,
    });
    await assert.rejects(c.sendText({ ...request, format: "bogus" as "markdown" }));
    await assert.rejects(
      c.sendText({
        ...request,
        format: "markdown",
        attachment: { kind: "file", path: "/tmp/fixture.txt" },
      }),
    );
    assert.equal(f.bodies.length, 4);
  } finally {
    await f.close();
  }
});
Deno.test("Markdown admission pins format across restart and rejects a format change", async () => {
  const f = fixture();
  const dir = await Deno.makeTempDir();
  let a = new QqSendActions(f.client(), dir);
  try {
    await a.initialize();
    await a.send("new", request);
    const path = dir + "/records/new.json";
    const raw = await Deno.readTextFile(path);
    assert.equal(JSON.parse(raw).request.format, "markdown");
    assert.equal(f.bodies[0].msg_type, 2);
    await a.close();
    a = new QqSendActions(f.client(), dir);
    await a.initialize();
    await a.send("new", request);
    assert.equal(f.bodies.length, 1);
    assert.equal(await Deno.readTextFile(path), raw);
    await assert.rejects(a.send("new", { ...request, format: "plain" }), /conflict/);
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
  const bytes = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical(request)));
  const hash = Array.from(new Uint8Array(bytes), (b) => b.toString(16).padStart(2, "0")).join("");
  const a = new QqSendActions(f.client(), dir);
  const originals = new Map<string, string>();
  for (
    const [id, result] of Object.entries({
      old: { status: "sent", messageId: "legacy-sent" },
      rejected: {
        status: "failed",
        attempted: true,
        error: { category: "platform", httpStatus: 400, code: 40034031 },
      },
      uncertain: { status: "unknown", attempted: true, error: { category: "invalid_response" } },
    })
  ) {
    const record = {
      schema: 1,
      actionId: id,
      fingerprint: hash,
      request,
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
  } finally {
    await a.close();
    await f.close();
    await Deno.remove(dir, { recursive: true });
  }
});
Deno.test("Markdown platform refusal is retained without plain-text fallback", async () => {
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
    assert.equal((bodies[0] as { msg_type: number }).msg_type, 2);
  } finally {
    await a.close();
    await server.shutdown();
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("Restored Markdown keeps explicit plain admissions and old finished format records immutable", async () => {
  const f = fixture();
  const dir = await Deno.makeTempDir();
  await Deno.mkdir(dir + "/records");
  const originals = new Map<string, string>();
  for (const format of ["plain", "markdown"] as const) {
    for (const status of ["sent", "failed", "unknown"] as const) {
      const id = format + "-" + status;
      const saved = { ...request, format };
      const bytes = await crypto.subtle.digest(
        "SHA-256",
        new TextEncoder().encode(canonical(saved)),
      );
      const fingerprint = Array.from(new Uint8Array(bytes), (b) => b.toString(16).padStart(2, "0"))
        .join("");
      const result = status === "sent" ? { status, messageId: id } : {
        status,
        attempted: true,
        error: { category: "platform", httpStatus: 400, code: 40034031 },
      };
      const raw = JSON.stringify({
        schema: 1,
        actionId: id,
        fingerprint,
        request: saved,
        startedAt: "2026-11-12T00:00:00.000Z",
        finishedAt: "2026-11-12T00:00:01.000Z",
        status: "finished",
        result,
      });
      originals.set(id, raw);
      await Deno.writeTextFile(dir + "/records/" + id + ".json", raw);
    }
  }
  let a = new QqSendActions(f.client(), dir);
  try {
    await a.initialize();
    for (const [id, raw] of originals) {
      const returned = await a.send(id, request);
      assert.equal(returned.status, "finished");
      assert.equal(await Deno.readTextFile(dir + "/records/" + id + ".json"), raw);
    }
    assert.equal(f.bodies.length, 0);
    await a.send("explicit-plain", { ...request, format: "plain" });
    assert.deepEqual(f.bodies[0], {
      msg_type: 0,
      content: request.text,
      message_reference: { message_id: "ref" },
    });
    const raw = await Deno.readTextFile(dir + "/records/explicit-plain.json");
    await a.close();
    a = new QqSendActions(f.client(), dir);
    await a.initialize();
    await a.send("explicit-plain", request);
    assert.equal(f.bodies.length, 1);
    assert.equal(await Deno.readTextFile(dir + "/records/explicit-plain.json"), raw);
    await assert.rejects(a.send("explicit-plain", { ...request, format: "markdown" }), /conflict/);
  } finally {
    await a.close();
    await f.close();
    await Deno.remove(dir, { recursive: true });
  }
});
