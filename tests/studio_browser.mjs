import { createRequire } from "node:module";
import { startAdminServer } from "../services/admin/server.ts";
import {
  BUDGET,
  fixture,
  HISTORY_FIRST,
  HISTORY_LAST,
  INSTANCE,
} from "./studio_browser_fixture.mjs";
import { check, meter } from "./studio_browser_meter.mjs";

const requireLocal = createRequire("/app/package.json");
const { chromium } = requireLocal("playwright-core");
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const functionalOnly = Deno.args.length === 1 && Deno.args[0] === "--functional-only";
check(
  Deno.args.length === 0 || functionalOnly,
  "usage: test-studio-browser.sh [--functional-only]",
);
const overviewWindow = 32_000, quietWindow = 17_000;

async function verifyEntry(origin) {
  const html = await Deno.readFile(new URL("../services/admin/studio/index.html", import.meta.url));
  check(
    new TextDecoder().decode(html).includes('src="/studio/app.js"'),
    "not the Studio shell; refusing navigation",
  );
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", html)).toHex();
  const head = await fetch(origin, { method: "HEAD", redirect: "error" });
  check(
    head.status === 200 && head.headers.get("etag") === `"${hash}"`,
    "served / is not this Studio index; refusing navigation",
  );
}

async function withBrowser(width, height, display, run) {
  const dir = await Deno.makeTempDir({ prefix: `studio-${width}-` });
  const child = new Deno.Command("/usr/lib/chromium/chromium", {
    args: [
      `--user-data-dir=${dir}`,
      "--remote-debugging-address=127.0.0.1",
      "--remote-debugging-port=0",
      "--no-sandbox",
      "--disable-background-networking",
      "--disable-sync",
      "--disable-extensions",
      "--no-first-run",
      "--no-default-browser-check",
      "--password-store=basic",
      "about:blank",
    ],
    env: { DISPLAY: display },
    stdin: "null",
    stdout: "null",
    stderr: "null",
  }).spawn();
  let browser;
  try {
    let port;
    for (let attempt = 0; attempt < 120 && !port; attempt++) {
      try {
        port = (await Deno.readTextFile(`${dir}/DevToolsActivePort`)).split("\n")[0];
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      if (!port) await sleep(50);
    }
    check(/^\d+$/.test(port), "Chromium failed to start on the private X display");
    browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`, {
      noDefaults: true,
      timeout: 6000,
    });
    const context = browser.contexts()[0], page = context.pages()[0];
    context.setDefaultTimeout(5000);
    await page.setViewportSize({ width, height });
    if (width === 384) {
      const cdp = await context.newCDPSession(page);
      await cdp.send("Emulation.setDeviceMetricsOverride", {
        width,
        height,
        screenWidth: width,
        screenHeight: height,
        deviceScaleFactor: 1,
        mobile: true,
      });
      await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true });
    }
    await page.addInitScript(() => {
      globalThis.studioAcceptance = { visibility: [], csp: [] };
      document.addEventListener("visibilitychange", (event) => {
        globalThis.studioAcceptance.visibility.push([document.visibilityState, event.isTrusted]);
      });
      document.addEventListener("securitypolicyviolation", (event) => {
        globalThis.studioAcceptance.csp.push(event.effectiveDirective);
      });
    });
    await run(page, browser.version());
  } finally {
    if (browser?.isConnected()) {
      const control = await browser.newBrowserCDPSession().catch(() => null);
      await control?.send("Browser.close").catch(() => {});
    }
    try {
      child.kill("SIGTERM");
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    await child.status;
    await browser?.close().catch(() => {});
    await Deno.remove(dir, { recursive: true });
  }
}

async function tabTo(page, target) {
  await target.waitFor({ state: "visible" });
  for (let count = 0; count < 100; count++) {
    if (await target.evaluate((node) => node === document.activeElement)) return;
    await page.keyboard.press("Tab");
  }
  throw new Error(`unreachable by Tab: ${target}`);
}

async function activate(page, target) {
  await tabTo(page, target);
  check(
    await target.evaluate((node) => node.matches(":focus-visible")),
    `no keyboard focus: ${target}`,
  );
  await page.keyboard.press("Enter");
}

async function layout(page, width) {
  const result = await page.evaluate(() => {
    const root = document.documentElement, outside = [];
    const visible = [
      ...document.querySelectorAll("nav a, main input, main textarea, main button, main select"),
    ];
    for (const node of visible) {
      const rect = node.getBoundingClientRect();
      if (rect.width > 0 && rect.height > 0 && (rect.left < -1 || rect.right > innerWidth + 1)) {
        outside.push(node.id || node.getAttribute("name") || node.textContent.trim().slice(0, 20));
      }
    }
    return {
      width: innerWidth,
      client: root.clientWidth,
      scroll: root.scrollWidth,
      body: document.body.scrollWidth,
      outside,
    };
  });
  check(
    result.width === width && Math.max(result.scroll, result.body) <= result.client + 1 &&
      !result.outside.length,
    `horizontal overflow / off-screen control: ${JSON.stringify(result)}`,
  );
  return result;
}

async function scenarios(width, height, display) {
  const data = fixture(), password = `dev-${crypto.randomUUID()}`;
  const server = await startAdminServer(data.backend, { token: password, port: 0 });
  const report = { width, height, phases: {}, transfers: {}, idle: {}, layout: {} }, errors = [];
  try {
    await verifyEntry(server.url);
    await withBrowser(width, height, display, async (page, version) => {
      report.chromium = version;
      const net = await meter(page, server.url), pageErrors = [];
      page.on("pageerror", (error) => pageErrors.push(error.message));
      const main = page.locator("#main-content");
      const link = (view) => page.locator(`.nav-link[data-view="${view}"]`);
      async function go(view) {
        await activate(page, link(view));
        await page.waitForFunction((route) =>
          location.hash === `#${route}` &&
          document.querySelector(`.nav-link[data-view="${route}"]`)?.getAttribute(
              "aria-current",
            ) ===
            "page", view);
      }
      async function step(name, fn) {
        try {
          await fn();
          report.phases[name] = "ok";
        } catch (error) {
          report.phases[name] = "FAILED";
          errors.push(`${name}: ${error.message}`);
        }
      }
      function status(path, method, code, since = 0) {
        check(
          net.entries.slice(since).some((entry) =>
            entry.path === path && entry.method === method && entry.status === code
          ),
          `missing ${method} ${path} -> ${code}`,
        );
      }
      async function first(name, start, files, limit) {
        await net.settle();
        const batch = net.batch(start);
        report.transfers[name] = batch;
        const staticFiles = batch.requests.filter((entry) =>
          entry.path === "/" || entry.path.startsWith("/studio/")
        );
        const expected = files.map((file) => file === "/" ? file : `/studio/${file}`).sort();
        check(
          JSON.stringify(staticFiles.map((entry) => entry.path).sort()) ===
            JSON.stringify(expected),
          `${name} unexpected/missing asset: ${staticFiles.map((entry) => entry.path).join(", ")}`,
        );
        check(
          staticFiles.every((entry) => entry.status === 200 && entry.bytes > 0) &&
            batch.requests.every((entry) => Number.isFinite(entry.bytes)),
          `${name} incomplete cold transfer`,
        );
        check(batch.bytes <= limit, `${name} transfer ${batch.bytes} > ${limit} B`);
      }
      async function idle(name, duration, allowed, min, max) {
        const start = net.entries.length, wsStart = net.frames.length, clock = performance.now();
        await sleep(duration);
        const requests = net.entries.slice(start).map(({ method, path }) => `${method} ${path}`);
        const sockets = net.frames.slice(wsStart);
        report.idle[name] = {
          ms: Math.round(performance.now() - clock),
          requests,
          socketEvents: sockets.length,
        };
        check(
          requests.length >= min && requests.length <= max && requests.every((path) =>
            path === allowed
          ) &&
            !sockets.length && !net.sockets.size,
          `${name}: ${requests.length} requests (${requests}), ${sockets.length} socket events`,
        );
      }
      async function hideAndReturn(name, duration) {
        const count = await page.evaluate(() => globalThis.studioAcceptance.visibility.length);
        const other = await page.context().newPage();
        try {
          await other.bringToFront();
          await page.waitForFunction(() => document.hidden, null, { polling: 75 });
          await idle(`${name}-hidden`, duration, null, 0, 0);
          await page.bringToFront();
          await page.waitForFunction(() => document.visibilityState === "visible");
          const transitions = await page.evaluate(
            (from) => globalThis.studioAcceptance.visibility.slice(from),
            count,
          );
          check(
            JSON.stringify(transitions) === '[["hidden",true],["visible",true]]',
            `not trusted native tab transitions: ${JSON.stringify(transitions)}`,
          );
          report.idle[`${name}-hidden`].visibility = transitions;
        } finally {
          await other.close();
        }
      }

      await page.goto(server.url, { waitUntil: "load" });
      const token = page.locator("#login-token");
      await token.waitFor({ state: "visible" });
      await step("anonymous cold entry", async () => {
        await first("login", 0, ["/", "base.css", "app.js"], 64 * 1024);
        status("/api/session", "GET", 401);
        check(
          !net.entries.some(({ path }) => path.startsWith("/api/") && path !== "/api/session"),
          "anonymous page read feature API",
        );
        report.layout.login = await layout(page, width);
      });
      await tabTo(page, token);
      await page.keyboard.type("wrong-dev-password");
      await page.keyboard.press("Enter");
      await page.locator('#login-token[aria-invalid="true"]').waitFor();
      await net.settle();
      status("/api/login", "POST", 401);
      await token.fill(password);
      const toggle = page.locator("#token-toggle");
      await tabTo(page, toggle);
      await page.keyboard.press("Space");
      check(await token.getAttribute("type") === "text", "password toggle did not work with Space");
      await page.keyboard.press("Space");
      const overviewStart = net.entries.length;
      await activate(page, page.locator("#login-submit"));
      await link("overview").waitFor();
      check(
        (await page.context().cookies(server.url)).some((cookie) =>
          cookie.name === "eve_admin_session" && cookie.httpOnly && cookie.sameSite === "Strict"
        ),
        "missing HttpOnly SameSite=Strict session",
      );
      report.phases["keyboard login / rejected password / cookie"] = "ok";

      await step("overview and first authenticated transfer", async () => {
        await main.getByText(INSTANCE, { exact: false }).waitFor();
        await first("overview", overviewStart, ["overview.js", "overview.css"], 96 * 1024);
        status("/api/overview", "GET", 200, overviewStart);
        report.layout.overview = await layout(page, width);
      });
      if (!functionalOnly) {
        await step("overview idle and real background tab", async () => {
          await idle("overview-visible", overviewWindow, "GET /api/overview", 1, 3);
          const before = net.entries.length;
          await hideAndReturn("overview", quietWindow);
          await net.settle();
          status("/api/overview", "GET", 200, before);
        });
      }
      await step("CSRF rejected before any write", async () => {
        const before = data.writes.length;
        const statuses = await page.evaluate(async ({ key, revision }) => {
          const result = [];
          for (const token of [null, "not-a-valid-dev-token"]) {
            const headers = { "content-type": "application/json" };
            if (token) headers["x-eve-csrf"] = token;
            const response = await fetch("/api/settings", {
              method: "POST",
              headers,
              body: JSON.stringify({ revision, changes: { [key]: 1 } }),
            });
            await response.arrayBuffer();
            result.push(response.status);
          }
          return result;
        }, { key: BUDGET, revision: data.revision() });
        check(
          statuses.join(",") === "403,403" && data.writes.length === before &&
            data.budget() === 42000,
          `invalid CSRF accepted or mutated fixture: ${statuses}`,
        );
        report.csrf = statuses;
      });
      await net.settle();
      await featureScenarios({
        page,
        data,
        width,
        report,
        net,
        main,
        go,
        step,
        first,
        idle,
        hideAndReturn,
        status,
      });
      await step("logout / session revoked", async () => {
        await activate(page, page.locator("#logout"));
        await token.waitFor({ state: "visible" });
        check(
          await page.evaluate(async () => {
            const response = await fetch("/api/session");
            await response.arrayBuffer();
            return response.status;
          }) === 401,
          "session survived logout",
        );
        check(
          !(await page.context().cookies(server.url)).some((cookie) =>
            cookie.name === "eve_admin_session"
          ),
          "logout retained cookie",
        );
      });
      await net.settle();
      await step("isolation / no unexpected operations", async () => {
        check(!pageErrors.length, `uncaught page exceptions: ${pageErrors}`);
        const csp = await page.evaluate(() => globalThis.studioAcceptance.csp);
        check(!csp.length, `CSP violations: ${csp}`);
        check(
          !net.entries.some(({ path }) => path.startsWith("external:")) && !net.frames.length,
          "external traffic or WebSocket",
        );
        check(!data.unexpected.length, `unexpected backend calls: ${data.unexpected}`);
        check(
          data.writes.join(",") === "chatSend,settingsUpdate",
          `unexpected writes: ${data.writes}`,
        );
      });
    });
  } catch (error) {
    errors.push(`scenario: ${error.message}`);
  } finally {
    await server.close();
  }
  report.errors = errors;
  return report;
}

async function featureScenarios(
  { page, data, width, report, net, main, go, step, first, idle, hideAndReturn, status },
) {
  const chatStart = net.entries.length;
  await go("chat");
  await step("chat loaded on demand / cursor history", async () => {
    await main.getByText(HISTORY_LAST, { exact: true }).waitFor();
    await first("chat", chatStart, ["chat.js", "chat.css"], 96 * 1024);
    check(!await main.getByText(HISTORY_FIRST, { exact: true }).count(), "history already fetched");
    report.layout.chat = await layout(page, width);
    await activate(page, main.getByRole("button", { name: "加载更早记录" }));
    await main.getByText(HISTORY_FIRST, { exact: true }).waitFor();
    await net.settle();
    status("/api/chat?before=5", "GET", 200, chatStart);
    check(
      await main.getByText(HISTORY_LAST, { exact: true }).count() === 1,
      "cursor overlapped or lost latest chat",
    );
    check(
      !await main.getByRole("button", { name: "加载更早记录" }).isVisible(),
      "terminal cursor still actionable",
    );
  });
  await step("chat keyboard / one confirmed local send", async () => {
    const input = main.getByRole("textbox", { name: "消息内容" });
    await tabTo(page, input);
    await page.keyboard.insertText(`私聊夹具 ${width}`);
    await page.keyboard.press("Enter");
    await page.keyboard.insertText("第二行");
    check(
      (await input.inputValue()).includes("\n") && !data.writes.includes("chatSend"),
      "plain Enter sent a draft",
    );
    const before = net.entries.length;
    await page.keyboard.press("Control+Enter");
    await main.getByText(`私聊夹具 ${width} 第二行`, { exact: true }).waitFor();
    await net.settle();
    check(
      data.writes.filter((action) => action === "chatSend").length === 1 &&
        await input.inputValue() === "",
      "local chat was not confirmed exactly once",
    );
    check(
      net.entries.slice(before).some((entry) =>
        entry.path === "/api/chat/send" && entry.method === "POST" &&
        [200, 202].includes(entry.status)
      ),
      "no successful protected chat send",
    );
  });
  if (!functionalOnly) {
    await step("chat idle and real background tab", async () => {
      await idle("chat-visible", quietWindow, "GET /api/chat", 1, 2);
      const before = net.entries.length;
      await hideAndReturn("chat", quietWindow);
      await net.settle();
      status("/api/chat", "GET", 200, before);
    });
  }

  const settingsStart = net.entries.length;
  await go("settings");
  await step("settings on demand / validation / reset / save / read back", async () => {
    const field = main.getByLabel("每日共享 token 预算", { exact: true });
    const save = main.getByRole("button", { name: "保存更改" });
    await field.waitFor();
    check(
      await field.inputValue() === "42000" && await save.isDisabled(),
      "wrong initial value or spurious dirty state",
    );
    await first("settings", settingsStart, ["settings.js", "settings.css"], 96 * 1024);
    report.layout.settings = await layout(page, width);
    await field.fill("-1");
    check(
      await field.getAttribute("aria-invalid") === "true" && await save.isDisabled(),
      "invalid budget can be saved",
    );
    await activate(page, main.getByRole("button", { name: "撤销更改" }));
    check(
      await field.inputValue() === "42000" && !data.writes.includes("settingsUpdate"),
      "reset wrote to backend",
    );
    await field.fill("43001");
    await activate(page, save);
    await main.getByText(/已保存，配置版本已更新/).waitFor();
    await activate(page, main.getByRole("button", { name: "重新读取" }));
    await net.settle();
    check(
      await field.inputValue() === "43001" && data.budget() === 43001 && await save.isDisabled(),
      "settings readback did not match commit",
    );
    status("/api/settings", "POST", 200, settingsStart);
  });
  if (!functionalOnly) {
    await step(
      "settings idle / no stale overview or chat polling",
      () => idle("settings-visible", quietWindow, null, 0, 0),
    );
  }

  const exploreStart = net.entries.length;
  await go("explore");
  await step("explore on demand / original records pagination", async () => {
    await first("explore", exploreStart, ["explore.js", "explore.css"], 96 * 1024);
    report.layout.explore = await layout(page, width);
    await explorePagination(page, main, data);
  });
}

async function explorePagination(page, main, data) {
  const first = main.locator(".archive-card").first();
  await first.waitFor();
  const before = await first.innerText();
  await activate(page, main.getByRole("button", { name: "下一页", exact: true }));
  await page.waitForFunction(
    (text) => document.querySelector(".archive-card")?.innerText !== text,
    before,
  );
  await activate(page, main.getByRole("button", { name: "上一页", exact: true }));
  await page.waitForFunction(
    (text) => document.querySelector(".archive-card")?.innerText === text,
    before,
  );
  check(
    data.queries.some((entry) => entry.resource === "timeline" && entry.args.afterSeq > 0),
    "no cursor read",
  );
}

const results = await Promise.all([scenarios(384, 832, ":91"), scenarios(1440, 900, ":92")]);
console.log(
  JSON.stringify({
    mode: functionalOnly ? "functional-only" : "acceptance",
    playwright: requireLocal("playwright-core/package.json").version,
    deno: Deno.version.deno,
    results,
  }),
);
if (results.some((result) => result.errors.length)) Deno.exitCode = 1;
