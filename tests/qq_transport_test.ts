import assert from "node:assert/strict";
import type { lookup } from "node:dns/promises";
import type { ServerResponse } from "node:http";
import { createServer } from "node:https";
import type { AddressInfo } from "node:net";
import {
  type QqConnectionDiagnostic,
  qqHttps,
  QqTransportError,
} from "../adapters/qq/transport.ts";

const cert = await Deno.readTextFile(new URL("./fixtures/qq-tls/cert.pem", import.meta.url));
const key = await Deno.readTextFile(new URL("./fixtures/qq-tls/key.pem", import.meta.url));
const local = "127.0.0.1", other = "127.0.0.2";
function dns(records: () => string[] | Promise<string[]>): typeof lookup {
  const resolve = async () => (await records()).map((address) => ({ address, family: 4 }));
  return resolve as unknown as typeof lookup;
}
const resolver = dns(() => [other, local]);
const post = {
  method: "POST",
  headers: { authorization: "QQBot secret-token" },
  body: "secret-message",
};
const path = "/messages?secret-query";
function send(
  port: number,
  resolve: typeof lookup,
  events: QqConnectionDiagnostic[],
  signal = AbortSignal.timeout(3000),
  target = path,
) {
  return qqHttps(
    new URL(`https://localhost:${port}${target}`),
    { ...post, signal },
    (event) => {
      events.push(event);
    },
    resolve,
    cert,
  );
}
function starts(events: QqConnectionDiagnostic[]) {
  return events.filter((event) => event.phase === "connect_start").map((event) => event.address);
}
function started(expected: boolean) {
  return (error: unknown) => {
    assert.ok(error instanceof QqTransportError);
    assert.equal(error.requestStarted, expected);
    assert.doesNotMatch(
      Deno.inspect(error, { showHidden: true, depth: 5 }),
      /authorization|secret/i,
    );
    return true;
  };
}
async function bounded<T>(operation: Promise<T>) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => reject(new Error("QQ fixture did not settle")), 2500);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
async function endpoint(host: string, port = 0, reply?: (response: ServerResponse) => void) {
  const counts = { connections: 0, handshakes: 0, posts: 0 };
  const server = createServer({ cert, key }, async (request, response) => {
    counts.posts++;
    let body = "";
    for await (const chunk of request) body += chunk;
    assert.equal(request.method, "POST");
    assert.equal(request.headers.authorization, post.headers.authorization);
    assert.equal(body, post.body);
    if (request.url === "/drop") request.socket.destroy();
    else if (request.url === "/busy") {
      response.writeHead(503, { "retry-after": "0" });
      response.end("secret-upstream-detail");
    } else {
      assert.equal(request.url, path);
      if (reply) reply(response);
      else response.end(host);
    }
  });
  server.on("connection", () => counts.connections++);
  server.on("secureConnection", () => counts.handshakes++);
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, host, resolve);
  });
  const close = async () => {
    if (server.listening) await server[Symbol.asyncDispose]();
  };
  return {
    port: (server.address() as AddressInfo).port,
    counts,
    close,
    [Symbol.asyncDispose]: close,
  };
}

Deno.test("QQ TLS：双地址冷启竞速，稳态每次重新 DNS 但只做一次握手", async () => {
  await using left = await endpoint(local);
  await using right = await endpoint(other, left.port);
  let records = [local, other], lookups = 0;
  const resolve = dns(() => {
    lookups++;
    return records;
  });
  const cold: QqConnectionDiagnostic[] = [];
  const winner = await (await send(left.port, resolve, cold)).text();
  assert.deepEqual(starts(cold), records);
  assert.ok(winner === local || winner === other);
  records = [winner === local ? other : local, winner];
  const rounds = 8;
  for (let index = 0; index < rounds; index++) {
    const events: QqConnectionDiagnostic[] = [];
    assert.equal(await (await send(left.port, resolve, events)).text(), winner);
    assert.deepEqual(starts(events), [winner]);
    for (
      const phase of [
        "dns_complete",
        "tls_connected",
        "connection_selected",
        "request_written",
        "response_headers",
        "response_complete",
      ]
    ) {
      assert.equal(events.filter((event) => event.phase === phase).length, 1, phase);
    }
    assert.doesNotMatch(JSON.stringify(events), /authorization|secret/i);
  }
  assert.equal(lookups, 1 + rounds);
  assert.equal(left.counts.connections + right.counts.connections, 2 + rounds);
  assert.equal((winner === local ? left : right).counts.handshakes, 1 + rounds);
  assert.equal(left.counts.posts + right.counts.posts, 1 + rounds);
});

Deno.test("QQ TLS：首选地址失败立即切换且 POST 仅一次，交给 HTTP 后断线仍是 unknown", async () => {
  await using primary = await endpoint(local);
  await using backup = await endpoint(other, primary.port);
  assert.equal(await (await send(primary.port, dns(() => [local]), [])).text(), local);
  await primary.close();

  const events: QqConnectionDiagnostic[] = [];
  assert.equal(await (await send(primary.port, resolver, events)).text(), other);
  assert.deepEqual(starts(events), [local, other]);
  const failure = events.find((event) =>
    event.phase === "connect_error" && event.address === local
  )!;
  const fallback = events.find((event) =>
    event.phase === "connect_start" && event.address === other
  )!;
  assert.ok(events.indexOf(failure) < events.indexOf(fallback));
  assert.ok(fallback.elapsedMs - failure.elapsedMs < 150);
  assert.equal(events.filter((event) => event.phase === "request_written").length, 1);
  assert.equal(backup.counts.posts, 1);
  assert.doesNotMatch(JSON.stringify(events), /authorization|secret/i);

  await using recovered = await endpoint(local, primary.port);
  const dropped: QqConnectionDiagnostic[] = [];
  await assert.rejects(send(primary.port, resolver, dropped, undefined, "/drop"), started(true));
  assert.deepEqual(starts(dropped), [other]);
  assert.equal(backup.counts.posts, 2);
  assert.equal(recovered.counts.connections, 0);
  assert.doesNotMatch(JSON.stringify(dropped), /authorization|secret/i);

  await backup.close();
  const mixed: QqConnectionDiagnostic[] = [];
  await assert.rejects(send(primary.port, resolver, mixed, undefined, "/drop"), started(true));
  assert.deepEqual(starts(mixed), [other, local]);
  assert.equal(mixed.filter((event) => event.phase === "connection_selected").length, 1);
  assert.equal(recovered.counts.posts, 1);
  assert.doesNotMatch(JSON.stringify(mixed), /authorization|secret/i);
});

Deno.test("QQ TLS：503 即使带有 Retry-After 也原样返回，双地址合计只收到一次 POST", async () => {
  await using left = await endpoint(local);
  await using right = await endpoint(other, left.port);
  const events: QqConnectionDiagnostic[] = [];
  const response = await send(left.port, resolver, events, undefined, "/busy");
  assert.equal(response.status, 503);
  assert.equal(response.headers.get("retry-after"), "0");
  assert.equal(await response.text(), "secret-upstream-detail");
  assert.equal(left.counts.posts + right.counts.posts, 1);
  assert.equal(events.filter((event) => event.phase === "request_written").length, 1);
  assert.doesNotMatch(JSON.stringify(events), /authorization|secret/i);
});

Deno.test("QQ TLS：DNS 诊断只允许已知错误码，不透出自定义 code、name、消息或 cause", async () => {
  const cases = [
    [{ code: "ENOTFOUND" }, "ENOTFOUND"],
    [{ code: "ESECRET_TOKEN" }, "UNKNOWN"],
    [{ name: "secret-resolver-name" }, "UNKNOWN"],
  ] as const;
  for (const [metadata, expected] of cases) {
    const events: QqConnectionDiagnostic[] = [];
    let lookups = 0;
    const resolve = dns(() => {
      lookups++;
      const cause = new Error(post.headers.authorization);
      throw Object.assign(new Error(`${path}: ${post.body}`, { cause }), metadata);
    });
    await assert.rejects(send(1, resolve, events), started(false));
    assert.equal(lookups, 1);
    assert.deepEqual(starts(events), []);
    assert.ok(!events.some((event) => event.phase === "request_written"));
    const errors = events.filter((event) => event.phase === "request_error");
    assert.deepEqual(errors.map((event) => event.code), [expected]);
    assert.doesNotMatch(JSON.stringify(events), /authorization|secret/i);
  }
});

Deno.test("QQ TLS：204、205、304 及时返回 null body，保留状态和响应头且不重发", async () => {
  for (const status of [204, 205, 304]) {
    await using server = await endpoint(local, 0, (response) => {
      response.writeHead(status, { "x-qq-fixture": "kept" });
      response.end();
    });
    const events: QqConnectionDiagnostic[] = [];
    const response = await bounded(send(server.port, resolver, events));
    assert.equal(response.status, status);
    assert.equal(response.headers.get("x-qq-fixture"), "kept");
    assert.equal(response.body, null);
    assert.equal(await response.text(), "");
    assert.equal(server.counts.posts, 1);
    assert.equal(events.filter((event) => event.phase === "request_written").length, 1);
  }
});

Deno.test("QQ TLS：响应头和部分正文到达后对端断开，读流返回 unknown，不改拨重发", async () => {
  const disconnect = Promise.withResolvers<void>();
  const prefix = new TextEncoder().encode("secret-partial");
  const reply = (response: ServerResponse) => {
    response.writeHead(200, { "content-length": String(prefix.length + 1) });
    response.write(prefix);
    void disconnect.promise.then(() => response.destroy());
  };
  await using left = await endpoint(local, 0, reply);
  await using right = await endpoint(other, left.port, reply);
  const events: QqConnectionDiagnostic[] = [];
  const signal = AbortSignal.timeout(5000);
  try {
    const response = await bounded(send(left.port, resolver, events, signal));
    const reader = response.body!.getReader();
    assert.deepEqual(await bounded(reader.read()), { value: prefix, done: false });
    disconnect.resolve();
    await assert.rejects(bounded(reader.closed), started(true));
    await assert.rejects(reader.read(), started(true));
    assert.equal(signal.aborted, false);
    assert.equal(left.counts.posts + right.counts.posts, 1);
    assert.equal(events.filter((event) => event.phase === "connection_selected").length, 1);
    assert.doesNotMatch(JSON.stringify(events), /authorization|secret/i);
  } finally {
    disconnect.resolve();
  }
});

Deno.test("QQ TLS：响应状态无法构造 Fetch Response 时及时返回 unknown，不重发", async () => {
  await using server = await endpoint(local, 0, (response) => {
    response.writeHead(600);
    response.end("secret-upstream-detail");
  });
  const events: QqConnectionDiagnostic[] = [];
  await assert.rejects(bounded(send(server.port, resolver, events)), started(true));
  assert.ok(events.some((event) => event.phase === "response_headers" && event.httpStatus === 600));
  assert.equal(server.counts.posts, 1);
  assert.doesNotMatch(JSON.stringify(events), /authorization|secret/i);
});

Deno.test("QQ TLS：交给 HTTP 后、写完前取消，仍返回 unknown 而不重发 POST", async () => {
  await using left = await endpoint(local);
  await using right = await endpoint(other, left.port);
  const controller = new AbortController();
  const events: QqConnectionDiagnostic[] = [];
  let writtenAtAbort: boolean | undefined;
  await assert.rejects(
    qqHttps(
      new URL(`https://localhost:${left.port}${path}`),
      { ...post, signal: controller.signal },
      (event) => {
        events.push(event);
        if (event.phase === "connection_selected") {
          queueMicrotask(() => {
            writtenAtAbort = events.some((seen) => seen.phase === "request_written");
            controller.abort(new Error("secret-abort-reason"));
          });
        }
      },
      resolver,
      cert,
    ),
    started(true),
  );
  assert.equal(writtenAtAbort, false);
  assert.equal(events.filter((event) => event.phase === "connection_selected").length, 1);
  assert.ok(left.counts.posts + right.counts.posts <= 1);
  assert.doesNotMatch(JSON.stringify(events), /authorization|secret/i);
});

Deno.test("QQ TLS：缓存只提示当前 origin 的本次 DNS 候选，不继承上一请求的 CA", async () => {
  await using primary = await endpoint(local);
  await using backup = await endpoint(other, primary.port);
  await using differentOrigin = await endpoint(local);
  assert.equal(await (await send(primary.port, dns(() => [local]), [])).text(), local);

  const separate: QqConnectionDiagnostic[] = [];
  assert.equal(await (await send(differentOrigin.port, resolver, separate)).text(), local);
  assert.deepEqual(starts(separate), [other, local]);

  const changed: QqConnectionDiagnostic[] = [];
  assert.equal(await (await send(primary.port, dns(() => [other]), changed)).text(), other);
  assert.deepEqual(starts(changed), [other]);

  const untrusted: QqConnectionDiagnostic[] = [];
  await assert.rejects(
    qqHttps(new URL(`https://localhost:${primary.port}${path}`), post, (event) => {
      untrusted.push(event);
    }, resolver),
    started(false),
  );
  assert.deepEqual(starts(untrusted), [other, local]);
  assert.ok(!untrusted.some((event) => event.phase === "connection_selected"));
  await assert.rejects(
    qqHttps(
      new URL(`https://wrong-name.invalid:${primary.port}${path}`),
      post,
      undefined,
      resolver,
      cert,
    ),
    started(false),
  );
  assert.equal(primary.counts.posts, 1);
  assert.equal(backup.counts.posts, 1);
  assert.doesNotMatch(JSON.stringify(untrusted), /authorization|secret/i);
});

async function stalledHandshake(listener: Deno.TcpListener, onHello?: () => void) {
  using peer = await listener.accept();
  const buffer = new Uint8Array(4096);
  let expired = false;
  const deadline = setTimeout(() => {
    expired = true;
    peer.close();
  }, 2500);
  try {
    assert.ok(await peer.read(buffer));
    onHello?.();
    let remaining: number | null;
    do {
      remaining = await peer.read(buffer);
    } while (remaining !== null);
    assert.equal(expired, false, "client left a stalled TLS socket open");
  } finally {
    clearTimeout(deadline);
  }
}

Deno.test("QQ TLS：主地址握手挂起，取消时不拨备用；未取消则交错并发并关闭慢连接", async () => {
  await using primary = await endpoint(local);
  assert.equal(await (await send(primary.port, dns(() => [local]), [])).text(), local);
  await primary.close();
  using stalled = Deno.listen({ hostname: local, port: primary.port });

  const controller = new AbortController();
  const aborted: QqConnectionDiagnostic[] = [];
  await Promise.all([
    stalledHandshake(stalled, () => controller.abort()),
    assert.rejects(send(primary.port, resolver, aborted, controller.signal), started(false)),
  ]);
  assert.deepEqual(starts(aborted), [local]);

  {
    using second = Deno.listen({ hostname: other, port: primary.port });
    const both = new AbortController();
    const inflight: QqConnectionDiagnostic[] = [];
    await Promise.all([
      stalledHandshake(stalled),
      stalledHandshake(second, () => both.abort()),
      assert.rejects(send(primary.port, resolver, inflight, both.signal), started(false)),
    ]);
    assert.deepEqual(starts(inflight), [local, other]);
    assert.ok(!inflight.some((event) => event.phase === "connection_selected"));
  }

  await using backup = await endpoint(other, primary.port);
  const events: QqConnectionDiagnostic[] = [];
  const [, receipt] = await Promise.all([
    stalledHandshake(stalled),
    send(primary.port, resolver, events).then((response) => response.text()),
  ]);
  assert.equal(receipt, other);
  assert.deepEqual(starts(events), [local, other]);
  const [first, second] = events.filter((event) => event.phase === "connect_start");
  assert.ok(second.elapsedMs - first.elapsedMs >= 100);
  assert.ok(!events.some((event) => event.phase === "connect_timeout"));
  assert.equal(backup.counts.posts, 1);
});

Deno.test("QQ TLS：DNS 尚未完成也可取消，迟到的地址不再建连", async () => {
  const pending = Promise.withResolvers<string[]>(), entered = Promise.withResolvers<void>();
  const controller = new AbortController();
  const events: QqConnectionDiagnostic[] = [];
  const resolve = dns(() => {
    entered.resolve();
    return pending.promise;
  });
  const rejected = assert.rejects(send(1, resolve, events, controller.signal), started(false));
  await entered.promise;
  let expired = false;
  const deadline = setTimeout(() => {
    expired = true;
    pending.resolve([local]);
  }, 1000);
  try {
    controller.abort();
    await rejected;
    assert.equal(expired, false);
  } finally {
    clearTimeout(deadline);
    pending.resolve([local]);
  }
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(starts(events), []);
});

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
    await assert.rejects(() => response.text(), started(true));
    assert.equal(sends, 1);
  } finally {
    await server.shutdown();
  }
});

Deno.test("QQ TLS：过期或被容量上限淘汰的提示恢复冷启动竞速", async () => {
  await using initial = await endpoint(local);
  const single = dns(() => [local]);
  const probe = async () => {
    const events: QqConnectionDiagnostic[] = [];
    assert.equal(await (await send(initial.port, resolver, events)).text(), local);
    return starts(events);
  };
  const now = Performance.prototype.now;
  try {
    Performance.prototype.now = function () {
      return now.call(this) - 6 * 60_000;
    };
    await (await send(initial.port, single, [])).text();
  } finally {
    Performance.prototype.now = now;
  }
  assert.deepEqual(await probe(), [other, local]);
  assert.deepEqual(await probe(), [local]);

  await using origins = new AsyncDisposableStack();
  for (let index = 0; index < 32; index++) {
    const next = origins.use(await endpoint(local));
    await (await send(next.port, single, [])).text();
  }
  assert.deepEqual(await probe(), [other, local]);
});
