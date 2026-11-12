import assert from "node:assert/strict";
import { QqActionError, type QqActionSendText, QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient, type QqSendResult, type QqSendText } from "../adapters/qq/client.ts";

class Client extends QqClient {
  requests: QqSendText[] = [];
  constructor(
    readonly response: (n: number) => QqSendResult = () => ({ status: "sent", messageId: "sent" }),
  ) {
    super({ appId: "fixture", appSecret: "fixture" });
  }
  override sendText(request: QqSendText): Promise<QqSendResult> {
    this.requests.push(structuredClone(request));
    return Promise.resolve(this.response(this.requests.length));
  }
}
const request: QqActionSendText = {
  destination: { kind: "user", openId: "fixture-user" },
  text: "reply",
  reply: { messageId: "source" },
};
const conflict = (e: unknown) => e instanceof QqActionError && e.category === "conflict";
function sequence(request: QqSendText) {
  assert.ok(request.reply && "sequence" in request.reply);
  return request.reply.sequence;
}

Deno.test("QQ回复序号：并发新动作自动分配，同动作重入/重启固定原序号，显式碰撞不落盘", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-sequences-" });
  const client = new Client();
  let actions = new QqSendActions(client, directory);
  try {
    await actions.initialize();
    await Promise.all([
      actions.send("first", request),
      actions.send("second", { ...request, text: "second" }),
      actions.send("first", request),
    ]);
    assert.deepEqual(client.requests.map(sequence), [1, 2]);
    await assert.rejects(
      actions.send("collision", { ...request, reply: { messageId: "source", sequence: 1 } }),
      conflict,
    );
    await assert.rejects(Deno.stat(`${directory}/records/collision.json`), Deno.errors.NotFound);
    await assert.rejects(actions.send("first", { ...request, text: "changed" }), conflict);
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    await actions.send("first", request);
    assert.equal(client.requests.length, 2);
    await actions.send("third", { ...request, text: "third" });
    assert.deepEqual(client.requests.map(sequence), [1, 2, 3]);
    await actions.send("different-source", { ...request, reply: { messageId: "other" } });
    await actions.send("different-user", {
      ...request,
      destination: { kind: "user", openId: "other" },
    });
    assert.deepEqual(client.requests.slice(-2).map(sequence), [1, 1]);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ回复序号：并发显式抢同序号只接纳一个，未知取消后仍保留占用，重启不重发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-sequence-conflict-" });
  const client = new Client(() => ({
    status: "unknown",
    attempted: true,
    error: { category: "duplicate" },
  }));
  let actions = new QqSendActions(client, directory);
  const explicit = { ...request, reply: { messageId: "source", sequence: 7 } };
  try {
    await actions.initialize();
    const results = await Promise.allSettled([
      actions.send("first", explicit),
      actions.send("second", explicit),
    ]);
    assert.equal(results.filter((r) => r.status === "fulfilled").length, 1);
    const failed = results.find((r) => r.status === "rejected");
    assert.ok(failed?.status === "rejected" && conflict(failed.reason));
    const head = (await actions.listSendQueue()).items[0];
    const record = await Deno.readTextFile(`${directory}/records/${head.actionId}.json`);
    const attempts = await Deno.readTextFile(
      `${directory}/attempts/${head.actionId}/1.result.json`,
    );
    const args = {
      actionId: head.actionId,
      expectedFingerprint: head.fingerprint,
      expectedReceiptDigest: head.receiptDigest!,
    };
    const cancelled = await actions.cancelSend(args);
    assert.ok(
      cancelled.status === "cancelled" && cancelled.resolution.kind === "unknown_retained" &&
        cancelled.originalResult?.status === "unknown",
    );
    assert.equal(await Deno.readTextFile(`${directory}/records/${head.actionId}.json`), record);
    assert.equal(
      await Deno.readTextFile(`${directory}/attempts/${head.actionId}/1.result.json`),
      attempts,
    );
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    assert.deepEqual(await actions.send(head.actionId, explicit), cancelled);
    assert.deepEqual(await actions.cancelSend(args), cancelled);
    assert.equal(client.requests.length, 1);
    await assert.rejects(actions.send("collision-after-cancel", explicit), conflict);
    await actions.send("new", request);
    assert.equal(sequence(client.requests[1]), 8);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ回复序号：自动序号的超时持续重试保持原消息和序号", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-sequence-retry-" });
  const client = new Client((n) =>
    n < 3
      ? {
        status: "unknown",
        attempted: true,
        error: { category: "transport", transportReason: "timeout" },
      }
      : { status: "sent", messageId: "confirmed" }
  );
  const actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0] });
  try {
    await actions.initialize();
    const result = await actions.send("same", request);
    assert.ok(result.status === "finished" && result.result.status === "sent");
    assert.equal(client.requests.length, 3);
    assert.deepEqual(client.requests[0], client.requests[1]);
    assert.deepEqual(client.requests[1], client.requests[2]);
    assert.equal(sequence(client.requests[0]), 1);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ未知等待取消：许可中断零尝试队首保留原件，取消后按序放行且重启不重发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-interrupted-cancel-" });
  const client = new Client();
  let actions = new QqSendActions(client, directory);
  try {
    await actions.initialize();
    assert.equal(
      (await actions.send("interrupted", request, {
        beforeAttempt: () => Promise.reject(new Error("paused")),
      })).status,
      "unknown",
    );
    assert.equal(client.requests.length, 0);
    await actions.send("follower", { ...request, text: "follower" });
    const original = await Deno.readTextFile(`${directory}/records/interrupted.json`);
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    const head = (await actions.listSendQueue({ failedHeadsOnly: true })).items[0];
    assert.equal(head.actionId, "interrupted");
    assert.equal(head.canCancel, true);
    const cancelled = await actions.cancelSend({
      actionId: head.actionId,
      expectedFingerprint: head.fingerprint,
      expectedReceiptDigest: head.receiptDigest!,
    }, { actor: "admin", failedHeadOnly: true });
    assert.ok(cancelled.status === "cancelled" && cancelled.resolution.kind === "unknown_retained");
    assert.equal(await Deno.readTextFile(`${directory}/records/interrupted.json`), original);
    await actions.send("follower", { ...request, text: "follower" });
    assert.equal(client.requests.length, 1);
    assert.equal(client.requests[0].text, "follower");
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    assert.deepEqual(await actions.send("interrupted", request), cancelled);
    assert.equal(client.requests.length, 1);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});
