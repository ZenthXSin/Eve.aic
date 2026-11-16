import assert from "node:assert/strict";
import type { lookup } from "node:dns/promises";
import { QqClient } from "../adapters/qq/client.ts";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { qqHttps, QqTransportError } from "../adapters/qq/transport.ts";

const credentials = { appId: "fixture", appSecret: "fixture" };
const input = { destination: { kind: "user" as const, openId: "fixture" }, text: "回复全文" };
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

Deno.test("QQ recovery: TLS failure before HTTP safely retries the same durable active sequence", async () => {
  const cert = await Deno.readTextFile(new URL("./fixtures/qq-tls/cert.pem", import.meta.url));
  const key = await Deno.readTextFile(new URL("./fixtures/qq-tls/key.pem", import.meta.url));
  const bodies: Record<string, unknown>[] = [];
  const tls = Deno.serve(
    { hostname: "127.0.0.1", port: 0, cert, key, onListen() {} },
    async (r) => {
      bodies.push(await r.json());
      return Response.json({ id: "delivered" });
    },
  );
  const auth = platform(() => Response.json({}));
  const directory = await Deno.makeTempDir();
  let connections = 0;
  const client = new QqClient(credentials, {
    baseUrl: `https://localhost:${tls.addr.port}`,
    tokenBaseUrl: auth.url,
    httpsTransport: (url, init, diagnostic) => {
      const resolver = (() =>
        Promise.resolve([
          { address: ++connections === 1 ? "127.0.0.2" : "127.0.0.1", family: 4 },
        ])) as unknown as typeof lookup;
      return qqHttps(url, init, diagnostic, resolver, cert);
    },
  });
  const actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0] });
  try {
    await actions.initialize();
    const sent = await actions.send("active", input);
    assert.equal(sent.status, "finished");
    assert.ok(sent.status === "finished" && sent.result.status === "sent");
    assert.equal(sent.attemptCount, 2);
    assert.equal(connections, 2);
    assert.deepEqual(bodies, [{ msg_type: 2, markdown: { content: input.text }, msg_seq: 2 }]);
    const first = JSON.parse(await Deno.readTextFile(`${directory}/attempts/active/1.result.json`));
    assert.equal(first.result.attempted, false);
    assert.equal(first.result.error.requestStarted, false);
    assert.equal(first.result.error.failureStage, "message");
  } finally {
    await actions.close();
    await tls.shutdown();
    await auth.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ recovery: a partial HTTP write remains unknown and is never replayed", async () => {
  const auth = platform(() => Response.json({}));
  const directory = await Deno.makeTempDir();
  let sends = 0;
  const client = new QqClient(credentials, {
    baseUrl: "https://localhost:443",
    tokenBaseUrl: auth.url,
    httpsTransport: () => {
      sends++;
      return Promise.reject(new QqTransportError(true));
    },
  });
  let actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0] });
  try {
    await actions.initialize();
    const outcome = await actions.send("partial", input);
    assert.ok(outcome.status === "finished" && outcome.result.status === "unknown");
    assert.ok(
      outcome.result.error.requestStarted === true,
    );
    assert.equal((await actions.send("later", input)).status, "blocked");
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    assert.deepEqual(await actions.send("partial", input), outcome);
    assert.equal(sends, 1);
  } finally {
    await actions.close();
    await auth.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ recovery: explicit failure keeps its receipt and releases later messages across restart", async () => {
  const sequences: unknown[] = [];
  const api = platform(async (r) => {
    const body = await r.json();
    sequences.push(body.msg_seq);
    return sequences.length === 1
      ? Response.json({ code: 40034105 }, { status: 400 })
      : Response.json({ id: `delivered-${sequences.length}` });
  });
  const directory = await Deno.makeTempDir();
  const client = new QqClient(credentials, { baseUrl: api.url });
  let actions = new QqSendActions(client, directory);
  try {
    await actions.initialize();
    const failed = await actions.send("denied", input);
    assert.ok(
      failed.status === "finished" && failed.result.status === "failed" &&
        failed.result.error.reason === "active_permission_denied",
    );
    const original = await Deno.readTextFile(`${directory}/records/denied.json`);
    assert.equal((await actions.listSendQueue()).items.length, 0);
    await actions.close();
    actions = new QqSendActions(new QqClient(credentials, { baseUrl: api.url }), directory);
    await actions.initialize();
    assert.deepEqual(await actions.send("denied", input), failed);
    const next = await actions.send("next", input);
    assert.ok(next.status === "finished" && next.result.status === "sent");
    assert.deepEqual(sequences, [2, 4]);
    assert.equal(await Deno.readTextFile(`${directory}/records/denied.json`), original);
  } finally {
    await actions.close();
    await api.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ recovery: only definite Markdown format rejection falls back, with persisted attempts", async () => {
  const bodies: Record<string, unknown>[] = [];
  const api = platform(async (r) => {
    bodies.push(await r.json());
    if (bodies.length === 1) {
      return Response.json({ code: 400, message: "invalid markdown format" }, { status: 400 });
    }
    if (bodies.length === 2) return Response.json({ code: 100016 }, { status: 401 });
    return Response.json({ id: "delivered" }, { status: 201 });
  });
  const directory = await Deno.makeTempDir();
  const client = new QqClient(credentials, { baseUrl: api.url });
  let actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0] });
  try {
    await actions.initialize();
    const result = await actions.send("markdown", input);
    assert.ok(result.status === "finished" && result.result.status === "sent");
    assert.equal(result.attemptCount, 3);
    assert.deepEqual(bodies, [
      { msg_type: 2, markdown: { content: input.text }, msg_seq: 2 },
      { msg_type: 0, content: input.text, msg_seq: 3 },
      { msg_type: 0, content: input.text, msg_seq: 3 },
    ]);
    const first = JSON.parse(
      await Deno.readTextFile(`${directory}/attempts/markdown/1.result.json`),
    );
    assert.equal(first.result.error.reason, "markdown_rejected");
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    assert.deepEqual(await actions.send("markdown", input), result);
    assert.equal(bodies.length, 3);
  } finally {
    await actions.close();
    await api.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ recovery: a failed upload is retried before any active message is sent", async () => {
  let uploads = 0, sends = 0;
  const api = platform((r) => {
    if (new URL(r.url).pathname.endsWith("/files")) {
      uploads++;
      return uploads === 1
        ? Response.json({ code: 850012 }, { status: 500 })
        : Response.json({ file_info: "file" });
    }
    sends++;
    return Response.json({ id: "delivered" });
  });
  const directory = await Deno.makeTempDir();
  const client = new QqClient(credentials, {
    baseUrl: api.url,
    attachmentReader: () => Promise.resolve(new Uint8Array([1, 2, 3])),
  });
  const actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0] });
  try {
    await actions.initialize();
    const result = await actions.send("file", {
      ...input,
      attachment: { path: "/tmp/file", kind: "file" },
    });
    assert.ok(result.status === "finished" && result.result.status === "sent");
    assert.equal(result.attemptCount, 2);
    assert.deepEqual([uploads, sends], [2, 1]);
  } finally {
    await actions.close();
    await api.close();
    await Deno.remove(directory, { recursive: true });
  }
});
