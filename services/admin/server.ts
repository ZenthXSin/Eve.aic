import { type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import { ADMIN_READS, type AdminBackend, type AdminControl, type AdminQuery } from "./contracts.ts";
import { panelHtml } from "./view.ts";

const MAX_BODY = 65_536;
const SESSION_MS = 8 * 60 * 60 * 1000;
const COOKIE = "eve_admin_session";

/** 先按字节限制再解码，避免 request.json() 为无界请求分配内存。 */
export async function readAdminBody(request: Request): Promise<Record<string, Json>> {
  if (!request.body || request.headers.get("content-type") !== "application/json") {
    throw new KernelError("rejected", "需要 JSON 请求");
  }
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const next = await reader.read();
      if (next.done) break;
      size += next.value.length;
      if (size > MAX_BODY) {
        await reader.cancel();
        throw new KernelError("rejected", "管理请求过大");
      }
      chunks.push(next.value);
    }
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  let body: Json;
  try {
    body = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    throw new KernelError("rejected", "管理请求格式无效");
  }
  if (!body || typeof body !== "object" || Array.isArray(body)) {
    throw new KernelError("rejected", "管理请求必须是对象");
  }
  return body;
}

export function validateQuery(body: Record<string, Json>): AdminQuery {
  const resource = body.resource as keyof typeof ADMIN_READS;
  if (
    !Object.hasOwn(ADMIN_READS, resource) ||
    !(ADMIN_READS[resource] as readonly Json[]).includes(body.operation) ||
    !body.args || typeof body.args !== "object" || Array.isArray(body.args)
  ) throw new KernelError("rejected", "不支持的管理查询");
  return body as unknown as AdminQuery;
}

export function validateControl(body: Record<string, Json>): AdminControl {
  if (
    body.action === "resumeModel" && body.args && typeof body.args === "object" &&
    !Array.isArray(body.args) && Object.keys(body.args).length === 1 &&
    typeof body.args.expectedRequestId === "string" &&
    /^model_[1-9][0-9]*$/.test(body.args.expectedRequestId)
  ) return body as unknown as AdminControl;
  if (
    (body.action === "pause" || body.action === "resume") &&
    body.args && typeof body.args === "object" && !Array.isArray(body.args) &&
    Object.keys(body.args).length === 0
  ) return body as unknown as AdminControl;
  if (
    body.action === "budget" && body.args && typeof body.args === "object" &&
    !Array.isArray(body.args) && Object.keys(body.args).length === 1 &&
    Number.isSafeInteger(body.args.dailyTokenBudget) && Number(body.args.dailyTokenBudget) >= 0
  ) return body as unknown as AdminControl;
  throw new KernelError("rejected", "管理操作参数无效");
}

async function digest(text: string) {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)));
}

function same(a: Uint8Array, b: Uint8Array) {
  let difference = a.length ^ b.length;
  for (let i = 0; i < a.length; i++) difference |= a[i] ^ (b[i] ?? 0);
  return difference === 0;
}

const secureHeaders = {
  "cache-control": "no-store",
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  "x-frame-options": "DENY",
  "permissions-policy": "camera=(), microphone=(), geolocation=()",
};

/** 仅监听 loopback；浏览器只收到有期限会话与 CSRF 值，永不收到后端连接密钥。 */
export async function startAdminServer(
  backend: AdminBackend,
  options: { token: string; port?: number; now?: () => number },
) {
  if (!/^[a-zA-Z0-9_-]{32,256}$/.test(options.token)) {
    throw new KernelError("rejected", "管理口令须为至少 32 位随机字符串");
  }
  if (
    options.port !== undefined &&
    (!Number.isInteger(options.port) || options.port < 0 || options.port > 65535)
  ) {
    throw new KernelError("rejected", "管理端口无效");
  }
  const tokenHash = await digest(options.token), now = options.now ?? Date.now;
  const sessions = new Map<string, { csrf: string; expires: number }>();
  let failedLogins = 0, retryAfter = 0, origin = "";
  const response = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
    Response.json(body, { status, headers: { ...secureHeaders, ...headers } });
  const server = Deno.serve(
    { hostname: "127.0.0.1", port: options.port ?? 0, onListen() {} },
    async (request) => {
      const url = new URL(request.url), path = url.pathname;
      // Host 校验同时拒绝 DNS rebinding。即使凭据被浏览器附带，跨站请求仍先拒绝。
      if (
        url.origin !== origin || request.headers.get("host") !== new URL(origin).host ||
        (request.headers.has("origin") && request.headers.get("origin") !== origin) ||
        ["cross-site", "same-site"].includes(request.headers.get("sec-fetch-site") ?? "")
      ) return response({ ok: false, error: { message: "拒绝跨站管理请求" } }, 403);
      if (request.method === "GET" && path === "/") {
        const nonce = crypto.randomUUID();
        return new Response(panelHtml(nonce), {
          headers: {
            ...secureHeaders,
            "content-type": "text/html; charset=utf-8",
            "content-security-policy":
              `default-src 'none'; script-src 'nonce-${nonce}'; style-src 'nonce-${nonce}'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'`,
          },
        });
      }
      try {
        for (const [id, session] of sessions) if (session.expires <= now()) sessions.delete(id);
        if (request.method === "POST" && path === "/api/login") {
          if (retryAfter > now()) {
            return response(
              { ok: false, error: { message: "请稍后重试" } },
              429,
            );
          }
          const body = await readAdminBody(request);
          const candidate = typeof body.token === "string" && body.token.length <= 256
            ? body.token
            : "";
          if (!same(await digest(candidate), tokenHash)) {
            failedLogins++;
            if (failedLogins >= 5) retryAfter = now() + 60_000;
            return response({ ok: false, error: { message: "管理口令无效" } }, 401);
          }
          failedLogins = 0;
          retryAfter = 0;
          if (sessions.size >= 32) {
            return response({
              ok: false,
              error: { message: "会话数量已达上限" },
            }, 429);
          }
          const id = crypto.randomUUID(),
            session = { csrf: crypto.randomUUID(), expires: now() + SESSION_MS };
          sessions.set(id, session);
          return response(
            { ok: true, value: { csrf: session.csrf, expires: session.expires } },
            200,
            {
              "set-cookie": `${COOKIE}=${id}; HttpOnly; SameSite=Strict; Path=/; Max-Age=${
                SESSION_MS / 1000
              }`,
            },
          );
        }
        const cookie = request.headers.get("cookie")?.split(";").map((item) => item.trim())
          .find((item) => item.startsWith(`${COOKIE}=`))?.slice(COOKIE.length + 1);
        const session = cookie ? sessions.get(cookie) : undefined;
        if (!session) return response({ ok: false, error: { message: "请先登录" } }, 401);
        if (request.method === "GET" && path === "/api/session") {
          return response({ ok: true, value: { csrf: session.csrf, expires: session.expires } });
        }
        if (request.method === "GET" && path === "/api/status") {
          return response({ ok: true, value: await backend.status() });
        }
        if (request.method !== "POST" || request.headers.get("x-eve-csrf") !== session.csrf) {
          return response({ ok: false, error: { message: "管理请求校验失败" } }, 403);
        }
        if (path === "/api/logout") {
          sessions.delete(cookie!);
          return response({ ok: true, value: null }, 200, {
            "set-cookie": `${COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0`,
          });
        }
        const body = await readAdminBody(request);
        if (path === "/api/query") {
          return response({
            ok: true,
            value: await backend.query(validateQuery(body)),
          });
        }
        if (path === "/api/control") {
          return response({
            ok: true,
            value: await backend.control(validateControl(body)),
          });
        }
        return response({ ok: false, error: { message: "没有该管理入口" } }, 404);
      } catch (error) {
        return response(
          { ok: false, error: toWireError(error) },
          error instanceof KernelError && error.kind === "rejected" ? 400 : 503,
        );
      }
    },
  );
  origin = `http://127.0.0.1:${server.addr.port}`;
  return {
    url: origin,
    async close() {
      sessions.clear();
      await server.shutdown();
    },
  };
}

/** 独立管理者的 IPC 出口；密钥只交给管理面板进程，模型及实例进程均不持有。 */
export async function startAdminControlServer(backend: AdminBackend, key: string) {
  if (!/^[a-zA-Z0-9_-]{32,256}$/.test(key)) throw new KernelError("rejected", "管理 IPC 凭据无效");
  const keyHash = await digest(`Bearer ${key}`);
  let origin = "";
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const url = new URL(request.url);
    if (
      request.method !== "POST" || url.origin !== origin || url.pathname !== "/admin-control" ||
      request.headers.get("host") !== new URL(origin).host ||
      request.headers.has("origin") ||
      !same(await digest(request.headers.get("authorization") ?? ""), keyHash)
    ) return Response.json({ ok: false }, { status: 403, headers: secureHeaders });
    try {
      const body = await readAdminBody(request);
      let value: Json;
      if (body.method === "status") value = await backend.status();
      else if (body.method === "query" || body.method === "control") {
        if (!body.request || typeof body.request !== "object" || Array.isArray(body.request)) {
          throw new KernelError("rejected", "需要管理操作参数");
        }
        value = body.method === "query"
          ? await backend.query(validateQuery(body.request))
          : await backend.control(validateControl(body.request));
      } else throw new KernelError("rejected", "未知管理操作");
      return Response.json({ ok: true, value }, { headers: secureHeaders });
    } catch (error) {
      return Response.json({ ok: false, error: toWireError(error) }, { headers: secureHeaders });
    }
  });
  origin = `http://127.0.0.1:${server.addr.port}`;
  return {
    url: `${origin}/admin-control`,
    async close() {
      await server.shutdown();
    },
  };
}
