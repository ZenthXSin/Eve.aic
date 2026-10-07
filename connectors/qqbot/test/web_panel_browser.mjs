// Optional browser acceptance. All traffic and data are synthetic and local.
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
const work = fs.mkdtempSync(path.join(os.tmpdir(), "eve-web-browser-"));
const token = "synthetic-browser-only-access-token-1234567890";
const untrusted = '<img src=x onerror="window.eveXss=true">';
let requests = 0;
const sockets = new Set();
const model = http.createServer(async (request, response) => {
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  assert.equal(request.url, "/v1/chat/completions");
  assert.equal(request.headers.authorization, "Bearer test-model-secret");
  requests++;
  const input = body.messages.at(-1).content;
  if (input === "等待面板取消的任务") {
    fs.writeFileSync(path.join(work, "model-wait"), "ready");
    return;
  }
  response.writeHead(200, { "Content-Type": "application/json" });
  response.end(JSON.stringify({ choices: [{ index: 0, finish_reason: "stop", message: { role: "assistant", content: input } }] }));
});
model.on("connection", socket => { sockets.add(socket); socket.on("close", () => sockets.delete(socket)); });
await new Promise(resolve => model.listen(0, "127.0.0.1", resolve));
const message = (id, text, extra = {}) => ({ id, text, scope: "c2c", user_id: "synthetic-user", target_id: "synthetic-user", expected: text, ...extra });
const script = [];
for (let i = 0; i < 26; i++) {
  const text = i === 25 ? `${untrusted}${"汉".repeat(2800)}` : `合成测试记录 ${i + 1}`;
  script.push({ send: message(`history-${i}`, text) }, { wait_receipt: { path: path.join(work, "state/state.json"), id: `history-${i}`, state: "Sent" } });
}
script.push({ send: message("active", "等待面板取消的任务", { expected_type: "finish" }) },
  { wait_file: path.join(work, "model-wait") },
  { send: message("keep", "/continue", { expected: "当前任务保持不变。" }) }, { wait_command: { id: "keep", type: "reply" } },
  { wait_command: { id: "active", type: "finish" } }, { wait_file: path.join(work, "end") });
const scenario = path.join(work, "scenario.json");
const bridgeError = path.join(work, "bridge-error");
fs.writeFileSync(scenario, JSON.stringify({ script, error_file: bridgeError }));
const env = Object.fromEntries(["PATH", "SystemRoot", "TEMP", "TMP"].filter(name => process.env[name]).map(name => [name, process.env[name]]));
Object.assign(env, { QQBOT_APP_SECRET: "test-app-secret", EVE_OPENAI_API_KEY: "test-model-secret",
  EVE_OPENAI_BASE_URL: `http://127.0.0.1:${model.address().port}`, EVE_OPENAI_PROTOCOL: "chat",
  EVE_LLM_RESPONSE_MODE: "complete", EVE_WEB_TOKEN: token });
const child = spawn(binary, ["--state-dir", path.join(work, "state"), "--agent", path.join(root, "AGENT.md"),
  "--bridge-script", path.join(root, "connectors/qqbot/test/web-panel-bridge.mjs"), "--bridge-arg", scenario,
  "--web-listen", "127.0.0.1:0"], { env, stdio: ["ignore", "pipe", "pipe"] });
let stdout = "", stderr = "";
child.stdout.on("data", bytes => { stdout += bytes; });
child.stderr.on("data", bytes => { stderr += bytes; });
const exited = new Promise(resolve => child.on("exit", (code, signal) => resolve({ code, signal })));
let browser;
const wait = async predicate => {
  const deadline = Date.now() + 20000;
  while (!predicate()) {
    assert.equal(child.exitCode, null, "owned Eve exited before browser synchronization");
    assert.ok(Date.now() < deadline, "browser synchronization timed out");
    await delay(20);
  }
};
try {
  await wait(() => stderr.includes("EVE_WEB_READY "));
  const url = stderr.match(/EVE_WEB_READY (http:\/\/[^\s]+)/)[1];
  await wait(() => fs.existsSync(path.join(work, "model-wait")));
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  const errors = [];
  const external = [];
  page.on("pageerror", error => errors.push(error.message));
  page.on("request", request => { if (!request.url().startsWith(url + "/")) external.push(request.url()); });
  await page.goto(url);
  await page.locator("#access-token").fill("short");
  await page.locator("#login-submit").click();
  assert.equal(await page.locator("#access-token").evaluate(input => input.checkValidity()), false);
  await page.locator("#access-token").fill("wrong-browser-token-12345678901234567890");
  await page.locator("#login-submit").click();
  await page.waitForFunction(() => document.querySelector("#login-error").textContent.includes("无效"));
  await page.locator("#access-token").fill(token);
  await page.locator("#login-submit").click();
  await page.locator("#console-view").waitFor({ state: "visible" });
  await page.locator("#tasks-list .record-button").first().click();
  await page.getByRole("button", { name: "请求取消此代任务" }).waitFor();
  assert.equal(await page.locator("#access-token").inputValue(), "");
  assert.equal(await page.evaluate(() => localStorage.length + sessionStorage.length), 0);
  assert.equal(await page.evaluate(() => document.body.textContent.includes("synthetic-browser-only-access-token")), false);
  await page.screenshot({ path: path.join(artifacts, "desktop.png"), fullPage: true });
  await page.getByRole("button", { name: "请求取消此代任务" }).click();
  assert.equal(requests, 27);
  await page.getByRole("button", { name: "确认请求取消" }).click();
  await page.waitForFunction(() => document.querySelector("#task-detail").textContent.includes("已结束"));
  assert.ok(await page.locator("#task-detail").textContent().then(value => value.includes("失败记录已保存")));
  assert.ok(await page.locator("#task-detail").textContent().then(value => value.includes("已准入，本代已结束")));
  assert.equal(await page.locator("#task-detail").textContent().then(value => value.includes("保存失败")), false);
  await page.locator('[data-view="sessions"]').click();
  await page.locator("#sessions-list .record-button").first().click();
  await page.getByRole("button", { name: "更早轮次" }).waitFor();
  await page.waitForFunction(() => document.querySelector("#session-detail").textContent.includes("仅显示前 8192 字节"));
  assert.equal(await page.locator("#session-detail img").count(), 0);
  assert.equal(await page.evaluate(() => window.eveXss), undefined);
  await page.getByRole("button", { name: "更早轮次" }).click();
  await page.getByRole("button", { name: "回到最新" }).waitFor();
  await page.waitForFunction(() => document.querySelector("#session-detail").textContent.includes("合成测试记录 1"));
  await page.getByRole("button", { name: "回到最新" }).click();
  await page.getByRole("button", { name: "更早轮次" }).waitFor();
  await page.locator('[data-view="judgments"]').click();
  await page.locator("#judgments-list .record-button").first().waitFor();
  assert.equal(await page.locator("#judgments-list .record-button").count(), 1);
  assert.ok((await page.locator("#judgments-scope").textContent()).includes("仅明确命令规则"));
  await page.locator("#judgments-list .record-button").first().click();
  await page.waitForFunction(() => document.querySelector("#judgment-detail").textContent.includes("明确命令规则"));
  const judgment = await page.locator("#judgment-detail").textContent();
  for (const expected of ["继续", "规则 1 · 辅助 0 · 主模型 0", "不代表网络请求"]) assert.ok(judgment.includes(expected), expected);
  for (const hidden of ["/continue", "synthetic-user", "keep"]) assert.equal(judgment.includes(hidden), false, hidden);
  await page.screenshot({ path: path.join(artifacts, "judgments.png"), fullPage: true });
  await page.locator('[data-view="goals"]').click();
  await page.waitForFunction(() => document.querySelector("#goals-error").textContent.includes("未开启认知"));
  assert.equal(await page.locator("#goals-list .record-button").count(), 0);
  await page.locator('[data-view="judgments"]').click();
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), "mobile judgments overflow horizontally");
  await page.screenshot({ path: path.join(artifacts, "mobile-judgments.png"), fullPage: true });
  await page.locator('[data-view="tasks"]').click();
  await page.screenshot({ path: path.join(artifacts, "mobile.png"), fullPage: true });
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), "mobile layout overflows horizontally");
  await page.locator("#logout-button").click();
  await page.locator("#login-view").waitFor({ state: "visible" });
  assert.equal(await page.locator("#tasks-list").textContent(), "");
  assert.equal(await page.locator("#judgments-list").textContent(), "");
  await page.reload();
  await page.locator("#login-view").waitFor({ state: "visible" });
  assert.deepEqual(errors, []);
  assert.deepEqual(external, []);
  assert.equal(await page.evaluate(() => localStorage.length + sessionStorage.length), 0);
  fs.writeFileSync(path.join(work, "end"), "done");
  const result = await Promise.race([exited, delay(10000).then(() => { throw new Error("owned Eve did not stop"); })]);
  assert.equal(result.code, 0, stderr);
  assert.equal(fs.existsSync(bridgeError), false);
  assert.equal(requests, 27);
  for (const secret of [token, "test-model-secret", "test-app-secret"]) assert.equal((stdout + stderr).includes(secret), false);
  console.log(JSON.stringify({ passed: true, model_requests: requests, browser_errors: errors.length,
    checks: ["login", "memory-only-token", "task-cancel-confirmation", "history-pagination", "truncation", "xss-text", "judgment-diagnostics", "goals-unavailable", "mobile-layout", "logout", "reload"], artifacts }));
} finally {
  if (browser) await browser.close();
  if (child.exitCode === null) { child.kill("SIGKILL"); await exited; }
  for (const socket of sockets) socket.destroy();
  await new Promise(resolve => model.close(resolve));
  fs.rmSync(work, { recursive: true, force: true });
}
