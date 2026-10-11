// 可选浏览器验收：由 plan_test.py 在真实 Eve 面板运行时调用。
// 在目标详情中查看计划与建议记录，确认步骤说明按纯文本显示，并只在二次确认后撤销计划。
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const { chromium } = await import(process.env.EVE_PLAYWRIGHT_MODULE ? pathToFileURL(process.env.EVE_PLAYWRIGHT_MODULE).href : "playwright");
const url = process.env.EVE_PANEL_URL;
const token = process.env.EVE_PANEL_TOKEN;
const goalId = process.env.EVE_GOAL_ID;
assert.ok(url && token && goalId, "需要 EVE_PANEL_URL、EVE_PANEL_TOKEN 与 EVE_GOAL_ID");
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
  await page.locator('[data-view="goals"]').click();
  await page.locator("#goals-list .record-button").first().waitFor();
  await page.locator("#goals-list .record-button").first().click();
  await page.waitForFunction(() => (document.querySelector("#goal-plans")?.textContent || "").includes("多步计划（1）"));
  const plans = await page.locator("#goal-plans").textContent();
  for (const expected of ["待确认", "模型建议", "查官方资料", "做最小模组并实际运行", "能力 eve.research.v1", "计划建议记录（1）", "已提出计划", "目标仍保持等待"]) {
    assert.ok(plans.includes(expected), `计划区块缺少：${expected}`);
  }
  assert.equal(await page.locator("#goal-plans img").count(), 0);
  await page.screenshot({ path: path.join(artifacts, "plans.png"), fullPage: true });

  // 第一次点击只出现确认框，不发送撤销请求。
  let withdrawals = 0;
  page.on("request", request => { if (request.url().endsWith("/api/goal/plans/withdraw")) withdrawals++; });
  await page.getByRole("button", { name: "撤销计划" }).click();
  await page.getByRole("button", { name: "确认撤销" }).waitFor();
  assert.equal(withdrawals, 0, "第一次点击不应撤销");
  await page.getByRole("button", { name: "返回" }).click();
  await page.getByRole("button", { name: "撤销计划" }).waitFor();
  assert.equal(withdrawals, 0, "返回不应撤销");
  await page.getByRole("button", { name: "撤销计划" }).click();
  await page.getByRole("button", { name: "确认撤销" }).click();
  await page.waitForFunction(() => (document.querySelector("#goal-plans")?.textContent || "").includes("计划已撤销"));
  await page.waitForFunction(() => (document.querySelector("#goal-plans")?.textContent || "").includes("已撤销"));
  assert.equal(withdrawals, 1);
  assert.equal(await page.getByRole("button", { name: "撤销计划" }).count(), 0, "撤销后不再提供撤销按钮");

  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: path.join(artifacts, "mobile-plans.png"), fullPage: true });
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  assert.ok(overflow <= 1, `手机宽度下出现横向滚动：${overflow}px`);
  assert.deepEqual(errors, []);
  assert.deepEqual(external, []);
  console.log(JSON.stringify({ ok: true, withdrawals }));
} finally {
  await browser.close();
}
