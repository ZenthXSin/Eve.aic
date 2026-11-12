import assert from "node:assert/strict";
import { QqClient } from "../adapters/qq/client.ts";
import { QqGateway } from "../adapters/qq/gateway.ts";
import { QQ_OFFICIAL_PROTOCOL } from "../adapters/qq/official/protocol.ts";

const credentials = { appId: "official-fixture", appSecret: "not-a-real-secret" };
function serve(handler: (r: Request) => Response | Promise<Response>) {
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, handler);
  return { url: `http://127.0.0.1:${server.addr.port}`, close: () => server.shutdown() };
}
const token = () => Response.json({ access_token: "private-token", expires_in: 7200 });
const request = {
  destination: { kind: "group" as const, openId: "group" },
  text: "same text",
  reply: { messageId: "source", sequence: 3 },
};

Deno.test("官方1.0.5：认证独立源、文件名及上传独立期限，主动媒体不携带旧被动字段", async () => {
  let tokens = 0, uploads = 0, messages = 0;
  const auth = serve(async (r) => {
    tokens++;
    assert.equal(new URL(r.url).pathname, "/app/getAppAccessToken");
    assert.equal(r.headers.has("authorization"), false);
    assert.deepEqual(await r.json(), {
      appId: credentials.appId,
      clientSecret: credentials.appSecret,
    });
    return token();
  });
  const api = serve(async (r) => {
    assert.equal(r.headers.get("authorization"), "QQBot private-token");
    const path = new URL(r.url).pathname, body = await r.json();
    if (path.endsWith("/files")) {
      uploads++;
      assert.deepEqual(body, {
        file_type: 4,
        file_data: "AQID",
        srv_send_msg: false,
        file_name: "source_.zip",
      });
      await new Promise((resolve) => setTimeout(resolve, 75));
      return Response.json({ file_info: "uploaded-file", ttl: 300 });
    }
    assert.equal(path, "/v2/groups/group/messages");
    messages++;
    assert.deepEqual(body, {
      content: "same text",
      msg_type: 7,
      media: { file_info: "uploaded-file" },
    });
    return Response.json({ id: "delivered" });
  });
  const client = new QqClient(credentials, {
    baseUrl: api.url,
    tokenBaseUrl: auth.url,
    timeoutMs: 50,
    tokenTimeoutMs: 500,
    uploadTimeoutMs: 1000,
    sendScope: "all",
    attachmentReader: () => Promise.resolve(new Uint8Array([1, 2, 3])),
  });
  try {
    assert.equal(
      (await client.sendText({
        ...request,
        attachment: { path: "/private/source?.zip", kind: "file" },
      })).status,
      "sent",
    );
    assert.deepEqual([tokens, uploads, messages], [1, 1, 1]);
    assert.equal(QQ_OFFICIAL_PROTOCOL.version, "1.0.5");
  } finally {
    client.close();
    await auth.close();
    await api.close();
  }
});

Deno.test("官方超频22009和HTML429保留结构化限流，未知40034031仍保留失败", async () => {
  for (
    const [status, body, expected] of [[400, { code: 22009 }, "rate_limit"], [
      429,
      "<html>busy</html>",
      "rate_limit",
    ], [400, { code: 40034031 }, "platform"]] as const
  ) {
    const local = serve((r) =>
      new URL(r.url).pathname === "/app/getAppAccessToken"
        ? token()
        : new Response(typeof body === "string" ? body : JSON.stringify(body), {
          status,
          headers: { "Retry-After": "120" },
        })
    );
    const client = new QqClient(credentials, { baseUrl: local.url, sendScope: "all" });
    try {
      const result = await client.sendText(request);
      assert.equal(result.status, "failed");
      assert.equal(result.error.category, expected);
      assert.equal(result.error.retryAfterMs, 120_000);
      assert.equal(result.error.failureStage, "message");
    } finally {
      client.close();
      await local.close();
    }
  }
});

Deno.test("明确鉴权拒绝使下一次尝试刷新凭据，同一主动请求正文不变", async () => {
  let auths = 0;
  const bodies: unknown[] = [];
  const local = serve(async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: `token-${++auths}`, expires_in: 7200 });
    }
    bodies.push(await r.json());
    return bodies.length === 1
      ? Response.json({ code: 100016 }, { status: 401 })
      : Response.json({ id: "delivered" });
  });
  const client = new QqClient(credentials, { baseUrl: local.url, sendScope: "all" });
  try {
    assert.equal((await client.sendText(request)).status, "failed");
    assert.equal((await client.sendText(request)).status, "sent");
    assert.equal(auths, 2);
    assert.deepEqual(bodies[0], bodies[1]);
  } finally {
    client.close();
    await local.close();
  }
});

for (const closeCode of [4004, 4008]) {
  Deno.test(`官方网关${closeCode}：${closeCode === 4004 ? "刷新凭据恢复" : "限流等待后恢复"}，不杀死适配器`, async () => {
    let auths = 0, sockets = 0, firstClosed = 0, reconnected = 0;
    const controller = new AbortController();
    const opened: WebSocket[] = [];
    const local = serve((r) => {
      const path = new URL(r.url).pathname;
      if (path === "/app/getAppAccessToken") {
        return Response.json({ access_token: `gateway-token-${++auths}`, expires_in: 7200 });
      }
      if (path === "/gateway") {
        return Response.json({ url: local.url.replace("http:", "ws:") + "/ws" });
      }
      const { socket, response } = Deno.upgradeWebSocket(r);
      const number = ++sockets;
      opened.push(socket);
      socket.onopen = () => socket.send(JSON.stringify({ op: 10, d: { heartbeat_interval: 500 } }));
      socket.onmessage = (event) => {
        const data = JSON.parse(event.data);
        if (data.op === 1) socket.send(JSON.stringify({ op: 11 }));
        else if (number === 1) {
          firstClosed = performance.now();
          socket.close(closeCode);
        } else {
          reconnected = performance.now();
          assert.equal(data.d.token, `QQBot gateway-token-${closeCode === 4004 ? 2 : 1}`);
          socket.send(
            JSON.stringify({ op: 0, s: 1, t: "READY", d: { session_id: "new-session" } }),
          );
        }
      };
      return response;
    });
    const client = new QqClient(credentials, { baseUrl: local.url });
    const gateway = new QqGateway(client, {
      intents: 1 << 25,
      checkpoint: { load: () => Promise.resolve(undefined), save: () => Promise.resolve() },
      onDispatch: () => Promise.resolve(),
      onStatus: (event) => {
        if (event.status === "ready") controller.abort();
      },
      initialBackoffMs: 1,
      maxBackoffMs: 5,
      rateLimitBackoffMs: 40,
      connectTimeoutMs: 500,
    });
    try {
      await gateway.run(controller.signal);
      assert.equal(sockets, 2);
      assert.equal(auths, closeCode === 4004 ? 2 : 1);
      if (closeCode === 4008) assert.ok(reconnected - firstClosed >= 35);
    } finally {
      controller.abort();
      client.close();
      for (const s of opened) s.close();
      await local.close();
    }
  });
}

Deno.test("网关连续快速READY后断连仍累计退避，第三次采用平台冷却", async () => {
  const controller = new AbortController();
  const sockets: WebSocket[] = [];
  let thirdClosed = 0, fourthOpened = 0;
  const local = serve((r) => {
    const path = new URL(r.url).pathname;
    if (path === "/app/getAppAccessToken") return token();
    if (path === "/gateway") {
      return Response.json({ url: local.url.replace("http:", "ws:") + "/ws" });
    }
    const { socket, response } = Deno.upgradeWebSocket(r);
    const number = sockets.push(socket);
    socket.onopen = () => socket.send(JSON.stringify({ op: 10, d: { heartbeat_interval: 500 } }));
    socket.onmessage = (event) => {
      const data = JSON.parse(event.data);
      if (data.op === 1) socket.send(JSON.stringify({ op: 11 }));
      else {
        socket.send(JSON.stringify({ op: 0, s: number, t: "READY", d: { session_id: "session" } }));
        if (number < 4) {
          if (number === 3) thirdClosed = performance.now();
          socket.close(4000);
        } else fourthOpened = performance.now();
      }
    };
    return response;
  });
  const client = new QqClient(credentials, { baseUrl: local.url });
  const gateway = new QqGateway(client, {
    intents: 1 << 25,
    checkpoint: { load: () => Promise.resolve(undefined), save: () => Promise.resolve() },
    onDispatch: () => Promise.resolve(),
    onStatus: (event) => {
      if (event.status === "ready" && sockets.length === 4) controller.abort();
    },
    initialBackoffMs: 2,
    maxBackoffMs: 8,
    rateLimitBackoffMs: 40,
    stableConnectionMs: 1000,
    connectTimeoutMs: 500,
  });
  try {
    await gateway.run(controller.signal);
    assert.equal(sockets.length, 4);
    assert.ok(fourthOpened - thirdClosed >= 35);
  } finally {
    controller.abort();
    client.close();
    for (const socket of sockets) socket.close();
    await local.close();
  }
});
