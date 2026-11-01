// 实际浏览器、本机面板与 Eve 进程；模型和 QQ 为替身，不使用正式凭据。
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
const work = fs.mkdtempSync(path.join(os.tmpdir(), "eve-web-notes-"));
const token = "synthetic-notes-browser-token-123456789012345";
const note = '<img src=x onerror="window.eveXss=true"> 模组编译通过，尚未核对游戏加载';
const changed = "实际产物已读回，尚未对外发布";
const statePath = path.join(work, "state/state.json");
const requests = [], sockets = new Set();
const model = http.createServer(async (request, response) => {
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  requests.push(body);
  const text = body.messages.filter(m => m.role === "user").at(-1).content;
  response.writeHead(200, { "Content-Type": "application/json" });
  response.end(JSON.stringify({ choices: [{ index: 0, finish_reason: "stop", message: { role: "assistant", content: text } }] }));
});
model.on("connection", socket => { sockets.add(socket); socket.on("close", () => sockets.delete(socket)); });
await new Promise(resolve => model.listen(0, "127.0.0.1", resolve));
const message = (id, text) => ({ id, text, scope: "c2c", user_id: "browser-user", target_id: "browser-user", expected_type: "reply", expected: text });
const send = item => [{ send: item }, { wait_command: { id: item.id, type: "reply" } }, { wait_receipt: { path: statePath, id: item.id, state: "Sent" } }];
const scenario = path.join(work, "scenario.json");
fs.writeFileSync(scenario, JSON.stringify({ script: [...send(message("seed", "本机便笺管理测试")), { wait_file: path.join(work,"continue") }, ...send(message("next", "读取下一轮便笺")), { wait_file: path.join(work,"end") }], events_file:path.join(work,"events.jsonl"), error_file:path.join(work,"bridge-error") }));
const env = Object.fromEntries(["PATH","SystemRoot","TEMP","TMP"].filter(name=>process.env[name]).map(name=>[name,process.env[name]]));
Object.assign(env, { QQBOT_APP_SECRET:"test-app-secret", EVE_OPENAI_API_KEY:"test-model-secret", EVE_OPENAI_BASE_URL:`http://127.0.0.1:${model.address().port}`, EVE_OPENAI_PROTOCOL:"chat", EVE_LLM_RESPONSE_MODE:"complete", EVE_WEB_TOKEN:token, EVE_DEBOUNCE_MS:"0", EVE_PROGRESS_STAGE_UPDATES:"false", EVE_PROGRESS_TOOL_MILESTONES:"false", EVE_PROGRESS_SLOW_AFTER_MS:"0" });
const child = spawn(process.env.EVE_QQBOT_BINARY || path.join(root,"target/debug/eve-qqbot"), ["--state-dir",path.join(work,"state"),"--agent",path.join(root,"AGENT.md"),"--bridge-script",path.join(root,"connectors/qqbot/test/fake-bridge.mjs"),"--bridge-arg",scenario,"--web-listen","127.0.0.1:0"], { env, stdio:["ignore","pipe","pipe"] });
let stdout="", stderr="", browser;
child.stdout.on("data",bytes=>{stdout+=bytes;});
child.stderr.on("data",bytes=>{stderr+=bytes;});
const exited = new Promise(resolve=>child.on("exit",(code,signal)=>resolve({code,signal})));
async function until(predicate, label, timeout=15000) {
  const end=Date.now()+timeout;
  while(Date.now()<end) { const value=await predicate(); if(value) return value; assert.equal(child.exitCode,null,stderr); await delay(20); }
  throw new Error(`等待失败：${label}\n${stderr.slice(-2000)}`);
}
try {
  const url=await until(()=>stderr.match(/EVE_WEB_READY (http:\/\/[^\s]+)/)?.[1],"面板启动");
  await until(()=>fs.existsSync(statePath) && JSON.parse(fs.readFileSync(statePath,"utf8")).entries["eve.channel.qqbot"] && requests.length===1,"首轮保存");
  browser=await chromium.launch({headless:true,...(process.env.EVE_TEST_BROWSER?{executablePath:process.env.EVE_TEST_BROWSER}:{})});
  const page=await browser.newPage({viewport:{width:1100,height:850}});
  const errors=[], external=[];
  page.on("pageerror",error=>errors.push(String(error)));
  page.on("request",request=>{if(!request.url().startsWith(url))external.push(request.url());});
  await page.goto(url);
  await page.locator("#access-token").fill(token);
  await page.locator("#login-submit").click();
  await page.locator('[data-view="sessions"]').click();
  await page.locator("#sessions-list .record-button").first().click();
  const card=page.getByRole("region",{name:"工作便笺管理"});
  await card.getByRole("button",{name:"读取当前便笺",exact:true}).click();
  await card.getByRole("textbox",{name:"工作便笺正文"}).waitFor({state:"visible"});
  await page.waitForFunction(()=>document.querySelector('.context-notes [role="status"]').textContent.includes("已读取修订 0"));
  await card.getByRole("textbox",{name:"工作便笺正文"}).fill(note);
  assert.equal(await card.getByRole("button",{name:"保存便笺",exact:true}).isDisabled(),true);
  await card.getByRole("button",{name:"预览便笺",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.context-notes [role="status"]').textContent.includes("尚未保存"));
  assert.equal(requests.length,1);
  assert.equal(JSON.parse(fs.readFileSync(statePath,"utf8")).entries["eve.context-notes"],undefined);
  assert.ok((await card.getByLabel("实际注入预览").textContent()).includes("模组编译通过"));
  assert.equal(await card.locator("img").count(),0);
  assert.equal(await page.evaluate(()=>window.eveXss),undefined);
  await card.getByRole("button",{name:"保存便笺",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.context-notes [role="status"]').textContent.includes("已保存修订 1"));

  // 用真实 HTTP 模拟另一个编辑者，旧预览不能覆盖其结果，草稿必须保留。
  await card.getByRole("textbox",{name:"工作便笺正文"}).fill("文".repeat(6000));
  assert.equal(await card.getByRole("button",{name:"预览便笺",exact:true}).isDisabled(),false);
  await card.getByRole("textbox",{name:"工作便笺正文"}).evaluate(editor=>{
    editor.value = "文".repeat(Math.floor(16*1024*1024/3)+1);
    editor.dispatchEvent(new Event("input", {bubbles:true}));
  });
  assert.equal(await card.getByRole("button",{name:"预览便笺",exact:true}).isDisabled(),true);
  await card.getByRole("textbox",{name:"工作便笺正文"}).fill("尚未保存的草稿");
  await card.getByRole("button",{name:"预览便笺",exact:true}).click();
  await page.waitForFunction(()=>!Array.from(document.querySelectorAll('.context-notes button')).find(b=>b.textContent==="保存便笺").disabled);
  // 使用浏览器请求上下文，但不把令牌写入页面或存储。
  const listing=await page.request.get(`${url}/api/sessions?limit=25`,{headers:{Authorization:`Bearer ${token}`}});
  const key=(await listing.json()).sessions[0].key;
  const concurrent=await page.request.post(`${url}/api/context-notes`,{headers:{Authorization:`Bearer ${token}`},data:{key,action:{operation:"update",expected_revision:1,text:changed}}});
  assert.equal(concurrent.status(),200);
  await card.getByRole("button",{name:"保存便笺",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.context-notes [role="status"]').textContent.includes("重新读取核对"));
  assert.equal(await card.getByRole("textbox",{name:"工作便笺正文"}).inputValue(),"尚未保存的草稿");
  assert.equal(await card.getByRole("button",{name:"保存便笺",exact:true}).isDisabled(),true);
  await card.getByRole("button",{name:"读取当前便笺",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.context-notes [role="status"]').textContent.includes("已读取修订 2"));
  assert.equal(await card.getByRole("textbox",{name:"工作便笺正文"}).inputValue(),"尚未保存的草稿");
  await card.getByRole("button",{name:"查看便笺版本",exact:true}).click();
  await card.getByRole("button",{name:"回退至修订 1",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.context-notes [role="status"]').textContent.includes("已保存修订 3"));
  assert.equal(await card.getByRole("textbox",{name:"工作便笺正文"}).inputValue(),note);
  assert.equal(requests.length,1);
  fs.writeFileSync(path.join(work,"continue"),"continue");
  await until(()=>requests.length===2,"下一轮自动装载");
  assert.ok(JSON.stringify(requests[1]).includes("eve-context-note-v1"));
  assert.ok(JSON.stringify(requests[1]).includes("模组编译通过"));
  await page.setViewportSize({width:390,height:844});
  assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1),"便笺在手机宽度不能横向溢出");
  const artifacts=process.env.EVE_WEB_ARTIFACTS || path.join(os.tmpdir(),"eve-web-notes-artifacts");
  fs.mkdirSync(artifacts,{recursive:true});
  await page.screenshot({path:path.join(artifacts,"context-notes.png"),fullPage:true});
  assert.deepEqual(errors,[]);
  assert.deepEqual(external,[]);
  let releaseLate, held=false;
  const lateGate=new Promise(resolve=>{releaseLate=resolve;});
  await page.route("**/api/context-notes",async route=>{
    const response=await route.fetch();
    held=true;
    await lateGate;
    try { await route.fulfill({response}); } catch { /* 退出会中止本测试持有的请求。 */ }
  });
  await card.getByRole("button",{name:"读取当前便笺",exact:true}).click();
  await until(()=>held,"退出前挂起的只读请求");
  await page.locator("#logout-button").click();
  await page.locator("#login-view").waitFor({state:"visible"});
  releaseLate();
  await delay(100);
  assert.equal(await page.locator("#session-detail").textContent(),"");
  assert.deepEqual(errors,[]);
  assert.equal(await page.evaluate(()=>localStorage.length+sessionStorage.length),0);
  fs.writeFileSync(path.join(work,"end"),"end");
  const result=await Promise.race([exited,delay(10000).then(()=>{throw new Error("持有的 Eve 未停止");})]);
  assert.equal(result.code,0,stderr);
  assert.equal(fs.existsSync(path.join(work,"bridge-error")),false);
  for(const secret of[token,"test-model-secret","test-app-secret"])assert.equal((stdout+stderr).includes(secret),false);
  console.log(JSON.stringify({passed:true,model_requests:requests.length,checks:["preview-no-write","save","cas-conflict","draft-preserved","version-rollback","next-turn-injection","xss-text","utf8-limit","mobile","late-response-after-logout"],artifacts}));
} finally {
  if(browser)await browser.close();
  if(child.exitCode===null){child.kill("SIGKILL");await exited;}
  for(const socket of sockets)socket.destroy();
  await new Promise(resolve=>model.close(resolve));
  fs.rmSync(work,{recursive:true,force:true});
}
