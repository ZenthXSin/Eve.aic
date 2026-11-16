import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient, QqRequestError, type QqSendText } from "../adapters/qq/client.ts";

const credentials = { appId: "fixture-app", appSecret: "fixture-secret" };
const task: QqSendText = {
  destination: { kind: "group", openId: "missing-group" },
  text: "message",
};
const platformMessage = "资源不存在 / 群不存在\n原始平台说明";
const cases = [
  {
    name: "code and err_code differ",
    body: { code: 11255, err_code: 40011028, trace_id: "body-trace", message: platformMessage },
    category: "not_found",
    code: 11255,
    errCode: 40011028,
    traceId: "body-trace",
  },
  {
    name: "only code",
    body: { code: 11255, message: platformMessage },
    category: "not_found",
    code: 11255,
    errCode: undefined,
    traceId: "header-trace",
  },
  {
    name: "only err_code",
    body: { err_code: 40011028, msg: platformMessage },
    category: "not_found",
    code: undefined,
    errCode: 40011028,
    traceId: "header-trace",
  },
  {
    name: "no error codes",
    body: { message: platformMessage },
    category: "platform",
    code: undefined,
    errCode: undefined,
    traceId: "header-trace",
  },
  {
    name: "non-JSON body",
    body: "<html>资源不存在 / 群不存在</html>\n",
    category: "platform",
    code: undefined,
    errCode: undefined,
    traceId: "header-trace",
  },
] as const;

for (const fixture of cases) {
  Deno.test("QQ HTTP 400 error parsing: " + fixture.name, async () => {
    let auths = 0, sends = 0;
    const rejected = () =>
      new Response(
        typeof fixture.body === "string" ? fixture.body : JSON.stringify(fixture.body),
        { status: 400, headers: { "X-Tps-trace-ID": "header-trace" } },
      );
    const local = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (r) => {
      if (new URL(r.url).pathname === "/app/getAppAccessToken") {
        if (auths++ === 0) return rejected();
        return Response.json({ access_token: "fixture-token", expires_in: 7200 });
      }
      sends++;
      return rejected();
    });
    const client = new QqClient(credentials, {
      baseUrl: "http://127.0.0.1:" + local.addr.port,
      sendScope: "all",
    });
    const expectedMessage = typeof fixture.body === "string" ? fixture.body : platformMessage;
    try {
      await assert.rejects(client.authenticate(), (error) => {
        assert.ok(error instanceof QqRequestError);
        assert.notEqual(error.info.category, "invalid_response");
        assert.equal(error.info.category, fixture.category);
        assert.equal(error.info.httpStatus, 400);
        assert.equal(error.info.code, fixture.code);
        assert.equal(error.info.errCode, fixture.errCode);
        assert.equal(error.info.traceId, fixture.traceId);
        assert.equal(error.info.message, expectedMessage);
        assert.ok(!error.message.includes(expectedMessage)); // Safe log summary.
        return true;
      });
      const result = await client.sendText(task);
      assert.ok(result.status === "failed" && result.attempted);
      assert.notEqual(result.error.category, "invalid_response");
      assert.equal(result.error.category, fixture.category);
      assert.equal(result.error.httpStatus, 400);
      assert.equal(result.error.code, fixture.code);
      assert.equal(result.error.errCode, fixture.errCode);
      assert.equal(result.error.traceId, fixture.traceId);
      assert.equal(result.error.message, expectedMessage);
      assert.equal(result.error.failureStage, "message");
      assert.equal(sends, 1);
    } finally {
      client.close();
      await local.shutdown();
    }
  });
}

Deno.test("QQ error parsing: err_code wins classification; code is retained, not compared or overwritten", async () => {
  const bodies = [
    { code: 100016, err_code: 40011028 },
    { code: 11255, err_code: 22009 },
    { code: 11255, err_code: 100016 },
    { code: 11255, err_code: "malformed" },
  ];
  let index = 0;
  const local = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture-token", expires_in: 7200 });
    }
    return Response.json(bodies[index++], { status: 400 });
  });
  const client = new QqClient(credentials, {
    baseUrl: "http://127.0.0.1:" + local.addr.port,
    sendScope: "all",
  });
  try {
    for (
      const [i, category] of ["not_found", "rate_limit", "authentication", "not_found"].entries()
    ) {
      const result = await client.sendText(task);
      assert.equal(result.status, "failed");
      assert.equal(result.error.category, category);
      assert.equal(result.error.code, bodies[i].code);
      assert.equal(
        result.error.errCode,
        typeof bodies[i].err_code === "number" ? bodies[i].err_code : undefined,
      );
      assert.equal(result.error.httpStatus, 400);
    }
  } finally {
    client.close();
    await local.shutdown();
  }
});

Deno.test("QQ error parsing: malformed success code fails closed without dropping HTTP metadata", async () => {
  const local = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture-token", expires_in: 7200 });
    }
    return Response.json({
      code: 11255,
      err_code: "bad",
      trace_id: "trace",
      message: "detail",
      id: "untrusted",
    });
  });
  const client = new QqClient(credentials, {
    baseUrl: "http://127.0.0.1:" + local.addr.port,
    sendScope: "all",
  });
  try {
    const result = await client.sendText(task);
    assert.equal(result.status, "unknown");
    assert.equal(result.error.category, "invalid_response");
    assert.equal(result.error.httpStatus, 200);
    assert.equal(result.error.code, 11255);
    assert.equal(result.error.traceId, "trace");
    assert.equal(result.error.message, "detail");
  } finally {
    client.close();
    await local.shutdown();
  }
});

Deno.test("QQ not_found receipt: both codes and original message survive action ledger restart without resend", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-error-code-" });
  let sends = 0;
  const local = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture-token", expires_in: 7200 });
    }
    sends++;
    return Response.json(cases[0].body, { status: 400 });
  });
  const client = () =>
    new QqClient(credentials, {
      baseUrl: "http://127.0.0.1:" + local.addr.port,
      sendScope: "all",
    });
  let actions = new QqSendActions(client(), directory, "all");
  try {
    await actions.initialize();
    const receipt = await actions.send("missing-group", task);
    assert.ok(receipt.status === "finished" && receipt.result.status === "failed");
    assert.equal(receipt.result.error.category, "not_found");
    assert.equal(receipt.result.error.httpStatus, 400);
    assert.equal(receipt.result.error.code, 11255);
    assert.equal(receipt.result.error.errCode, 40011028);
    assert.equal(receipt.result.error.traceId, "body-trace");
    assert.equal(receipt.result.error.message, platformMessage);
    await actions.close();
    actions = new QqSendActions(client(), directory, "all");
    await actions.initialize();
    assert.deepEqual(actions.status("missing-group"), receipt);
    assert.deepEqual(await actions.send("missing-group", task), receipt);
    assert.equal(sends, 1);
  } finally {
    await actions.close();
    await local.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
