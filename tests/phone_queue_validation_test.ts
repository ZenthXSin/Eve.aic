import assert from "node:assert/strict";
import { KernelError } from "../kernel/protocol.ts";
import { PhoneRpcClient, PhoneRpcError } from "../plugins/phone/client.ts";

Deno.test("手机队列：limit50在派发前给出修正办法，修正和next分页可继续", async () => {
  let requests = 0;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    requests++;
    const body = await request.json();
    return Response.json({
      ok: true,
      value: {
        items: [],
        ...(body.args.afterSequence ? {} : { next: { afterSequence: 32, limit: 32 } }),
      },
    });
  });
  const client = new PhoneRpcClient(`http://127.0.0.1:${server.addr.port}/rpc`, "k".repeat(48));
  try {
    await assert.rejects(
      () => client.call("listSendQueue", { limit: 50 }),
      (error) =>
        error instanceof KernelError && error.kind === "rejected" &&
        /范围1\.\.32.*未派发请求/.test(error.message),
    );
    assert.equal(requests, 0);
    const page = await client.call("listSendQueue", { limit: 32 }) as {
      next: { afterSequence: number };
    };
    await client.call("listSendQueue", page.next);
    assert.equal(requests, 2);
    const rejected = new PhoneRpcError("request", undefined, "cancelSend");
    assert.equal(rejected.kind, "rejected");
    assert.match(rejected.message, /cancelSend.*请求参数被拒绝/);
  } finally {
    client.close();
    await server.shutdown();
  }
});
