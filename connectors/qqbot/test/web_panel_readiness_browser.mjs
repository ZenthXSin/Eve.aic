// 可选浏览器验收：由 model_health_test.py 在真实 Eve 面板运行时调用。
// 核对“运行就绪”区块分开显示工具、主模型、视觉与对话模型最近的失败，手机宽度下不横向滚动。
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const { chromium } = await import(process.env.EVE_PLAYWRIGHT_MODULE ? pathToFileURL(process.env.EVE_PLAYWRIGHT_MODULE).href : "playwright");
const url = process.env.EVE_PANEL_URL;
const token = process.env.EVE_PANEL_TOKEN;
assert.ok(url && token, "需要 EVE_PANEL_URL 与 EVE_PANEL_TOKEN");
const artifacts = process.env.EVE_WEB_ARTIFACTS || path.join(os.tmpdir(), "eve-web-browser-artifacts");
fs.mkdirSync(artifacts, { recursive: true });

const browser = await chromium.launch({ headless: true });
try {
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
  await page.waitForFunction(() => (document.querySelector("#readiness-roles")?.textContent || "").includes("请求失败"));
  const readiness = await page.locator("#readiness").textContent();
  for (const expected of ["对话工具", "主模型", "单次输出上限", "provider.openai.max_output_tokens", "视觉", "未配置",
    "对话", "请求失败", "额度不足", "HTTP 402", "请求 1 次", "失败 1 次", "重启后清零"]) {
    assert.ok(readiness.includes(expected), `就绪区块缺少：${expected}`);
  }
  assert.ok(!readiness.includes("local provider unavailable"), "不显示供应商错误正文");
  assert.equal(await page.locator("#channel-state").textContent(), "已就绪");
  await page.screenshot({ path: path.join(artifacts, "readiness.png"), fullPage: true });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: path.join(artifacts, "mobile-readiness.png"), fullPage: true });
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  assert.ok(overflow <= 1, `手机宽度下出现横向滚动：${overflow}px`);
  assert.deepEqual(errors, []);
  assert.deepEqual(external, []);
  console.log(JSON.stringify({ ok: true }));
} finally {
  await browser.close();
}
