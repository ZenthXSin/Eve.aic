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

/** Race TLS connections before any authentication or message bytes are sent. Never retry a POST. */
export async function qqHttps(
  url: URL,
  init: RequestInit,
  diagnostic?: DiagnosticSink,
  resolver = lookup,
  trustedCa?: string,
): Promise<Response> {
  const started = performance.now(), requestId = crypto.randomUUID();
  const log = (phase: string, extra: Partial<QqConnectionDiagnostic> = {}) =>
    diagnostic?.({
      at: new Date().toISOString(),
      requestId,
      host: url.hostname,
      phase,
      elapsedMs: Math.round(performance.now() - started),
      ...extra,
    });
  const signal = init.signal;
  const sockets = new Set<TLSSocket>();
  let selected: TLSSocket | undefined;
  let closed = false;
  const abort = () => {
    closed = true;
    for (const socket of sockets) socket.destroy();
  };
  signal?.throwIfAborted();
  signal?.addEventListener("abort", abort, { once: true });
  const agent = new Agent({ keepAlive: false });
  agent.createConnection =
    ((_options: unknown, callback: (error: Error | null, socket?: TLSSocket) => void) => {
      void (async () => {
        log("dns_start");
        const addresses = await resolver(url.hostname, { all: true, verbatim: true });
        log("dns_complete");
        if (!addresses.length) throw new Error("DNS returned no QQ addresses");
        if (closed) throw signal?.reason ?? new Error("QQ connection aborted");
        const winner = await Promise.any(
          addresses.map(({ address, family }) =>
            new Promise<TLSSocket>((resolve, reject) => {
              log("connect_start", { address, family });
              const socket = connect({
                host: address,
                port: Number(url.port || 443),
                servername: url.hostname,
                rejectUnauthorized: true,
                ...(trustedCa ? { ca: trustedCa } : {}),
                ALPNProtocols: ["http/1.1"],
              });
              sockets.add(socket);
              const timer = setTimeout(() => {
                log("connect_timeout", { address, family });
                socket.destroy(new Error("QQ TLS connection timed out"));
              }, 4000);
              socket.once("connect", () => log("tcp_connected", { address, family }));
              socket.once("secureConnect", () => {
                clearTimeout(timer);
                log("tls_connected", { address, family });
                if (closed) {
                  socket.destroy();
                  reject(new Error("QQ connection closed"));
                } else resolve(socket);
              });
              socket.once("error", (error: Error & { code?: string }) => {
                clearTimeout(timer);
                log("connect_error", { address, family, code: error.code ?? error.name });
                reject(error);
              });
              socket.once("close", () => {
                clearTimeout(timer);
                reject(new Error("QQ socket closed"));
              });
            })
          ),
        );
        if (closed) {
          winner.destroy();
          throw signal?.reason ?? new Error("QQ connection aborted");
        }
        selected = winner;
        for (const socket of sockets) if (socket !== winner) socket.destroy();
        log("connection_selected", { address: winner.remoteAddress });
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
        log("response_headers", {
          httpStatus: response.statusCode,
          address: selected?.remoteAddress,
        });
        const resultHeaders = new Headers();
        for (const [name, value] of Object.entries(response.headers)) {
          if (value !== undefined) {
            resultHeaders.set(name, Array.isArray(value) ? value.join(", ") : value);
          }
        }
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
        resolve(
          new Response(Readable.toWeb(response) as ReadableStream<Uint8Array>, {
            status: response.statusCode!,
            headers: resultHeaders,
          }),
        );
      });
      req.once("error", (error: Error & { code?: string }) => {
        log("request_error", { code: error.code ?? error.name });
        abort();
        signal?.removeEventListener("abort", abort);
        agent.destroy();
        reject(error);
      });
      req.once("finish", () => log("request_written", { address: selected?.remoteAddress }));
      req.end(init.body === undefined || init.body === null ? undefined : init.body);
    });
  } catch (error) {
    abort();
    signal?.removeEventListener("abort", abort);
    agent.destroy();
    throw error;
  }
}
