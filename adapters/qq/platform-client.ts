import type { PhonePlatformReadResult } from "../../contracts/phone-platform.ts";
import { QqRequestError } from "./client.ts";
import * as fixedCatalog from "./platform-catalog.ts";
import { platformDigest, platformLedger } from "./platform-tool.ts";

export type QqPlatformAccess = fixedCatalog.PlatformAccess;
export type QqPlatformRequest = (
  method: string,
  path: string,
  body?: unknown,
) => Promise<{ httpStatus: number; data: unknown }>;

type Route =
  & Pick<fixedCatalog.PlatformOperation, "access" | "method" | "path">
  & Partial<
    Pick<
      fixedCatalog.PlatformOperation,
      "queryParams" | "bodyParams" | "requiredQuery" | "requiredBody"
    >
  >;

export interface QqPlatformCatalog {
  lookup(operation: string): Route | undefined;
  build(operation: string, params: Record<string, unknown>): { path: string; body?: unknown };
}

export type QqPlatformConfirm = (
  plan: Readonly<{
    operation: string;
    method: Route["method"];
    template: string;
    access: QqPlatformAccess;
    digest: string;
  }>,
) => boolean | Promise<boolean>;

type Json = string | number | boolean | null | Json[] | { [key: string]: Json };
const reasons = [
  "invalid_params",
  "not_allowed",
  "confirmation_required",
  "idempotency_required",
  "idempotency_conflict",
  "incomplete",
  "audit_unavailable",
  "authentication",
  "not_started",
  "request_rejected",
  "transport",
  "invalid_response",
  "http_error",
  "platform_error",
  "rate_limit",
  "pending",
  "duplicate",
] as const;
type Reason = typeof reasons[number];
interface Facts {
  httpStatus?: number;
  code?: number;
  errCode?: number;
}
export type QqPlatformResult =
  | { status: "ok"; httpStatus: number; data: Json; truncated?: true }
  | ({ status: "failed" | "unknown"; reason: Reason } & Facts);

const tiers = ["read", "write", "admin"] as const;
const methods = ["GET", "POST", "PUT", "PATCH", "DELETE"];
const encoder = new TextEncoder();

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null &&
    (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);
}

function invalid(): never {
  throw new TypeError("QQ 平台参数无效");
}

function natural(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function httpStatus(value: unknown): value is number {
  return natural(value) && value >= 100 && value <= 599;
}

function jsonCopy(root: unknown): Json {
  let nodes = 0;
  let characters = 0;
  function copy(value: unknown, depth: number): Json {
    if (++nodes > 1024 || depth > 12) invalid();
    if (typeof value === "string") {
      characters += value.length;
      if (value.length > 16_384 || characters > 65_536) invalid();
      return value;
    }
    if (value === null || typeof value === "boolean") return value;
    if (typeof value === "number" && Number.isFinite(value)) return value;
    if (Array.isArray(value)) {
      if (value.length > 256 || Reflect.ownKeys(value).length !== value.length + 1) invalid();
      return Array.from({ length: value.length }, (_, index) => {
        const field = Object.getOwnPropertyDescriptor(value, index);
        if (!field?.enumerable || !("value" in field)) invalid();
        return copy(field.value, depth + 1);
      });
    }
    if (!record(value)) invalid();
    const keys = Object.keys(value);
    if (keys.length > 128 || keys.length !== Reflect.ownKeys(value).length) invalid();
    const result: Record<string, Json> = {};
    for (const key of keys.sort()) {
      characters += key.length;
      if (
        characters > 65_536 || key.length > 96 ||
        ["__proto__", "constructor", "prototype", "toJSON"].includes(key)
      ) invalid();
      const field = Object.getOwnPropertyDescriptor(value, key)!;
      if (!("value" in field)) invalid();
      result[key] = copy(field.value, depth + 1);
    }
    return result;
  }
  const value = copy(root, 0);
  if (encoder.encode(JSON.stringify(value)).length > 65_536) invalid();
  return value;
}

function relativeTarget(route: Route, params: Record<string, unknown>, target: string): boolean {
  const template = route.path;
  if (typeof target !== "string" || target.length > 8192 || typeof template !== "string") {
    return false;
  }
  const [path, query, extra] = target.split("?");
  if (extra !== undefined || !path.startsWith("/") || !template.startsWith("/")) return false;
  const segments = path.slice(1).split("/");
  const pattern = template.slice(1).split("/");
  const supplied = Object.hasOwn(params, "path") ? params.path : {};
  if (!record(supplied) || Object.keys(supplied).some((key) => !pattern.includes(`{${key}}`))) {
    return false;
  }
  if (
    segments.length !== pattern.length || !segments.every((segment, index) => {
      const token = pattern[index];
      const placeholder = /^\{([A-Za-z]\w*)\}$/.exec(token);
      if (placeholder) {
        const value = supplied[placeholder[1]];
        return index > 0 && typeof value === "string" && value.length <= 256 &&
          ((/^[A-Za-z0-9_~.+=-]+$/.test(value) && /[A-Za-z0-9]/.test(value)) ||
            (placeholder[1] === "emojiId" && /^\p{RGI_Emoji}$/v.test(value))) &&
          segment === encodeURIComponent(value);
      }
      return /^@?[A-Za-z0-9_-]+$/.test(token) && token === segment;
    })
  ) return false;
  const inputQuery = Object.hasOwn(params, "query") ? params.query : {};
  if (!record(inputQuery) || route.requiredQuery?.some((key) => !Object.hasOwn(inputQuery, key))) {
    return false;
  }
  const keys = Object.keys(inputQuery);
  if (query === undefined) return keys.length === 0;
  if (!query || query.length > 4096) return false;
  const search = new URLSearchParams(query);
  return search.size === keys.length && search.toString() === query && keys.every((key) => {
    const value = inputQuery[key];
    return route.queryParams?.includes(key) &&
      ["string", "number", "boolean"].includes(typeof value) &&
      (typeof value !== "string" || !/\p{C}/u.test(value)) &&
      search.get(key) === String(value);
  });
}

const idFields = [
  "id",
  "openid",
  "group_openid",
  "user_openid",
  "member_openid",
  "guild_id",
  "channel_id",
  "parent_id",
  "user_id",
  "role_id",
  "message_id",
  "schedule_id",
  "thread_id",
  "owner_id",
];
const labelFields = ["name", "username", "nick", "group_name"];
const numberFields = [
  "member_count",
  "max_members",
  "online_nums",
  "count",
  "total",
  "limit",
  "position",
  "type",
  "sub_type",
  "status",
  "permissions",
  "color",
];
const boolFields = [
  "bot",
  "owner",
  "allow_proactive_msg",
  "has_more",
  "is_private",
  "hoist",
  "mentionable",
];
const containerFields = [
  "data",
  "items",
  "list",
  "guilds",
  "guild",
  "channels",
  "channel",
  "members",
  "member",
  "users",
  "user",
  "author",
  "roles",
  "role",
  "messages",
  "schedules",
  "schedule",
  "threads",
  "thread",
  "reactions",
  "pins",
];
const fields = [...idFields, ...labelFields, ...numberFields, ...boolFields, ...containerFields];

function identifier(value: unknown): number | string | undefined {
  if (
    natural(value) || (typeof value === "string" && /^[A-Za-z0-9_~.+=-]{1,128}$/.test(value) &&
      !sensitive(value))
  ) return value;
}

function sensitive(value: string) {
  return /bearer|qqbot[\s:_-]|token|secret|signature|credential|file_info|^eyJ[\w-]*\./i.test(
    value,
  );
}

function label(value: unknown): string | undefined {
  if (
    typeof value === "string" && /^[\p{L}\p{N}\p{M} ()（）_-]{1,64}$/u.test(value) &&
    !sensitive(value)
  ) return value;
}

function projectSelf(root: unknown) {
  const info = record(root) ? root : {};
  const id = identifier(info.id), username = label(info.username);
  return {
    data: {
      ...(id === undefined ? {} : { id }),
      ...(username === undefined ? {} : { username }),
      ...(typeof info.bot === "boolean" ? { bot: info.bot } : {}),
    },
  };
}

function project(root: unknown): { data: Json; truncated?: true } {
  let remaining = 8192;
  let nodes = 256;
  let truncated = false;
  function spend(bytes: number) {
    if (bytes > remaining) {
      truncated = true;
      return false;
    }
    remaining -= bytes;
    return true;
  }
  function visit(value: unknown, depth: number): Json | undefined {
    if (--nodes < 0 || depth > 6 || !spend(2)) {
      truncated = true;
      return undefined;
    }
    if (Array.isArray(value)) {
      const result: Json[] = [];
      if (value.length > 32) truncated = true;
      for (const item of value.slice(0, 32)) {
        if (!spend(1)) break;
        if (record(item)) {
          const safe = visit(item, depth + 1);
          if (safe !== undefined) result.push(safe);
        }
      }
      return result;
    }
    if (!record(value)) return undefined;
    const result: Record<string, Json> = {};
    for (const key of fields) {
      if (!Object.hasOwn(value, key)) continue;
      const field = value[key];
      let scalar: string | number | boolean | undefined;
      if (idFields.includes(key)) {
        scalar = identifier(field);
      } else if (labelFields.includes(key)) {
        scalar = label(field);
      } else if (numberFields.includes(key)) {
        if (natural(field) || (typeof field === "string" && /^\d{1,20}$/.test(field))) {
          scalar = field;
        }
      } else if (boolFields.includes(key)) {
        if (typeof field === "boolean") scalar = field;
      } else if ((record(field) || Array.isArray(field)) && spend(key.length + 4)) {
        const child = visit(field, depth + 1);
        if (child !== undefined) result[key] = child;
      }
      if (
        scalar !== undefined &&
        spend(key.length + 4 + encoder.encode(JSON.stringify(scalar)).length)
      ) {
        result[key] = scalar;
      }
    }
    return result;
  }
  const data = visit(root, 0) ?? {};
  return truncated ? { data, truncated } : { data };
}

function facts(status: unknown, code: unknown, errCode: unknown): Facts {
  return {
    ...(httpStatus(status) ? { httpStatus: status } : {}),
    ...(natural(code) ? { code } : {}),
    ...(natural(errCode) ? { errCode } : {}),
  };
}

function messageId(value: unknown) {
  return record(value) && typeof value.id === "string" ? identifier(value.id) : undefined;
}

function projectAck(value: unknown, message: boolean): { data: Json } {
  const id = message ? messageId(value) : undefined;
  return { data: id === undefined ? {} : { id } };
}

function decodeReceipt(value: unknown, message: boolean): QqPlatformResult | undefined {
  if (!record(value)) return;
  if (value.status === "ok") {
    if (
      !httpStatus(value.httpStatus) || ![200, 201, 204].includes(value.httpStatus) ||
      Object.keys(value).some((key) => !["status", "httpStatus", "data"].includes(key)) ||
      !record(value.data)
    ) return;
    const data = projectAck(value.data, message).data;
    if (
      JSON.stringify(value.data) !== JSON.stringify(data) || (message &&
        (value.httpStatus === 204 || messageId(value.data) === undefined))
    ) return;
    return { status: "ok", httpStatus: value.httpStatus, data };
  }
  const allowed = value.status === "failed"
    ? [
      "authentication",
      "not_started",
      "request_rejected",
      "http_error",
      "platform_error",
      "rate_limit",
    ]
    : value.status === "unknown"
    ? ["transport", "invalid_response", "http_error", "pending", "duplicate"]
    : [];
  if (
    (value.status === "failed" || value.status === "unknown") && typeof value.reason === "string" &&
    allowed.includes(value.reason) &&
    Object.keys(value).every((key) =>
      ["status", "reason", "httpStatus", "code", "errCode"].includes(key)
    ) &&
    (value.httpStatus === undefined || httpStatus(value.httpStatus)) &&
    (value.code === undefined || natural(value.code)) &&
    (value.errCode === undefined || natural(value.errCode))
  ) {
    return {
      status: value.status,
      reason: value.reason as Reason,
      ...facts(value.httpStatus, value.code, value.errCode),
    };
  }
}

function isMessagePost(method: string, path: string) {
  return method === "POST" && /\/(?:stream_)?messages$/.test(path.split("?", 1)[0]);
}

async function dispatch<Projection extends { data: Json; truncated?: true }>(
  request: QqPlatformRequest,
  method: string,
  path: string,
  body: Json | undefined,
  projectData: (value: unknown) => Projection,
): Promise<
  | ({ status: "ok"; httpStatus: number } & Projection)
  | Extract<PhonePlatformReadResult, { reason: unknown }>
> {
  try {
    const { httpStatus: status, data } = await request(method, path, body);
    if (!httpStatus(status)) return { status: "unknown", reason: "invalid_response" };
    const info = record(data) ? data : {};
    const meta = facts(status, info.code, info.err_code);
    const codes = [meta.errCode, meta.code];
    if (status === 202 || codes.includes(304023) || codes.includes(304024)) {
      return { status: "unknown", reason: "pending", ...meta };
    }
    if (codes.includes(40054005)) return { status: "unknown", reason: "duplicate", ...meta };
    if (
      status < 200 || (status >= 300 && status < 400) || status >= 500 ||
      [408, 409].includes(status)
    ) {
      return { status: "unknown", reason: "http_error", ...meta };
    }
    if (status === 429 || [100001, 40034100, 22009].some((code) => codes.includes(code))) {
      return { status: "failed", reason: "rate_limit", ...meta };
    }
    if (status === 401 || codes.includes(100016) || codes.includes(11243)) {
      return { status: "failed", reason: "authentication", ...meta };
    }
    if (status >= 400) return { status: "failed", reason: "http_error", ...meta };
    if (
      (method === "GET" ? status !== 200 : ![200, 201, 204].includes(status)) ||
      (status !== 204 && !record(data) && !Array.isArray(data)) ||
      ["code", "err_code"].some((key) => Object.hasOwn(info, key) && !natural(info[key]))
    ) {
      return { status: "unknown", reason: "invalid_response", ...meta };
    }
    if (codes.some(Boolean)) return { status: "failed", reason: "platform_error", ...meta };
    if (path === "/users/@me" && identifier(info.id) === undefined) {
      return { status: "unknown", reason: "invalid_response", ...meta };
    }
    if (isMessagePost(method, path) && (status === 204 || messageId(info) === undefined)) {
      return {
        status: "unknown",
        reason: status === 201 ? "pending" : "invalid_response",
        ...meta,
      };
    }
    return { status: "ok", httpStatus: status, ...projectData(status === 204 ? {} : data) };
  } catch (error) {
    if (error instanceof QqRequestError) {
      const info = error.info;
      const meta = facts(info.httpStatus, info.code, info.errCode);
      if (info.failureStage === "auth") {
        if (info.category === "rate_limit") {
          return { status: "failed", reason: "rate_limit", ...meta };
        }
        if (info.category === "transport" || info.category === "invalid_response") {
          return { status: "failed", reason: "not_started", ...meta };
        }
        return { status: "failed", reason: "authentication", ...meta };
      }
      if (info.requestStarted === false) {
        return { status: "failed", reason: "not_started", ...meta };
      }
      if (info.category === "scope" || info.category === "request") {
        return { status: "failed", reason: "request_rejected", ...meta };
      }
      if (info.category === "invalid_response") {
        return { status: "unknown", reason: "invalid_response", ...meta };
      }
      return { status: "unknown", reason: "transport", ...meta };
    }
    return { status: "unknown", reason: "transport" };
  }
}

interface Options {
  catalog?: QqPlatformCatalog;
  request: QqPlatformRequest;
}

function parseCall(raw: unknown) {
  const input = jsonCopy(raw);
  if (
    !record(input) || !record(input.params) || typeof input.operation !== "string" ||
    !/^[A-Za-z][\w.-]{0,95}$/.test(input.operation) ||
    Object.keys(input).some((key) => !["operation", "params", "idempotencyKey"].includes(key))
  ) invalid();
  return { operation: input.operation, params: input.params, key: input.idempotencyKey };
}

function planFor(
  catalog: QqPlatformCatalog,
  route: Route,
  operation: string,
  params: Record<string, unknown>,
) {
  if (Object.keys(params).some((key) => !["path", "query", "body"].includes(key))) invalid();
  const supplied = params.body;
  if (
    supplied !== undefined &&
    (!record(supplied) || !route.bodyParams?.length || route.method === "GET" ||
      Object.keys(supplied).some((key) => !route.bodyParams?.includes(key)))
  ) invalid();
  if (route.requiredBody?.some((key) => !record(supplied) || !Object.hasOwn(supplied, key))) {
    invalid();
  }
  const expectedBody = JSON.stringify(supplied);
  const { path, body: builtBody } = catalog.build(
    operation,
    jsonCopy(params) as Record<string, Json>,
  );
  if (!relativeTarget(route, params, path) || (route.method === "GET" && builtBody !== undefined)) {
    invalid();
  }
  const body = builtBody === undefined ? undefined : jsonCopy(builtBody);
  if (JSON.stringify(body) !== expectedBody) invalid();
  return { method: route.method, path, body };
}

export function createQqPlatformOperator(
  { catalog = fixedCatalog, request, directory, access }: Options & {
    directory: string;
    access: QqPlatformAccess;
  },
) {
  if (!tiers.includes(access) || typeof request !== "function") {
    throw new TypeError("QQ 平台通道配置无效");
  }
  const ledger = platformLedger(directory);
  return async (raw: unknown, confirm?: QqPlatformConfirm): Promise<QqPlatformResult> => {
    try {
      const { operation, params, key } = parseCall(raw);
      const descriptor = catalog.lookup(operation);
      const route = descriptor && { ...descriptor };
      if (
        !route || !methods.includes(route.method) || !tiers.includes(route.access) ||
        tiers.indexOf(route.access) > tiers.indexOf(access) ||
        (route.method !== "GET" && route.access === "read")
      ) {
        return { status: "failed", reason: "not_allowed" };
      }
      if (
        route.method !== "GET" &&
        (typeof key !== "string" || !/^[A-Za-z0-9][\w.:-]{0,127}$/.test(key))
      ) return { status: "failed", reason: "idempotency_required" };
      if (route.method === "GET" && key !== undefined) invalid();
      const { method, path, body } = planFor(catalog, route, operation, params);
      const fingerprint = platformDigest(JSON.stringify([operation, method, path, body ?? null]));
      const requiredAccess = route.access;
      if (requiredAccess !== "read") {
        const digest = platformDigest(JSON.stringify([requiredAccess, fingerprint, key ?? null]));
        try {
          if (
            typeof confirm !== "function" ||
            await confirm(
                Object.freeze({
                  operation,
                  method,
                  access: requiredAccess,
                  template: route.path,
                  digest,
                }),
              ) !== true
          ) {
            return { status: "failed", reason: "confirmation_required" };
          }
        } catch {
          return { status: "failed", reason: "confirmation_required" };
        }
      }
      const message = isMessagePost(method, path);
      const projection = method !== "GET"
        ? (data: unknown) => projectAck(data, message)
        : path === "/users/@me"
        ? projectSelf
        : project;
      return await ledger(
        {
          operation,
          access: requiredAccess,
          fingerprint,
          ...(typeof key === "string" ? { key } : {}),
        },
        () => dispatch(request, method, path, body, projection),
        (value) => decodeReceipt(value, message),
      );
    } catch {
      return { status: "failed", reason: "invalid_params" };
    }
  };
}

export function createQqPlatformRead({ catalog = fixedCatalog, request }: Options) {
  return async (raw: unknown): Promise<PhonePlatformReadResult> => {
    try {
      const { operation, params, key } = parseCall(raw);
      if (operation !== "getMe") return { status: "failed", reason: "not_allowed" };
      if (key !== undefined || Object.keys(params).length) invalid();
      const route = catalog.lookup(operation);
      if (route?.method !== "GET" || route.path !== "/users/@me" || route.access !== "read") {
        return { status: "failed", reason: "not_allowed" };
      }
      const plan = planFor(catalog, { ...route }, operation, params);
      if (plan.method !== "GET" || plan.path !== "/users/@me" || plan.body !== undefined) {
        return { status: "failed", reason: "not_allowed" };
      }
      return await dispatch(request, "GET", "/users/@me", undefined, projectSelf);
    } catch {
      return { status: "failed", reason: "invalid_params" };
    }
  };
}
