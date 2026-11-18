const byId = (id) => document.getElementById(id);
const gate = byId("gate");
const workspace = byId("workspace");
const form = byId("login-form");
const token = byId("login-token");
const toggle = byId("token-toggle");
const submit = byId("login-submit");
const gateMessage = byId("gate-message");
const retrySession = byId("session-retry");
const logoutButton = byId("logout");
const main = byId("main-content");
const host = byId("view-host");
const notice = byId("notice");
const links = [...document.querySelectorAll(".nav-link[data-view]")];

const views = new Map([
  ["overview", { title: "总览", load: () => import("./overview.js") }],
  ["chat", { title: "对话", load: () => import("./chat.js") }],
  ["settings", { title: "设置", load: () => import("./settings.js") }],
  ["explore", { title: "探索", load: () => import("./explore.js") }],
]);

let session = null;
let authTask = null;
let page = null;
let expiryTimer;
let noticeTimer;

class ApiError extends Error {
  constructor(message, status) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

async function request(path, body, options = {}) {
  const url = new URL(path, location.origin);
  if (url.origin !== location.origin || !url.pathname.startsWith("/api/")) {
    throw new TypeError("只能请求本站的 /api/ 接口。");
  }
  const method = (options.method ?? (body === undefined ? "GET" : "POST")).toUpperCase();
  const reading = method === "GET" || url.pathname === "/api/query";
  const deadline = AbortSignal.timeout(reading ? 30000 : 90000);
  const signal = options.signal ? AbortSignal.any([options.signal, deadline]) : deadline;
  const timedOut = () =>
    new ApiError(
      reading ? "读取超时，请稍后刷新。" : "未收到确认，请先重新读取状态，不要重复提交。",
      0,
    );
  const headers = new Headers(options.headers);
  headers.set("Accept", "application/json");
  if (body !== undefined) headers.set("Content-Type", "application/json");
  if (session && method !== "GET" && method !== "HEAD" && url.pathname !== "/api/login") {
    headers.set("x-eve-csrf", session.csrf);
  }
  const payload = body === undefined ? undefined : JSON.stringify(body);
  let response;
  try {
    response = await fetch(url, {
      method,
      headers,
      body: payload,
      signal,
      mode: "same-origin",
      credentials: "same-origin",
      redirect: "error",
      cache: "no-store",
    });
  } catch (error) {
    if (deadline.aborted && !options.signal?.aborted) throw timedOut();
    if (signal?.aborted) throw error;
    throw new ApiError("无法连接工作台。请检查网络后重试。", 0);
  }
  let envelope;
  try {
    envelope = await response.json();
  } catch (error) {
    if (deadline.aborted && !options.signal?.aborted) throw timedOut();
    if (signal?.aborted) throw error;
    throw new ApiError(`服务响应不是有效的 JSON（HTTP ${response.status}）。`, response.status);
  }
  if (!response.ok || envelope?.ok !== true || !Object.hasOwn(envelope, "value")) {
    const detail = envelope?.error?.message;
    throw new ApiError(
      typeof detail === "string" && detail ? detail : `服务响应异常（HTTP ${response.status}）。`,
      response.status,
    );
  }
  return envelope.value;
}

function normalizeRoute(route) {
  return String(route ?? "").replace(/^#?\/*/u, "") || "overview";
}

function navigate(route) {
  const next = normalizeRoute(route);
  if (normalizeRoute(location.hash) !== next) location.hash = next;
}

function notify(text) {
  clearTimeout(noticeTimer);
  notice.textContent = String(text ?? "");
  if (notice.textContent) {
    noticeTimer = setTimeout(() => {
      notice.textContent = "";
    }, 6500);
  }
}

async function dispose(cleanup) {
  if (typeof cleanup !== "function") return;
  try {
    await cleanup();
  } catch {
    console.warn("页面未能完成清理。");
  }
}

function leavePage() {
  const previous = page;
  page = null;
  if (previous) {
    previous.controller.abort();
    void dispose(previous.cleanup);
  }
  host.replaceChildren();
  host.removeAttribute("aria-busy");
}

function keepSession(value) {
  if (typeof value?.csrf !== "string" || !value.csrf) {
    throw new Error("服务未返回有效的会话校验值。");
  }
  clearTimeout(expiryTimer);
  session = value;
  if (Number.isFinite(value.expires)) {
    expiryTimer = setTimeout(
      () => showGate("login", "会话已到期，请重新登录。"),
      Math.max(0, value.expires - Date.now()),
    );
  }
}

function setTokenVisible(visible) {
  token.type = visible ? "text" : "password";
  toggle.textContent = visible ? "隐藏" : "显示";
  toggle.setAttribute("aria-label", visible ? "隐藏口令" : "显示口令");
}

function loginBusy(busy) {
  token.disabled = toggle.disabled = submit.disabled = busy;
  form.setAttribute("aria-busy", String(busy));
  submit.textContent = busy ? "正在登录…" : "登录工作台";
}

function showGate(mode, message = "") {
  authTask?.abort();
  authTask = null;
  clearTimeout(expiryTimer);
  session = null;
  leavePage();
  workspace.hidden = true;
  gate.hidden = false;
  notify("");
  form.reset();
  token.removeAttribute("aria-invalid");
  setTokenVisible(false);
  loginBusy(false);
  const login = mode === "login";
  const checking = mode === "checking";
  const title = login ? "登录工作台" : checking ? "打开工作台" : "暂时无法确认会话";
  byId("gate-title").textContent = title;
  byId("gate-copy").textContent = login
    ? "使用管理员口令，继续访问当前页面。"
    : checking
    ? "正在确认此浏览器的登录状态。"
    : "请检查连接，然后重新检查登录状态。";
  form.hidden = !login;
  byId("gate-wait").hidden = !checking;
  retrySession.hidden = login || checking;
  gateMessage.textContent = message;
  document.title = `${title} · EVE Studio`;
  if (login) token.focus();
  else if (!checking) retrySession.focus();
}

function enter(value) {
  keepSession(value);
  authTask = null;
  form.reset();
  setTokenVisible(false);
  gateMessage.textContent = "";
  gate.hidden = true;
  workspace.hidden = false;
  logoutButton.disabled = false;
  byId("logout-label").textContent = "退出登录";
  if (!location.hash) history.replaceState(history.state, "", "#overview");
  void renderRoute();
}

async function checkSession() {
  showGate("checking");
  const task = authTask = new AbortController();
  try {
    const value = await request("/api/session", undefined, { signal: task.signal });
    if (!task.signal.aborted) enter(value);
  } catch (error) {
    if (task.signal.aborted) return;
    if (error.status === 401) showGate("login");
    else showGate("error", error.message);
  }
}

async function login(event) {
  event.preventDefault();
  if (authTask || form.hidden || !token.value) return;
  const task = authTask = new AbortController();
  loginBusy(true);
  gateMessage.textContent = "";
  token.removeAttribute("aria-invalid");
  try {
    const value = await request("/api/login", { token: token.value }, { signal: task.signal });
    if (!task.signal.aborted) enter(value);
  } catch (error) {
    if (task.signal.aborted) return;
    authTask = null;
    loginBusy(false);
    gateMessage.textContent = error.message;
    if (error.status === 401) token.setAttribute("aria-invalid", "true");
    token.focus();
    token.select();
  }
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function panel(kind, title, detail, action, onAction) {
  const box = element("section", `shell-state shell-state-${kind}`);
  box.setAttribute("role", kind === "error" ? "alert" : "status");
  const mark = element(
    "span",
    kind === "wait" ? "shell-spinner" : "state-mark",
    kind === "error" ? "!" : kind === "missing" ? "?" : "",
  );
  mark.setAttribute("aria-hidden", "true");
  box.append(mark, element("h2", "state-title", title), element("p", "muted", detail));
  if (action) {
    const button = element("button", "btn", action);
    button.type = "button";
    button.addEventListener("click", onAction);
    box.append(button);
  }
  return box;
}

async function logout() {
  if (!session || authTask) return;
  const task = authTask = new AbortController();
  leavePage();
  logoutButton.disabled = true;
  byId("logout-label").textContent = "正在退出…";
  host.replaceChildren(panel("wait", "正在退出登录", "正在结束此浏览器的管理员会话。"));
  try {
    await request("/api/logout", {}, { signal: task.signal });
    if (!task.signal.aborted) showGate("login", "已退出工作台。");
  } catch (error) {
    if (task.signal.aborted) return;
    if (error.status === 401) return showGate("login", "会话已结束，请重新登录。");
    authTask = null;
    logoutButton.disabled = false;
    byId("logout-label").textContent = "退出登录";
    const recheck = error.status === 403;
    host.replaceChildren(
      panel(
        "error",
        "退出未获确认",
        error.message,
        recheck ? "重新检查会话" : "重试退出",
        recheck ? checkSession : logout,
      ),
    );
    main.focus();
  }
}

function contextFor(owner) {
  const signal = owner.controller.signal;
  const current = () => page === owner && !signal.aborted;
  async function api(path, body, options = {}) {
    const caller = options.signal;
    const joint = caller && caller !== signal ? new AbortController() : null;
    const effective = joint?.signal ?? signal;
    const abort = (event) => joint.abort(event.target.reason);
    if (joint) {
      if (signal.aborted) joint.abort(signal.reason);
      else if (caller.aborted) joint.abort(caller.reason);
      else {
        caller.addEventListener("abort", abort, { once: true });
        signal.addEventListener("abort", abort, { once: true });
      }
    }
    try {
      effective.throwIfAborted();
      const value = await request(path, body, { ...options, signal: effective });
      effective.throwIfAborted();
      return value;
    } catch (error) {
      if (error?.status === 401 && current() && !effective.aborted) {
        showGate("login", "会话已结束，请重新登录。");
      }
      throw error;
    } finally {
      if (joint) {
        caller.removeEventListener("abort", abort);
        signal.removeEventListener("abort", abort);
      }
    }
  }
  return {
    api,
    signal,
    route: owner.route,
    navigate(route) {
      if (current()) navigate(route);
    },
    notify(text) {
      if (current()) notify(text);
    },
    async refreshSession() {
      const value = await api("/api/session");
      if (current()) keepSession(value);
      return value;
    },
  };
}

async function renderRoute(force = false) {
  if (!session || authTask) return;
  const route = normalizeRoute(location.hash);
  if (!force && page?.route === route) return;
  leavePage();
  const key = route.startsWith("explore/") ? "explore" : route;
  const spec = views.get(key);
  const owner = page = { route, controller: new AbortController() };
  const title = spec?.title ?? "未找到页面";
  document.title = `${title} · EVE Studio`;
  byId("view-label").textContent = title;
  for (const link of links) {
    if (link.dataset.view === key) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  main.focus({ preventScroll: true });
  window.scrollTo(0, 0);
  if (!spec) {
    host.append(
      panel(
        "missing",
        "没有找到这个页面",
        "地址可能有误，你可以回到总览或选择其他栏目。",
        "返回总览",
        () => navigate("overview"),
      ),
    );
    return;
  }
  const root = element("div", "view-root");
  const pending = panel("wait", `正在打开${title}`, "页面就绪后将在这里显示。");
  host.setAttribute("aria-busy", "true");
  host.append(root, pending);
  let loaded = false;
  try {
    const module = await spec.load();
    if (page !== owner) return;
    if (typeof module.mount !== "function") throw new Error("页面模块未提供 mount 入口。");
    loaded = true;
    const cleanup = await module.mount(root, contextFor(owner));
    if (page !== owner) return void dispose(cleanup);
    owner.cleanup = cleanup;
    pending.remove();
    host.removeAttribute("aria-busy");
  } catch (error) {
    if (page !== owner) return;
    leavePage();
    const detail = loaded
      ? (error?.message || "页面初始化失败。")
      : "页面文件暂不可用。请检查连接后重新加载。";
    host.append(
      panel(
        "error",
        `${title}未能打开`,
        detail,
        loaded ? "重试此页" : "重新加载页面",
        loaded ? () => void renderRoute(true) : () => location.reload(),
      ),
    );
  }
}

form.addEventListener("submit", login);
toggle.addEventListener("click", () => setTokenVisible(token.type === "password"));
retrySession.addEventListener("click", checkSession);
logoutButton.addEventListener("click", logout);
byId("skip-content").addEventListener("click", (event) => {
  event.preventDefault();
  main.focus();
});
window.addEventListener("hashchange", () => void renderRoute());
window.addEventListener("pageshow", (event) => {
  if (event.persisted) void checkSession();
});
void checkSession();
