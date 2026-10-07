// 实际浏览器验收；由 test-web-plugins.py 启动真实 Eve，不使用 mock API。
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";
const { chromium } = await import(process.env.EVE_PLAYWRIGHT_MODULE
  ? pathToFileURL(process.env.EVE_PLAYWRIGHT_MODULE).href : "playwright-core");
const [url, token] = process.argv.slice(2);
const browser = await chromium.launch({ executablePath: process.env.EVE_TEST_BROWSER || "/usr/bin/chromium", args: ["--no-sandbox"] });
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.goto(url);
  await page.locator("#access-token").fill(token);
  await page.locator("#login-submit").click();
  await page.locator("#console-view").waitFor({ state: "visible" });
  await page.getByRole("button", { name: "插件页面", exact: true }).click();
  await page.getByRole("button", { name: "主模型连接", exact: true }).click();
  const input = page.getByLabel("model", { exact: true });
  await input.fill("browser-model");
  await page.getByRole("button", { name: "保存修改", exact: true }).click();
  await page.getByText(/已保存修订/).waitFor();
  assert.equal(await input.inputValue(), "browser-model");
  await input.fill("unsaved-draft");
  await page.waitForTimeout(3300);
  assert.equal(await input.inputValue(), "unsaved-draft");
  // 另一个操作者提交更新后，旧表单保存应保留编辑并报冲突。
  await page.evaluate(async ({ token }) => {
    const api = async (path, body) => (await fetch(path, { method: "POST", headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" }, body: JSON.stringify(body) })).json();
    const p = await api("/api/plugin-pages/read", { plugin_id: "eve.config", page_id: "provider.openai" });
    await api("/api/plugin-pages/save", { plugin_id: "eve.config", page_id: "provider.openai", instance: p.instance, expected_revision: p.revision, values: { model: "concurrent-model" } });
  }, { token });
  await page.getByRole("button", { name: "保存修改", exact: true }).click();
  await page.getByText(/记录或配置已经变化/).waitFor();
  assert.equal(await input.inputValue(), "unsaved-draft");
  await page.getByRole("button", { name: "重新读取（放弃编辑）", exact: true }).click();
  await page.waitForFunction(() => document.querySelector("#plugin-field-2")?.value === "concurrent-model");
  await input.fill("initial-model");
  await page.getByRole("button", { name: "保存修改", exact: true }).click();
  await page.getByText(/已保存修订/).waitFor();
  await page.getByRole("button", { name: "插件管理", exact: true }).click();
  const plugin = page.locator("#plugins-list article").filter({ has: page.getByRole("heading", { name: "eve.segment.preferences", exact: true }) });
  await plugin.getByRole("button", { name: "停止插件" }).click();
  await plugin.getByRole("button", { name: "启动插件" }).waitFor();
  await page.locator("#plugin-operations").getByText("已完成", { exact: true }).waitFor();
  await plugin.getByRole("button", { name: "启动插件" }).click();
  await plugin.getByRole("button", { name: "停止插件" }).waitFor();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole("button", { name: "插件页面", exact: true }).click();
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), "窄屏存在水平溢出");
  await page.locator("#logout-button").click();
  await page.locator("#login-view").waitFor({ state: "visible" });
  assert.equal(await page.locator("#plugin-page-detail").textContent(), "");
  assert.deepEqual(errors, []);
  console.log("浏览器验收通过：页面注册、保存、冲突、保留编辑、启停、窄屏和退出清理。");
} finally { await browser.close(); }
