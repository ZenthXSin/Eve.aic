// Optional browser acceptance for the read-only cognition view. Synthetic local data only.
// EVE_PLAYWRIGHT_MODULE may point to an isolated installation's index.mjs.
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import http from "node:http";
import { spawn } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const { chromium } = await import(process.env.EVE_PLAYWRIGHT_MODULE ? pathToFileURL(process.env.EVE_PLAYWRIGHT_MODULE).href : "playwright");
const binary = process.env.EVE_QQBOT_BINARY || path.join(root, "target/debug/eve-qqbot");
const artifacts = process.env.EVE_WEB_ARTIFACTS || path.join(os.tmpdir(), "eve-web-browser-artifacts");
fs.mkdirSync(artifacts, { recursive: true });
const work = fs.mkdtempSync(path.join(os.tmpdir(), "eve-web-cognition-"));
const token = "synthetic-cognition-browser-token-1234567890";
const untrusted = '<img src=x onerror="window.eveXss=true">';
const drafts = [`${untrusted}旧版本草稿`, "新版本草稿：只整理书桌。"];
let requests = 0;
const sockets = new Set();
const model = http.createServer(async (request, response) => {
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  assert.equal(request.url, "/v1/chat/completions");
  assert.ok(body.messages.at(-1).content.includes("unverified_waiting_input"));
  const artifact = { summary: drafts[Math.min(requests, 1)], next_step: "请用户确认后再行动。", needs_user_input: true };
  requests++;
  response.writeHead(200, { "Content-Type": "application/json" });
  response.end(JSON.stringify({ choices: [{ index: 0, finish_reason: "stop",
    message: { role: "assistant", content: JSON.stringify(artifact) } }] }));
});
model.on("connection", socket => { sockets.add(socket); socket.on("close", () => sockets.delete(socket)); });
await new Promise(resolve => model.listen(0, "127.0.0.1", resolve));
const message = (id, text, contains) => ({ id, text, scope: "c2c", user_id: "synthetic-user", target_id: "synthetic-user",
  expected_type: "reply", contains, excludes: [], delivery_ok: true });
const send = (item) => [{ send: item }, { wait_command: { id: item.id, type: "reply", count: 1 } },
  { wait_receipt: { id: item.id, state: "Sent" } }];
const paused = path.join(work, "paused");
const end = path.join(work, "end");
const script = [
  ...send(message("goal", `/goal 整理房间 ${untrusted}`, ["待办已保存"])),
  { capture_goal: { name: "before", source: "goal" } }, { wait_child: { parent: "before", status: "Completed" } },
  ...send(message("feedback", "/goal-feedback {{before.id}} {{before.revision}} 只整理书桌", ["反馈已保存"])),
  { capture_goal: { name: "after", source: "goal" } }, { wait_child: { parent: "after", status: "Completed" } },
  { touch: paused }, { wait_file: end },
];
const scenario = path.join(work, "scenario.json");
const bridgeError = path.join(work, "bridge-error");
fs.writeFileSync(scenario, JSON.stringify({ script, state_file: path.join(work, "state/state.json"),
  events_file: path.join(work, "events.jsonl"), error_file: bridgeError, bindings_file: path.join(work, "bindings.json") }));
const env = Object.fromEntries(["PATH", "SystemRoot", "TEMP", "TMP"].filter(name => process.env[name]).map(name => [name, process.env[name]]));
Object.assign(env, { QQBOT_APP_SECRET: "test-app-secret", EVE_OPENAI_API_KEY: "test-model-secret",
  EVE_OPENAI_BASE_URL: `http://127.0.0.1:${model.address().port}`, EVE_OPENAI_PROTOCOL: "chat",
  EVE_LLM_RESPONSE_MODE: "complete", EVE_WEB_TOKEN: token });
const child = spawn(binary, ["--state-dir", path.join(work, "state"), "--agent", path.join(root, "AGENT.md"),
  "--bridge-script", path.join(root, "connectors/qqbot/test/goal-feedback-bridge.mjs"), "--bridge-arg", scenario,
  "--cognition", "--cognition-max-executions", "2", "--web-listen", "127.0.0.1:0"], { env, stdio: ["ignore", "pipe", "pipe"] });
let stdout = "", stderr = "";
child.stdout.on("data", bytes => { stdout += bytes; });
child.stderr.on("data", bytes => { stderr += bytes; });
const exited = new Promise(resolve => child.on("exit", (code, signal) => resolve({ code, signal })));
let browser;
const wait = async predicate => {
  const deadline = Date.now() + 20000;
  while (!predicate()) {
    assert.equal(child.exitCode, null, `owned Eve exited before browser synchronization: ${fs.existsSync(bridgeError) ? fs.readFileSync(bridgeError, "utf8") : ""}`);
    assert.ok(Date.now() < deadline, "browser synchronization timed out");
    await delay(20);
  }
};
try {
  await wait(() => stderr.includes("EVE_WEB_READY "));
  const url = stderr.match(/EVE_WEB_READY (http:\/\/[^\s]+)/)[1];
  await wait(() => fs.existsSync(paused));
  const stateBefore = fs.readFileSync(path.join(work, "state/state.json"), "utf8");
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  const errors = [];
  const external = [];
  page.on("pageerror", error => errors.push(error.message));
  page.on("request", request => { if (!request.url().startsWith(url + "/")) external.push(request.url()); });
  await page.goto(url);
  await page.locator("#access-token").fill(token);
  await page.locator("#login-submit").click();
  await page.locator("#console-view").waitFor({ state: "visible" });
  await page.locator('[data-view="goals"]').click();
  await page.locator("#goals-list .record-button").first().waitFor();
  assert.equal(await page.locator("#goals-list .record-button").count(), 1, "reflection children stay nested under the parent");
  const listed = await page.locator("#goals-list").textContent();
  for (const expected of ["等待中", "反思草稿 2 份", "版本 2"]) assert.ok(listed.includes(expected), expected);
  await page.locator("#goals-list .record-button").first().click();
  await page.waitForFunction(() => document.querySelector("#goal-detail").textContent.includes("历史草稿"));
  const detail = await page.locator("#goal-detail").textContent();
  for (const expected of ["当前草稿", "对应目标版本 2", "对应目标版本 1", drafts[1], drafts[0], "只整理书桌", "草稿是模型建议，尚未验证", "不代表现实目标完成"]) {
    assert.ok(detail.includes(expected), expected);
  }
  assert.ok(detail.indexOf(drafts[1]) < detail.indexOf(drafts[0]), "current draft is listed before history");
  for (const hidden of ["synthetic-user", "test-model-secret"]) assert.equal(detail.includes(hidden), false, hidden);
  assert.equal(await page.locator("#goal-detail img, #goals-list img").count(), 0);
  assert.equal(await page.evaluate(() => window.eveXss), undefined);
  await page.screenshot({ path: path.join(artifacts, "goals.png"), fullPage: true });
  await page.getByRole("button", { name: "刷新此目标" }).click();
  await page.waitForFunction(() => document.querySelector("#goal-detail").textContent.includes("当前草稿"));
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), "mobile goals overflow horizontally");
  await page.screenshot({ path: path.join(artifacts, "mobile-goals.png"), fullPage: true });
  await page.locator("#logout-button").click();
  await page.locator("#login-view").waitFor({ state: "visible" });
  assert.equal(await page.locator("#goals-list").textContent(), "");
  assert.equal(await page.locator("#goal-detail").textContent(), "");
  assert.deepEqual(errors, []);
  assert.deepEqual(external, []);
  assert.equal(await page.evaluate(() => localStorage.length + sessionStorage.length), 0);
  assert.equal(fs.readFileSync(path.join(work, "state/state.json"), "utf8"), stateBefore, "browser reads must not write state");
  fs.writeFileSync(end, "done");
  const result = await Promise.race([exited, delay(10000).then(() => { throw new Error("owned Eve did not stop"); })]);
  assert.equal(result.code, 0, stderr);
  assert.equal(fs.existsSync(bridgeError), false);
  assert.equal(requests, 2);
  for (const secret of [token, "test-model-secret", "test-app-secret"]) assert.equal((stdout + stderr).includes(secret), false);
  console.log(JSON.stringify({ passed: true, model_requests: requests, browser_errors: errors.length,
    checks: ["goal-list-nesting", "current-and-history-drafts", "unverified-labels", "xss-text", "read-only-state", "mobile-layout", "logout"], artifacts }));
} finally {
  if (browser) await browser.close();
  if (child.exitCode === null) { child.kill("SIGKILL"); await exited; }
  for (const socket of sockets) socket.destroy();
  await new Promise(resolve => model.close(resolve));
  fs.rmSync(work, { recursive: true, force: true });
}
