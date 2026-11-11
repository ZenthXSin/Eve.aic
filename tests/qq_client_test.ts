import assert from "node:assert/strict";
import { QqClient, QqRequestError, type QqSendText } from "../adapters/qq/client.ts";

const credentials = { appId: "fixture-app", appSecret: "fixture-secret" };
const token = () => Response.json({ access_token: "fixture-token", expires_in: "7200" });
const message: QqSendText = {
  destination: { kind: "user", openId: "fixture-user" },
  text: "第一行\n第二行，不改写。",
  reply: { messageId: "inbound-1", sequence: 2 },
};

function server(handler: (request: Request) => Response | Promise<Response>) {
  const instance = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, handler);
  return { url: `http://127.0.0.1:${instance.addr.port}`, close: () => instance.shutdown() };
}

Deno.test("HTTP 200 的认证业务错误阻止发送，不泄露秘密", async () => {
  let sends = 0;
  const local = await server((r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ code: 100016, message: credentials.appSecret });
    }
    sends++;
    return Response.json({ id: "unexpected" });
  });
  try {
    const client = new QqClient(credentials, { baseUrl: local.url });
    const result = await client.sendText(message);
    assert.equal(result.status, "failed");
    assert.ok(result.status === "failed" && !result.attempted);
    assert.equal(result.status === "failed" && result.error.code, 100016);
    assert.equal(sends, 0);
    assert.ok(!JSON.stringify(result).includes(credentials.appSecret));
    await assert.rejects(
      client.authenticate(),
      (e) => e instanceof QqRequestError && !e.message.includes(credentials.appSecret),
    );
  } finally {
    await local.close();
  }
});

Deno.test("并发读取复用认证，临近过期只刷新一次", async () => {
  let now = 1_000;
  let auths = 0;
  const local = await server(async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      auths++;
      assert.deepEqual(await r.json(), {
        appId: credentials.appId,
        clientSecret: credentials.appSecret,
      });
      return token();
    }
    assert.equal(r.headers.get("Authorization"), "QQBot fixture-token");
    return Response.json({ id: "fixture-bot", username: "测试", bot: true });
  });
  try {
    const client = new QqClient(credentials, { baseUrl: local.url, now: () => now });
    const ids = await Promise.all([client.botIdentity(), client.botIdentity()]);
    assert.equal(ids[0].id, "fixture-bot");
    assert.equal(auths, 1);
    now += 7_139_000;
    await client.botIdentity();
    assert.equal(auths, 1);
    now += 1_000;
    await Promise.all([client.botIdentity(), client.botIdentity()]);
    assert.equal(auths, 2);
  } finally {
    await local.close();
  }
});

for (
  const response of [
    { code: "0", access_token: "fixture-token", expires_in: 7200 },
    { access_token: "fixture-token" },
    { access_token: "", expires_in: 7200 },
    { access_token: "fixture-token", expires_in: -1 },
  ]
) {
  Deno.test(`无效凭证响应不执行发送 ${JSON.stringify(response)}`, async () => {
    let sends = 0;
    const local = await server((r) => {
      if (new URL(r.url).pathname === "/app/getAppAccessToken") return Response.json(response);
      sends++;
      return Response.json({ id: "unexpected" });
    });
    try {
      const result = await new QqClient(credentials, { baseUrl: local.url }).sendText(message);
      assert.ok(
        result.status === "failed" && !result.attempted &&
          result.error.category === "invalid_response",
      );
      assert.equal(sends, 0);
    } finally {
      await local.close();
    }
  });
}

Deno.test("等待凭证期间调用方修改参数，不改变已开始的动作", async () => {
  let notifyReady!: () => void;
  let releaseAuth!: () => void;
  const ready = new Promise<void>((resolve) => {
    notifyReady = resolve;
  });
  const blocked = new Promise<void>((resolve) => {
    releaseAuth = resolve;
  });
  let path = "";
  let body: unknown;
  const local = await server(async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      notifyReady();
      await blocked;
      return token();
    }
    path = new URL(r.url).pathname;
    body = await r.json();
    return Response.json({ id: "sent-1" });
  });
  try {
    const input = structuredClone(message);
    const sending = new QqClient(credentials, { baseUrl: local.url }).sendText(input);
    await ready;
    input.destination.openId = "changed-user";
    input.text = "changed-text";
    releaseAuth();
    assert.equal((await sending).status, "sent");
    assert.equal(path, "/v2/users/fixture-user/messages");
    assert.deepEqual(body, { content: message.text, msg_type: 0, msg_id: "inbound-1", msg_seq: 2 });
  } finally {
    releaseAuth();
    await local.close();
  }
});

Deno.test("发送保留全文、引用与固定序号，只凭真实编号返回 sent", async () => {
  const bodies: unknown[] = [];
  const paths: string[] = [];
  const local = await server(async (r) => {
    const path = new URL(r.url).pathname;
    if (path === "/app/getAppAccessToken") return token();
    paths.push(path);
    assert.equal(r.headers.get("Authorization"), "QQBot fixture-token");
    bodies.push(await r.json());
    return Response.json({
      id: `sent-${paths.length}`,
      timestamp: "2026-11-11T21:00:00+08:00",
      ext_info: { ref_idx: "reference-1" },
    }, { headers: { "X-Tps-trace-ID": "trace-1" } });
  });
  try {
    const client = new QqClient(credentials, { baseUrl: local.url });
    const result = await client.sendText({ ...message, referenceIndex: "ref-inbound" });
    assert.deepEqual(result, {
      status: "sent",
      messageId: "sent-1",
      timestamp: "2026-11-11T21:00:00+08:00",
      referenceIndex: "reference-1",
      traceId: "trace-1",
    });
    await client.sendText({
      destination: { kind: "group", openId: "group/id" },
      text: "群里的消息",
      reply: { eventId: "event-1" },
    });
    assert.deepEqual(paths, ["/v2/users/fixture-user/messages", "/v2/groups/group%2Fid/messages"]);
    assert.deepEqual(bodies, [
      {
        content: message.text,
        msg_type: 0,
        msg_id: "inbound-1",
        msg_seq: 2,
        message_reference: { message_id: "ref-inbound" },
      },
      { content: "群里的消息", msg_type: 0, event_id: "event-1" },
    ]);
  } finally {
    await local.close();
  }
});

for (
  const [label, status, body, expected, category] of [
    ["HTTP200 没有消息编号", 200, {}, "unknown", "invalid_response"],
    ["HTTP200 业务拒绝", 200, { err_code: 40034105 }, "failed", "platform"],
    ["异步审核", 202, { err_code: 304024 }, "unknown", "pending"],
    ["服务端去重", 200, { err_code: 40054005 }, "unknown", "duplicate"],
    ["限频", 429, { err_code: 40034100 }, "failed", "rate_limit"],
    ["服务端异常", 500, { id: "uncertain" }, "unknown", "platform"],
    ["错误字段类型不对", 200, { err_code: "0", id: "invalid" }, "unknown", "invalid_response"],
  ] as const
) {
  Deno.test(`${label}：区分状态且只发送一次`, async () => {
    let sends = 0;
    const local = await server(async (r) => {
      if (new URL(r.url).pathname === "/app/getAppAccessToken") return token();
      sends++;
      assert.equal((await r.json()).msg_seq, 2);
      return Response.json(body, { status });
    });
    try {
      const result = await new QqClient(credentials, { baseUrl: local.url }).sendText(message);
      assert.equal(result.status, expected);
      assert.equal(result.error.category, category);
      assert.equal(sends, 1);
    } finally {
      await local.close();
    }
  });
}

Deno.test("回复断线和超时保留未知，不重发也不改主动消息", async () => {
  let sends = 0;
  const local = await server(async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") return token();
    sends++;
    const body = await r.json();
    assert.equal(body.msg_id, "inbound-1");
    assert.equal(body.is_wakeup, undefined);
    await new Promise((resolve) => setTimeout(resolve, 100));
    return Response.json({ id: "late-message" });
  });
  try {
    const result = await new QqClient(credentials, { baseUrl: local.url, timeoutMs: 30 }).sendText(
      message,
    );
    assert.ok(result.status === "unknown" && result.error.category === "transport");
    assert.equal(sends, 1);
  } finally {
    await local.close();
  }
});

Deno.test("重定向不携带凭据跟随发送", async () => {
  let followed = 0;
  let sends = 0;
  const target = await server(() => {
    followed++;
    return Response.json({ id: "wrong-server" });
  });
  const local = await server((r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") return token();
    sends++;
    return new Response("", { status: 302, headers: { Location: target.url } });
  });
  try {
    const result = await new QqClient(credentials, { baseUrl: local.url }).sendText(message);
    assert.equal(result.status, "unknown");
    assert.equal(sends, 1);
    assert.equal(followed, 0);
  } finally {
    await local.close();
    await target.close();
  }
});

Deno.test("超大回包和无效 JSON 保留未知", async () => {
  let sends = 0;
  const local = await server((r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") return token();
    sends++;
    return new Response(sends === 1 ? "{invalid" : JSON.stringify({ id: "x".repeat(300) }));
  });
  try {
    const client = new QqClient(credentials, { baseUrl: local.url, maxResponseBytes: 128 });
    for (let i = 0; i < 2; i++) {
      const result = await client.sendText(message);
      assert.ok(result.status === "unknown" && result.error.category === "invalid_response");
    }
    assert.equal(sends, 2);
  } finally {
    await local.close();
  }
});

Deno.test("请求无效在网络前拒绝，序号和回复身份不得自动补写", async () => {
  let calls = 0;
  const local = await server(() => {
    calls++;
    return token();
  });
  try {
    const client = new QqClient(credentials, { baseUrl: local.url });
    for (
      const request of [
        { ...message, reply: { messageId: "inbound-1", sequence: 0 } },
        { ...message, reply: { messageId: "inbound-1" } },
        { ...message, reply: { messageId: "inbound-1", sequence: 1, eventId: "event" } },
        { ...message, wakeup: true },
        { ...message, destination: { kind: "group", openId: "g" }, reply: undefined, wakeup: true },
        { ...message, text: "" },
      ]
    ) {
      await assert.rejects(
        client.sendText(request as QqSendText),
        (e) => e instanceof QqRequestError && e.info.category === "request",
      );
    }
    assert.equal(calls, 0);
  } finally {
    await local.close();
  }
});
