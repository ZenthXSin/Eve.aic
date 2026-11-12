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
function canonical(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${
    Object.keys(record).filter((k) => record[k] !== undefined).sort().map((k) =>
      `${JSON.stringify(k)}:${canonical(record[k])}`
    ).join(",")
  }}`;
}
const timeout: QqSendResult = {
  status: "unknown",
  attempted: true,
  error: { category: "transport", transportReason: "timeout" },
};
async function legacyFixture(
  directory: string,
  interrupted = false,
  mixed?: "unknown_rate_limit" | "interrupted_auth" | "aborted_auth" | "unknown_sent",
) {
  const old: QqSendText = { ...request, reply: { messageId: "source", sequence: 7 } };
  const hash = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical(old)));
  const fingerprint = Array.from(new Uint8Array(hash), (b) => b.toString(16).padStart(2, "0")).join(
    "",
  );
  const at = new Date().toISOString(), actionId = "legacy";
  await Deno.mkdir(`${directory}/records`, { recursive: true });
  await Deno.mkdir(`${directory}/attempts/legacy`, { recursive: true });
  const record = {
    schema: 1,
    actionId,
    fingerprint,
    request: old,
    startedAt: at,
    policy: { kind: "until_sent_v1", sequence: 1 },
    ...(interrupted || mixed ? { status: "started" } : {
      status: "finished",
      finishedAt: at,
      result: timeout,
      attemptCount: 2,
    }),
  };
  await Deno.writeTextFile(`${directory}/records/legacy.json`, JSON.stringify(record));
  for (let attempt = 1; attempt <= 2; attempt++) {
    const common = { schema: 1, actionId, fingerprint, attempt };
    await Deno.writeTextFile(
      `${directory}/attempts/legacy/${attempt}.started.json`,
      JSON.stringify({ ...common, startedAt: at }),
    );
    if (mixed === "interrupted_auth" && attempt === 1) {
      await Deno.writeTextFile(
        `${directory}/attempts/legacy/${attempt}.interrupted.json`,
        JSON.stringify({ ...common, interruptedAt: at }),
      );
    } else if (!interrupted || attempt === 1 || mixed) {
      const result: QqSendResult = mixed && attempt === 2
        ? mixed === "unknown_sent" ? { status: "sent", messageId: "legacy-receipt" } : {
          status: "failed",
          attempted: true,
          error: { category: mixed === "unknown_rate_limit" ? "rate_limit" : "authentication" },
        }
        : mixed === "aborted_auth"
        ? {
          status: "unknown",
          attempted: true,
          error: {
            category: "transport",
            transportReason: "aborted",
          },
        }
        : timeout;
      await Deno.writeTextFile(
        `${directory}/attempts/legacy/${attempt}.result.json`,
        JSON.stringify({ ...common, finishedAt: at, result }),
      );
    }
  }
  return old;
}

Deno.test("QQ主动准入：忽略有效旧回复参数，不再分配被动序号，同动作幂等及重启不重发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-active-admission-" });
  const client = new Client();
  let actions = new QqSendActions(client, directory);
  try {
    await actions.initialize();
    await Promise.all([
      actions.send("first", request),
      actions.send("second", { ...request, text: "second" }),
      actions.send("first", { ...request, reply: undefined }),
    ]);
    assert.equal(client.requests.length, 2);
    assert.ok(client.requests.every((r) => r.reply === undefined));
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${directory}/records/first.json`)).request.reply,
      undefined,
    );
    await actions.send("same-old-sequence", {
      ...request,
      reply: { messageId: "source", sequence: 1 },
    });
    await assert.rejects(actions.send("first", { ...request, text: "changed" }), conflict);
    await assert.rejects(
      actions.send("invalid", {
        ...request,
        reply: { messageId: "", sequence: -1 },
      }),
      (e) => e instanceof QqActionError && e.category === "request",
    );
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    await actions.send("first", { ...request, reply: undefined });
    assert.equal(client.requests.length, 3);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ主动未知：尝试后超时不依赖旧回复键重试，保留队首可取消，随后才发送下一条", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-active-unknown-" });
  const client = new Client((n) => n === 1 ? timeout : { status: "sent", messageId: "next" });
  const actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0] });
  try {
    await actions.initialize();
    const first = await actions.send("first", request);
    assert.ok(first.status === "finished" && first.result.status === "unknown");
    assert.equal(client.requests.length, 1);
    assert.deepEqual(await actions.send("second", { ...request, text: "second" }), {
      actionId: "second",
      status: "blocked",
      blockedBy: "first",
    });
    const head = (await actions.listSendQueue()).items[0];
    const cancelled = await actions.cancelSend({
      actionId: head.actionId,
      expectedFingerprint: head.fingerprint,
      expectedReceiptDigest: head.receiptDigest!,
    });
    assert.ok(cancelled.status === "cancelled" && cancelled.resolution.kind === "unknown_retained");
    await actions.send("second", { ...request, text: "second", reply: undefined });
    assert.equal(client.requests.length, 2);
    assert.ok(client.requests.every((r) => !r.reply));
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ主动重试：仅未尝试超时与明确限流可按序重试，重启验证账本成功", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-active-retry-" });
  const client = new Client((n) =>
    n === 1
      ? {
        status: "failed",
        attempted: false,
        error: {
          category: "transport",
          transportReason: "timeout",
          failureStage: "upload",
        },
      }
      : n === 2
      ? { status: "failed", attempted: true, error: { category: "rate_limit" } }
      : { status: "sent", messageId: "confirmed" }
  );
  let actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0] });
  try {
    await actions.initialize();
    const result = await actions.send("same", request);
    assert.ok(result.status === "finished" && result.result.status === "sent");
    assert.equal(result.attemptCount, 3);
    assert.equal(client.requests.length, 3);
    assert.ok(client.requests.every((r) => !r.reply));
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    assert.deepEqual(await actions.send("same", { ...request, reply: undefined }), result);
    assert.equal(client.requests.length, 3);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ旧被动账本：原指纹和多次未知尝试仍能恢复，省略reply重入及取消均不重发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-legacy-reply-" });
  const old = await legacyFixture(directory);
  const original = await Deno.readTextFile(`${directory}/records/legacy.json`);
  const client = new Client();
  let actions = new QqSendActions(client, directory);
  try {
    await actions.initialize();
    const result = await actions.send("legacy", { ...old, reply: undefined });
    assert.ok(result.status === "finished" && result.result.status === "unknown");
    assert.equal(result.attemptCount, 2);
    assert.equal(client.requests.length, 0);
    await assert.rejects(actions.send("legacy", { ...old, text: "changed" }), conflict);
    const head = (await actions.listSendQueue()).items[0];
    const cancelled = await actions.cancelSend({
      actionId: head.actionId,
      expectedFingerprint: head.fingerprint,
      expectedReceiptDigest: head.receiptDigest!,
    });
    assert.equal(await Deno.readTextFile(`${directory}/records/legacy.json`), original);
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    assert.deepEqual(await actions.send("legacy", { ...old, reply: undefined }), cancelled);
    await actions.send("new", request);
    assert.equal(client.requests.length, 1);
    assert.equal(client.requests[0].reply, undefined);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ旧被动在途：中断恢复不再凭旧msg_id重新主动发送，队首屏障仍可取消", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-legacy-interrupted-" });
  const old = await legacyFixture(directory, true);
  const client = new Client();
  const actions = new QqSendActions(client, directory);
  try {
    await actions.initialize();
    assert.deepEqual(await actions.send("legacy", { ...old, reply: undefined }), {
      actionId: "legacy",
      status: "unknown",
      reason: "interrupted",
      attemptCount: 2,
    });
    assert.equal(client.requests.length, 0);
    const interrupted = JSON.parse(
      await Deno.readTextFile(`${directory}/attempts/legacy/2.interrupted.json`),
    );
    assert.ok(Number.isFinite(Date.parse(interrupted.interruptedAt)));
    const head = (await actions.listSendQueue()).items[0];
    assert.equal(head.canCancel, true);
    await actions.cancelSend({
      actionId: head.actionId,
      expectedFingerprint: head.fingerprint,
      expectedReceiptDigest: head.receiptDigest!,
    });
    await actions.send("follower", { ...request, text: "follower" });
    assert.equal(client.requests.length, 1);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ旧混合尝试链：早先未知或中断不被后来限流/认证拒绝覆盖，切主动不续发且可取消", async () => {
  for (const mixed of ["unknown_rate_limit", "interrupted_auth", "aborted_auth"] as const) {
    const directory = await Deno.makeTempDir({ prefix: "eve-qq-legacy-mixed-" });
    const old = await legacyFixture(directory, false, mixed);
    const original = new Map<string, string>();
    for await (const entry of Deno.readDir(`${directory}/attempts/legacy`)) {
      original.set(
        entry.name,
        await Deno.readTextFile(`${directory}/attempts/legacy/${entry.name}`),
      );
    }
    const client = new Client();
    let actions = new QqSendActions(client, directory);
    try {
      await actions.initialize();
      const resumed = await actions.send("legacy", { ...old, reply: undefined });
      if (mixed === "interrupted_auth") {
        assert.deepEqual(resumed, {
          actionId: "legacy",
          status: "unknown",
          reason: "interrupted",
          attemptCount: 2,
        });
      } else {
        assert.ok(resumed.status === "finished" && resumed.result.status === "unknown");
      }
      assert.equal(client.requests.length, 0);
      assert.deepEqual(await actions.send("tail", { ...request, text: "tail" }), {
        actionId: "tail",
        status: "blocked",
        blockedBy: "legacy",
      });
      for (const [name, bytes] of original) {
        assert.equal(await Deno.readTextFile(`${directory}/attempts/legacy/${name}`), bytes);
      }
      const head = (await actions.listSendQueue()).items[0];
      assert.equal(head.canCancel, true);
      const cancelled = await actions.cancelSend({
        actionId: head.actionId,
        expectedFingerprint: head.fingerprint,
        expectedReceiptDigest: head.receiptDigest!,
      });
      assert.ok(
        cancelled.status === "cancelled" && cancelled.resolution.kind === "unknown_retained",
      );
      await actions.send("tail", { ...request, text: "tail", reply: undefined });
      assert.equal(client.requests.length, 1);
      await actions.close();
      actions = new QqSendActions(client, directory);
      await actions.initialize();
      assert.deepEqual(await actions.send("legacy", { ...old, reply: undefined }), cancelled);
      assert.equal(client.requests.length, 1);
    } finally {
      await actions.close();
      await Deno.remove(directory, { recursive: true });
    }
  }
});

Deno.test("QQ旧混合尝试链：未知后真实sent已持久而汇总中断，恢复采用回执且零网络重放", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-legacy-sent-adoption-" });
  const old = await legacyFixture(directory, false, "unknown_sent");
  const path = `${directory}/attempts/legacy/2.result.json`;
  const original = await Deno.readTextFile(path);
  const client = new Client();
  let actions = new QqSendActions(client, directory);
  try {
    await actions.initialize();
    const sent = await actions.send("legacy", { ...old, reply: undefined });
    assert.deepEqual(sent, {
      actionId: "legacy",
      status: "finished",
      attemptCount: 2,
      result: { status: "sent", messageId: "legacy-receipt" },
    });
    assert.equal(client.requests.length, 0);
    assert.equal(await Deno.readTextFile(path), original);
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${directory}/records/legacy.json`)).status,
      "finished",
    );
    await actions.send("tail", { ...request, text: "tail", reply: undefined });
    assert.equal(client.requests.length, 1);
    await actions.close();
    actions = new QqSendActions(client, directory);
    await actions.initialize();
    assert.deepEqual(await actions.send("legacy", { ...old, reply: undefined }), sent);
    assert.equal(client.requests.length, 1);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});
