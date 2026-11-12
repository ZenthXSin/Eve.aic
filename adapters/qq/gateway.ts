// 网关只接收事件并监督连接，不调用模型或发消息。消费完成指回调已持久化事件，不代表 Eve 已回复。
import { QqClient, type QqGatewayConnection, QqRequestError } from "./client.ts";
import type { GatewayCheckpoint, GatewaySession } from "./checkpoint.ts";
import type { QqDispatch } from "./inbound.ts";

export class QqGatewayError extends Error {
  constructor(
    readonly category: "protocol" | "handler" | "checkpoint" | "closed",
    readonly code?: number,
  ) {
    super(`QQ 网关未完成：${category}${code === undefined ? "" : ` (${code})`}`);
    this.name = "QqGatewayError";
  }
}

export interface QqGatewayOptions {
  /** 只选需接收的事件。C2C 为 1<<25；发送范围另在传输层设置。 */
  intents: number;
  onDispatch(event: QqDispatch): Promise<void>;
  checkpoint: GatewayCheckpoint;
  onStatus?(
    event: {
      status: "connecting" | "connected" | "hello" | "ready" | "resumed" | "disconnected";
      code?: number;
    },
  ): void;
  connectTimeoutMs?: number;
  initialBackoffMs?: number;
  maxBackoffMs?: number;
  rateLimitBackoffMs?: number;
  stableConnectionMs?: number;
  invalidSessionBackoffMs?: number;
}

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function wait(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const finish = () => {
      clearTimeout(timer);
      signal.removeEventListener("abort", finish);
      resolve();
    };
    const timer = setTimeout(finish, ms);
    signal.addEventListener("abort", finish, { once: true });
    if (signal.aborted) finish();
  });
}

export class QqGateway {
  #running = false;
  #session?: GatewaySession;
  #options: Required<Omit<QqGatewayOptions, "onStatus">> & Pick<QqGatewayOptions, "onStatus">;

  constructor(readonly client: QqClient, options: QqGatewayOptions) {
    this.#options = {
      connectTimeoutMs: 30_000,
      initialBackoffMs: 1000,
      maxBackoffMs: 60_000,
      rateLimitBackoffMs: 60_000,
      stableConnectionMs: 5_000,
      invalidSessionBackoffMs: 3_000,
      ...options,
    };
    if (!Number.isSafeInteger(options.intents) || options.intents < 0) {
      throw new QqGatewayError("protocol");
    }
    for (
      const duration of [
        this.#options.connectTimeoutMs,
        this.#options.initialBackoffMs,
        this.#options.maxBackoffMs,
        this.#options.rateLimitBackoffMs,
        this.#options.stableConnectionMs,
        this.#options.invalidSessionBackoffMs,
      ]
    ) {
      if (!Number.isSafeInteger(duration) || duration < 1 || duration > 2_147_483_647) {
        throw new QqGatewayError("protocol");
      }
    }
  }

  async #checkpoint(session: GatewaySession | undefined): Promise<void> {
    try {
      await this.#options.checkpoint.save(session);
    } catch {
      throw new QqGatewayError("checkpoint");
    }
    this.#session = session;
  }

  async #dispatch(event: QqDispatch): Promise<"ready" | "resumed" | undefined> {
    if (event.t === "READY") {
      if (!object(event.d) || typeof event.d.session_id !== "string" || !event.d.session_id) {
        throw new QqGatewayError("protocol");
      }
      // Ready/session 的消费同样先交回调保存，再提交恢复态。
      try {
        await this.#options.onDispatch(event);
      } catch {
        throw new QqGatewayError("handler");
      }
      await this.#checkpoint({ sessionId: event.d.session_id, sequence: event.s });
      return "ready";
    }
    if (!this.#session) throw new QqGatewayError("protocol");
    if (event.s <= this.#session.sequence) return event.t === "RESUMED" ? "resumed" : undefined;
    try {
      await this.#options.onDispatch(event);
    } catch {
      throw new QqGatewayError("handler");
    }
    await this.#checkpoint({ ...this.#session, sequence: event.s });
    return event.t === "RESUMED" ? "resumed" : undefined;
  }

  async #connection(
    connection: QqGatewayConnection,
    signal: AbortSignal,
  ): Promise<{ code: number; ready: boolean; livedMs: number; reconnectDelayMs?: number }> {
    const startedAt = performance.now();
    const socket = connection.socket;
    let ready = false;
    let hello = false;
    let receiving = true;
    let awaitingHeartbeat = false;
    let fatal: QqGatewayError | undefined;
    let heartbeatTimer: ReturnType<typeof setTimeout> | undefined;
    let heartbeatIntervalMs = 0;
    let reconnectDelayMs: number | undefined;
    let queue: Promise<void> = Promise.resolve();
    let pending = 0;
    let resolveClosed!: (code: number) => void;
    const closed = new Promise<number>((resolve) => {
      resolveClosed = resolve;
    });
    const close = (code: number, reason: string) => {
      receiving = false;
      if (socket.readyState === WebSocket.CLOSED) return;
      try {
        socket.close(code, reason);
      } catch {
        resolveClosed(1006);
      }
    };
    const timeout = setTimeout(
      () => close(4000, "connection deadline"),
      this.#options.connectTimeoutMs,
    );
    const abort = () => close(1000, "stopped");
    signal.addEventListener("abort", abort, { once: true });
    const fail = (error: QqGatewayError) => {
      fatal ??= error;
      close(4000, "local failure");
    };
    const heartbeat = (requested = false) => {
      if (socket.readyState !== WebSocket.OPEN) return;
      if (awaitingHeartbeat && !requested) {
        close(4000, "heartbeat deadline");
        return;
      }
      // A server request must be answered even while an earlier ACK is outstanding.
      // Keep the oldest outstanding deadline, rather than extending it on requests.
      if (!awaitingHeartbeat) {
        clearTimeout(heartbeatTimer);
        heartbeatTimer = setTimeout(() => heartbeat(), heartbeatIntervalMs);
      }
      awaitingHeartbeat = true;
      try {
        socket.send(JSON.stringify({ op: 1, d: this.#session?.sequence ?? null }));
      } catch {
        close(4000, "heartbeat transport");
      }
    };
    socket.onopen = () => this.#options.onStatus?.({ status: "connected" });
    socket.onclose = (event) => {
      receiving = false;
      resolveClosed(event.code);
    };
    socket.onerror = () => {
      try {
        socket.close();
      } catch {
        resolveClosed(1006);
      }
    };
    socket.onmessage = (incoming) => {
      if (!receiving || fatal || signal.aborted) return;
      let payload: unknown;
      try {
        if (typeof incoming.data !== "string" || incoming.data.length > 2_097_152) {
          throw new Error();
        }
        payload = JSON.parse(incoming.data);
        if (!object(payload) || !Number.isSafeInteger(payload.op)) throw new Error();
      } catch {
        fail(new QqGatewayError("protocol"));
        return;
      }
      const data = payload as Record<string, unknown>;
      if (data.op === 10) {
        if (
          hello || !object(data.d) || !Number.isSafeInteger(data.d.heartbeat_interval) ||
          (data.d.heartbeat_interval as number) < 1 ||
          (data.d.heartbeat_interval as number) > 2_147_483_647
        ) {
          fail(new QqGatewayError("protocol"));
          return;
        }
        hello = true;
        this.#options.onStatus?.({ status: "hello" });
        try {
          connection.authenticate(this.#options.intents, this.#session);
        } catch {
          fail(new QqGatewayError("protocol"));
          return;
        }
        heartbeatIntervalMs = data.d.heartbeat_interval as number;
        heartbeatTimer = setTimeout(() => heartbeat(), heartbeatIntervalMs);
      } else if (data.op === 11) {
        awaitingHeartbeat = false;
      } else if (data.op === 1) {
        heartbeat(true);
      } else if (data.op === 7) {
        close(4000, "server reconnect");
      } else if (data.op === 9) {
        if (typeof data.d !== "boolean") {
          fail(new QqGatewayError("protocol"));
          return;
        }
        const resumable = data.d;
        reconnectDelayMs = this.#options.invalidSessionBackoffMs;
        receiving = false;
        // Finish in-flight persistence before clearing a non-resumable session.
        // A resumable INVALID_SESSION retains the last durable sequence.
        queue = queue.then(async () => {
          if (fatal) return;
          if (!resumable) {
            await this.#checkpoint(undefined);
            this.client.invalidateToken();
          }
          close(4000, "invalid session");
        }).catch((error) =>
          fail(error instanceof QqGatewayError ? error : new QqGatewayError("protocol"))
        );
      } else if (data.op === 0) {
        if (
          !hello || !Number.isSafeInteger(data.s) || (data.s as number) < 0 ||
          typeof data.t !== "string" || !data.t
        ) {
          fail(new QqGatewayError("protocol"));
          return;
        }
        const event = data as unknown as QqDispatch;
        if (pending >= 64) {
          // 不确认尚未入队的序号，让平台从已持久化水位补发，避免无限内存队列。
          close(4000, "consumer backlog");
          return;
        }
        pending++;
        queue = queue.then(async () => {
          if (fatal) return;
          const status = await this.#dispatch(event);
          if (status) {
            ready = true;
            clearTimeout(timeout);
            this.#options.onStatus?.({ status });
            if (!signal.aborted && !awaitingHeartbeat) heartbeat();
          }
        }).catch((error) =>
          fail(error instanceof QqGatewayError ? error : new QqGatewayError("handler"))
        ).finally(() => {
          pending--;
        });
      } else fail(new QqGatewayError("protocol"));
    };
    if (signal.aborted) abort();
    try {
      const code = await closed;
      clearTimeout(heartbeatTimer);
      clearTimeout(timeout);
      // 断线不能中断已开始的落盘；等完成后才允许下一连接恢复序号。
      await queue;
      if (fatal) throw fatal;
      return { code, ready, livedMs: performance.now() - startedAt, reconnectDelayMs };
    } finally {
      clearTimeout(heartbeatTimer);
      clearTimeout(timeout);
      signal.removeEventListener("abort", abort);
      socket.onmessage = null;
      socket.onopen = null;
      socket.onclose = null;
      socket.onerror = null;
    }
  }

  async run(signal: AbortSignal): Promise<void> {
    if (this.#running) throw new QqGatewayError("protocol");
    this.#running = true;
    const stopRequests = () => this.client.close();
    signal.addEventListener("abort", stopRequests, { once: true });
    let delay = this.#options.initialBackoffMs;
    let quickDisconnects = 0;
    try {
      try {
        this.#session = await this.#options.checkpoint.load();
      } catch {
        throw new QqGatewayError("checkpoint");
      }
      while (!signal.aborted) {
        this.#options.onStatus?.({ status: "connecting" });
        let result: { code: number; ready: boolean; livedMs: number; reconnectDelayMs?: number };
        let platformDelay = 0;
        try {
          result = await this.#connection(await this.client.openGateway(), signal);
        } catch (error) {
          if (signal.aborted && error instanceof QqRequestError) break;
          if (
            !(error instanceof QqRequestError) ||
            (!["transport", "rate_limit", "authentication"].includes(error.info.category) &&
              !(error.info.httpStatus !== undefined && error.info.httpStatus >= 500))
          ) throw error;
          if (error.info.category === "authentication") this.client.invalidateToken();
          if (error.info.category === "rate_limit") {
            platformDelay = Math.max(
              this.#options.rateLimitBackoffMs,
              error.info.retryAfterMs ?? 0,
            );
          }
          result = { code: 1006, ready: false, livedMs: 0 };
        }
        if (signal.aborted) break;
        this.#options.onStatus?.({ status: "disconnected", code: result.code });
        if ([4001, 4002, 4010, 4011, 4012, 4013, 4014, 4914, 4915].includes(result.code)) {
          throw new QqGatewayError("closed", result.code);
        }
        platformDelay = Math.max(platformDelay, result.reconnectDelayMs ?? 0);
        if (result.code === 4004) this.client.invalidateToken();
        if (result.code === 4008) platformDelay = this.#options.rateLimitBackoffMs;
        if (
          [4006, 4007, 4009].includes(result.code) || (result.code >= 4900 && result.code <= 4913)
        ) {
          await this.#checkpoint(undefined);
          this.client.invalidateToken();
        }
        if (result.ready && result.livedMs >= this.#options.stableConnectionMs) {
          delay = this.#options.initialBackoffMs;
          quickDisconnects = 0;
        } else if (++quickDisconnects >= 3) {
          platformDelay = Math.max(platformDelay, this.#options.rateLimitBackoffMs);
          quickDisconnects = 0;
        }
        await wait(Math.max(delay, platformDelay), signal);
        delay = Math.min(this.#options.maxBackoffMs, delay * 2);
      }
    } finally {
      signal.removeEventListener("abort", stopRequests);
      this.#running = false;
    }
  }
}
