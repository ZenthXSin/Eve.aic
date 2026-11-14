// 独立适配器的 QQ HTTP 传输。不依赖内核或插件，不承担身份映射、通知和行为判断。
// 返回平台事实；持久动作记录、入站日志和所有权交接由适配器服务另行接入。
import { createHash } from "node:crypto";
import { Buffer } from "node:buffer";
import { sdkRequest } from "./satori/api.ts";
import { type DiagnosticSink, qqHttps } from "./transport.ts";
import {
  QQ_API_ORIGIN,
  QQ_REQUEST_TIMEOUT_MS,
  QQ_TOKEN_ORIGIN,
  QQ_TOKEN_TIMEOUT_MS,
  QQ_UPLOAD_TIMEOUT_MS,
} from "./satori/api.ts";

export interface QqCredentials {
  appId: string;
  appSecret: string;
}

export interface QqClientOptions {
  /** 生产必须由组合层注入低权限文件读取器；缺省拒绝本机路径上传。 */
  attachmentReader?: (path: string) => Promise<Uint8Array>;
  diagnostics?: DiagnosticSink;
  /** 正式环境使用官方 HTTPS 地址；HTTP 只允许本机测试地址。 */
  baseUrl?: string;
  tokenBaseUrl?: string;
  timeoutMs?: number;
  uploadTimeoutMs?: number;
  tokenTimeoutMs?: number;
  maxResponseBytes?: number;
  now?: () => number;
  /** 缺省仅私信；显式 all 才允许群聊，在认证或发送请求前落实。 */
  sendScope?: "all" | "private";
}

export type QqErrorCategory =
  | "scope"
  | "request"
  | "authentication"
  | "rate_limit"
  | "platform"
  | "transport"
  | "invalid_response"
  | "pending"
  | "duplicate";

export interface QqErrorInfo {
  category: QqErrorCategory;
  httpStatus?: number;
  code?: number;
  traceId?: string;
  /** 仅记录受控分类，不携带 fetch 原始异常、URL、凭据或请求正文。 */
  transportReason?: "timeout" | "aborted" | "network";
  /** headers 包括建连及等待响应头；不据此猜测请求是否已经送达。 */
  phase?: "headers" | "body";
  elapsedMs?: number;
  /** Platform-declared delay, bounded to one hour; never inferred from free-form messages. */
  retryAfterMs?: number;
  failureStage?: "auth" | "upload" | "message";
}

export class QqRequestError extends Error {
  constructor(readonly info: QqErrorInfo) {
    // 不把服务端任意正文、URL、凭据或 fetch 错误原文写进日志。
    super(`QQ 请求未完成：${info.category}${info.code === undefined ? "" : ` (${info.code})`}`);
    this.name = "QqRequestError";
  }
}

export interface QqSendText {
  destination: { kind: "user" | "group"; openId: string };
  text: string;
  /** Pinned at admission; absent in legacy records means plain text. */
  format?: "plain" | "markdown";
  /** 兼容旧动作账本的回复来源；新发送统一主动，不写入平台被动回复字段。 */
  reply?: { messageId: string; sequence: number } | { eventId: string };
  /** 引用展示用的 ref_idx，不是被动回复用的 msg_id。 */
  referenceIndex?: string;
  wakeup?: boolean;
  /** 先上传平台取得 file_info，再提交媒体消息。任何阶段都不擅自重试。 */
  attachment?: { path: string; kind: "image" | "file" };
}

/** 鉴权动作只把凭据写入适配器自己的连接，不返回 token 给外部调用方。 */
export interface QqGatewayConnection {
  socket: WebSocket;
  authenticate(intents: number, resume?: { sessionId: string; sequence: number }): void;
}

export type QqSendResult =
  | {
    status: "sent";
    messageId: string;
    timestamp?: string;
    referenceIndex?: string;
    traceId?: string;
  }
  | { status: "failed"; attempted: boolean; error: QqErrorInfo }
  | { status: "unknown"; attempted: true; error: QqErrorInfo };

interface ResponseInfo {
  httpStatus: number;
  data: Record<string, unknown>;
  traceId?: string;
  retryAfterMs?: number;
}

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonempty(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

function requestError(): never {
  throw new QqRequestError({ category: "request" });
}

function codeOf(data: Record<string, unknown>): number | undefined {
  // 新文档用 err_code，旧接口/访问凭证用 code。不能把错误字段类型不对当成成功。
  let code: number | undefined;
  for (const key of ["err_code", "code"]) {
    if (!(key in data)) continue;
    const value = data[key];
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
      throw new QqRequestError({ category: "invalid_response" });
    }
    if (code !== undefined && value !== code) {
      throw new QqRequestError({ category: "invalid_response" });
    }
    code = value;
  }
  return code;
}

function traceOf(data: Record<string, unknown>, header: string | null): string | undefined {
  const candidate = data.trace_id ?? header;
  return typeof candidate === "string" && /^[a-zA-Z0-9_.:-]{1,256}$/.test(candidate)
    ? candidate
    : undefined;
}

function errorInfo(response: ResponseInfo, code?: number): QqErrorInfo {
  const category = response.httpStatus === 429 || code === 100001 || code === 40034100 ||
      code === 22009
    ? "rate_limit"
    : response.httpStatus === 401 || code === 100016 || code === 11243
    ? "authentication"
    : "platform";
  return {
    category,
    httpStatus: response.httpStatus,
    code,
    traceId: response.traceId,
    ...(response.retryAfterMs === undefined ? {} : { retryAfterMs: response.retryAfterMs }),
  };
}

function retryAfter(value: string | null, now: number): number | undefined {
  if (!value) return undefined;
  const delay = /^\d+$/.test(value.trim()) ? Number(value.trim()) * 1000 : Date.parse(value) - now;
  return Number.isFinite(delay) && delay >= 0 ? Math.min(3_600_000, Math.ceil(delay)) : undefined;
}

/** Same policy as the official SDK's FILE upload: send a basename, not a local pathname. */
function fileName(path: string): string {
  // Explicitly strip control bytes from the filename accepted by the upstream FILE API.
  // deno-lint-ignore no-control-regex
  return path.split("/").at(-1)!.replace(/[\x00-\x1f\x7f\\/:*?"<>|]/g, "_").slice(0, 255) || "file";
}

async function readObject(response: Response, limit: number): Promise<Record<string, unknown>> {
  if (!response.body) {
    throw new QqRequestError({ category: "invalid_response", httpStatus: response.status });
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    while (true) {
      const next = await reader.read();
      if (next.done) break;
      length += next.value.byteLength;
      if (length > limit) {
        await reader.cancel();
        throw new QqRequestError({ category: "invalid_response", httpStatus: response.status });
      }
      chunks.push(next.value);
    }
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    throw new QqRequestError({ category: "invalid_response", httpStatus: response.status });
  }
  if (!object(parsed)) {
    throw new QqRequestError({ category: "invalid_response", httpStatus: response.status });
  }
  return parsed;
}

function textBody(request: QqSendText): Record<string, unknown> {
  if (
    !object(request) || !object(request.destination) ||
    !["user", "group"].includes(request.destination.kind) ||
    !nonempty(request.destination.openId) || typeof request.text !== "string" ||
    (!nonempty(request.text) && request.attachment === undefined)
  ) requestError();
  if (request.format !== undefined && !["plain", "markdown"].includes(request.format)) {
    requestError();
  }
  if (request.attachment !== undefined && request.format === "markdown") requestError();
  const body: Record<string, unknown> = request.format === "markdown"
    ? { msg_type: 2, markdown: { content: request.text } }
    : { content: request.text, msg_type: 0 };
  if (request.attachment !== undefined) {
    if (
      !object(request.attachment) || typeof request.attachment.path !== "string" ||
      !request.attachment.path.startsWith("/") || request.attachment.path.includes("\0") ||
      !["image", "file"].includes(request.attachment.kind)
    ) requestError();
    body.msg_type = 7;
  }
  if (request.reply !== undefined) {
    if (!object(request.reply)) requestError();
    const reply: Record<string, unknown> = request.reply;
    const hasMessage = "messageId" in reply;
    const hasEvent = "eventId" in reply;
    if (hasMessage === hasEvent) requestError();
    if (hasMessage) {
      if (
        !nonempty(reply.messageId) || typeof reply.sequence !== "number" ||
        !Number.isSafeInteger(reply.sequence) || reply.sequence < 1
      ) requestError();
    } else {
      if (!nonempty(reply.eventId)) requestError();
    }
  }
  if (request.referenceIndex !== undefined) {
    if (!nonempty(request.referenceIndex)) requestError();
    // Official Markdown omits display references; never enable passive replies.
    if (request.format !== "markdown") {
      body.message_reference = { message_id: request.referenceIndex };
    }
  }
  if (request.wakeup !== undefined) {
    if (typeof request.wakeup !== "boolean") requestError();
    if (request.wakeup) {
      if (request.destination.kind !== "user" || request.attachment) {
        requestError();
      }
      body.is_wakeup = true;
    }
  }
  return body;
}

/** 无网络预检，动作服务可在登记“执行中”之前确认参数确实满足当前传输契约。 */
export function validateQqSendText(request: QqSendText): void {
  textBody(request);
}

export class QqClient {
  #base: URL;
  #tokenBase: URL;
  #credentials: QqCredentials;
  #timeout: number;
  #uploadTimeout: number;
  #tokenTimeout: number;
  #limit: number;
  #diagnostics?: DiagnosticSink;
  #now: () => number;
  #token?: { value: string; expiresAt: number };
  #refresh?: Promise<string>;
  #scope: "all" | "private";
  #closed = new AbortController();
  #attachmentReader?: (path: string) => Promise<Uint8Array>;
  #downloadedReader?: (path: string) => Promise<Uint8Array | undefined>;

  constructor(credentials: QqCredentials, options: QqClientOptions = {}) {
    if (!nonempty(credentials.appId) || !nonempty(credentials.appSecret)) requestError();
    const base = new URL(options.baseUrl ?? QQ_API_ORIGIN);
    const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(base.hostname);
    if (
      base.username || base.password || base.search || base.hash || base.pathname !== "/" ||
      (base.protocol !== "https:" && !(base.protocol === "http:" && loopback))
    ) requestError();
    const tokenBase = new URL(options.tokenBaseUrl ?? (options.baseUrl ?? QQ_TOKEN_ORIGIN));
    if (
      tokenBase.username || tokenBase.password || tokenBase.search || tokenBase.hash ||
      tokenBase.pathname !== "/" || (tokenBase.protocol !== "https:" &&
        !(tokenBase.protocol === "http:" &&
          ["127.0.0.1", "localhost", "[::1]"].includes(tokenBase.hostname)))
    ) requestError();
    const timeout = options.timeoutMs ?? QQ_REQUEST_TIMEOUT_MS;
    const uploadTimeout = options.uploadTimeoutMs ?? options.timeoutMs ?? QQ_UPLOAD_TIMEOUT_MS;
    const tokenTimeout = options.tokenTimeoutMs ?? options.timeoutMs ?? QQ_TOKEN_TIMEOUT_MS;
    const limit = options.maxResponseBytes ?? 1_048_576;
    if (options.sendScope !== undefined && !["all", "private"].includes(options.sendScope)) {
      requestError();
    }
    if (
      [timeout, uploadTimeout, tokenTimeout].some((ms) =>
        !Number.isSafeInteger(ms) || ms < 1 || ms > 2_147_483_647
      ) || !Number.isSafeInteger(limit) || limit < 1
    ) {
      requestError();
    }
    this.#diagnostics = options.diagnostics;
    this.#base = base;
    this.#tokenBase = tokenBase;
    this.#credentials = { ...credentials };
    this.#timeout = timeout;
    this.#uploadTimeout = uploadTimeout;
    this.#tokenTimeout = tokenTimeout;
    this.#limit = limit;
    this.#now = options.now ?? Date.now;
    this.#scope = options.sendScope ?? "private";
    this.#attachmentReader = options.attachmentReader;
  }

  get sendScope(): "all" | "private" {
    return this.#scope;
  }

  /** 独立手机存储只允许自己的完整下载回执原件，不允许任意root路径。 */
  useDownloadedAttachmentReader(reader: (path: string) => Promise<Uint8Array | undefined>) {
    this.#downloadedReader = reader;
  }

  async #request(
    path: string,
    init: RequestInit,
    options: { origin?: URL; timeoutMs?: number; raw?: boolean } = {},
  ): Promise<ResponseInfo> {
    const startedAt = performance.now();
    const timeout = AbortSignal.timeout(options.timeoutMs ?? this.#timeout);
    const signal = AbortSignal.any([this.#closed.signal, timeout]);
    let phase: "headers" | "body" = "headers";
    try {
      const url = new URL(path, options.origin ?? this.#base);
      const send = url.protocol === "https:"
        ? (init: RequestInit) => qqHttps(url, init, this.#diagnostics)
        : (init: RequestInit) => fetch(url, init);
      const response = await send({
        ...init,
        redirect: "manual",
        signal,
      });
      phase = "body";
      let data: Record<string, unknown>;
      try {
        if (options.raw || response.status === 204) {
          await response.body?.cancel();
          data = {};
        } else data = await readObject(response, this.#limit);
      } catch (error) {
        // A known HTTP rejection remains useful even when a gateway serves HTML.
        // A body timeout still retains the unknown delivery boundary.
        if (!(error instanceof QqRequestError) || response.status < 400) throw error;
        data = {};
      }
      return {
        httpStatus: response.status,
        data,
        traceId: traceOf(data, response.headers.get("X-Tps-trace-ID")),
        retryAfterMs: retryAfter(response.headers.get("Retry-After"), this.#now()),
      };
    } catch (error) {
      if (error instanceof QqRequestError) throw error;
      throw new QqRequestError({
        category: "transport",
        transportReason: signal.aborted
          ? timeout.aborted && signal.reason === timeout.reason ? "timeout" : "aborted"
          : "network",
        phase,
        elapsedMs: Math.max(0, Math.round(performance.now() - startedAt)),
      });
    }
  }

  async #sdk(
    operation: Parameters<typeof sdkRequest>[0],
    args: unknown[],
    init: RequestInit,
    options: { origin?: URL; timeoutMs?: number } = {},
  ): Promise<ResponseInfo> {
    const body = init.body === undefined ? [] : [JSON.parse(String(init.body))];
    return await sdkRequest<ResponseInfo>(
      operation,
      [...args, ...body],
      (path, wire) => this.#request(path, { ...init, ...wire }, options),
    );
  }

  /** Large resources use QQ's signed binary upload, without forwarding bot credentials. */
  async #uploadChunks(
    bytes: Uint8Array,
    name: string,
    target: string,
    privateChat: boolean,
    token: string,
  ): Promise<ResponseInfo> {
    const deadline = performance.now() + this.#uploadTimeout;
    const remaining = () => {
      const ms = Math.ceil(deadline - performance.now());
      if (ms <= 0) throw new QqRequestError({ category: "transport", transportReason: "timeout" });
      return ms;
    };
    const hash = (data: Uint8Array, algorithm = "md5") =>
      createHash(algorithm).update(data).digest("hex");
    const post = async (
      operation: Parameters<typeof sdkRequest>[0],
      data: unknown,
      statuses: number[],
    ) => {
      const response = await this.#sdk(operation, [target], {
        method: "POST",
        headers: { Authorization: `QQBot ${token}`, "Content-Type": "application/json" },
        body: JSON.stringify(data),
      }, { timeoutMs: remaining() });
      const code = codeOf(response.data);
      if (!statuses.includes(response.httpStatus) || (code !== undefined && code !== 0)) {
        throw new QqRequestError(errorInfo(response, code));
      }
      return response;
    };
    const prepare = await post(privateChat ? "uploadPreparePrivate" : "uploadPrepareGuild", {
      file_type: 4,
      file_size: String(bytes.length),
      file_name: name,
      md5: hash(bytes),
      sha1: hash(bytes, "sha1"),
      md5_10m: hash(bytes.subarray(0, 10002432)),
    }, [200, 201]);
    const { upload_id: uploadId, parts, block_size: blockSize } = prepare.data;
    const invalid = () => {
      throw new QqRequestError({ category: "invalid_response" });
    };
    if (!nonempty(uploadId) || !Array.isArray(parts) || !parts.length || parts.length > 4096) {
      invalid();
    }
    const sorted = (parts as unknown[]).map((part) => {
      if (!object(part)) return invalid();
      return part;
    }).sort((a, b) => Number(a.index) - Number(b.index));
    let offset = 0;
    // Validate the entire plan before sending any bytes to a signed destination.
    const plan = sorted.map((part, i) => {
      const declaredSize = part.block_size ?? blockSize;
      const size = typeof declaredSize === "string" && /^[1-9][0-9]*$/.test(declaredSize)
        ? Number(declaredSize)
        : declaredSize;
      if (
        part.index !== i + 1 || !Number.isSafeInteger(size) || Number(size) <= 0 ||
        offset >= bytes.length || !nonempty(part.presigned_url)
      ) return invalid();
      let url: URL;
      try {
        url = new URL(part.presigned_url as string);
      } catch {
        return invalid();
      }
      const localFixture = this.#base.protocol === "http:" && url.origin === this.#base.origin;
      if (
        url.username || url.password || url.hash ||
        (!localFixture &&
          (url.protocol !== "https:" || url.port ||
            !/\.(?:myqcloud\.com|qq\.com)$/.test(url.hostname)))
      ) return invalid();
      const end = Math.min(offset + Number(size), bytes.length);
      const chunk = bytes.subarray(offset, end);
      offset = end;
      return { url, chunk, index: i + 1 };
    });
    if (offset !== bytes.length) invalid();
    for (const { url, chunk, index } of plan) {
      const uploaded = await this.#request(url.href, {
        method: "PUT",
        body: new Uint8Array(chunk),
      }, { timeoutMs: remaining(), raw: true });
      if (uploaded.httpStatus < 200 || uploaded.httpStatus >= 300) {
        throw new QqRequestError(errorInfo(uploaded));
      }
      await post(privateChat ? "uploadPartFinishPrivate" : "uploadPartFinishGuild", {
        upload_id: uploadId,
        part_index: index,
        block_size: String(chunk.length),
        md5: hash(chunk),
      }, [200, 201, 202, 204]);
    }
    return await post(privateChat ? "sendFilePrivate" : "sendFileGuild", {
      file_type: 4,
      srv_send_msg: false,
      file_name: name,
      upload_id: uploadId,
    }, [200, 201]);
  }

  async #authenticate(): Promise<string> {
    const startedAt = this.#now();
    const response = await this.#request("/app/getAppAccessToken", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        appId: this.#credentials.appId,
        clientSecret: this.#credentials.appSecret,
      }),
    }, { origin: this.#tokenBase, timeoutMs: this.#tokenTimeout });
    const code = codeOf(response.data);
    if (response.httpStatus !== 200 || (code !== undefined && code !== 0)) {
      throw new QqRequestError({ ...errorInfo(response, code), failureStage: "auth" });
    }
    const token = response.data.access_token;
    const rawExpiry = response.data.expires_in;
    const expiry = typeof rawExpiry === "string" && /^\d+$/.test(rawExpiry)
      ? Number(rawExpiry)
      : rawExpiry;
    if (
      !nonempty(token) || typeof expiry !== "number" || !Number.isSafeInteger(expiry) ||
      expiry <= 0 ||
      !Number.isSafeInteger(startedAt + expiry * 1000)
    ) {
      throw new QqRequestError({
        category: "invalid_response",
        httpStatus: response.httpStatus,
        traceId: response.traceId,
      });
    }
    this.#token = { value: token, expiresAt: startedAt + expiry * 1000 };
    return token;
  }

  /** 凭证留在适配器内存；公开接口不返回 access_token。 */
  async authenticate(): Promise<void> {
    await this.#accessToken();
  }

  close(): void {
    this.#closed.abort();
  }

  /** A definite platform auth rejection permits the next attempt to obtain a new token. */
  invalidateToken(): void {
    this.#token = undefined;
  }

  /** 获取官方接入点并创建连接；Hello 到达后由网关处理器调用鉴权。 */
  async openGateway(): Promise<QqGatewayConnection> {
    const token = await this.#accessToken();
    const response = await this.#sdk("getGateway", [], {
      headers: { Authorization: `QQBot ${token}` },
    });
    const code = codeOf(response.data);
    if (response.httpStatus !== 200 || (code !== undefined && code !== 0)) {
      throw new QqRequestError(errorInfo(response, code));
    }
    let url: URL;
    try {
      url = new URL(String(response.data.url));
    } catch {
      throw new QqRequestError({ category: "invalid_response" });
    }
    const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname);
    // 当前官方 HTTPS 响应仍可能返回旧的 api.sgroup.qq.com 网关；只允许明确列出的官方主机。
    const trustedHost = url.hostname === this.#base.hostname ||
      (["api.bot.qq.com", "api.sgroup.qq.com"].includes(this.#base.hostname) &&
        ["api.bot.qq.com", "api.sgroup.qq.com"].includes(url.hostname));
    if (
      url.username || url.password || url.hash || !trustedHost ||
      (url.protocol !== "wss:" &&
        !(url.protocol === "ws:" && loopback && this.#base.protocol === "http:"))
    ) throw new QqRequestError({ category: "invalid_response" });
    const starts = response.data.session_start_limit;
    if (object(starts) && starts.remaining === 0) {
      const reset = starts.reset_after;
      const resetAfterMs = typeof reset === "number" && Number.isSafeInteger(reset) && reset >= 0
        ? Math.min(3_600_000, reset)
        : undefined;
      const delay = Math.max(response.retryAfterMs ?? 0, resetAfterMs ?? 0);
      throw new QqRequestError({
        category: "rate_limit",
        httpStatus: response.httpStatus,
        ...(delay > 0 ? { retryAfterMs: delay } : {}),
      });
    }
    const socket = new WebSocket(url.href);
    return {
      socket,
      authenticate: (intents, resume) => {
        if (!Number.isSafeInteger(intents) || intents < 0) requestError();
        if (
          resume &&
          (!nonempty(resume.sessionId) || !Number.isSafeInteger(resume.sequence) ||
            resume.sequence < 0)
        ) {
          requestError();
        }
        socket.send(
          JSON.stringify(
            resume
              ? {
                op: 6,
                d: { token: `QQBot ${token}`, session_id: resume.sessionId, seq: resume.sequence },
              }
              : { op: 2, d: { token: `QQBot ${token}`, intents, shard: [0, 1] } },
          ),
        );
      },
    };
  }

  async #accessToken(): Promise<string> {
    if (this.#token && this.#now() < this.#token.expiresAt - 60_000) return this.#token.value;
    if (!this.#refresh) this.#refresh = this.#authenticate();
    const refresh = this.#refresh;
    try {
      return await refresh;
    } finally {
      if (this.#refresh === refresh) this.#refresh = undefined;
    }
  }

  async botIdentity(): Promise<{ id: string; username?: string; bot?: boolean }> {
    const token = await this.#accessToken();
    const response = await this.#sdk("getMe", [], {
      headers: { Authorization: `QQBot ${token}` },
    });
    const code = codeOf(response.data);
    if (response.httpStatus !== 200 || (code !== undefined && code !== 0)) {
      throw new QqRequestError(errorInfo(response, code));
    }
    if (!nonempty(response.data.id)) {
      throw new QqRequestError({ category: "invalid_response", httpStatus: response.httpStatus });
    }
    return {
      id: response.data.id,
      ...(typeof response.data.username === "string" ? { username: response.data.username } : {}),
      ...(typeof response.data.bot === "boolean" ? { bot: response.data.bot } : {}),
    };
  }

  /** 一次调用只尝试主动发送一次；不自动重试、改正文或补写被动回复字段。 */
  async sendText(request: QqSendText): Promise<QqSendResult> {
    request = structuredClone(request);
    const body = textBody(request);
    if (this.#scope === "private" && request.destination.kind !== "user") {
      return { status: "failed", attempted: false, error: { category: "scope" } };
    }
    let token: string;
    try {
      token = await this.#accessToken();
    } catch (error) {
      return { status: "failed", attempted: false, error: (error as QqRequestError).info };
    }
    let response: ResponseInfo;
    const scope = request.destination.kind === "user" ? "c2c" : "group";
    const target = encodeURIComponent(request.destination.openId);
    if (request.attachment) {
      let bytes: Uint8Array;
      try {
        const downloaded = await this.#downloadedReader?.(request.attachment.path);
        if (downloaded) bytes = downloaded;
        else if (this.#attachmentReader) {
          bytes = await this.#attachmentReader(request.attachment.path);
        } else requestError();
      } catch {
        return {
          status: "failed",
          attempted: false,
          error: { category: "request", failureStage: "upload" },
        };
      }
      let upload: ResponseInfo;
      try {
        upload = request.attachment.kind === "file" && bytes.length > 5 * 1024 * 1024
          ? await this.#uploadChunks(
            bytes,
            fileName(request.attachment.path),
            target,
            scope === "c2c",
            token,
          )
          : await this.#sdk(
            scope === "c2c" ? "sendFilePrivate" : "sendFileGuild",
            [target],
            {
              method: "POST",
              headers: {
                Authorization: `QQBot ${token}`,
                "Content-Type": "application/json; charset=utf-8",
              },
              body: JSON.stringify({
                file_type: request.attachment.kind === "image" ? 1 : 4,
                file_data: Buffer.from(bytes).toString("base64"),
                srv_send_msg: false,
                ...(request.attachment.kind === "file"
                  ? { file_name: fileName(request.attachment.path) }
                  : {}),
              }),
            },
            { timeoutMs: this.#uploadTimeout },
          );
      } catch (error) {
        if (error instanceof QqRequestError && error.info.category === "authentication") {
          this.invalidateToken();
        }
        return {
          // srv_send_msg:false only uploads a resource; /messages has not been attempted.
          status: "failed",
          attempted: false,
          error: {
            ...(error instanceof QqRequestError ? error.info : { category: "request" as const }),
            failureStage: "upload",
          },
        };
      }
      let code: number | undefined;
      try {
        code = codeOf(upload.data);
      } catch (error) {
        return {
          status: "failed",
          attempted: false,
          error: {
            ...(error instanceof QqRequestError ? error.info : { category: "request" as const }),
            failureStage: "upload",
          },
        };
      }
      if (![200, 201].includes(upload.httpStatus) || (code !== undefined && code !== 0)) {
        const error = { ...errorInfo(upload, code), failureStage: "upload" as const };
        if (error.category === "authentication") this.invalidateToken();
        return {
          status: "failed",
          attempted: false,
          error,
        };
      }
      if (!nonempty(upload.data.file_info)) {
        return {
          status: "failed",
          attempted: false,
          error: {
            category: "invalid_response",
            httpStatus: upload.httpStatus,
            traceId: upload.traceId,
            failureStage: "upload",
          },
        };
      }
      body.media = { file_info: upload.data.file_info };
    }
    try {
      response = await this.#sdk(
        scope === "c2c" ? "sendPrivateMessage" : "sendMessage",
        [target],
        {
          method: "POST",
          headers: {
            Authorization: `QQBot ${token}`,
            "Content-Type": "application/json; charset=utf-8",
          },
          body: JSON.stringify(body),
        },
      );
    } catch (error) {
      return { status: "unknown", attempted: true, error: (error as QqRequestError).info };
    }
    let code: number | undefined;
    try {
      code = codeOf(response.data);
    } catch (error) {
      return { status: "unknown", attempted: true, error: (error as QqRequestError).info };
    }
    const error: QqErrorInfo = { ...errorInfo(response, code), failureStage: "message" };
    if (error.category === "authentication") this.invalidateToken();
    // 异步审核没有送达回执；去重也不能替代查询原动作，尤其不能换序号再发送。
    if ([304023, 304024].includes(code ?? -1) || [201, 202].includes(response.httpStatus)) {
      return { status: "unknown", attempted: true, error: { ...error, category: "pending" } };
    }
    if (code === 40054005) {
      return { status: "unknown", attempted: true, error: { ...error, category: "duplicate" } };
    }
    if (response.httpStatus >= 500 || (response.httpStatus >= 300 && response.httpStatus < 400)) {
      return { status: "unknown", attempted: true, error };
    }
    if (code !== undefined && code !== 0) return { status: "failed", attempted: true, error };
    if (response.httpStatus >= 400) return { status: "failed", attempted: true, error };
    if (response.httpStatus !== 200 || !nonempty(response.data.id)) {
      return {
        status: "unknown",
        attempted: true,
        error: { ...error, category: "invalid_response" },
      };
    }
    const ext = response.data.ext_info;
    return {
      status: "sent",
      messageId: response.data.id,
      ...(typeof response.data.timestamp === "string"
        ? { timestamp: response.data.timestamp }
        : {}),
      ...(object(ext) && nonempty(ext.ref_idx) ? { referenceIndex: ext.ref_idx } : {}),
      ...(response.traceId ? { traceId: response.traceId } : {}),
    };
  }
}
