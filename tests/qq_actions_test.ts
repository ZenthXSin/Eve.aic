import assert from "node:assert/strict";
import { QqActionError, QqSendActions } from "../adapters/qq/actions.ts";
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
    client: () =>
      new QqClient({ appId: "fixture", appSecret: "fixture" }, {
        baseUrl: `http://127.0.0.1:${server.addr.port}`,
      }),
    count: () => sends,
    close: () => server.shutdown(),
  };
}

Deno.test("QQ 发送动作：网络前started已落盘，同键并发只发送一次，重启采用实际回执", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-actions-" });
  const local = service(async (r) => {
    assert.deepEqual(await r.json(), {
      content: request.text,
      msg_type: 0,
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
      actions.send("action-1", { ...request, text: "更改原动作" }),
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
    await assert.rejects(actions.send("bad-parameter", { ...request, text: "" }), /request/);
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
    const original = await Deno.readTextFile(path);
    assert.equal(JSON.parse(original).status, "started");
    child.kill("SIGKILL");
    await child.status;
    child = undefined;
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

Deno.test("QQ 发送动作：实际已发但回执存盘失败，不宣称完成也不再次发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-actions-" });
  const local = service(async () => {
    await Deno.rename(`${directory}/records`, `${directory}/preserved`);
    return Response.json({ id: "already-sent" });
  });
  const actions = new QqSendActions(local.client(), directory);
  let restart: QqSendActions | undefined;
  try {
    await actions.initialize();
    await assert.rejects(actions.send("save-failed", request), /storage/);
    await assert.rejects(actions.close(), /storage/);
    assert.equal(local.count(), 1);
    assert.equal(
      JSON.parse(await Deno.readTextFile(`${directory}/preserved/save-failed.json`)).status,
      "started",
    );
    await Deno.rename(`${directory}/preserved`, `${directory}/records`);
    restart = new QqSendActions(local.client(), directory);
    await restart.initialize();
    assert.deepEqual(await restart.send("save-failed", request), {
      actionId: "save-failed",
      status: "unknown",
      reason: "interrupted",
    });
    assert.equal(local.count(), 1);
  } finally {
    await actions.close().catch(() => {});
    await restart?.close();
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 发送动作：第二实例被锁拒绝，停止等已开始实际结果提交后释放锁", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-send-actions-" });
  const entered = deferred<void>();
  const release = deferred<void>();
  const local = service(async () => {
    entered.resolve();
    await release.promise;
    return Response.json({ id: "completed-before-unlock" });
  });
  const first = new QqSendActions(local.client(), directory);
  let restart: QqSendActions | undefined;
  try {
    await first.initialize();
    const sending = first.send("inflight", request);
    await entered.promise;
    assert.deepEqual(first.status("inflight"), { actionId: "inflight", status: "running" });
    await assert.rejects(new QqSendActions(local.client(), directory).initialize(), /unavailable/);
    const stopping = first.close();
    await assert.rejects(first.send("new-during-close", request), /unavailable/);
    await assert.rejects(new QqSendActions(local.client(), directory).initialize(), /unavailable/);
    release.resolve();
    const sent = await sending;
    await stopping;
    restart = new QqSendActions(local.client(), directory);
    await restart.initialize();
    assert.deepEqual(restart.status("inflight"), sent);
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
