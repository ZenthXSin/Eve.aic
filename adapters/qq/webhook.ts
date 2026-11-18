import type { QqInboundEvent } from "./inbound.ts";

export interface QqWebhookConfig {
  /** Only true enables this handler. The caller owns the listener, TLS and routing. */
  enabled?: boolean;
  path?: string;
  /** Default 1 MiB; hard ceiling 8 MiB. Applies to actual bytes, not just Content-Length. */
  maxBodyBytes?: number;
  /** Local replay window in either direction, in Unix seconds. Default 300, maximum 3600. */
  maxClockSkewSeconds?: number;
  bodyTimeoutMs?: number;
  maxInFlight?: number;
  unsignedRegistrationUntilUnixSeconds?: number;
}

export type QqWebhookDispatch = QqInboundEvent;

export interface QqWebhookDelivery {
  rawBody: Uint8Array<ArrayBuffer>;
  /** X-Signature-Timestamp as received, not d.event_ts or a gateway sequence. */
  timestamp: string;
  signal: AbortSignal;
}

export type QqWebhookReceipt = "persisted" | "already_persisted";

export interface QqWebhookOptions {
  appId: string;
  config?: QqWebhookConfig;
  secret: string;
  /** Resolve only after this original is durable, or the store confirms a durable duplicate. */
  onDispatch(event: QqWebhookDispatch, delivery: QqWebhookDelivery): Promise<QqWebhookReceipt>;
  signal?: AbortSignal;
  /** Unix milliseconds, like Date.now. */
  now?: () => number;
}

const utf8 = new TextEncoder();
const strictUtf8 = new TextDecoder("utf-8", { fatal: true });
const noStore = { "cache-control": "no-store" };
const unixSeconds = /^(?:0|[1-9]\d{0,9})$/;
const jsonNumberSuffix = /^\d*(?:[eE]\d+)?$/;

export function webhookSettings({
  path = "/qq/webhook",
  maxBodyBytes = 1024 * 1024,
  maxClockSkewSeconds = 300,
  bodyTimeoutMs = 5_000,
  maxInFlight = 16,
  unsignedRegistrationUntilUnixSeconds = 0,
}: Omit<QqWebhookConfig, "enabled">, nowMs = Date.now()) {
  if (
    typeof path !== "string" || !path.startsWith("/") || path.startsWith("//") ||
    /[?#\\]/.test(path) || new URL(path, "http://localhost").pathname !== path ||
    !Number.isSafeInteger(maxBodyBytes) || maxBodyBytes < 1 || maxBodyBytes > 8 * 1024 * 1024 ||
    !Number.isSafeInteger(maxClockSkewSeconds) || maxClockSkewSeconds < 0 ||
    maxClockSkewSeconds > 3600 ||
    !Number.isSafeInteger(bodyTimeoutMs) || bodyTimeoutMs < 1 || bodyTimeoutMs > 30_000 ||
    !Number.isSafeInteger(maxInFlight) || maxInFlight < 1 || maxInFlight > 32 ||
    !Number.isSafeInteger(unsignedRegistrationUntilUnixSeconds) ||
    unsignedRegistrationUntilUnixSeconds < 0 ||
    unsignedRegistrationUntilUnixSeconds > Math.floor(nowMs / 1000) + 300
  ) throw new TypeError("QQ Webhook 配置不合法");
  return {
    path,
    maxBodyBytes,
    maxClockSkewSeconds,
    bodyTimeoutMs,
    maxInFlight,
    unsignedRegistrationUntilUnixSeconds,
  };
}

async function ed25519Keys(secret: string): Promise<CryptoKeyPair> {
  const secretBytes = utf8.encode(secret);
  const prefix = Uint8Array.fromHex("302e020100300506032b657004220420");
  const pkcs8 = new Uint8Array(prefix.length + 32);
  pkcs8.set(prefix);
  try {
    for (let index = 0; index < 32; index++) {
      pkcs8[prefix.length + index] = secretBytes[index % secretBytes.length];
    }
    const seedKey = await crypto.subtle.importKey("pkcs8", pkcs8, "Ed25519", true, ["sign"]);
    const { x } = await crypto.subtle.exportKey("jwk", seedKey);
    const publicKey = await crypto.subtle.importKey(
      "jwk",
      { kty: "OKP", crv: "Ed25519", x },
      "Ed25519",
      false,
      ["verify"],
    );
    const privateKey = await crypto.subtle.importKey("pkcs8", pkcs8, "Ed25519", false, ["sign"]);
    return { privateKey, publicKey };
  } finally {
    pkcs8.fill(0);
    secretBytes.fill(0);
  }
}

function signedBytes(timestamp: string, body: Uint8Array): Uint8Array<ArrayBuffer> {
  const prefix = utf8.encode(timestamp);
  const message = new Uint8Array(prefix.length + body.length);
  message.set(prefix);
  message.set(body, prefix.length);
  return message;
}

class BodyTooLarge extends Error {}
class BodyTimedOut extends Error {}

async function readBody(request: Request, limit: number, timeoutMs: number, signal: AbortSignal) {
  if (!request.body) throw new TypeError();
  const reader = request.body.getReader();
  const cancel = () => void reader.cancel().catch(() => {});
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    cancel();
  }, timeoutMs);
  signal.addEventListener("abort", cancel, { once: true });
  try {
    signal.throwIfAborted();
    const declared = request.headers.get("content-length");
    if (declared !== null) {
      if (!/^\d+$/.test(declared)) throw new TypeError();
      if (Number(declared) > limit) throw new BodyTooLarge();
    }
    let buffer = new Uint8Array(0);
    let length = 0;
    while (true) {
      const { done, value } = await reader.read();
      signal.throwIfAborted();
      if (timedOut) throw new BodyTimedOut();
      if (done) break;
      const needed = length + value.byteLength;
      if (needed > limit) throw new BodyTooLarge();
      if (needed > buffer.length) {
        const capacity = Math.min(limit, Math.max(needed, 1024, 2 * buffer.length));
        const grown = new Uint8Array(capacity);
        grown.set(buffer.subarray(0, length));
        buffer = grown;
      }
      buffer.set(value, length);
      length = needed;
    }
    if (declared !== null && length !== Number(declared)) throw new TypeError();
    return buffer.subarray(0, length);
  } catch (error) {
    cancel();
    throw error;
  } finally {
    clearTimeout(timer);
    signal.removeEventListener("abort", cancel);
    reader.releaseLock();
  }
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function registrationChallenge(bytes: Uint8Array) {
  if (bytes.some((byte) => byte > 0x7f)) return;
  const text = strictUtf8.decode(bytes);
  let payload: unknown;
  try {
    payload = JSON.parse(text);
  } catch {
    return;
  }
  if (
    !record(payload) || payload.op !== 13 || Object.keys(payload).length !== 2 ||
    !record(payload.d) || Object.keys(payload.d).length !== 2
  ) return;
  const { event_ts, plain_token } = payload.d;
  if (
    typeof event_ts !== "string" || !unixSeconds.test(event_ts) ||
    typeof plain_token !== "string" ||
    plain_token.length < 16 || plain_token.length > 64 || /[^A-Za-z0-9]/.test(plain_token) ||
    jsonNumberSuffix.test(plain_token) ||
    text.replace(/[ \r\n\t]/g, "") !== JSON.stringify(payload)
  ) return;
  return { event_ts, plain_token };
}

function dispatch(value: Record<string, unknown>): value is QqWebhookDispatch {
  return value.op === 0 && typeof value.t === "string" && value.t.trim() !== "" &&
    value.t !== "READY" && value.t !== "RESUMED" &&
    Object.hasOwn(value, "d") && (value.id === undefined || typeof value.id === "string") &&
    (value.s === undefined ||
      (typeof value.s === "number" && Number.isSafeInteger(value.s) && value.s >= 0));
}

function empty(status: number): Response {
  return new Response(null, { status, headers: noStore });
}

function rejectUnread(request: Request, status: number): Response {
  void request.body?.cancel().catch(() => {});
  const response = empty(status);
  if (status === 405) response.headers.set("allow", "POST");
  return response;
}

function reply(body: unknown): Response {
  return Response.json(body, { headers: noStore });
}

function ack(persisted: boolean): Response {
  return reply({ op: 12, d: persisted ? 0 : 1 });
}

/** Signed events; optional one-shot registration. Wire vectors: Tencent bot-docs and qqbot-nodejs v1.0.5. */
export function createQqWebhookHandler(
  { appId, config, secret, onDispatch, signal: shutdown, now = Date.now }: QqWebhookOptions,
): (request: Request) => Promise<Response> {
  if (config?.enabled !== true) return (request) => Promise.resolve(rejectUnread(request, 404));
  const openedAt = now();
  const {
    path,
    maxBodyBytes,
    maxClockSkewSeconds,
    bodyTimeoutMs,
    maxInFlight,
    unsignedRegistrationUntilUnixSeconds,
  } = webhookSettings(config, openedAt);
  if (
    typeof appId !== "string" || !/^[\w-]+$/.test(appId) || typeof secret !== "string" ||
    secret.length === 0 || typeof onDispatch !== "function"
  ) throw new TypeError("QQ Webhook 配置不合法");

  const fresh = (stamp: string, skew = maxClockSkewSeconds) =>
    Math.abs(Math.floor(now() / 1000) - Number(stamp)) <= skew;
  let keys: Promise<CryptoKeyPair> | undefined;
  const keyPair = () => keys ??= ed25519Keys(secret);
  async function proof(event_ts: string, plain_token: string, signal: AbortSignal) {
    try {
      const pair = await keyPair();
      const result = await crypto.subtle.sign(
        "Ed25519",
        pair.privateKey,
        signedBytes(event_ts, utf8.encode(plain_token)),
      );
      return signal.aborted
        ? empty(503)
        : reply({ plain_token, signature: new Uint8Array(result).toHex() });
    } catch {
      return empty(503);
    }
  }

  const deadline = unsignedRegistrationUntilUnixSeconds * 1000;
  const monotonicDeadline = performance.now() + Math.max(0, deadline - openedAt);
  let armed = openedAt < deadline, registering = false, attempts = 0, nextAttempt = 0;
  function registrationOpen() {
    return armed &&= now() < deadline && performance.now() < monotonicDeadline;
  }
  async function handleRegistration(request: Request, signal: AbortSignal) {
    if (!registrationOpen()) return rejectUnread(request, 401);
    if (registering || attempts >= 3 || now() < nextAttempt) return rejectUnread(request, 429);
    registering = true;
    attempts++;
    nextAttempt = now() + 1000;
    try {
      const body = await readBody(
        request,
        Math.min(maxBodyBytes, 512),
        Math.min(bodyTimeoutMs, 1000),
        signal,
      );
      const challenge = registrationChallenge(body);
      const current = () =>
        challenge && registrationOpen() &&
        fresh(challenge.event_ts, Math.min(30, maxClockSkewSeconds));
      if (!challenge || !current()) return empty(401);
      const response = await proof(challenge.event_ts, challenge.plain_token, signal);
      if (!current()) return empty(401);
      if (response.ok) armed = false;
      return response;
    } catch (error) {
      return bodyFailure(error, signal);
    } finally {
      registering = false;
    }
  }

  function bodyFailure(error: unknown, signal: AbortSignal) {
    if (signal.aborted) return empty(503);
    return empty(error instanceof BodyTooLarge ? 413 : error instanceof BodyTimedOut ? 408 : 400);
  }
  async function handleSigned(
    request: Request,
    timestamp: string,
    signature: Uint8Array<ArrayBuffer>,
    signal: AbortSignal,
  ) {
    let rawBody: Uint8Array<ArrayBuffer>;
    try {
      rawBody = await readBody(request, maxBodyBytes, bodyTimeoutMs, signal);
    } catch (error) {
      return bodyFailure(error, signal);
    }
    let pair: CryptoKeyPair;
    try {
      pair = await keyPair();
      const valid = await crypto.subtle.verify(
        "Ed25519",
        pair.publicKey,
        signature,
        signedBytes(timestamp, rawBody),
      );
      if (!valid || !fresh(timestamp)) return empty(401);
    } catch {
      return empty(503);
    }
    if (signal.aborted) return empty(503);
    let payload: unknown;
    try {
      payload = JSON.parse(strictUtf8.decode(rawBody));
    } catch {
      return empty(400);
    }
    if (!record(payload)) return empty(400);

    if (payload.op === 13) {
      if (!record(payload.d)) return empty(400);
      const { plain_token, event_ts } = payload.d;
      if (
        typeof plain_token !== "string" || !plain_token || typeof event_ts !== "string" ||
        !unixSeconds.test(event_ts)
      ) {
        return empty(400);
      }
      return proof(event_ts, plain_token, signal);
    }
    if (payload.op === 1) {
      const seq = payload.d;
      if (typeof seq !== "number" || !Number.isInteger(seq) || seq < 0 || seq > 0xffff_ffff) {
        return empty(400);
      }
      return reply({ op: 11, d: seq });
    }
    if (!dispatch(payload)) return empty(400);
    try {
      const receipt = await onDispatch(payload, { rawBody, timestamp, signal });
      return ack(!signal.aborted && (receipt === "persisted" || receipt === "already_persisted"));
    } catch {
      return ack(false);
    }
  }

  let inFlight = 0;
  return async (request) => {
    const url = new URL(request.url);
    if (url.pathname !== path) return rejectUnread(request, 404);
    if (request.method !== "POST") return rejectUnread(request, 405);
    if (shutdown?.aborted || request.signal.aborted) return rejectUnread(request, 503);
    if (request.headers.get("X-Bot-Appid") !== appId) return rejectUnread(request, 401);
    const timestamp = request.headers.get("X-Signature-Timestamp");
    const hex = request.headers.get("X-Signature-Ed25519");
    let signature: Uint8Array<ArrayBuffer> | undefined;
    if (timestamp === null && hex === null) {
      if (
        !registrationOpen() || url.search ||
        [...request.headers.keys()].some((name) => name.startsWith("x-signature-"))
      ) return rejectUnread(request, 401);
    } else {
      if (
        timestamp === null || hex === null || !unixSeconds.test(timestamp) || !fresh(timestamp) ||
        !/^[\da-f]{128}$/i.test(hex)
      ) return rejectUnread(request, 401);
      signature = Uint8Array.fromHex(hex);
      if ((signature[63] & 0xe0) !== 0) return rejectUnread(request, 401);
    }
    if (inFlight >= maxInFlight) return rejectUnread(request, 503);
    inFlight++;
    try {
      const signal = shutdown ? AbortSignal.any([shutdown, request.signal]) : request.signal;
      if (!signature || timestamp === null) return await handleRegistration(request, signal);
      return await handleSigned(request, timestamp, signature, signal);
    } finally {
      inFlight--;
    }
  };
}
