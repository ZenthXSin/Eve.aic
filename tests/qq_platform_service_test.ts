import assert from "node:assert/strict";
import { QqClient } from "../adapters/qq/client.ts";
import type { QqInboundEvent } from "../adapters/qq/inbound.ts";
import { startQqPhoneAdapter } from "../adapters/qq/service.ts";
import type { PhonePlatformEvents } from "../contracts/phone-platform.ts";
import { PhoneRpcClient } from "../plugins/phone/client.ts";

Deno.test("QQ 服务在同一所有权边界接通 Webhook、事件续页和只读工具，重启不重投", async () => {
  const directory = await Deno.makeTempDir();
  const key = "private_test_phone_key_0000000000000000";
  const requests: string[] = [];
  const platform = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (request) => {
    const path = new URL(request.url).pathname;
    requests.push(`${request.method} ${path}`);
    if (request.method === "POST" && path === "/app/getAppAccessToken") {
      return Response.json({ access_token: "test_only", expires_in: 7200 });
    }
    if (request.method === "GET" && path === "/users/@me") {
      return Response.json({ id: "bot", username: "机器人", bot: true, private_note: "hidden" });
    }
    return new Response(null, { status: 404 });
  });
  const event: QqInboundEvent = {
    op: 0,
    t: "GROUP_ADD_ROBOT",
    id: "event-opaque-1",
    d: { group_openid: "hidden-group", timestamp: 1, secret: "must-not-leave-adapter" },
  };
  let accept!: (event: QqInboundEvent) => Promise<void>;
  const start = () =>
    startQqPhoneAdapter(
      new QqClient({ appId: "fixture", appSecret: "fixture" }, {
        baseUrl: `http://127.0.0.1:${platform.addr.port}`,
      }),
      directory,
      key,
      undefined,
      undefined,
      {
        ingress: {
          webhook: async (signal, dispatch, listening) => {
            accept = dispatch;
            listening();
            if (!signal.aborted) {
              await new Promise<void>((resolve) =>
                signal.addEventListener("abort", () => resolve(), { once: true })
              );
            }
          },
        },
      },
    );
  let adapter: Awaited<ReturnType<typeof start>> | undefined;
  try {
    adapter = await start();
    const rpc = new PhoneRpcClient(adapter.url, key);
    const health = async () =>
      (await rpc.call("health") as { gateway: { status: string } }).gateway.status;
    assert.equal(await health(), "webhook_listening");
    await accept(event);
    await accept({ ...event, s: 80 });
    assert.equal(await health(), "webhook_verified");
    const page = await rpc.call("platformEvents", { limit: 1 }) as unknown as PhonePlatformEvents;
    assert.equal(page.items.length, 1);
    assert.equal(page.items[0].type, event.t);
    assert.deepEqual(Object.keys(page.items[0]).sort(), ["receivedAt", "seq", "type"]);
    assert.equal(page.hasMore, false);
    assert.equal(page.throughSeq, page.items[0].seq);
    const capabilities = await rpc.call("platformCapabilities", { operation: "getMe" }) as {
      items: { chatReadable: boolean }[];
    };
    assert.equal(capabilities.items[0].chatReadable, true);
    assert.deepEqual(await rpc.call("platformRead", { operation: "getMe", params: {} }), {
      status: "ok",
      httpStatus: 200,
      data: { id: "bot", username: "机器人", bot: true },
    });
    const count = requests.length;
    assert.deepEqual(
      await rpc.call("platformRead", { operation: "sendGroupMessage", params: {} }),
      { status: "failed", reason: "not_allowed" },
    );
    assert.equal(requests.length, count);
    await adapter.close();
    adapter = undefined;
    adapter = await start();
    await accept(event);
    const restored = new PhoneRpcClient(adapter.url, key);
    assert.deepEqual(await restored.call("platformEvents", {}), page);
    assert.deepEqual(await restored.call("platformEvents", { afterSeq: page.throughSeq }), {
      items: [],
      throughSeq: page.throughSeq,
      hasMore: false,
    });
    assert.ok(
      requests.every((path) => path === "POST /app/getAppAccessToken" || path === "GET /users/@me"),
    );
  } finally {
    try {
      await adapter?.close();
    } finally {
      await platform.shutdown();
      await Deno.remove(directory, { recursive: true });
    }
  }
});
