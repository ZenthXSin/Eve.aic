import assert from "node:assert/strict";
import {
  QqActionError,
  QqSendActions,
  type SendCancellation,
  type SendQueueItem,
} from "../adapters/qq/actions.ts";
import { QqClient, type QqSendResult, type QqSendText } from "../adapters/qq/client.ts";

const request: QqSendText = {
  destination: { kind: "user", openId: "fixture-user" },
  text: "原始完整消息",
  reply: { messageId: "inbound-fixed", sequence: 7 },
};
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
function platform(handler: (r: Request) => Response | Promise<Response>) {
  let count = 0;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture-token", expires_in: 7200 });
    }
    count++;
    return handler(r);
  });
  return {
    client: () =>
      new QqClient({ appId: "fixture", appSecret: "fixture" }, {
        baseUrl: `http://127.0.0.1:${server.addr.port}`,
      }),
    count: () => count,
    close: () => server.shutdown(),
  };
}
const permanentFailure = () => Response.json({ err_code: 40034105 }, { status: 400 });
const conflict = (error: unknown) =>
  error instanceof QqActionError && error.category === "conflict";
const corrupt = (error: unknown) => error instanceof QqActionError && error.category === "corrupt";
function cancellation(item: SendQueueItem): SendCancellation {
  assert.ok(item.receiptDigest);
  return {
    actionId: item.actionId,
    expectedFingerprint: item.fingerprint,
    expectedReceiptDigest: item.receiptDigest,
  };
}
async function snapshot(path: string): Promise<Record<string, string>> {
  const entries: Record<string, string> = {};
  async function visit(directory: string, prefix: string) {
    for await (const item of Deno.readDir(directory)) {
      const name = `${prefix}${item.name}`;
      if (item.isDirectory) await visit(`${directory}/${item.name}`, `${name}/`);
      else entries[name] = await Deno.readTextFile(`${directory}/${item.name}`);
    }
  }
  await visit(path, "");
  return entries;
}
async function waitForFile(path: string) {
  const deadline = Date.now() + 3000;
  for (;;) {
    try {
      return await Deno.readTextFile(path);
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
      assert.ok(Date.now() < deadline, `file was not persisted: ${path}`);
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  }
}

Deno.test("QQ失败结束：精确摘要、并发幂等、原始回执与尝试不变，重启不重发且放行后项", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-resolution-exact-" });
  let reject = true;
  const local = platform(() => reject ? permanentFailure() : Response.json({ id: "next-sent" }));
  let actions = new QqSendActions(local.client(), directory);
  const queueChanges: string[] = [];
  const notifiedResolutions: Promise<string>[] = [];
  actions.onQueueChange(() => {
    queueChanges.push(actions.status("failed")!.status);
    notifiedResolutions.push(Deno.readTextFile(`${directory}/resolutions/failed.json`));
  });
  try {
    await actions.initialize();
    const failed = await actions.send("failed", request);
    assert.ok(failed.status === "finished" && failed.result.status === "failed");
    assert.deepEqual(
      await actions.send("next", {
        ...request,
        reply: { messageId: "inbound-fixed" },
        text: "后项",
      }),
      {
        actionId: "next",
        status: "blocked",
        blockedBy: "failed",
      },
    );
    const records = await snapshot(`${directory}/records`);
    const attempts = await snapshot(`${directory}/attempts`);
    const page = await actions.listSendQueue({ failedHeadsOnly: true });
    assert.equal(page.items.length, 1);
    const item = page.items[0];
    assert.equal(item.canCancel, true);
    const args = cancellation(item);
    await assert.rejects(
      actions.cancelSend({ ...args, expectedFingerprint: "0".repeat(64) }),
      conflict,
    );
    await assert.rejects(
      actions.cancelSend({ ...args, expectedReceiptDigest: "0".repeat(64) }),
      conflict,
    );
    assert.deepEqual(queueChanges, []);
    const results = await Promise.all([
      actions.cancelSend(args, { actor: "admin", failedHeadOnly: true }),
      actions.cancelSend(args, { actor: "admin", failedHeadOnly: true }),
      actions.cancelSend(args, { actor: "admin", failedHeadOnly: true }),
    ]);
    assert.deepEqual(results[0], results[1]);
    assert.deepEqual(results[1], results[2]);
    assert.equal(results[0].status, "cancelled");
    if (results[0].status !== "cancelled") throw new Error("missing cancellation");
    assert.equal(results[0].resolution.kind, "failed_retained");
    assert.equal(results[0].resolution.actor, "admin");
    assert.deepEqual(results[0].originalResult, failed.result);
    assert.deepEqual(queueChanges, ["cancelled"]);
    assert.equal(
      JSON.parse(await notifiedResolutions[0]).receiptDigest,
      args.expectedReceiptDigest,
    );
    assert.deepEqual(await snapshot(`${directory}/records`), records);
    assert.deepEqual(await snapshot(`${directory}/attempts`), attempts);
    assert.equal(Object.keys(await snapshot(`${directory}/resolutions`)).length, 1);
    assert.equal(local.count(), 1);
    await assert.rejects(
      actions.cancelSend({ ...args, expectedReceiptDigest: "0".repeat(64) }),
      conflict,
    );
    await actions.close();
    actions = new QqSendActions(local.client(), directory);
    await actions.initialize();
    const deliveredEvents: string[] = [];
    actions.onQueueChange(() => deliveredEvents.push(actions.status("next")!.status));
    assert.deepEqual(await actions.send("failed", request), results[0]);
    assert.equal(local.count(), 1);
    reject = false;
    const next = await actions.send("next", {
      ...request,
      reply: { messageId: "inbound-fixed" },
      text: "后项",
    });
    assert.ok(next.status === "finished" && next.result.status === "sent");
    assert.deepEqual(deliveredEvents, ["finished"]);
    assert.equal(local.count(), 2);
    assert.equal(
      await Deno.readTextFile(`${directory}/records/failed.json`),
      records["failed.json"],
    );
    assert.deepEqual(await snapshot(`${directory}/attempts/failed`), {
      "1.started.json": attempts["failed/1.started.json"],
      "1.result.json": attempts["failed/1.result.json"],
    });
  } finally {
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ排队取消：只结束零尝试后项，保留队首与其余顺序，重启仍不发送取消项", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-resolution-queued-" });
  const contents: string[] = [];
  let reject = true;
  const local = platform(async (r) => {
    contents.push((await r.json()).content);
    return reject ? permanentFailure() : Response.json({ id: "remaining-sent" });
  });
  let actions = new QqSendActions(local.client(), directory);
  try {
    await actions.initialize();
    await actions.send("head", request);
    await actions.send("cancel-me", {
      ...request,
      reply: { messageId: "inbound-fixed" },
      text: "取消此项",
    });
    await actions.send("remaining", {
      ...request,
      reply: { messageId: "inbound-fixed" },
      text: "仍需发送",
    });
    const before = await Deno.readTextFile(`${directory}/records/cancel-me.json`);
    const page = await actions.listSendQueue();
    assert.deepEqual(page.items.map((i) => i.actionId), ["head", "cancel-me", "remaining"]);
    const args = cancellation(page.items[1]);
    assert.equal(page.items[1].canCancel, true);
    await assert.rejects(
      actions.cancelSend(args, { actor: "admin", failedHeadOnly: true }),
      conflict,
    );
    const cancelled = await actions.cancelSend(args);
    assert.equal(cancelled.status, "cancelled");
    if (cancelled.status !== "cancelled") throw new Error("missing cancellation");
    assert.equal(cancelled.resolution.kind, "queued_cancelled");
    assert.equal(cancelled.resolution.blockedBy, "head");
    assert.equal(cancelled.originalResult, undefined);
    assert.equal(await Deno.readTextFile(`${directory}/records/cancel-me.json`), before);
    await assert.rejects(Deno.stat(`${directory}/attempts/cancel-me`), Deno.errors.NotFound);
    assert.equal((await actions.listSendQueue()).items.length, 2);
    await actions.close();
    actions = new QqSendActions(local.client(), directory);
    await actions.initialize();
    assert.deepEqual(
      await actions.send("cancel-me", {
        ...request,
        reply: { messageId: "inbound-fixed" },
        text: "取消此项",
      }),
      cancelled,
    );
    assert.equal(local.count(), 1);
    const head = (await actions.listSendQueue({ failedHeadsOnly: true })).items[0];
    await actions.cancelSend(cancellation(head));
    reject = false;
    const remaining = await actions.send("remaining", {
      ...request,
      reply: { messageId: "inbound-fixed" },
      text: "仍需发送",
    });
    assert.ok(remaining.status === "finished" && remaining.result.status === "sent");
    assert.deepEqual(contents, [request.text, "仍需发送"]);
  } finally {
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ取消保留未知事实：unknown队首可取消等待，活跃请求不可取消", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-resolution-unknown-" });
  const entered = deferred<void>();
  const release = deferred<void>();
  const local = platform(async (r) => {
    const body = await r.json();
    if (body.content === "inflight") {
      entered.resolve();
      await release.promise;
      return Response.json({ id: "sent" });
    }
    return Response.json({});
  });
  const actions = new QqSendActions(local.client(), directory);
  let inflight: Promise<unknown> | undefined;
  try {
    await actions.initialize();
    const result = await actions.send("unknown", request);
    assert.ok(result.status === "finished" && result.result.status === "unknown");
    const unknown = (await actions.listSendQueue()).items[0];
    assert.equal(unknown.canCancel, true);
    const cancelled = await actions.cancelSend(cancellation(unknown));
    assert.equal(cancelled.status, "cancelled");
    if (cancelled.status !== "cancelled") throw new Error("missing cancellation");
    assert.equal(cancelled.resolution.kind, "unknown_retained");
    assert.deepEqual(cancelled.originalResult, result.result);
    inflight = actions.send("inflight", {
      ...request,
      text: "inflight",
      destination: { kind: "user", openId: "other" },
    });
    await entered.promise;
    const item = (await actions.listSendQueue()).items.find((i) => i.actionId === "inflight")!;
    assert.equal(item.canCancel, false);
    assert.equal(item.cancelBlockedReason, "running");
    assert.equal(item.receiptDigest, undefined);
    await assert.rejects(
      actions.cancelSend({
        actionId: item.actionId,
        expectedFingerprint: item.fingerprint,
        expectedReceiptDigest: "0".repeat(64),
      }),
      conflict,
    );
    assert.equal(Object.keys(await snapshot(`${directory}/resolutions`)).length, 1);
    release.resolve();
    await inflight;
  } finally {
    release.resolve();
    await inflight;
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ取消提交前校验许可，保留原件；持取消预留时同键send等待取消结果", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-resolution-checkpoint-" });
  const local = platform(permanentFailure);
  const actions = new QqSendActions(local.client(), directory);
  const entered = deferred<void>();
  const release = deferred<void>();
  let pending: Promise<unknown> | undefined;
  try {
    await actions.initialize();
    await actions.send("head", request);
    const args = cancellation((await actions.listSendQueue()).items[0]);
    await assert.rejects(
      actions.cancelSend(args, { checkpoint: () => Promise.reject(new Error("stale ownership")) }),
      /stale ownership/,
    );
    assert.deepEqual(await snapshot(`${directory}/resolutions`), {});
    assert.equal(actions.status("head")?.status, "finished");
    const cancelling = actions.cancelSend(args, {
      checkpoint: async () => {
        entered.resolve();
        await release.promise;
      },
    });
    pending = cancelling;
    await entered.promise;
    const duplicateSend = actions.send("head", request);
    release.resolve();
    const [cancelled, sent] = await Promise.all([cancelling, duplicateSend]);
    assert.equal(cancelled.status, "cancelled");
    assert.deepEqual(sent, cancelled);
    assert.equal(local.count(), 1);
  } finally {
    release.resolve();
    await pending;
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ取消校验磁盘事实：原始摘要过期拒绝，summary与attempt篡改均fail closed", async () => {
  for (const tamper of ["whitespace", "summary", "attempt"] as const) {
    const directory = await Deno.makeTempDir({ prefix: `eve-resolution-${tamper}-` });
    const local = platform(permanentFailure);
    const actions = new QqSendActions(local.client(), directory);
    try {
      await actions.initialize();
      await actions.send("head", request);
      const args = cancellation((await actions.listSendQueue()).items[0]);
      const path = `${directory}/${
        tamper === "attempt" ? "attempts/head/1.result.json" : "records/head.json"
      }`;
      const raw = await Deno.readTextFile(path);
      if (tamper === "whitespace") {
        await Deno.writeTextFile(path, raw + "\n");
        await assert.rejects(actions.cancelSend(args), conflict);
        assert.equal(actions.available, true);
      } else {
        const parsed = JSON.parse(raw);
        if (tamper === "summary") parsed.request.text = "替换原文";
        else parsed.result.error.code = 12345;
        await Deno.writeTextFile(path, JSON.stringify(parsed));
        await assert.rejects(actions.cancelSend(args), corrupt);
        assert.equal(actions.available, false);
      }
      assert.deepEqual(await snapshot(`${directory}/resolutions`), {});
      assert.equal(local.count(), 1);
    } finally {
      await actions.close().catch(() => {});
      await local.close();
      await Deno.remove(directory, { recursive: true });
    }
  }
});

Deno.test("QQ恢复取消事实拒绝摘要分叉，完整失败回执仍保留", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-resolution-restore-" });
  const local = platform(permanentFailure);
  const actions = new QqSendActions(local.client(), directory);
  let restart: QqSendActions | undefined;
  try {
    await actions.initialize();
    await actions.send("head", request);
    const original = await Deno.readTextFile(`${directory}/records/head.json`);
    await actions.cancelSend(cancellation((await actions.listSendQueue()).items[0]));
    await actions.close();
    const path = `${directory}/resolutions/head.json`;
    const resolution = JSON.parse(await Deno.readTextFile(path));
    resolution.receiptDigest = "0".repeat(64);
    await Deno.writeTextFile(path, JSON.stringify(resolution));
    restart = new QqSendActions(local.client(), directory);
    await assert.rejects(restart.initialize(), corrupt);
    assert.equal(await Deno.readTextFile(`${directory}/records/head.json`), original);
    assert.equal(local.count(), 1);
  } finally {
    await actions.close();
    await restart?.close().catch(() => {});
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ显式鉴权与限流失败按原请求重试，Retry-After加基础退避且后项不越过", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-resolution-retry-" });
  const bodies: unknown[] = [];
  const times: number[] = [];
  const local = platform(async (r) => {
    bodies.push(await r.json());
    times.push(Date.now());
    if (bodies.length === 1) return Response.json({ err_code: 100016 }, { status: 401 });
    if (bodies.length === 2) {
      return Response.json({ err_code: 100001 }, { status: 429, headers: { "retry-after": "1" } });
    }
    return Response.json({ id: `sent-${bodies.length}` });
  });
  let actions = new QqSendActions(local.client(), directory, "private", { retryDelaysMs: [20] });
  let checkpoints = 0;
  try {
    await actions.initialize();
    const first = actions.send("first", request, {
      beforeAttempt: () => {
        checkpoints++;
        return Promise.resolve();
      },
    });
    const second = actions.send("second", {
      ...request,
      text: "后项",
      reply: { messageId: "inbound-fixed", sequence: 8 },
    });
    const [a, b] = await Promise.all([first, second]);
    assert.ok(a.status === "finished" && a.result.status === "sent");
    assert.ok(b.status === "finished" && b.result.status === "sent");
    assert.equal(a.attemptCount, 3);
    assert.equal(checkpoints, 3);
    assert.deepEqual(bodies.slice(0, 3), [bodies[0], bodies[0], bodies[0]]);
    assert.deepEqual(bodies[0], {
      content: request.text,
      msg_type: 0,
      msg_id: "inbound-fixed",
      msg_seq: 7,
    });
    assert.ok(times[1] - times[0] >= 20, `auth backoff ${times[1] - times[0]}`);
    assert.ok(times[2] - times[1] >= 1020, `rate backoff ${times[2] - times[1]}`);
    assert.equal((bodies[3] as { content: string }).content, "后项");
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${directory}/attempts/first/2.result.json`)).result.error
        .retryAfterMs,
      1000,
    );
    await actions.close();
    actions = new QqSendActions(local.client(), directory);
    await actions.initialize();
    assert.deepEqual(await actions.send("first", request), a);
    assert.equal(local.count(), 4);
  } finally {
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

class ScriptedClient extends QqClient {
  calls: QqSendText[] = [];
  constructor(readonly result: QqSendResult) {
    super({ appId: "fixture", appSecret: "fixture" });
  }
  override sendText(value: QqSendText): Promise<QqSendResult> {
    this.calls.push(structuredClone(value));
    return Promise.resolve(structuredClone(this.result));
  }
}
Deno.test("QQ未知鉴权、限流和平台错误不自动重试，可取消等待但保留未知事实", async () => {
  for (const category of ["authentication", "rate_limit", "platform"] as const) {
    const directory = await Deno.makeTempDir({ prefix: "eve-resolution-no-guess-" });
    const client = new ScriptedClient({
      status: "unknown",
      attempted: true,
      error: { category, retryAfterMs: 0 },
    });
    const actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0] });
    try {
      await actions.initialize();
      const result = await actions.send("unknown", request);
      assert.ok(result.status === "finished" && result.result.status === "unknown");
      assert.equal(client.calls.length, 1);
      assert.deepEqual(await actions.send("unknown", request), result);
      const item = (await actions.listSendQueue()).items[0];
      assert.equal(item.canCancel, true);
      const cancelled = await actions.cancelSend(cancellation(item));
      assert.ok(cancelled.status === "cancelled" && cancelled.originalResult?.status === "unknown");
      assert.equal(client.calls.length, 1);
    } finally {
      await actions.close();
      await Deno.remove(directory, { recursive: true });
    }
  }
});

Deno.test("QQ长Retry-After等待不可取消，shutdown及时唤醒且不伪造完成", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-resolution-stop-retry-" });
  const local = platform(() =>
    Response.json({ err_code: 100001 }, { status: 429, headers: { "retry-after": "3600" } })
  );
  const actions = new QqSendActions(local.client(), directory, "private", { retryDelaysMs: [50] });
  let send: Promise<unknown> | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await actions.initialize();
    send = actions.send("waiting", request);
    await waitForFile(`${directory}/attempts/waiting/1.result.json`);
    const item = (await actions.listSendQueue()).items[0];
    assert.equal(item.canCancel, false);
    assert.equal(item.cancelBlockedReason, "running");
    await assert.rejects(
      actions.cancelSend({
        actionId: item.actionId,
        expectedFingerprint: item.fingerprint,
        expectedReceiptDigest: "0".repeat(64),
      }),
      conflict,
    );
    const start = Date.now();
    await Promise.race([
      actions.close(),
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(new Error("shutdown failed to interrupt Retry-After")),
          1000,
        );
      }),
    ]);
    clearTimeout(timer);
    assert.ok(Date.now() - start < 1000);
    assert.deepEqual(await send, { actionId: "waiting", status: "unknown", reason: "interrupted" });
    assert.equal(local.count(), 1);
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${directory}/records/waiting.json`)).status,
      "started",
    );
    assert.deepEqual(await snapshot(`${directory}/resolutions`), {});
  } finally {
    clearTimeout(timer);
    await actions.close();
    await send;
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ活跃队首重试期间可取消等待中的零尝试后项，并立即结清原pending send", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-resolution-pending-follower-" });
  const secondAttempt = deferred<void>();
  const contents: string[] = [];
  const local = platform(async (r) => {
    contents.push((await r.json()).content);
    if (contents.length === 2) secondAttempt.resolve();
    return Response.json({ err_code: 100001 }, {
      status: 429,
      headers: { "retry-after": contents.length === 1 ? "1" : "3600" },
    });
  });
  const actions = new QqSendActions(local.client(), directory, "private", { retryDelaysMs: [5] });
  let head: Promise<unknown> | undefined;
  let follower: Promise<unknown> | undefined;
  const timers: ReturnType<typeof setTimeout>[] = [];
  function within<T>(operation: Promise<T>, duration: number, label: string) {
    return Promise.race([
      operation,
      new Promise<never>((_, reject) => {
        timers.push(setTimeout(() => reject(new Error(label)), duration));
      }),
    ]);
  }
  try {
    await actions.initialize();
    head = actions.send("head", request);
    await waitForFile(`${directory}/attempts/head/1.result.json`);
    follower = actions.send("pending-follower", {
      ...request,
      reply: { messageId: "inbound-fixed" },
      text: "绝不能发送的后项",
    });
    await waitForFile(`${directory}/records/pending-follower.json`);
    const original = await Deno.readTextFile(`${directory}/records/pending-follower.json`);
    const items = (await actions.listSendQueue()).items;
    const pending = items.find((item) => item.actionId === "pending-follower")!;
    assert.equal(items.find((item) => item.actionId === "head")!.canCancel, false);
    assert.equal(pending.canCancel, true);
    assert.deepEqual(pending.status, {
      actionId: "pending-follower",
      status: "blocked",
      blockedBy: "head",
    });
    const cancelled = await within(
      actions.cancelSend(cancellation(pending)),
      500,
      "queued cancellation waited for retrying head",
    );
    assert.equal(cancelled.status, "cancelled");
    assert.deepEqual(
      await within(follower, 500, "original pending send did not settle on cancellation"),
      cancelled,
    );
    assert.equal(actions.status("head")?.status, "running");
    assert.equal(await Deno.readTextFile(`${directory}/records/pending-follower.json`), original);
    await assert.rejects(Deno.stat(`${directory}/attempts/pending-follower`), Deno.errors.NotFound);
    await within(secondAttempt.promise, 2500, "uncancelled head did not continue retrying");
    assert.deepEqual(contents, [request.text, request.text]);
    actions.requestStop();
    assert.deepEqual(await head, {
      actionId: "head",
      status: "unknown",
      reason: "interrupted",
      attemptCount: 2,
    });
    assert.equal(local.count(), 2);
  } finally {
    for (const timer of timers) clearTimeout(timer);
    actions.requestStop();
    await actions.close();
    await head;
    await follower;
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});
