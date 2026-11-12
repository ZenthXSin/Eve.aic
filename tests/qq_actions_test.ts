import assert from "node:assert/strict";
import { QqActionError, type QqActionSendText, QqSendActions } from "../adapters/qq/actions.ts";
import { phoneReceipt } from "../adapters/qq/phone.ts";
import { QqClient, type QqSendText } from "../adapters/qq/client.ts";

const request: QqSendText = {
  destination: { kind: "user", openId: "user" },
  text: "完整的一条",
  reply: { messageId: "inbound-1", sequence: 1 },
};
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
function service(handler: (request: Request) => Response | Promise<Response>) {
  let sends = 0;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture-token", expires_in: 7200 });
    }
    sends++;
    return handler(r);
  });
  return {
    url: `http://127.0.0.1:${server.addr.port}`,
    client: (sendScope: "all" | "private" = "all", timeoutMs = 20_000) =>
      new QqClient({ appId: "fixture", appSecret: "fixture" }, {
        baseUrl: `http://127.0.0.1:${server.addr.port}`,
        sendScope,
        timeoutMs,
      }),
    count: () => sends,
    close: () => server.shutdown(),
  };
}

Deno.test("QQ 发送动作：网络前started已落盘，同键并发只发送一次，重启采用实际回执", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-actions-" });
  const local = service(async (r) => {
    assert.deepEqual(await r.json(), {
      markdown: { content: request.text },
      msg_type: 2,
      msg_id: "inbound-1",
      msg_seq: 1,
    });
    const persisted = JSON.parse(await Deno.readTextFile(`${directory}/records/action-1.json`));
    assert.equal(persisted.status, "started");
    assert.equal(persisted.result, undefined);
    return Response.json({ id: "platform-sent-1" });
  });
  let actions: QqSendActions | undefined;
  try {
    actions = new QqSendActions(local.client(), directory);
    await actions.initialize();
    const result = await Promise.all([
      actions.send("action-1", request),
      actions.send("action-1", structuredClone(request)),
    ]);
    assert.deepEqual(result[0], result[1]);
    assert.deepEqual(result[0], {
      actionId: "action-1",
      status: "finished",
      result: { status: "sent", messageId: "platform-sent-1" },
    });
    assert.equal(local.count(), 1);
    await actions.close();
    actions = new QqSendActions(local.client(), directory);
    await actions.initialize();
    assert.deepEqual(actions.status("action-1"), result[0]);
    assert.deepEqual(await actions.send("action-1", request), result[0]);
    assert.equal(local.count(), 1);
    assert.equal((await Deno.stat(`${directory}/records/action-1.json`)).mode! & 0o777, 0o600);
    await assert.rejects(
      actions.send("action-1", {
        ...request,
        reply: { messageId: "inbound-1" },
        text: "更改原动作",
      }),
      (e) => e instanceof QqActionError && e.category === "conflict",
    );
    assert.equal(local.count(), 1);
  } finally {
    await actions?.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 发送动作：平台未给编号记unknown，重复调用与重启不再次发送", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-actions-" });
  const local = service(() => Response.json({}));
  let actions: QqSendActions | undefined;
  try {
    actions = new QqSendActions(local.client(), directory);
    await actions.initialize();
    const result = await actions.send("unknown-1", request);
    assert.ok(result.status === "finished" && result.result.status === "unknown");
    assert.deepEqual(await actions.send("unknown-1", request), result);
    await actions.close();
    actions = new QqSendActions(local.client(), directory);
    await actions.initialize();
    assert.deepEqual(await actions.send("unknown-1", request), result);
    assert.equal(local.count(), 1);
  } finally {
    await actions?.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 发送动作：私信范围独立生效，通用客户端也不能发群", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-actions-" });
  const local = service(() => Response.json({ id: "unexpected" }));
  const actions = new QqSendActions(local.client(), directory);
  try {
    await actions.initialize();
    const result = await actions.send("group-denied", {
      ...request,
      destination: { kind: "group", openId: "group" },
    });
    assert.deepEqual(result, {
      actionId: "group-denied",
      status: "finished",
      result: { status: "failed", attempted: false, error: { category: "scope" } },
    });
    assert.equal(local.count(), 0);
    await assert.rejects(actions.send("../escape", request), /request/);
    await assert.rejects(
      actions.send("bad-parameter", { ...request, reply: { messageId: "inbound-1" }, text: "" }),
      /request/,
    );
    assert.equal(local.count(), 0);
  } finally {
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 发送动作：真实子进程发送期间被终止，恢复started为unknown而不重放", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-interrupted-" });
  const entered = deferred<void>();
  const release = deferred<void>();
  const local = service(async () => {
    entered.resolve();
    await release.promise;
    return Response.json({ id: "late-platform-id" });
  });
  const source = new URL("../adapters/qq/actions.ts", import.meta.url).href;
  const clientSource = new URL("../adapters/qq/client.ts", import.meta.url).href;
  const childPath = `${directory}/child.ts`;
  let child: Deno.ChildProcess | undefined;
  let restart: QqSendActions | undefined;
  try {
    await Deno.writeTextFile(
      childPath,
      `import {QqSendActions} from ${JSON.stringify(source)};\n` +
        `import {QqClient} from ${JSON.stringify(clientSource)};\n` +
        `const actions=new QqSendActions(new QqClient({appId:"fixture",appSecret:"fixture"},{baseUrl:${
          JSON.stringify(local.url)
        }}),${JSON.stringify(directory + "/data")});\n` +
        `await actions.initialize();await actions.send("interrupted",${
          JSON.stringify(request)
        });await actions.close();\n`,
    );
    child = new Deno.Command(Deno.execPath(), {
      args: ["run", "--allow-read", "--allow-write", "--allow-net=127.0.0.1", childPath],
      stdout: "null",
      stderr: "null",
    }).spawn();
    await entered.promise;
    const path = `${directory}/data/records/interrupted.json`;
    assert.equal(JSON.parse(await Deno.readTextFile(path)).status, "started");
    child.kill("SIGKILL");
    await child.status;
    child = undefined;
    const legacy = JSON.parse(await Deno.readTextFile(path));
    delete legacy.policy;
    await Deno.writeTextFile(path, JSON.stringify(legacy));
    const original = await Deno.readTextFile(path);
    release.resolve();
    restart = new QqSendActions(local.client(), `${directory}/data`);
    await restart.initialize();
    assert.deepEqual(restart.status("interrupted"), {
      actionId: "interrupted",
      status: "unknown",
      reason: "interrupted",
    });
    assert.deepEqual(await restart.send("interrupted", request), {
      actionId: "interrupted",
      status: "unknown",
      reason: "interrupted",
    });
    assert.equal(local.count(), 1);
    assert.equal(await Deno.readTextFile(path), original);
  } finally {
    if (child) {
      try {
        child.kill("SIGKILL");
      } catch { /* 已停止 */ }
      await child.status;
    }
    release.resolve();
    await restart?.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 发送动作：实际已发但最终记录存盘失败，恢复只据尝试回执补完成不再次发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-actions-" });
  const local = service(async () => {
    await Deno.rename(`${directory}/records`, `${directory}/preserved`);
    return Response.json({ id: "already-sent" });
  });
  const sentRequest: QqSendText = { ...request, destination: { kind: "group", openId: "group" } };
  const actions = new QqSendActions(local.client(), directory, "all");
  let restart: QqSendActions | undefined;
  try {
    await actions.initialize();
    await assert.rejects(actions.send("save-failed", sentRequest), /storage/);
    await assert.rejects(actions.close(), /storage/);
    assert.equal(local.count(), 1);
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${directory}/preserved/save-failed.json`)).status,
      "started",
    );
    await Deno.rename(`${directory}/preserved`, `${directory}/records`);
    restart = new QqSendActions(local.client(), directory);
    await restart.initialize();
    assert.deepEqual(await restart.send("save-failed", sentRequest), {
      actionId: "save-failed",
      status: "finished",
      result: { status: "sent", messageId: "already-sent" },
    });
    assert.equal(local.count(), 1);
  } finally {
    await actions.close().catch(() => {});
    await restart?.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ停止中止在途重试且保留屏障，释放锁后可恢复状态", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-actions-" });
  const entered = deferred<void>(), release = deferred<void>();
  const local = service(async () => {
    entered.resolve();
    await release.promise;
    return Response.json({ id: "late" });
  });
  const first = new QqSendActions(local.client(), directory);
  let restart: QqSendActions | undefined;
  try {
    await first.initialize();
    const sending = first.send("inflight", request);
    await entered.promise;
    await assert.rejects(new QqSendActions(local.client(), directory).initialize(), /unavailable/);
    await first.close();
    assert.deepEqual(await sending, {
      actionId: "inflight",
      status: "unknown",
      reason: "interrupted",
    });
    await assert.rejects(first.send("new-during-close", request), /unavailable/);
    restart = new QqSendActions(local.client(), directory);
    await restart.initialize();
    assert.deepEqual(restart.status("inflight"), {
      actionId: "inflight",
      status: "unknown",
      reason: "interrupted",
    });
    assert.deepEqual(
      await restart.send("blocked", {
        ...request,
        reply: { messageId: "inbound-1" },
        text: "blocked",
      }),
      {
        actionId: "blocked",
        status: "blocked",
        blockedBy: "inflight",
      },
    );
    assert.equal(local.count(), 1);
  } finally {
    release.resolve();
    await first.close().catch(() => {});
    await restart?.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 发送动作：损坏回执或未完成文件原样保留，不当作完成或空账本", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-actions-" });
  const local = service(() => Response.json({}));
  const actions = new QqSendActions(local.client(), directory);
  try {
    await actions.initialize();
    await actions.send("corrupt", request);
    await actions.close();
    const path = `${directory}/records/corrupt.json`;
    const record = JSON.parse(await Deno.readTextFile(path));
    record.result.error.code = "not-a-real-error-code";
    const corrupt = JSON.stringify(record);
    await Deno.writeTextFile(path, corrupt);
    await assert.rejects(new QqSendActions(local.client(), directory).initialize(), /corrupt/);
    assert.equal(await Deno.readTextFile(path), corrupt);
    await Deno.writeTextFile(`${directory}/records/incomplete.tmp`, "incomplete evidence");
    await assert.rejects(new QqSendActions(local.client(), directory).initialize(), /corrupt/);
    assert.equal(
      await Deno.readTextFile(`${directory}/records/incomplete.tmp`),
      "incomplete evidence",
    );
    assert.equal(local.count(), 1);
  } finally {
    await actions.close().catch(() => {});
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ群范围升级保留旧拒绝和unknown原件，新动作才发送，重启不重放", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-scope-upgrade-" });
  const paths: string[] = [];
  const local = service(async (r) => {
    paths.push(new URL(r.url).pathname);
    const body = await r.json();
    return (body.markdown?.content ?? body.content) === "未知群消息"
      ? Response.json({})
      : Response.json({ id: "group-sent" });
  });
  const group: QqActionSendText = {
    ...request,
    reply: { messageId: "inbound-1" },
    destination: { kind: "group", openId: "group/id" },
  };
  let actions = new QqSendActions(local.client(), directory);
  try {
    await actions.initialize();
    const denied = await actions.send("old-denied", group);
    assert.deepEqual(denied, {
      actionId: "old-denied",
      status: "finished",
      result: { status: "failed", attempted: false, error: { category: "scope" } },
    });
    const legacy = JSON.parse(await Deno.readTextFile(`${directory}/records/old-denied.json`));
    delete legacy.policy;
    await Deno.writeTextFile(`${directory}/records/old-denied.json`, JSON.stringify(legacy));
    const oldBytes = await Deno.readTextFile(`${directory}/records/old-denied.json`);
    assert.equal(local.count(), 0);
    await actions.close();
    actions = new QqSendActions(local.client(), directory, "all");
    await actions.initialize();
    assert.deepEqual(await actions.send("old-denied", group), denied);
    assert.equal(await Deno.readTextFile(`${directory}/records/old-denied.json`), oldBytes);
    assert.equal(local.count(), 0);
    const sent = await actions.send("new-group", group);
    assert.deepEqual(sent, {
      actionId: "new-group",
      status: "finished",
      result: { status: "sent", messageId: "group-sent" },
    });
    const unknownRequest = { ...group, text: "未知群消息" };
    const unknown = await actions.send("unknown-group", unknownRequest);
    assert.ok(unknown.status === "finished" && unknown.result.status === "unknown");
    const sentBytes = await Deno.readTextFile(`${directory}/records/new-group.json`);
    const unknownBytes = await Deno.readTextFile(`${directory}/records/unknown-group.json`);
    await actions.close();
    actions = new QqSendActions(local.client(), directory, "all");
    await actions.initialize();
    assert.deepEqual(await actions.send("old-denied", group), denied);
    assert.deepEqual(await actions.send("new-group", group), sent);
    assert.deepEqual(await actions.send("unknown-group", unknownRequest), unknown);
    assert.equal(await Deno.readTextFile(`${directory}/records/old-denied.json`), oldBytes);
    assert.equal(await Deno.readTextFile(`${directory}/records/new-group.json`), sentBytes);
    assert.equal(await Deno.readTextFile(`${directory}/records/unknown-group.json`), unknownBytes);
    assert.deepEqual(paths, ["/v2/groups/group%2Fid/messages", "/v2/groups/group%2Fid/messages"]);
    assert.equal(local.count(), 2);
    assert.throws(() => new QqSendActions(local.client(), directory, "group" as "all"), /request/);
  } finally {
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("动作层all不能绕过传输层private", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-scope-layers-" });
  const local = service(() => Response.json({ id: "unexpected" }));
  const actions = new QqSendActions(local.client("private"), directory, "all");
  try {
    await actions.initialize();
    const result = await actions.send("transport-denied", {
      ...request,
      destination: { kind: "group", openId: "group" },
    });
    assert.deepEqual(result, {
      actionId: "transport-denied",
      status: "finished",
      result: { status: "failed", attempted: false, error: { category: "scope" } },
    });
    assert.equal(local.count(), 0);
  } finally {
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ超时按序重试：固定防重请求、真实尝试账本、成功及重启摘要一致", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-timeout-retry-" });
  const release = deferred<void>();
  const bodies: unknown[] = [];
  const local = service(async (r) => {
    bodies.push(await r.json());
    const n = bodies.length;
    const started = JSON.parse(
      await Deno.readTextFile(`${directory}/attempts/retry-success/${n}.started.json`),
    );
    assert.equal(started.attempt, n);
    assert.equal(started.actionId, "retry-success");
    if (n === 1) await release.promise;
    return Response.json({ id: "actual-retry-success" });
  });
  const client = local.client("all", 80);
  let actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0, 0] });
  try {
    await client.authenticate();
    await actions.initialize();
    const result = await actions.send("retry-success", request);
    assert.deepEqual(result, {
      actionId: "retry-success",
      status: "finished",
      attemptCount: 2,
      result: { status: "sent", messageId: "actual-retry-success" },
    });
    assert.deepEqual(phoneReceipt(result), {
      actionId: "retry-success",
      state: "sent",
      platformMessageId: "actual-retry-success",
      attemptCount: 2,
    });
    assert.deepEqual(bodies, [
      { markdown: { content: request.text }, msg_type: 2, msg_id: "inbound-1", msg_seq: 1 },
      { markdown: { content: request.text }, msg_type: 2, msg_id: "inbound-1", msg_seq: 1 },
    ]);
    const original = new Map<string, string>();
    for (
      const path of [
        "records/retry-success.json",
        ...[1, 2].flatMap((
          n,
        ) => [
          `attempts/retry-success/${n}.started.json`,
          `attempts/retry-success/${n}.result.json`,
        ]),
      ]
    ) {
      original.set(path, await Deno.readTextFile(`${directory}/${path}`));
    }
    const first = JSON.parse(original.get("attempts/retry-success/1.result.json")!);
    assert.equal(first.result.status, "unknown");
    assert.equal(first.result.error.transportReason, "timeout");
    assert.equal(
      JSON.parse(original.get("attempts/retry-success/2.result.json")!).result.messageId,
      "actual-retry-success",
    );
    await actions.close();
    actions = new QqSendActions(local.client(), directory, "private", { retryDelaysMs: [0, 0] });
    await actions.initialize();
    assert.deepEqual(actions.status("retry-success"), result);
    assert.deepEqual(await actions.send("retry-success", request), result);
    assert.deepEqual(phoneReceipt(actions.status("retry-success")!), phoneReceipt(result));
    assert.equal(local.count(), 2);
    for (const [path, bytes] of original) {
      assert.equal(await Deno.readTextFile(`${directory}/${path}`), bytes);
    }
  } finally {
    release.resolve();
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ重试后续拒绝或去重保留未知，永久屏障不放行后续动作", async () => {
  for (const ending of ["duplicate", "permanent"] as const) {
    const directory = await Deno.makeTempDir({ prefix: "eve-qq-retry-bound-" });
    const release = deferred<void>();
    let calls = 0;
    const local = service(async () => {
      calls++;
      if (calls === 1) {
        await release.promise;
        return Response.json({ id: "too-late" });
      }
      return ending === "duplicate"
        ? Response.json({ err_code: 40054005 })
        : Response.json({ err_code: 40034105 }, { status: 400 });
    });
    const client = local.client("all", 60);
    const actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0, 0] });
    try {
      await client.authenticate();
      await actions.initialize();
      const result = await actions.send("bounded", request);
      assert.ok(result.status === "finished" && result.result.status === "unknown");
      assert.equal(result.attemptCount, 2);
      assert.equal(calls, 2);
      assert.equal(
        result.result.error.category,
        ending === "duplicate" ? "duplicate" : "transport",
      );
      assert.deepEqual(await actions.send("bounded", request), result);
      assert.equal(calls, 2);
      assert.deepEqual(
        await actions.send("following", {
          ...request,
          reply: { messageId: "inbound-1" },
          text: "must-wait",
        }),
        {
          actionId: "following",
          status: "blocked",
          blockedBy: "bounded",
        },
      );
      assert.equal(calls, 2);
    } finally {
      release.resolve();
      await actions.close();
      await local.close();
      await Deno.remove(directory, { recursive: true });
    }
  }
});

Deno.test("QQ认证超时未发消息可重试，实际消息只发一次", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-auth-retry-" });
  const release = deferred<void>();
  let auths = 0, sends = 0;
  const platform = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      auths++;
      if (auths === 1) await release.promise;
      return Response.json({ access_token: "fixture-token", expires_in: 7200 });
    }
    sends++;
    return Response.json({ id: "auth-retried-once" });
  });
  const client = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
    baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    timeoutMs: 60,
  });
  const actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0, 0] });
  try {
    await actions.initialize();
    const result = await actions.send("auth-retry", { ...request, reply: undefined });
    assert.ok(result.status === "finished" && result.result.status === "sent");
    assert.equal(result.attemptCount, 2);
    assert.equal(auths, 2);
    assert.equal(sends, 1);
    const first = JSON.parse(
      await Deno.readTextFile(`${directory}/attempts/auth-retry/1.result.json`),
    );
    assert.equal(first.result.status, "failed");
    assert.equal(first.result.attempted, false);
    assert.equal(first.result.error.transportReason, "timeout");
  } finally {
    release.resolve();
    await actions.close();
    await platform.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ无平台防重键、媒体、永久错误、network及aborted不自动重试", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-retry-ineligible-" });
  const release = deferred<void>();
  let mode = "timeout";
  let sends = 0;
  const local = service(async (r) => {
    if (new URL(r.url).pathname.endsWith("/files")) {
      return Response.json({ file_info: "uploaded-once" });
    }
    sends++;
    if (mode === "permanent") return Response.json({ err_code: 40034105 }, { status: 400 });
    await release.promise;
    return Response.json({ id: "too-late" });
  });
  const client = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
    baseUrl: local.url,
    sendScope: "all",
    timeoutMs: 60,
    attachmentReader: () => Promise.resolve(new Uint8Array([1, 2, 3])),
  });
  const actions = new QqSendActions(client, directory, "all", { retryDelaysMs: [0, 0] });
  try {
    await client.authenticate();
    await actions.initialize();
    for (
      const [id, change] of [
        ["active", { reply: undefined }],
        ["event", { reply: { eventId: "event-1" } }],
        ["media", { attachment: { path: "/fixture/image.png", kind: "image" } }],
      ] as const
    ) {
      const before = sends;
      const result = await actions.send(id, {
        ...request,
        ...change,
        destination: { kind: "user", openId: id },
      });
      assert.ok(result.status === "finished" && result.result.status === "unknown");
      assert.equal(result.attemptCount, undefined);
      assert.equal(sends, before + 1);
    }
    mode = "permanent";
    const rejected = await actions.send("rejected", {
      ...request,
      destination: { kind: "user", openId: "rejected" },
    });
    assert.ok(rejected.status === "finished" && rejected.result.status === "failed");
    assert.equal(rejected.attemptCount, undefined);
    release.resolve();
    await local.close();
    const disconnected = await actions.send("network", {
      ...request,
      destination: { kind: "user", openId: "network" },
    });
    assert.ok(disconnected.status === "finished" && disconnected.result.status === "unknown");
    assert.equal(disconnected.result.error.transportReason, "network");
    assert.equal(disconnected.attemptCount, undefined);
    client.close();
    const aborted = await actions.send("aborted", {
      ...request,
      destination: { kind: "user", openId: "aborted" },
    });
    assert.deepEqual(aborted, { actionId: "aborted", status: "unknown", reason: "interrupted" });
    assert.equal(aborted.attemptCount, undefined);
    assert.equal(sends, 4);
  } finally {
    release.resolve();
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ同会话队首不限次重试直到成功再第二条，跨会话并行且排队先持久", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-conversation-fifo-" });
  const release = deferred<void>();
  const entered = deferred<void>();
  const order: string[] = [];
  let firstAttempts = 0;
  const local = service(async (r) => {
    const body = await r.json();
    order.push(body.markdown?.content ?? body.content);
    if ((body.markdown?.content ?? body.content) === "first") {
      firstAttempts++;
      if (firstAttempts < 5) {
        entered.resolve();
        await release.promise;
      }
    }
    return Response.json({ id: `sent-${(body.markdown?.content ?? body.content)}` });
  });
  const client = local.client("all", 100);
  const actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [5, 5] });
  try {
    await client.authenticate();
    await actions.initialize();
    const first = actions.send("first", {
      ...request,
      reply: { messageId: "inbound-1" },
      text: "first",
    });
    const second = actions.send("second", {
      ...request,
      text: "second",
      reply: { messageId: "inbound-1", sequence: 2 },
    });
    const third = actions.send("third", {
      ...request,
      text: "third",
      reply: { messageId: "inbound-1", sequence: 3 },
    });
    await entered.promise;
    const other = await actions.send("other", {
      ...request,
      destination: { kind: "user", openId: "different" },
      text: "other",
    });
    assert.ok(other.status === "finished" && other.result.status === "sent");
    assert.deepEqual(order, ["first", "other"]);
    assert.deepEqual(actions.status("second"), {
      actionId: "second",
      status: "blocked",
      blockedBy: "first",
    });
    const queued = JSON.parse(await Deno.readTextFile(`${directory}/records/second.json`));
    assert.equal(queued.status, "started");
    assert.equal(queued.policy.kind, "until_sent_v1");
    assert.equal(queued.policy.sequence, 2);
    const results = await Promise.all([first, second, third]);
    assert.equal(results[0].attemptCount, 5);
    assert.deepEqual(order, [
      "first",
      "other",
      "first",
      "first",
      "first",
      "first",
      "second",
      "third",
    ]);
    await actions.close();
    await assert.rejects(actions.send("after-close", request), /unavailable/);
    assert.ok(
      results.every((result) => result.status === "finished" && result.result.status === "sent"),
    );
  } finally {
    release.resolve();
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ新完成动作的整个尝试目录丢失时拒绝恢复，不冒充旧账本", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-attempt-missing-" });
  const local = service(() => Response.json({ id: "actual-once" }));
  const actions = new QqSendActions(local.client(), directory);
  try {
    await actions.initialize();
    await actions.send("missing-chain", request);
    await actions.close();
    const original = await Deno.readTextFile(`${directory}/records/missing-chain.json`);
    assert.equal(JSON.parse(original).attemptCount, 1);
    await Deno.rename(`${directory}/attempts/missing-chain`, `${directory}/preserved-attempts`);
    await assert.rejects(new QqSendActions(local.client(), directory).initialize(), /corrupt/);
    assert.equal(await Deno.readTextFile(`${directory}/records/missing-chain.json`), original);
    assert.equal(local.count(), 1);
  } finally {
    await actions.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ重试第二次在途被终止：重启只恢复unknown和次数，绝不继续或改原件", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-retry-interrupted-" });
  const enteredSecond = deferred<void>();
  const release = deferred<void>();
  let calls = 0;
  const local = service(async () => {
    calls++;
    if (calls === 2) enteredSecond.resolve();
    await release.promise;
    return Response.json({ id: "late-uncertain" });
  });
  const source = new URL("../adapters/qq/actions.ts", import.meta.url).href;
  const clientSource = new URL("../adapters/qq/client.ts", import.meta.url).href;
  const childPath = `${directory}/child.ts`;
  let child: Deno.ChildProcess | undefined;
  let restart: QqSendActions | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Deno.writeTextFile(
      childPath,
      `import {QqSendActions} from ${JSON.stringify(source)};\n` +
        `import {QqClient} from ${JSON.stringify(clientSource)};\n` +
        `const client=new QqClient({appId:"fixture",appSecret:"fixture"},{baseUrl:${
          JSON.stringify(local.url)
        },timeoutMs:150});\n` +
        `await client.authenticate();const actions=new QqSendActions(client,${
          JSON.stringify(directory + "/data")
        },"private",{retryDelaysMs:[0,0]});\n` +
        `await actions.initialize();await actions.send("second-inflight",${
          JSON.stringify(request)
        });await actions.close();\n`,
    );
    child = new Deno.Command(Deno.execPath(), {
      args: ["run", "--allow-read", "--allow-write", "--allow-net=127.0.0.1", childPath],
      stdout: "null",
      stderr: "null",
    }).spawn();
    await Promise.race([
      enteredSecond.promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error("第二次真实尝试未按时开始")), 5000);
      }),
    ]);
    clearTimeout(timer);
    timer = undefined;
    child.kill("SIGKILL");
    await child.status;
    child = undefined;
    const originals = new Map<string, string>();
    for (
      const path of [
        "records/second-inflight.json",
        "attempts/second-inflight/1.started.json",
        "attempts/second-inflight/1.result.json",
        "attempts/second-inflight/2.started.json",
      ]
    ) {
      originals.set(path, await Deno.readTextFile(`${directory}/data/${path}`));
    }
    assert.equal(JSON.parse(originals.get("records/second-inflight.json")!).status, "started");
    assert.equal(
      JSON.parse(originals.get("attempts/second-inflight/1.result.json")!).result.error
        .transportReason,
      "timeout",
    );
    restart = new QqSendActions(local.client(), `${directory}/data`, "private", {
      retryDelaysMs: [0, 0],
    });
    await restart.initialize();
    const expected = {
      actionId: "second-inflight",
      status: "unknown",
      reason: "interrupted",
      attemptCount: 2,
    };
    assert.deepEqual(restart.status("second-inflight"), expected);
    assert.deepEqual(
      await restart.send("second-inflight", request, {
        beforeAttempt: () => Promise.reject(new Error("maintenance pause")),
      }),
      expected,
    );
    assert.equal(calls, 2);
    for (const [path, bytes] of originals) {
      assert.equal(await Deno.readTextFile(`${directory}/data/${path}`), bytes);
    }
    await assert.rejects(
      Deno.stat(`${directory}/data/attempts/second-inflight/2.result.json`),
      Deno.errors.NotFound,
    );
  } finally {
    if (timer !== undefined) clearTimeout(timer);
    if (child) {
      try {
        child.kill("SIGKILL");
      } catch { /* 已停止 */ }
      await child.status;
    }
    release.resolve();
    await restart?.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ尝试结果存储失败立即停止，不发下一次和排队动作", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-attempt-save-fail-" });
  const release = deferred<void>();
  let calls = 0;
  const local = service(async () => {
    calls++;
    await Deno.rename(`${directory}/attempts/broken-chain`, `${directory}/preserved-attempts`);
    await Deno.writeTextFile(`${directory}/attempts/broken-chain`, "preserved storage failure");
    await release.promise;
    return Response.json({ id: "late" });
  });
  const client = local.client("all", 80);
  const actions = new QqSendActions(client, directory, "private", { retryDelaysMs: [0, 0] });
  try {
    await client.authenticate();
    await actions.initialize();
    const first = actions.send("broken-chain", request);
    const queued = actions.send("queued-after-broken", {
      ...request,
      reply: { messageId: "inbound-1" },
      text: "queued",
    });
    await assert.rejects(first, /storage/);
    await assert.rejects(queued, /storage/);
    assert.equal(calls, 1);
    assert.equal(actions.available, false);
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${directory}/records/broken-chain.json`)).status,
      "started",
    );
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${directory}/records/queued-after-broken.json`)).status,
      "started",
    );
    await assert.rejects(
      Deno.stat(`${directory}/attempts/queued-after-broken`),
      Deno.errors.NotFound,
    );
    await assert.rejects(actions.close(), /storage/);
  } finally {
    release.resolve();
    await actions.close().catch(() => {});
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("新策略在途被kill后仅新许可重入：固定防重键续链，成功才放行，永久拒绝仍未知屏障", async () => {
  for (const ending of ["sent", "permanent"] as const) {
    const directory = await Deno.makeTempDir({ prefix: "eve-qq-head-resume-" });
    const entered = deferred<void>(), release = deferred<void>();
    const bodies: { markdown: { content: string }; msg_id: string; msg_seq: number }[] = [];
    const local = service(async (r) => {
      const body = await r.json();
      bodies.push(body);
      if (bodies.length === 1) {
        entered.resolve();
        await release.promise;
      }
      if (ending === "permanent" && (body.markdown?.content ?? body.content) === "head") {
        return Response.json({ err_code: 40034105 }, { status: 400 });
      }
      return Response.json({ id: `actual-${(body.markdown?.content ?? body.content)}` });
    });
    const head = { ...request, reply: { messageId: "inbound-1" }, text: "head" };
    const tail = { ...request, text: "tail", reply: { messageId: "inbound-1", sequence: 2 } };
    let child: Deno.ChildProcess | undefined;
    let restart: QqSendActions | undefined;
    try {
      const script = `${directory}/child.ts`;
      await Deno.writeTextFile(
        script,
        `import {QqSendActions} from ${
          JSON.stringify(new URL("../adapters/qq/actions.ts", import.meta.url).href)
        };\n` +
          `import {QqClient} from ${
            JSON.stringify(new URL("../adapters/qq/client.ts", import.meta.url).href)
          };\n` +
          `const actions=new QqSendActions(new QqClient({appId:"fixture",appSecret:"fixture"},{baseUrl:${
            JSON.stringify(local.url)
          }}),${
            JSON.stringify(directory + "/data")
          });await actions.initialize();await actions.send("head",${JSON.stringify(head)});`,
      );
      child = new Deno.Command(Deno.execPath(), {
        args: ["run", "--allow-read", "--allow-write", "--allow-net=127.0.0.1", script],
        stdout: "null",
        stderr: "null",
      }).spawn();
      await entered.promise;
      child.kill("SIGKILL");
      await child.status;
      child = undefined;
      const startedBytes = await Deno.readTextFile(
        `${directory}/data/attempts/head/1.started.json`,
      );
      restart = new QqSendActions(local.client(), `${directory}/data`, "private", {
        retryDelaysMs: [0],
      });
      await restart.initialize();
      assert.equal(bodies.length, 1);
      assert.deepEqual(await restart.send("tail", tail), {
        actionId: "tail",
        status: "blocked",
        blockedBy: "head",
      });
      let checks = 0;
      const resumed = await restart.send("head", head, {
        beforeAttempt: () => {
          checks++;
          return Promise.resolve();
        },
      });
      assert.equal(checks, 1);
      assert.equal(resumed.attemptCount, 2);
      assert.equal(
        await Deno.readTextFile(`${directory}/data/attempts/head/1.started.json`),
        startedBytes,
      );
      const interrupted = JSON.parse(
        await Deno.readTextFile(`${directory}/data/attempts/head/1.interrupted.json`),
      );
      assert.ok(Number.isFinite(Date.parse(interrupted.interruptedAt)));
      assert.deepEqual(bodies[0], bodies[1]);
      if (ending === "sent") {
        assert.ok(resumed.status === "finished" && resumed.result.status === "sent");
        const sentTail = await restart.send("tail", tail);
        assert.ok(sentTail.status === "finished" && sentTail.result.status === "sent");
        assert.deepEqual(bodies.map((b) => b.markdown.content), ["head", "head", "tail"]);
      } else {
        assert.deepEqual(resumed, {
          actionId: "head",
          status: "unknown",
          reason: "interrupted",
          attemptCount: 2,
        });
        assert.equal(
          JSON.parse(await Deno.readTextFile(`${directory}/data/records/head.json`)).status,
          "started",
        );
        assert.deepEqual(await restart.send("head", head), resumed);
        assert.deepEqual(await restart.send("tail", tail), {
          actionId: "tail",
          status: "blocked",
          blockedBy: "head",
        });
        assert.equal(bodies.length, 2);
      }
      await restart.close();
      restart = new QqSendActions(local.client(), `${directory}/data`);
      await restart.initialize();
      assert.equal(bodies.length, ending === "sent" ? 3 : 2);
    } finally {
      if (child) {
        try {
          child.kill("SIGKILL");
        } catch { /* 已停止 */ }
        await child.status;
      }
      release.resolve();
      await restart?.close();
      await local.close();
      await Deno.remove(directory, { recursive: true });
    }
  }
});

Deno.test("未发消息的认证aborted可持新许可恢复，非被动文本尝试链重启仍合法", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-auth-aborted-resume-" });
  const entered = deferred<void>(), release = deferred<void>();
  let auths = 0, sends = 0;
  const platform = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    if (new URL(r.url).pathname === "/app/getAppAccessToken") {
      auths++;
      if (auths === 1) {
        entered.resolve();
        await release.promise;
      }
      return Response.json({ access_token: "fixture-token", expires_in: 7200 });
    }
    sends++;
    return Response.json({ id: "actual-active" });
  });
  const client = () =>
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${platform.addr.port}`,
    });
  let actions = new QqSendActions(client(), directory);
  const active = { ...request, reply: undefined };
  try {
    await actions.initialize();
    const running = actions.send("active", active);
    await entered.promise;
    await actions.close();
    assert.deepEqual(await running, {
      actionId: "active",
      status: "unknown",
      reason: "interrupted",
    });
    assert.equal(sends, 0);
    actions = new QqSendActions(client(), directory);
    await actions.initialize();
    const result = await actions.send("active", active);
    assert.deepEqual(result, {
      actionId: "active",
      status: "finished",
      attemptCount: 2,
      result: { status: "sent", messageId: "actual-active" },
    });
    assert.equal(sends, 1);
    await actions.close();
    actions = new QqSendActions(client(), directory);
    await actions.initialize();
    assert.deepEqual(actions.status("active"), result);
    assert.equal(sends, 1);
  } finally {
    release.resolve();
    await actions.close();
    await platform.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
