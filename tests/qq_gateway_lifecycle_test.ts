import assert from "node:assert/strict";
import { QqClient, type QqGatewayConnection } from "../adapters/qq/client.ts";
import type { GatewaySession } from "../adapters/qq/checkpoint.ts";
import { QqGateway, type QqGatewayStatus } from "../adapters/qq/gateway.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => resolve = done);
  return { promise, resolve };
}

/** Simulates a transport that never emits close, including already-CLOSED sockets. */
class SilentCloseSocket {
  readyState: number = WebSocket.OPEN;
  onopen: WebSocket["onopen"] = null;
  onerror: WebSocket["onerror"] = null;
  onclose: WebSocket["onclose"] = null;
  #onmessage: WebSocket["onmessage"] = null;
  attached = deferred<void>();
  closing = deferred<void>();
  sent: unknown[] = [];
  closes = 0;

  constructor(readonly closesImmediately = false) {}
  get onmessage() {
    return this.#onmessage;
  }
  set onmessage(handler: WebSocket["onmessage"]) {
    this.#onmessage = handler;
    if (handler) this.attached.resolve();
  }
  send(value: string) {
    this.sent.push(JSON.parse(value));
  }
  close() {
    this.closes++;
    this.readyState = this.closesImmediately ? WebSocket.CLOSED : WebSocket.CLOSING;
    this.closing.resolve();
  }
  emit(payload: unknown, handler = this.onmessage) {
    handler?.call(
      this as unknown as WebSocket,
      new MessageEvent("message", { data: JSON.stringify(payload) }),
    );
  }
  hello(interval = 30_000) {
    this.emit({ op: 10, d: { heartbeat_interval: interval } });
  }
}

async function within<T>(promise: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error("gateway lifecycle did not finish")), 5_000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

function fixture(
  setup: (socket: SilentCloseSocket, number: number) => void,
  closesImmediately = false,
) {
  const client = new QqClient({ appId: "local-fixture", appSecret: "local-fixture" });
  const sockets: SilentCloseSocket[] = [];
  const authentications: Array<GatewaySession | undefined> = [];
  client.openGateway = () => {
    const socket = new SilentCloseSocket(closesImmediately);
    sockets.push(socket);
    void socket.attached.promise.then(() => setup(socket, sockets.length));
    return Promise.resolve(
      {
        socket: socket as unknown as WebSocket,
        authenticate: (_intents, session) => {
          authentications.push(session);
          socket.emit(
            session
              ? { op: 0, t: "RESUMED", s: session.sequence, d: {} }
              : { op: 0, t: "READY", s: 10, d: { session_id: "durable-session" } },
          );
        },
      } satisfies QqGatewayConnection,
    );
  };
  return { client, sockets, authentications };
}

for (
  const cause of ["server_reconnect", "transport_error", "connection_timeout", "heartbeat_timeout"]
) {
  Deno.test(`QQ lifecycle: missing close callback still reconnects after ${cause}`, async () => {
    const controller = new AbortController();
    const statuses: QqGatewayStatus[] = [];
    let session: GatewaySession | undefined;
    const local = fixture((socket, number) => {
      if (number === 1 && cause === "connection_timeout") return;
      socket.hello(cause === "heartbeat_timeout" ? 20 : 30_000);
    });
    const gateway = new QqGateway(local.client, {
      intents: 1 << 25,
      closeTimeoutMs: 20,
      connectTimeoutMs: cause === "connection_timeout" ? 20 : 1_000,
      initialBackoffMs: 1,
      checkpoint: {
        load: () => Promise.resolve(session),
        save: (next) => {
          session = next;
          return Promise.resolve();
        },
      },
      onDispatch: () => Promise.resolve(),
      onStatus: (status) => {
        statuses.push(status);
        if (status.status === "ready" && local.sockets.length === 1) {
          const socket = local.sockets[0];
          if (cause === "server_reconnect") socket.emit({ op: 7 });
          if (cause === "transport_error") {
            socket.onerror?.call(socket as unknown as WebSocket, new Event("error"));
          }
        }
        if (local.sockets.length === 2 && ["ready", "resumed"].includes(status.status)) {
          controller.abort();
        }
      },
    });
    try {
      await within(gateway.run(controller.signal));
      assert.equal(local.sockets.length, 2);
      assert.deepEqual(statuses.find((s) => s.status === "disconnected"), {
        status: "disconnected",
        code: 4000,
        reason: cause,
        closeTimedOut: true,
      });
      assert.deepEqual(
        local.authentications[1],
        cause === "connection_timeout" ? undefined : { sessionId: "durable-session", sequence: 10 },
      );
      for (const socket of local.sockets) {
        assert.equal(socket.closes, 1);
        assert.equal(socket.onmessage, null);
        assert.equal(socket.onclose, null);
        assert.equal(socket.onerror, null);
        assert.equal(socket.onopen, null);
      }
    } finally {
      controller.abort();
    }
  });
}

Deno.test("QQ lifecycle: already CLOSED without callback does not stall shutdown", async () => {
  const controller = new AbortController();
  const local = fixture((socket) => {
    socket.readyState = WebSocket.CLOSED;
    controller.abort();
  });
  try {
    await within(new QqGateway(local.client, {
      intents: 1 << 25,
      closeTimeoutMs: 20,
      checkpoint: { load: () => Promise.resolve(undefined), save: () => Promise.resolve() },
      onDispatch: () => Promise.resolve(),
    }).run(controller.signal));
    assert.equal(local.sockets[0].closes, 0);
    assert.equal(local.sockets[0].onmessage, null);
  } finally {
    controller.abort();
  }
});

Deno.test("QQ lifecycle: drain durable READY before Resume and fence late old callbacks", async () => {
  const controller = new AbortController();
  const entered = deferred<void>();
  const releaseSave = deferred<void>();
  const statuses: QqGatewayStatus[] = [];
  let session: GatewaySession | undefined;
  let consumed = 0;
  let oldMessage: WebSocket["onmessage"];
  let oldOpen: WebSocket["onopen"];
  let oldError: WebSocket["onerror"];
  let oldClose: WebSocket["onclose"];
  const local = fixture((socket, number) => {
    if (number === 1) {
      oldMessage = socket.onmessage;
      oldOpen = socket.onopen;
      oldError = socket.onerror;
      oldClose = socket.onclose;
    } else {
      const old = local.sockets[0];
      oldOpen?.call(old as unknown as WebSocket, new Event("open"));
      oldError?.call(old as unknown as WebSocket, new Event("error"));
      oldClose?.call(old as unknown as WebSocket, new CloseEvent("close", { code: 4009 }));
      old.emit({ op: 0, t: "READY", s: 99, d: { session_id: "stale" } }, oldMessage);
      old.emit({ op: 1 }, oldMessage);
    }
    socket.hello();
  });
  const running = new QqGateway(local.client, {
    intents: 1 << 25,
    closeTimeoutMs: 20,
    initialBackoffMs: 1,
    checkpoint: {
      load: () => Promise.resolve(session),
      save: (next) => {
        session = next;
        return Promise.resolve();
      },
    },
    onDispatch: async (event) => {
      consumed++;
      if (event.t === "READY") {
        entered.resolve();
        await releaseSave.promise;
      }
    },
    onStatus: (status) => {
      statuses.push(status);
      if (status.status === "resumed") controller.abort();
    },
  }).run(controller.signal);
  try {
    await within(entered.promise);
    const old = local.sockets[0];
    old.emit({ op: 7 });
    await old.closing.promise;
    await new Promise((resolve) => setTimeout(resolve, 60));
    assert.equal(local.sockets.length, 1, "reconnect must wait for durable consumption");
    assert.equal(session, undefined);
    assert.equal(statuses.some((s) => s.status === "ready"), false);
    releaseSave.resolve();
    await within(running);
    assert.deepEqual(session, { sessionId: "durable-session", sequence: 10 });
    assert.deepEqual(local.authentications[1], session);
    assert.equal(consumed, 1, "duplicate RESUMED and stale READY must not be consumed");
    assert.equal(statuses.some((s) => s.status === "connected" || s.status === "ready"), false);
    assert.equal(old.sent.length, 0, "closed connection must never send another heartbeat");
    assert.equal(old.closes, 1);
  } finally {
    releaseSave.resolve();
    controller.abort();
    await within(running).catch(() => {});
  }
});
