import assert from "node:assert/strict";
import type { lookup } from "node:dns/promises";
import { type QqConnectionDiagnostic, qqHttps } from "../adapters/qq/transport.ts";

const cert = await Deno.readTextFile(new URL("./fixtures/qq-tls/cert.pem", import.meta.url));
const key = await Deno.readTextFile(new URL("./fixtures/qq-tls/key.pem", import.meta.url));
const resolver = (() =>
  Promise.resolve([
    { address: "127.0.0.2", family: 4 },
    { address: "127.0.0.1", family: 4 },
  ])) as unknown as typeof lookup;
Deno.test("QQ TLS：坏地址与好地址竞速，鉴权POST仅一次，诊断不含凭据和正文", async () => {
  let sends = 0;
  const events: QqConnectionDiagnostic[] = [];
  const server = Deno.serve(
    { hostname: "127.0.0.1", port: 0, cert, key, onListen() {} },
    async (r) => {
      sends++;
      assert.equal(r.headers.get("authorization"), "QQBot secret-token");
      assert.equal(await r.text(), "secret-message");
      return Response.json({ id: "actual-receipt" });
    },
  );
  try {
    const response = await qqHttps(
      new URL(`https://localhost:${server.addr.port}/messages`),
      {
        method: "POST",
        headers: { authorization: "QQBot secret-token" },
        body: "secret-message",
        signal: AbortSignal.timeout(5000),
      },
      (e) => events.push(e),
      resolver,
      cert,
    );
    assert.equal((await response.json()).id, "actual-receipt");
    assert.equal(sends, 1);
    assert.equal(events.filter((e) => e.phase === "request_written").length, 1);
    assert.equal(events.filter((e) => e.phase === "connection_selected").length, 1);
    assert.ok(events.some((e) => e.phase === "connect_error" && e.address === "127.0.0.2"));
    assert.ok(!JSON.stringify(events).includes("secret"));
  } finally {
    await server.shutdown();
  }
});
Deno.test("QQ TLS：不信任证书时拒绝发送，已取消的请求不进入DNS", async () => {
  let sends = 0;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, cert, key, onListen() {} }, () => {
    sends++;
    return Response.json({ id: "unexpected" });
  });
  try {
    const url = new URL(`https://localhost:${server.addr.port}/messages`);
    await assert.rejects(() =>
      qqHttps(
        url,
        { method: "POST", body: "message", signal: AbortSignal.timeout(5000) },
        undefined,
        resolver,
      )
    );
    const events: QqConnectionDiagnostic[] = [];
    await assert.rejects(() =>
      qqHttps(url, { signal: AbortSignal.abort() }, (e) => events.push(e), resolver, cert)
    );
    assert.equal(sends, 0);
    assert.deepEqual(events, []);
  } finally {
    await server.shutdown();
  }
});
Deno.test("QQ TLS：收到响应头后正文超时仍中止连接，POST不重放", async () => {
  let sends = 0;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, cert, key, onListen() {} }, () => {
    sends++;
    return new Response(
      new ReadableStream({
        start(c) {
          c.enqueue(new TextEncoder().encode("{"));
        },
      }),
    );
  });
  try {
    const response = await qqHttps(
      new URL(`https://localhost:${server.addr.port}/messages`),
      {
        method: "POST",
        body: "once",
        signal: AbortSignal.timeout(300),
      },
      undefined,
      resolver,
      cert,
    );
    await assert.rejects(() => response.text());
    assert.equal(sends, 1);
  } finally {
    await server.shutdown();
  }
});
