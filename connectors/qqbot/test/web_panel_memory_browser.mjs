// Optional browser acceptance for the read-only memory view. Synthetic local data only.
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
const work = fs.mkdtempSync(path.join(os.tmpdir(), "eve-web-memory-"));
const token = "synthetic-memory-browser-token-12345678901234";
const untrusted = '<img src=x onerror="window.eveXss=true">';
const first = `${untrusted}回答先给结论`;
const corrected = "更正：先给证据再给结论";
const learned = `${untrusted}学到的偏好：先列要点`;
const statePath = path.join(work, "state/state.json");
let requests = 0;
const sockets = new Set();
const model = http.createServer(async (request, response) => {
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  assert.equal(request.url, "/v1/chat/completions");
  requests++;
  const latest = body.messages.at(-1).content;
  let content = latest;
  try {
    const decoded = JSON.parse(latest);
    // 偏好提炼请求：返回一条只引用真实批次证据的候选。
    if (decoded && decoded.extractor_version && Array.isArray(decoded.evidence)) {
      content = JSON.stringify({ candidates: [{ text: learned, confidence: 83,
        evidence_ids: decoded.evidence.map(item => item.id).slice(0, 2) }] });
    }
  } catch {}
  response.writeHead(200, { "Content-Type": "application/json" });
  response.end(JSON.stringify({ choices: [{ index: 0, finish_reason: "stop",
    message: { role: "assistant", content } }] }));
});
model.on("connection", socket => { sockets.add(socket); socket.on("close", () => sockets.delete(socket)); });
await new Promise(resolve => model.listen(0, "127.0.0.1", resolve));
const message = (id, text, contains) => ({ id, text, scope: "c2c", user_id: "synthetic-user", target_id: "synthetic-user",
  expected_type: "reply", ...(contains === undefined ? { expected: text } : { expected_contains: contains }) });
const send = (item) => [{ send: item }, { wait_command: { id: item.id, type: "reply" } },
  { wait_receipt: { path: statePath, id: item.id, state: "Sent" } }];
const env = Object.fromEntries(["PATH", "SystemRoot", "TEMP", "TMP"].filter(name => process.env[name]).map(name => [name, process.env[name]]));
Object.assign(env, { QQBOT_APP_SECRET: "test-app-secret", EVE_OPENAI_API_KEY: "test-model-secret",
  EVE_OPENAI_BASE_URL: `http://127.0.0.1:${model.address().port}`, EVE_OPENAI_PROTOCOL: "chat",
  EVE_LLM_RESPONSE_MODE: "complete", EVE_WEB_TOKEN: token });
let runs = 0;
const launch = (script, panel, extra = []) => {
  runs++;
  const scenario = path.join(work, `scenario-${runs}.json`);
  fs.writeFileSync(scenario, JSON.stringify({ script, events_file: path.join(work, `events-${runs}.jsonl`),
    error_file: path.join(work, `bridge-error-${runs}`) }));
  const args = ["--state-dir", path.join(work, "state"), "--agent", path.join(root, "AGENT.md"), "--memory",
    "--bridge-script", path.join(root, "connectors/qqbot/test/fake-bridge.mjs"), "--bridge-arg", scenario];
  if (panel) args.push("--web-listen", "127.0.0.1:0");
  args.push(...extra);
  const child = spawn(binary, args, { env, stdio: ["ignore", "pipe", "pipe"] });
  const run = { child, stdout: "", stderr: "", error: path.join(work, `bridge-error-${runs}`) };
  child.stdout.on("data", bytes => { run.stdout += bytes; });
  child.stderr.on("data", bytes => { run.stderr += bytes; });
  run.exited = new Promise(resolve => child.on("exit", (code, signal) => resolve({ code, signal })));
  return run;
};
const finish = async (run) => {
  const result = await Promise.race([run.exited, delay(15000).then(() => { throw new Error("owned Eve did not stop"); })]);
  assert.equal(result.code, 0, run.stderr);
  assert.equal(fs.existsSync(run.error), false, fs.existsSync(run.error) ? fs.readFileSync(run.error, "utf8") : "");
  for (const secret of [token, "test-model-secret", "test-app-secret"]) assert.equal((run.stdout + run.stderr).includes(secret), false);
};
const memory = () => {
  const state = JSON.parse(fs.readFileSync(statePath, "utf8"));
  return JSON.parse(Buffer.from(state.entries["eve.memory"]["memory.v1"]).toString("utf8"));
};
let browser;
let active;
try {
  active = launch([...send(message("remember-a", `/remember ${first}`, "偏好已保存：")),
    ...send(message("remember-b", "/remember 用中文回复", "偏好已保存：")),
    ...send(message("chat-1", "你好")), ...send(message("chat-2", "今天天气如何")),
    ...send(message("chat-3", "请总结一下"))], false);
  await finish(active);
  const preference = memory().scopes[0].snapshot.preferences.find(item => item.text === first);
  assert.ok(preference, "first preference saved");
  const paused = path.join(work, "paused");
  const end = path.join(work, "end");
  active = launch([...send(message("correct", `/correct-memory ${preference.id} ${corrected}`, "偏好已修正：")),
    { touch: paused }, { wait_file: end }], true, ["--memory-learning"]);
  const deadline = Date.now() + 20000;
  while (!active.stderr.includes("EVE_WEB_READY ") || !fs.existsSync(paused)) {
    assert.equal(active.child.exitCode, null, "owned Eve exited before browser synchronization");
    assert.ok(Date.now() < deadline, "browser synchronization timed out");
    await delay(20);
  }
  const url = active.stderr.match(/EVE_WEB_READY (http:\/\/[^\s]+)/)[1];
  // 等待后台提炼批次完成；面板读取本身不触发提炼。
  const jobs = () => {
    const entry = JSON.parse(fs.readFileSync(statePath, "utf8")).entries["eve.learning"]?.["learning.v1"];
    return entry ? JSON.parse(Buffer.from(entry).toString("utf8")).jobs : [];
  };
  while (!jobs().some(record => record.job.status === "Completed")) {
    assert.equal(active.child.exitCode, null, "owned Eve exited before learning finished");
    assert.ok(Date.now() < deadline, "learning batch did not finish");
    await delay(20);
  }
  const stateBefore = fs.readFileSync(statePath, "utf8");
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
  await page.locator('[data-view="memory"]').click();
  await page.locator("#memory-list .record-button").first().waitFor();
  assert.equal(await page.locator("#memory-list .record-button").count(), 1);
  assert.ok((await page.locator("#memory-list").textContent()).includes("2 条生效"));
  await page.locator("#memory-list .record-button").first().click();
  await page.waitForFunction(() => document.querySelector("#memory-detail").textContent.includes("版本 2（最新）"));
  const detail = await page.locator("#memory-detail").textContent();
  for (const expected of [corrected, first, "用中文回复", "当前生效", "版本 1 · 已确认", "用户明确声明", "保留历史与来源"]) {
    assert.ok(detail.includes(expected), expected);
  }
  assert.equal(await page.locator("#memory-detail img").count(), 0);
  await page.locator(".reflection-card", { hasText: corrected }).getByRole("button", { name: "查看版本 1 的来源" }).click();
  await page.waitForFunction(() => document.querySelector("#memory-evidence").textContent.includes("用户声明"));
  const source = await page.locator("#memory-evidence").textContent();
  for (const expected of [`/remember ${first}`, "引用此来源的偏好版本", "不是已核实的事实"]) assert.ok(source.includes(expected), expected);
  for (const hidden of ["remember-a", "synthetic-user", "test-model-secret"]) assert.equal((detail + source).includes(hidden), false, hidden);
  assert.equal(await page.locator("#memory-detail img").count(), 0);
  assert.equal(await page.evaluate(() => window.eveXss), undefined);
  await page.waitForFunction(() => document.querySelector("#memory-learning").textContent.includes("学习候选（1）"));
  const learning = await page.locator("#memory-learning").textContent();
  for (const expected of ["手动模式", learned, "待确认", "模型自评 83（不是校准概率）", "尚无自动学习决策", "实际保存：记忆历史中没有", "决策记录是提交前的意图"]) {
    assert.ok(learning.includes(expected), expected);
  }
  assert.equal(await page.locator("#memory-detail img").count(), 0);
  await page.screenshot({ path: path.join(artifacts, "memory.png"), fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), "mobile memory overflows horizontally");
  await page.screenshot({ path: path.join(artifacts, "mobile-memory.png"), fullPage: true });
  await page.locator("#logout-button").click();
  await page.locator("#login-view").waitFor({ state: "visible" });
  assert.equal(await page.locator("#memory-list").textContent(), "");
  assert.equal(await page.locator("#memory-detail").textContent(), "");
  assert.deepEqual(errors, []);
  assert.deepEqual(external, []);
  assert.equal(await page.evaluate(() => localStorage.length + sessionStorage.length), 0);
  assert.equal(fs.readFileSync(statePath, "utf8"), stateBefore, "browser reads must not write state");
  fs.writeFileSync(end, "done");
  await finish(active);
  assert.equal(requests, 4);
  console.log(JSON.stringify({ passed: true, model_requests: requests, browser_errors: errors.length,
    checks: ["scope-list", "preference-history", "explicit-source", "learning-candidates", "xss-text", "read-only-state", "mobile-layout", "logout"], artifacts }));
} finally {
  if (browser) await browser.close();
  if (active && active.child.exitCode === null) { active.child.kill("SIGKILL"); await active.exited; }
  for (const socket of sockets) socket.destroy();
  await new Promise(resolve => model.close(resolve));
  fs.rmSync(work, { recursive: true, force: true });
}
