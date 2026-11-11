// 独立适配器的 QQ HTTP 传输。不依赖内核或插件，不承担身份映射、通知和行为判断。
// 返回平台事实；持久动作记录、入站日志和所有权交接由适配器服务另行接入。

export interface QqCredentials {
  appId: string;
  appSecret: string;
}

export interface QqClientOptions {
  /** 正式环境使用官方 HTTPS 地址；HTTP 只允许本机测试地址。 */
  baseUrl?: string;
  timeoutMs?: number;
  maxResponseBytes?: number;
  now?: () => number;
}

export type QqErrorCategory =
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
  /** 调用方为一个动作固定序号；传输层不因失败或未知擅自递增。 */
  reply?: { messageId: string; sequence: number } | { eventId: string };
  /** 引用展示用的 ref_idx，不是被动回复用的 msg_id。 */
  referenceIndex?: string;
  wakeup?: boolean;
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
  const category = response.httpStatus === 429 || code === 100001 || code === 40034100
    ? "rate_limit"
    : response.httpStatus === 401 || code === 100016 || code === 11243
    ? "authentication"
    : "platform";
  return { category, httpStatus: response.httpStatus, code, traceId: response.traceId };
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
    !nonempty(request.destination.openId) || !nonempty(request.text)
  ) requestError();
  const body: Record<string, unknown> = { content: request.text, msg_type: 0 };
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
      body.msg_id = reply.messageId;
      body.msg_seq = reply.sequence;
    } else {
      if (!nonempty(reply.eventId)) requestError();
      body.event_id = reply.eventId;
    }
  }
  if (request.referenceIndex !== undefined) {
    if (!nonempty(request.referenceIndex)) requestError();
    body.message_reference = { message_id: request.referenceIndex };
  }
  if (request.wakeup !== undefined) {
    if (typeof request.wakeup !== "boolean") requestError();
    if (request.wakeup) {
      if (request.reply || request.destination.kind !== "user") requestError();
      body.is_wakeup = true;
    }
  }
  return body;
}

export class QqClient {
  #base: URL;
  #credentials: QqCredentials;
  #timeout: number;
  #limit: number;
  #now: () => number;
  #token?: { value: string; expiresAt: number };
  #refresh?: Promise<string>;

  constructor(credentials: QqCredentials, options: QqClientOptions = {}) {
    if (!nonempty(credentials.appId) || !nonempty(credentials.appSecret)) requestError();
    const base = new URL(options.baseUrl ?? "https://api.bot.qq.com");
    const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(base.hostname);
    if (
      base.username || base.password || base.search || base.hash || base.pathname !== "/" ||
      (base.protocol !== "https:" && !(base.protocol === "http:" && loopback))
    ) requestError();
    const timeout = options.timeoutMs ?? 20_000;
    const limit = options.maxResponseBytes ?? 1_048_576;
    if (!Number.isFinite(timeout) || timeout <= 0 || !Number.isSafeInteger(limit) || limit < 1) {
      requestError();
    }
    this.#base = base;
    this.#credentials = { ...credentials };
    this.#timeout = timeout;
    this.#limit = limit;
    this.#now = options.now ?? Date.now;
  }

  async #request(path: string, init: RequestInit): Promise<ResponseInfo> {
    try {
      const response = await fetch(new URL(path, this.#base), {
        ...init,
        redirect: "manual",
        signal: AbortSignal.timeout(this.#timeout),
      });
      const data = await readObject(response, this.#limit);
      return {
        httpStatus: response.status,
        data,
        traceId: traceOf(data, response.headers.get("X-Tps-trace-ID")),
      };
    } catch (error) {
      if (error instanceof QqRequestError) throw error;
      throw new QqRequestError({ category: "transport" });
    }
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
    });
    const code = codeOf(response.data);
    if (response.httpStatus !== 200 || (code !== undefined && code !== 0)) {
      throw new QqRequestError(errorInfo(response, code));
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
    const response = await this.#request("/users/@me", {
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

  /** 一次调用只尝试发送一次，不自动重试、递增序号、改正文或切换为主动消息。 */
  async sendText(request: QqSendText): Promise<QqSendResult> {
    request = structuredClone(request);
    const body = textBody(request);
    let token: string;
    try {
      token = await this.#accessToken();
    } catch (error) {
      return { status: "failed", attempted: false, error: (error as QqRequestError).info };
    }
    let response: ResponseInfo;
    try {
      const resource = request.destination.kind === "user" ? "users" : "groups";
      response = await this.#request(
        `/v2/${resource}/${encodeURIComponent(request.destination.openId)}/messages`,
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
    const error = errorInfo(response, code);
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
