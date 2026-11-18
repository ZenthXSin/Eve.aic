import type { LookupAddress } from "node:dns";
import { lookup } from "node:dns/promises";
import { Agent, request } from "node:https";
import { connect, type TLSSocket } from "node:tls";
import { Readable } from "node:stream";

export interface QqConnectionDiagnostic {
  at: string;
  requestId: string;
  host: string;
  phase: string;
  elapsedMs: number;
  address?: string;
  family?: number;
  code?: string;
  httpStatus?: number;
}
export type DiagnosticSink = (event: QqConnectionDiagnostic) => void;

/** False only while DNS/TLS has not handed a socket to the HTTP writer. */
export class QqTransportError extends Error {
  constructor(readonly requestStarted: boolean) {
    super("QQ HTTPS transport failed");
    this.name = "QqTransportError";
  }
}

const HINT_TTL_MS = 5 * 60_000;
const MAX_HINTS = 32;
const FALLBACK_DELAY_MS = 200;
const hints = new Map<string, LookupAddress & { expiresAt: number }>();
type Connection = { candidate: LookupAddress; socket: TLSSocket };

const DIAGNOSTIC_CODES = new Set([
  "ABORT_ERR",
  "CERT_HAS_EXPIRED",
  "CERT_NOT_YET_VALID",
  "DEPTH_ZERO_SELF_SIGNED_CERT",
  "EAI_AGAIN",
  "ECONNREFUSED",
  "ECONNRESET",
  "EHOSTUNREACH",
  "ENETUNREACH",
  "ENOTFOUND",
  "EPIPE",
  "ERR_STREAM_PREMATURE_CLOSE",
  "ERR_TLS_CERT_ALTNAME_INVALID",
  "ERR_TLS_HANDSHAKE_TIMEOUT",
  "ETIMEDOUT",
  "SELF_SIGNED_CERT_IN_CHAIN",
  "UNABLE_TO_GET_ISSUER_CERT_LOCALLY",
  "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
]);

function diagnosticCode(code: unknown): string {
  return typeof code === "string" && DIAGNOSTIC_CODES.has(code) ? code : "UNKNOWN";
}

function guardedResponseBody(response: Readable): ReadableStream<Uint8Array> {
  const reader = (Readable.toWeb(response) as ReadableStream<Uint8Array>).getReader();
  return new ReadableStream<Uint8Array>({
    start(controller) {
      void reader.closed.catch(() => controller.error(new QqTransportError(true)));
    },
    async pull(controller) {
      try {
        const chunk = await reader.read();
        if (chunk.done) {
          reader.releaseLock();
          controller.close();
        } else controller.enqueue(chunk.value);
      } catch {
        reader.releaseLock();
        throw new QqTransportError(true);
      }
    },
    async cancel() {
      try {
        await reader.cancel();
      } catch {
        throw new QqTransportError(true);
      } finally {
        reader.releaseLock();
      }
    },
  }, { highWaterMark: 0 });
}

function rememberAddress(origin: string, { address, family }: LookupAddress) {
  hints.delete(origin);
  if (hints.size >= MAX_HINTS) hints.delete(hints.keys().next().value!);
  hints.set(origin, { address, family, expiresAt: performance.now() + HINT_TTL_MS });
}

async function raceAddresses(
  origin: string,
  candidates: LookupAddress[],
  dial: (candidate: LookupAddress) => Promise<Connection>,
): Promise<Connection> {
  const hint = hints.get(origin);
  const preferred = hint && hint.expiresAt > performance.now() &&
    candidates.find(({ address, family }) => address === hint.address && family === hint.family);
  if (!preferred) hints.delete(origin);
  if (!preferred || candidates.length === 1) return Promise.any(candidates.map(dial));

  const first = dial(preferred);
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const early = await Promise.race([
      first.catch(() => undefined),
      new Promise<void>((resolve) => timer = setTimeout(resolve, FALLBACK_DELAY_MS)),
    ]);
    if (early) return early;
  } finally {
    clearTimeout(timer);
  }
  return Promise.any([
    first,
    ...candidates.filter((candidate) => candidate !== preferred).map(dial),
  ]);
}

/** Race TLS connections before any authentication or message bytes are sent. Never retry a POST. */
export async function qqHttps(
  url: URL,
  init: RequestInit,
  diagnostic?: DiagnosticSink,
  resolver = lookup,
  trustedCa?: string,
): Promise<Response> {
  const { origin, hostname, port } = url;
  const started = performance.now(), requestId = crypto.randomUUID();
  const log = (phase: string, extra: Partial<QqConnectionDiagnostic> = {}) =>
    diagnostic?.({
      at: new Date().toISOString(),
      requestId,
      host: hostname,
      phase,
      elapsedMs: Math.round(performance.now() - started),
      ...extra,
    });
  const signal = init.signal;
  const sockets = new Map<TLSSocket, () => void>();
  let selected: TLSSocket | undefined;
  let cancelLookup: (() => void) | undefined;
  let requestStarted = false;
  let closed = false;
  const abort = () => {
    closed = true;
    cancelLookup?.();
    for (const cancel of sockets.values()) cancel();
  };
  if (signal?.aborted) throw new QqTransportError(false);
  signal?.addEventListener("abort", abort, { once: true });
  const dial = (candidate: LookupAddress) =>
    new Promise<Connection>((resolve, reject) => {
      const { address, family } = candidate;
      if (!closed) log("connect_start", { address, family });
      if (closed) throw new Error("QQ connection aborted");
      const socket = connect({
        host: address,
        port: Number(port || 443),
        servername: hostname,
        rejectUnauthorized: true,
        ...(trustedCa ? { ca: trustedCa } : {}),
        ALPNProtocols: ["http/1.1"],
      });
      const timer = setTimeout(() => {
        log("connect_timeout", { address, family });
        socket.destroy(new Error("QQ TLS connection timed out"));
      }, 4000);
      const cancel = () => {
        clearTimeout(timer);
        socket.destroy();
        reject(new Error("QQ socket closed"));
      };
      sockets.set(socket, cancel);
      socket.once("connect", () => log("tcp_connected", { address, family }));
      socket.once("secureConnect", () => {
        clearTimeout(timer);
        log("tls_connected", { address, family });
        resolve({ candidate, socket });
      });
      socket.once("error", (error: Error & { code?: string }) => {
        clearTimeout(timer);
        log("connect_error", { address, family, code: diagnosticCode(error.code) });
        reject(error);
      });
      socket.once("close", () => {
        clearTimeout(timer);
        sockets.delete(socket);
        reject(new Error("QQ socket closed"));
      });
    });
  const agent = new Agent({ keepAlive: false });
  agent.createConnection =
    ((_options: unknown, callback: (error: Error | null, socket?: TLSSocket) => void) => {
      void (async () => {
        log("dns_start");
        const addresses = await new Promise<LookupAddress[]>((resolve, reject) => {
          cancelLookup = () => reject(new Error("QQ DNS lookup aborted"));
          if (closed) cancelLookup();
          else resolver(hostname, { all: true, verbatim: true }).then(resolve, reject);
        }).finally(() => {
          cancelLookup = undefined;
        });
        log("dns_complete");
        if (!addresses.length) throw new Error("DNS returned no QQ addresses");
        if (closed) throw signal?.reason ?? new Error("QQ connection aborted");
        const { socket: winner, candidate } = await raceAddresses(origin, addresses, dial);
        if (closed) {
          winner.destroy();
          throw signal?.reason ?? new Error("QQ connection aborted");
        }
        selected = winner;
        for (const [socket, cancel] of sockets) if (socket !== winner) cancel();
        log("connection_selected", { address: winner.remoteAddress });
        if (closed) throw new Error("QQ connection aborted");
        rememberAddress(origin, candidate);
        // req.end may buffer before TLS completes. Once handed over, even a
        // partial write is ambiguous; do not infer safety from "finish".
        requestStarted = true;
        callback(null, winner);
      })().catch((error) =>
        callback(error instanceof Error ? error : new Error("QQ connection failed"))
      );
      return undefined;
    }) as unknown as typeof agent.createConnection;
  try {
    return await new Promise<Response>((resolve, reject) => {
      const headers = Object.fromEntries(new Headers(init.headers));
      const req = request(url, {
        method: init.method ?? "GET",
        headers,
        agent,
        signal: signal ?? undefined,
      }, (response) => {
        const finish = () => {
          signal?.removeEventListener("abort", abort);
          agent.destroy();
        };
        response.once("end", () => {
          log("response_complete", { httpStatus: response.statusCode });
          finish();
        });
        response.once("error", () => {
          log("response_error");
          finish();
        });
        response.once("close", finish);
        try {
          const status = response.statusCode!;
          log("response_headers", { httpStatus: status, address: selected?.remoteAddress });
          const resultHeaders = new Headers();
          for (const [name, value] of Object.entries(response.headers)) {
            if (value !== undefined) {
              resultHeaders.set(name, Array.isArray(value) ? value.join(", ") : value);
            }
          }
          const bodyless = status === 204 || status === 205 || status === 304;
          const body = bodyless ? null : guardedResponseBody(response);
          const result = new Response(body, { status, headers: resultHeaders });
          if (bodyless) response.resume();
          resolve(result);
        } catch {
          response.destroy();
          reject(new QqTransportError(true));
        }
      });
      req.once("error", (error: Error & { code?: string }) => {
        log("request_error", { code: diagnosticCode(error.code) });
        abort();
        signal?.removeEventListener("abort", abort);
        agent.destroy();
        reject(new QqTransportError(requestStarted));
      });
      req.once("finish", () => log("request_written", { address: selected?.remoteAddress }));
      req.end(init.body === undefined || init.body === null ? undefined : init.body);
    });
  } catch (error) {
    abort();
    signal?.removeEventListener("abort", abort);
    agent.destroy();
    throw error instanceof QqTransportError ? error : new QqTransportError(requestStarted);
  }
}
