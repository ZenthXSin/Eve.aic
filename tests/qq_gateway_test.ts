import assert from "node:assert/strict";
import { QqClient, QqRequestError } from "../adapters/qq/client.ts";
import {
  FileGatewayCheckpoint,
  type GatewayCheckpoint,
  type GatewaySession,
} from "../adapters/qq/checkpoint.ts";
import { QqGateway, QqGatewayError } from "../adapters/qq/gateway.ts";
import { qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqReceiver } from "../adapters/qq/receiver.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
function emit(socket: WebSocket, data: unknown) {
  socket.send(JSON.stringify(data));
}
function ready(socket: WebSocket, session = "gateway-session", sequence = 0) {
  emit(socket, { op: 0, t: "READY", s: sequence, d: { session_id: session } });
}
function event(sequence: number, message = "message-1") {
  return {
    op: 0 as const,
    t: "C2C_MESSAGE_CREATE",
    s: sequence,
    d: {
      id: message,
      author: { user_openid: "user-platform", username: "用户" },
      content: "真实本地私信",
      message_scene: { ext: [`msg_idx=${message}-index`] },
    },
  };
}

function gatewayServer(
  onConnection: (socket: WebSocket, connection: number) => void,
  heartbeatIntervalMs = 30,
) {
  let connections = 0;
  let identities = 0;
  let tokens = 0;
  const sockets: WebSocket[] = [];
  const server = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    (request): Response => {
      const path = new URL(request.url).pathname;
      if (path === "/app/getAppAccessToken") {
        tokens++;
        return Response.json({ access_token: "fixture-gateway-secret", expires_in: 7200 });
      }
      if (path === "/gateway") {
        identities++;
        assert.equal(request.headers.get("Authorization"), "QQBot fixture-gateway-secret");
        return Response.json({
          url: `ws://127.0.0.1:${server.addr.port}/websocket`,
          shards: 1,
          session_start_limit: { remaining: 10 },
        });
      }
      const { socket, response } = Deno.upgradeWebSocket(request);
      sockets.push(socket);
      const number = ++connections;
      socket.onopen = () =>
        emit(socket, { op: 10, d: { heartbeat_interval: heartbeatIntervalMs } });
      onConnection(socket, number);
      return response;
    },
  );
  return {
    client: () =>
      new QqClient({ appId: "fixture", appSecret: "fixture" }, {
        baseUrl: `http://127.0.0.1:${server.addr.port}`,
        sendScope: "private",
      }),
    count: () => ({ connections, identities, tokens }),
    close: async () => {
      for (const socket of sockets) {
        try {
          socket.close();
        } catch { /* 已关闭 */ }
      }
      await server.shutdown();
    },
  };
}

const durations = {
  initialBackoffMs: 1,
  maxBackoffMs: 5,
  connectTimeoutMs: 500,
  invalidSessionBackoffMs: 5,
};

Deno.test("QQ 网关：Identify心跳继续，事件落盘前不前移恢复序号，断线后串行Resume", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-gateway-test-" });
  const controller = new AbortController();
  const handlerEntered = deferred<void>();
  const allowSave = deferred<void>();
  const handledSecond = deferred<void>();
  let heartbeatWhileSaving = false;
  let requestedReconnect = false;
  const auth: Record<string, unknown>[] = [];
  const local = gatewayServer((socket, connection) => {
    socket.onmessage = (incoming) => {
      const payload = JSON.parse(incoming.data);
      if (payload.op === 1) {
        if (connection === 1 && payload.d === 0) {
          heartbeatWhileSaving = true;
          if (!requestedReconnect) {
            requestedReconnect = true;
            emit(socket, { op: 7 });
          }
        }
        emit(socket, { op: 11 });
      } else {
        auth.push(payload);
        assert.equal(payload.d.token, "QQBot fixture-gateway-secret");
        if (connection === 1) {
          assert.equal(payload.op, 2);
          assert.equal(payload.d.intents, 1 << 25);
          ready(socket);
          emit(socket, event(1));
        } else {
          assert.equal(payload.op, 6);
          assert.equal(payload.d.seq, 1);
          emit(socket, { op: 0, t: "RESUMED", s: 1, d: "" });
          emit(socket, event(1)); // 恢复重复网关水位不会重新调用消费。
          emit(socket, event(2, "message-2"));
        }
      }
    };
  });
  const journal = new QqInboundJournal(`${directory}/inbound`);
  const checkpoint = new FileGatewayCheckpoint(`${directory}/gateway/session.json`);
  let consumerCalls = 0;
  let running: Promise<void> | undefined;
  try {
    await journal.initialize();
    const gateway = new QqGateway(local.client(), {
      ...durations,
      intents: 1 << 25,
      checkpoint,
      onDispatch: async (dispatch) => {
        if (dispatch.t === "READY") return;
        consumerCalls++;
        if (dispatch.s === 1) {
          handlerEntered.resolve();
          await allowSave.promise;
        }
        const message = qqMessage(dispatch);
        if (message) await journal.append(message, dispatch);
        if (dispatch.s === 2) handledSecond.resolve();
      },
    });
    running = gateway.run(controller.signal);
    await handlerEntered.promise;
    assert.equal((await checkpoint.load())?.sequence, 0);
    await new Promise((r) => setTimeout(r, 40));
    assert.equal(heartbeatWhileSaving, true);
    // 断线发生在消费仍等待时，已接收事件保存完才允许第二连接。
    allowSave.resolve();
    await handledSecond.promise;
    controller.abort();
    await running;
    assert.equal((await checkpoint.load())?.sequence, 2);
    assert.equal(consumerCalls, 2);
    assert.equal(journal.records().length, 2);
    assert.equal(local.count().connections, 2);
  } finally {
    allowSave.resolve();
    controller.abort();
    await running?.catch(() => {});
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 网关：队列满停收而不跨越未保存事件，恢复后补发每条一次", async () => {
  let session: GatewaySession | undefined;
  const checkpoint: GatewayCheckpoint = {
    load: () => Promise.resolve(session),
    save: (next) => {
      session = next;
      return Promise.resolve();
    },
  };
  const firstEntered = deferred<void>();
  const allowFirst = deferred<void>();
  const finished = deferred<void>();
  const consumed: number[] = [];
  const controller = new AbortController();
  const local = gatewayServer((socket, connection) => {
    socket.onmessage = (incoming) => {
      const payload = JSON.parse(incoming.data);
      if (payload.op === 1) {
        emit(socket, { op: 11 });
        return;
      }
      if (connection === 1) ready(socket, "session", 0);
      else {
        assert.equal(payload.op, 6);
        assert.ok(payload.d.seq < 96);
        emit(socket, { op: 0, t: "RESUMED", s: payload.d.seq, d: "" });
      }
      const start = connection === 1 ? 1 : payload.d.seq + 1;
      for (let i = start; i <= 96; i++) emit(socket, event(i, `burst-${i}`));
    };
  });
  let running: Promise<void> | undefined;
  try {
    const gateway = new QqGateway(local.client(), {
      ...durations,
      intents: 1 << 25,
      checkpoint,
      onDispatch: async (dispatch) => {
        if (dispatch.t !== "C2C_MESSAGE_CREATE") return;
        if (dispatch.s === 1) {
          firstEntered.resolve();
          await allowFirst.promise;
        }
        consumed.push(dispatch.s);
        if (dispatch.s === 96) finished.resolve();
      },
    });
    running = gateway.run(controller.signal);
    await firstEntered.promise;
    await new Promise((resolve) => setTimeout(resolve, 25));
    assert.equal(session?.sequence, 0);
    allowFirst.resolve();
    await finished.promise;
    controller.abort();
    await running;
    assert.deepEqual(consumed, Array.from({ length: 96 }, (_, i) => i + 1));
    assert.equal(session?.sequence, 96);
    assert.ok(local.count().connections >= 2);
  } finally {
    allowFirst.resolve();
    controller.abort();
    await running?.catch(() => {});
    await local.close();
  }
});

Deno.test("QQ 网关：取消仍等已开始消费保存，不把持久化失败吞成正常停止", async () => {
  let session: GatewaySession | undefined;
  const checkpoint: GatewayCheckpoint = {
    load: () => Promise.resolve(session),
    save: (next) => {
      if (next?.sequence === 2) return Promise.reject(new Error("save failed"));
      session = next;
      return Promise.resolve();
    },
  };
  const entered = deferred<void>();
  const release = deferred<void>();
  const controller = new AbortController();
  const local = gatewayServer((socket) => {
    socket.onmessage = (incoming) => {
      const data = JSON.parse(incoming.data);
      if (data.op === 1) emit(socket, { op: 11 });
      if (data.op === 2) {
        ready(socket, "session", 1);
        emit(socket, event(2));
      }
    };
  });
  let running: Promise<void> | undefined;
  try {
    const gateway = new QqGateway(local.client(), {
      ...durations,
      intents: 1 << 25,
      checkpoint,
      onDispatch: async (dispatch) => {
        if (dispatch.t === "READY") return;
        entered.resolve();
        await release.promise;
      },
    });
    running = gateway.run(controller.signal);
    await entered.promise;
    controller.abort();
    release.resolve();
    await assert.rejects(
      running,
      (e) => e instanceof QqGatewayError && e.category === "checkpoint",
    );
    assert.equal(session?.sequence, 1);
  } finally {
    release.resolve();
    controller.abort();
    await running?.catch(() => {});
    await local.close();
  }
});

Deno.test("QQ 网关：处理器失败不推进checkpoint、不重连偷偷跳过", async () => {
  let session: GatewaySession | undefined;
  const saved: GatewaySession[] = [];
  const checkpoint: GatewayCheckpoint = {
    load: () => Promise.resolve(session),
    save: (next) => {
      session = next;
      if (next) saved.push(next);
      return Promise.resolve();
    },
  };
  const local = gatewayServer((socket) => {
    socket.onmessage = (incoming) => {
      const data = JSON.parse(incoming.data);
      if (data.op === 1) emit(socket, { op: 11 });
      if (data.op === 2) {
        ready(socket, "session", 1);
        emit(socket, event(2));
        emit(socket, event(3, "later"));
      }
    };
  });
  try {
    const gateway = new QqGateway(local.client(), {
      ...durations,
      intents: 1 << 25,
      checkpoint,
      onDispatch: (dispatch) => {
        if (dispatch.t !== "READY") return Promise.reject(new Error("private payload secret"));
        return Promise.resolve();
      },
    });
    await assert.rejects(
      gateway.run(new AbortController().signal),
      (e) =>
        e instanceof QqGatewayError && e.category === "handler" && !e.message.includes("secret"),
    );
    assert.deepEqual(saved.map((s) => s.sequence), [1]);
    assert.equal(local.count().connections, 1);
  } finally {
    await local.close();
  }
});

Deno.test("QQ 网关：消费落盘后checkpoint失败保留旧序号，平台补发复用入站原件", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-gateway-test-" });
  const journal = new QqInboundJournal(directory);
  let session: GatewaySession | undefined;
  const checkpoint: GatewayCheckpoint = {
    load: () => Promise.resolve(session),
    save: (next) => {
      if (next?.sequence === 2) return Promise.reject(new Error("disk failure"));
      session = next;
      return Promise.resolve();
    },
  };
  const local = gatewayServer((socket) => {
    socket.onmessage = (incoming) => {
      const data = JSON.parse(incoming.data);
      if (data.op === 1) emit(socket, { op: 11 });
      if (data.op === 2) {
        ready(socket, "session", 1);
        emit(socket, event(2));
      }
    };
  });
  try {
    await journal.initialize();
    const gateway = new QqGateway(local.client(), {
      ...durations,
      intents: 1 << 25,
      checkpoint,
      onDispatch: async (dispatch) => {
        const message = qqMessage(dispatch);
        if (message) await journal.append(message, dispatch);
      },
    });
    await assert.rejects(
      gateway.run(new AbortController().signal),
      (e) => e instanceof QqGatewayError && e.category === "checkpoint",
    );
    assert.equal(session?.sequence, 1);
    assert.equal(journal.records().length, 1);
    const restart = new QqInboundJournal(directory);
    await restart.initialize();
    const replay = event(2);
    assert.equal((await restart.append(qqMessage(replay)!, replay)).fresh, false);
  } finally {
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 网关：InvalidSession清持久态后Identify，权限拒绝停止，不无限重连", async () => {
  let session: GatewaySession | undefined = { sessionId: "old", sequence: 8 };
  const saved: Array<GatewaySession | undefined> = [];
  const checkpoint: GatewayCheckpoint = {
    load: () => Promise.resolve(session),
    save: (next) => {
      session = next;
      saved.push(next);
      return Promise.resolve();
    },
  };
  const local = gatewayServer((socket, number) => {
    socket.onmessage = (incoming) => {
      const data = JSON.parse(incoming.data);
      if (data.op === 1) emit(socket, { op: 11 });
      else if (number === 1) {
        assert.equal(data.op, 6);
        emit(socket, { op: 9, d: false });
      } else {
        assert.equal(data.op, 2);
        socket.close(4014, "disallowed intents");
      }
    };
  });
  try {
    const gateway = new QqGateway(local.client(), {
      ...durations,
      intents: 1 << 25,
      checkpoint,
      onDispatch: () => Promise.resolve(),
    });
    await assert.rejects(
      gateway.run(new AbortController().signal),
      (e) => e instanceof QqGatewayError && e.code === 4014,
    );
    assert.deepEqual(saved, [undefined]);
    assert.equal(local.count().connections, 2);
  } finally {
    await local.close();
  }
});

Deno.test("QQ 网关接入点：拒绝第三方主机和明文非本机地址，不泄露凭据", async () => {
  let address = "wss://untrusted.invalid/websocket";
  const server = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    (request): Response => {
      if (new URL(request.url).pathname === "/app/getAppAccessToken") {
        return Response.json({ access_token: "fixture-secret", expires_in: 7200 });
      }
      return Response.json({ url: address });
    },
  );
  try {
    const client = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${server.addr.port}`,
    });
    for (
      const url of [
        "wss://untrusted.invalid/websocket",
        "ws://remote.invalid/websocket",
        "wss://fixture-user:fixture-password@127.0.0.1/websocket",
      ]
    ) {
      address = url;
      await assert.rejects(
        client.openGateway(),
        (e) => e instanceof Error && !e.message.includes("fixture-password"),
      );
    }
    client.close();
  } finally {
    await server.shutdown();
  }
});

Deno.test("QQ 网关水位：重启恢复，损坏文件保留，存盘失败不能伪装消费成功", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-gateway-cp-" });
  const path = `${directory}/checkpoint.json`;
  try {
    const checkpoint = new FileGatewayCheckpoint(path);
    assert.equal(await checkpoint.load(), undefined);
    await checkpoint.save({ sessionId: "session", sequence: 0 });
    const restart = new FileGatewayCheckpoint(path);
    assert.deepEqual(await restart.load(), { sessionId: "session", sequence: 0 });
    assert.equal((await Deno.stat(path)).mode! & 0o777, 0o600);
    await restart.save(undefined);
    assert.equal(await new FileGatewayCheckpoint(path).load(), undefined);
    await Deno.writeTextFile(path, "broken private evidence");
    await assert.rejects(new FileGatewayCheckpoint(path).load(), /corrupt/);
    assert.equal(await Deno.readTextFile(path), "broken private evidence");
    const bad = new FileGatewayCheckpoint(`${directory}/blocked/file`);
    await Deno.writeTextFile(`${directory}/blocked`, "file instead of directory");
    await assert.rejects(bad.save({ sessionId: "session", sequence: 2 }), /storage/);
    await assert.rejects(bad.save({ sessionId: "session", sequence: 2 }), /storage/);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 独立接收器：目录锁挡住第二实例，已持久化消息跨新实例恢复", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-receiver-test-" });
  const readySignal = deferred<void>();
  const controller = new AbortController();
  const local = gatewayServer((socket, connection) => {
    socket.onmessage = (incoming) => {
      const data = JSON.parse(incoming.data);
      if (data.op === 1) emit(socket, { op: 11 });
      else if (connection === 1) {
        ready(socket, "receiver-session", 0);
        emit(socket, event(1));
      } else {
        assert.equal(data.op, 6);
        assert.equal(data.d.seq, 1);
        emit(socket, { op: 0, t: "RESUMED", s: 1, d: "" });
      }
    };
  });
  let running: Promise<void> | undefined;
  try {
    const first = new QqReceiver(local.client(), directory);
    running = first.run(controller.signal, (status) => {
      if (status.status === "ready") readySignal.resolve();
    });
    await readySignal.promise;
    const second = new QqReceiver(local.client(), directory);
    await assert.rejects(second.run(new AbortController().signal), /已有 QQ 接收器/);
    assert.equal(local.count().connections, 1);
    const checkpoint = new FileGatewayCheckpoint(`${directory}/gateway/session.json`);
    while ((await checkpoint.load())?.sequence !== 1) await new Promise((r) => setTimeout(r, 1));
    controller.abort();
    await running;
    assert.equal(first.messages().length, 1);
    const resumedController = new AbortController();
    const restart = new QqReceiver(local.client(), directory);
    await restart.run(resumedController.signal, (status) => {
      if (status.status === "resumed") resumedController.abort();
    });
    assert.equal(restart.messages().length, 1);
    assert.equal(local.count().connections, 2);
  } finally {
    controller.abort();
    await running?.catch(() => {});
    await local.close();
    await Deno.remove(directory, { recursive: true });
  }
});

for (const mode of ["timeout", "resumable", "invalid"] as const) {
  Deno.test(`QQ 网关恢复：${mode}按官方规则保留或清理水位及凭据`, async () => {
    let session: GatewaySession | undefined = { sessionId: "old", sequence: 8 };
    const saved: Array<GatewaySession | undefined> = [];
    const controller = new AbortController();
    let disconnectedAt = 0;
    let reconnectedAt = 0;
    const checkpoint: GatewayCheckpoint = {
      load: () => Promise.resolve(session),
      save: (next) => {
        session = next;
        saved.push(next);
        return Promise.resolve();
      },
    };
    const local = gatewayServer((socket, number) => {
      socket.onmessage = (incoming) => {
        const data = JSON.parse(incoming.data);
        if (data.op === 1) {
          emit(socket, { op: 11 });
          return;
        }
        if (number === 1) {
          assert.equal(data.op, 6);
          assert.equal(data.d.seq, 8);
          if (mode === "timeout") socket.close(4009, "session timed out");
          else emit(socket, { op: 9, d: mode === "resumable" });
        } else {
          reconnectedAt = performance.now();
          if (mode === "resumable") {
            assert.equal(data.op, 6);
            assert.equal(data.d.session_id, "old");
            assert.equal(data.d.seq, 8);
            emit(socket, { op: 0, t: "RESUMED", s: 8, d: "" });
          } else {
            assert.equal(data.op, 2);
            ready(socket, "new", 0);
          }
        }
      };
    });
    try {
      const gateway = new QqGateway(local.client(), {
        ...durations,
        invalidSessionBackoffMs: 40,
        intents: 1 << 25,
        checkpoint,
        onDispatch: () => Promise.resolve(),
        onStatus: (status) => {
          if (status.status === "disconnected") disconnectedAt = performance.now();
          if (status.status === "ready" || status.status === "resumed") controller.abort();
        },
      });
      await gateway.run(controller.signal);
      assert.equal(local.count().connections, 2);
      assert.equal(local.count().tokens, mode === "resumable" ? 1 : 2);
      if (mode === "resumable") assert.deepEqual(saved, []);
      else assert.deepEqual(saved, [undefined, { sessionId: "new", sequence: 0 }]);
      if (mode !== "timeout") assert.ok(reconnectedAt - disconnectedAt >= 35);
    } finally {
      controller.abort();
      await local.close();
    }
  });
}

Deno.test("QQ 网关心跳：READY后的确认有完整周期，服务器请求不误判未ACK为断线", async () => {
  const controller = new AbortController();
  let session: GatewaySession | undefined;
  const received: number[] = [];
  let requested = false;
  let running: Promise<void> | undefined;
  const timers: Array<ReturnType<typeof setTimeout>> = [];
  const local = gatewayServer((socket) => {
    socket.onmessage = (incoming) => {
      const data = JSON.parse(incoming.data);
      if (data.op === 2) {
        // READY late in HELLO's first interval exposed the old early-ACK deadline.
        timers.push(setTimeout(() => ready(socket), 150));
      } else if (data.op === 1) {
        received.push(performance.now());
        if (!requested) {
          requested = true;
          timers.push(setTimeout(() => emit(socket, { op: 1 }), 10));
          timers.push(setTimeout(() => emit(socket, { op: 11 }), 100));
        } else if (received.length === 2) {
          // The explicit server request is answered while the first ACK is pending.
        } else {
          emit(socket, { op: 11 });
          controller.abort();
        }
      }
    };
  }, 200);
  try {
    const gateway = new QqGateway(local.client(), {
      ...durations,
      intents: 1 << 25,
      checkpoint: {
        load: () => Promise.resolve(session),
        save: (next) => {
          session = next;
          return Promise.resolve();
        },
      },
      onDispatch: () => Promise.resolve(),
    });
    running = gateway.run(controller.signal);
    await running;
    assert.equal(local.count().connections, 1);
    assert.equal(received.length, 3);
    assert.ok(received[1] - received[0] < 100);
    assert.ok(received[2] - received[0] >= 180);
  } finally {
    for (const timer of timers) clearTimeout(timer);
    controller.abort();
    await running?.catch(() => {});
    await local.close();
  }
});

Deno.test("QQ 网关发现：认证和gateway的HTTP 5xx自动恢复，明确4xx结束", async () => {
  let authFailures = 1;
  let gatewayFailures = 1;
  let gatewayStatus = 503;
  let session: GatewaySession | undefined;
  const controller = new AbortController();
  const server = Deno.serve(
    { hostname: "127.0.0.1", port: 0, onListen() {} },
    (request): Response => {
      const path = new URL(request.url).pathname;
      if (path === "/app/getAppAccessToken") {
        if (authFailures-- > 0) return Response.json({ code: 90001 }, { status: 502 });
        return Response.json({ access_token: "fixture", expires_in: 7200 });
      }
      if (path === "/gateway") {
        if (gatewayFailures-- > 0) return Response.json({ code: 90002 }, { status: gatewayStatus });
        return Response.json({ url: `ws://127.0.0.1:${server.addr.port}/websocket` });
      }
      const { socket, response } = Deno.upgradeWebSocket(request);
      socket.onopen = () => emit(socket, { op: 10, d: { heartbeat_interval: 200 } });
      socket.onmessage = (incoming) => {
        const data = JSON.parse(incoming.data);
        if (data.op === 2) ready(socket);
        else if (data.op === 1) emit(socket, { op: 11 });
      };
      return response;
    },
  );
  const client = () =>
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: `http://127.0.0.1:${server.addr.port}`,
    });
  const checkpoint: GatewayCheckpoint = {
    load: () => Promise.resolve(session),
    save: (next) => {
      session = next;
      return Promise.resolve();
    },
  };
  try {
    await new QqGateway(client(), {
      ...durations,
      intents: 1 << 25,
      checkpoint,
      onDispatch: () => Promise.resolve(),
      onStatus: (status) => {
        if (status.status === "ready") controller.abort();
      },
    }).run(controller.signal);
    assert.ok(authFailures < 0);
    assert.equal(gatewayFailures, -1);
    gatewayFailures = 1;
    gatewayStatus = 403;
    await assert.rejects(
      new QqGateway(client(), {
        ...durations,
        intents: 1 << 25,
        checkpoint,
        onDispatch: () => Promise.resolve(),
      }).run(new AbortController().signal),
      (error) => error instanceof QqRequestError && error.info.httpStatus === 403,
    );
    assert.equal(gatewayFailures, 0);
  } finally {
    controller.abort();
    await server.shutdown();
  }
});

Deno.test("QQ 网关心跳：持续服务器请求不能延长未确认心跳的期限", async () => {
  const controller = new AbortController();
  let session: GatewaySession | undefined;
  let firstHeartbeatAt = 0;
  let reconnectAt = 0;
  let requestTimer: ReturnType<typeof setInterval> | undefined;
  let beats = 0;
  const local = gatewayServer((socket, number) => {
    socket.onmessage = (incoming) => {
      const data = JSON.parse(incoming.data);
      if (data.op === 2 && number === 1) ready(socket);
      else if (data.op === 1 && number === 1) {
        beats++;
        if (!firstHeartbeatAt) {
          firstHeartbeatAt = performance.now();
          requestTimer = setInterval(() => {
            if (socket.readyState === WebSocket.OPEN) emit(socket, { op: 1 });
          }, 15);
        }
      } else if (number === 2 && data.op === 6) {
        reconnectAt = performance.now();
        emit(socket, { op: 0, t: "RESUMED", s: 0, d: "" });
      }
    };
  }, 100);
  try {
    await new QqGateway(local.client(), {
      ...durations,
      intents: 1 << 25,
      checkpoint: {
        load: () => Promise.resolve(session),
        save: (next) => {
          session = next;
          return Promise.resolve();
        },
      },
      onDispatch: () => Promise.resolve(),
      onStatus: (status) => {
        if (status.status === "resumed") controller.abort();
      },
    }).run(controller.signal);
    assert.ok(beats > 2);
    assert.equal(local.count().connections, 2);
    assert.ok(reconnectAt - firstHeartbeatAt >= 90);
    assert.ok(reconnectAt - firstHeartbeatAt < 400);
  } finally {
    clearInterval(requestTimer);
    controller.abort();
    await local.close();
  }
});
