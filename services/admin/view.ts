import { agentsMarkup, agentsScript } from "./agents-view.ts";
import { historyScript } from "./history-view.ts";
import { liveScript } from "./live-view.ts";
import { panelCss } from "./styles.ts";
import { stageScript } from "./stage-view.ts";
import { timelineScript } from "./timeline-view.ts";
import { usageScript } from "./usage-view.ts";
import { workspaceMarkup, workspaceScript, workspaceSocketScript } from "./workspace-view.ts";

/** 导航图标均为本站静态常量。 */
const paths: Record<string, string> = {
  live: '<path d="M3 12h4l3-8 4 16 3-8h4"/>',
  overview:
    '<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/>',
  records: '<path d="M5 4h11a3 3 0 0 1 3 3v13H8a3 3 0 0 1-3-3Z"/><path d="M9 9h6M9 13h4"/>',
  agents:
    '<circle cx="9" cy="8" r="3.5"/><path d="M2.5 20a6.5 6.5 0 0 1 13 0"/><path d="M16 4.6a3.5 3.5 0 0 1 0 6.8M18 14.5a6.5 6.5 0 0 1 3.5 5.5"/>',
  queue: '<path d="M4 13 6.5 5h11L20 13v6H4Z"/><path d="M4 13h4.5l1.2 2.5h4.6l1.2-2.5H20"/>',
  settings:
    '<path d="M4 7h10M18 7h2M4 17h2M10 17h10"/><circle cx="16" cy="7" r="2"/><circle cx="8" cy="17" r="2"/>',
  more:
    '<circle cx="5" cy="12" r="1.4"/><circle cx="12" cy="12" r="1.4"/><circle cx="19" cy="12" r="1.4"/>',
  refresh: '<path d="M20 12a8 8 0 1 1-2.6-5.9"/><path d="M20 4v5h-5"/>',
  theme: '<path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5Z"/>',
  logout: '<path d="M14 4h4a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-4"/><path d="M10 16l-4-4 4-4M6 12h9"/>',
};
const icon = (id: string) =>
  `<svg class="icon" viewBox="0 0 24 24" aria-hidden="true">${paths[id]}</svg>`;

/** [目标视图, 导航分组, 图标, 名称]。记录分组包含意识流、便签、记忆与动作回执。 */
const destinations = [
  ["live", "live", "live", "实时"],
  ["overview", "overview", "overview", "总览"],
  ["timeline", "records", "records", "记录"],
  ["agents", "agents", "agents", "子代理"],
  ["qqFailures", "qqFailures", "queue", "待处理"],
  ["settings", "settings", "settings", "设置"],
] as const;
const navButton = ([view, group, image, label]: readonly string[]) =>
  `<button class="nav-button" data-view="${view}" data-nav="${group}">${
    icon(image)
  }<span>${label}</span>${
    group === "qqFailures" ? '<span class="count queue-count" hidden></span>' : ""
  }</button>`;
const brand = (caption: string) =>
  `<div class="brand"><span class="mark" aria-hidden="true">e</span><div><strong>Eve</strong><small>${caption}</small></div></div>`;
const controls =
  `<button data-control="pause">暂停运行</button><button data-control="resume" class="primary" hidden>恢复运行</button>`;

const loginMarkup = `<div id="login" class="login-wrap"><section class="login-card">${
  brand("管理面板")
}
<h1>登录</h1><p>查看 Eve 的运行情况，管理预算与待处理消息。</p>
<form id="loginForm"><label for="token">管理员密码</label><input id="token" type="password" autocomplete="current-password" required maxlength="1024"><button class="primary">登录</button></form>
<p id="loginMessage" role="status" aria-live="polite"></p></section></div>`;

const shellMarkup = `<aside class="sidebar">${brand("管理面板")}
<nav aria-label="主导航">${destinations.map(navButton).join("")}</nav>
<div class="side-bottom"><button class="nav-button" data-theme-toggle>${
  icon("theme")
}<span>切换明暗</span></button><button class="nav-button" id="logout">${
  icon("logout")
}<span>退出登录</span></button></div></aside>`;

const topbarMarkup =
  `<header class="topbar"><div class="topbar-title"><span class="mark" aria-hidden="true">e</span><div><h1 id="pageTitle">实时</h1><p id="pageHint"></p></div></div>
<div class="top-actions"><span id="sync" class="sync">等待同步</span><span id="liveBadge" class="badge warn">读取中</span><button id="refresh" class="icon-button" aria-label="刷新" title="刷新">${
    icon("refresh")
  }</button></div></header>
<span id="loadingText" class="skip" role="status" aria-live="polite"></span>`;

const overviewMarkup = `<section id="overview" class="view" hidden>
<div class="hero" id="hero"><div><h2 id="greeting">正在读取状态</h2><p id="healthSummary">正在向运行管理者查询。</p></div><div class="row">${controls}<button data-view="live">看看她在做什么</button></div></div>
<div id="attention" class="attention" aria-label="需要留意"></div>
<div class="tiles">
<article class="tile"><div class="label">今日预算已用</div><div class="number" id="tileBudget">—</div><svg class="meter" viewBox="0 0 100 6" preserveAspectRatio="none" aria-hidden="true"><rect class="meter-known" id="meterKnown" height="6" width="0"/><rect class="meter-reserved" id="meterReserved" height="6" width="0"/></svg><small id="tileBudgetHint">等待读取</small></article>
<article class="tile"><div class="label">当前模型</div><div class="number text" id="modelValue">—</div><small id="modelHint">等待读取</small></article>
<article class="tile"><div class="label">意识流记录</div><div class="number" id="recordValue">—</div><button class="link" data-view="timeline">阅读最近记录</button></article>
<article class="tile"><div class="label">待处理消息</div><div class="number" id="queueValue">—</div><button class="link" data-view="qqFailures">查看并处理</button></article>
</div>
<div class="grid-two">
<section class="panel usage-panel"><div class="panel-title"><div><h2>每次请求的输入量</h2><p class="hint" id="usageRange">读取最近的真实请求用量…</p></div></div>
<div class="usage-summary"><div class="usage-stat"><strong id="inputAverage">—</strong><span>平均输入 token</span></div><div class="usage-stat"><strong id="cacheHit">—</strong><span>缓存命中率（按 token 加权）</span></div><div class="usage-stat"><strong>150k</strong><span>平均输入目标，非硬上限</span></div></div>
<p id="usageInsight" class="chart-insight">正在分析已报告的请求用量。</p>
<div class="chart-legend"><span><i class="dot"></i>已缓存</span><span><i class="dot remaining"></i>未缓存</span><span><i class="dot unknown"></i>缓存未报告</span><span>┄ 150k 目标</span></div>
<div class="chart-scroll"><svg id="usageChart" class="usage-chart" viewBox="0 0 900 230" role="img" aria-labelledby="usageChartTitle"><title id="usageChartTitle">最近真实请求输入与缓存分布</title></svg></div>
<p id="chartReadout" class="chart-readout" aria-live="polite">点按柱形查看单次请求。</p><p id="usageEmpty" class="chart-empty" hidden></p>
<details><summary>以表格查看</summary><div class="chart-table"><table class="chart-data"><thead><tr><th>完成时间</th><th>请求</th><th>输入 token</th><th>缓存 token</th></tr></thead><tbody id="usageTable"></tbody></table></div></details>
<p class="hint">取自最近 80 条意识流记录。未报告缓存的请求不计入命中率；柱形按请求顺序排列，间距不代表时间；这个窗口不代表全天平均。</p></section>
<div class="stack">
<section class="panel"><div class="panel-title"><h2>今日预算</h2><button class="link" data-view="settings">调整</button></div>
<div class="budget-viz"><div class="ring"><svg viewBox="0 0 140 140" role="img" aria-labelledby="budgetChartTitle"><title id="budgetChartTitle">今日 token 预算分配</title><circle class="ring-base" cx="70" cy="70" r="58"/><circle id="knownArc" class="ring-known" cx="70" cy="70" r="58" stroke-dasharray="0 365"/><circle id="reservedArc" class="ring-reserved" cx="70" cy="70" r="58" stroke-dasharray="0 365"/></svg><div class="ring-label"><strong id="budgetPercent">—</strong><small>已占用</small></div></div>
<div class="legend"><div class="legend-row"><span><i class="dot"></i>已确认消耗</span><strong id="knownValue">—</strong></div><div class="legend-row"><span><i class="dot reserved"></i>为未知用量预留</span><strong id="reservedValue">—</strong></div><div class="legend-row"><span><i class="dot remaining"></i>还可使用</span><strong id="remainingValue">—</strong></div><div class="legend-row"><span>每日上限</span><strong id="budgetValue">—</strong></div></div></div>
<p class="hint" id="budgetHint">按安全口径记账：预留不等于实际消耗。</p></section>
<section class="panel"><div class="panel-title"><h2>运行详情</h2></div><div class="facts">
<div><span>执行服务</span><strong id="runtimeHealth">—</strong></div>
<div><span>当前实例</span><strong id="runningHint">—</strong></div>
<div><span>实例所有权</span><strong id="ownerHealth">—</strong></div>
<div><span>版本更新</span><strong id="upgradeHealth">—</strong></div>
<div><span>便签 / 记忆记录数</span><strong id="knowledgeHealth">—</strong></div>
<div><span>动作回执记录数</span><strong id="receiptsHealth">—</strong></div>
</div><p class="hint">记录数是日志累计序号；便签的每个版本分别计入。</p></section>
</div></div></section>`;

const recordsMarkup = `<section id="records" class="view" hidden>
<div class="segmented" id="recordTabs" role="group" aria-label="记录类型"><button data-view="timeline" data-tab="timeline">意识流</button><button data-view="notes" data-tab="notes">便签</button><button data-view="memory" data-tab="memory">记忆</button><button data-view="receipts" data-tab="receipts">动作回执</button></div>
<form id="searchForm" class="searchbar"><input id="query" type="search" placeholder="搜索记录中的文字" aria-label="搜索文字" maxlength="512"><select id="scope" aria-label="记录范围"><option value="recent">最近 80 条</option><option value="all">全部记录</option></select><button class="primary">搜索</button><button type="button" id="clear" class="quiet">清除</button></form>
<div id="recordChips" class="chips" role="group" aria-label="按类型筛选"><button data-record-filter="all" aria-pressed="true">全部</button><button data-record-filter="thought" aria-pressed="false">想法</button><button data-record-filter="message" aria-pressed="false">通知</button><button data-record-filter="tool" aria-pressed="false">工具</button><button data-record-filter="error" aria-pressed="false">异常</button><button data-record-filter="other" aria-pressed="false">其他</button></div>
<p id="recordsHint" class="hint"></p><p id="listSummary" class="hint" role="status"></p>
<div id="items" class="items"></div>
<div class="page-end"><button id="more" hidden>加载更多</button></div></section>`;

const settingsMarkup = `<section id="settings" class="view settings-grid" hidden>
<div id="modelBlocked" class="notice wide" hidden><p id="blockedText"></p><button id="resumeModel">解除模型阻塞</button></div>
<section class="panel"><h2>运行控制</h2><p>暂停后，运行管理者会等当前这一步收尾再停下；恢复后 Eve 继续工作。</p><div class="row">${controls}</div></section>
<section class="panel"><h2>每日 token 预算</h2><p>按安全口径记账。设为 0 会阻止新的模型请求。</p><form id="budgetForm"><div class="row"><label for="budget">每日上限</label><input id="budget" type="number" min="0" step="1" required inputmode="numeric"><button class="primary">保存</button></div><p id="budgetPreview" class="hint"></p></form></section>
<section class="panel"><h2>外观</h2><p>只保存在这台设备的浏览器中。</p><div class="segmented" role="group" aria-label="外观"><button data-theme-choice="auto" aria-pressed="true">跟随系统</button><button data-theme-choice="light" aria-pressed="false">浅色</button><button data-theme-choice="dark" aria-pressed="false">深色</button></div></section>
<section class="panel"><h2>技术详情</h2><p>排查问题时，可核对运行管理者返回的原始状态。</p><details><summary>展开原始状态</summary><pre id="status"></pre></details><p class="hint">语义检索模型接入待办。当前检索能力以运行服务实际配置为准。</p></section>
</section>`;

const overlayMarkup = `<nav class="tabbar" aria-label="快捷导航">${
  destinations.filter(([, group]) => !["agents", "settings"].includes(group)).map(navButton).join(
    "",
  )
}<button class="nav-button" id="navMore" data-nav="more" aria-haspopup="dialog">${
  icon("more")
}<span>更多</span></button></nav>
<dialog id="moreSheet" class="sheet" aria-label="更多">${
  destinations.filter(([, group]) => ["agents", "settings"].includes(group)).map(navButton).join("")
}<button class="nav-button" data-theme-toggle>${
  icon("theme")
}<span>切换明暗</span></button><button class="nav-button" id="logoutSheet">${
  icon("logout")
}<span>退出登录</span></button></dialog>
<dialog id="dialog" class="drawer" aria-labelledby="detailTitle"><div class="dialog-head"><h2 id="detailTitle">记录详情</h2><button id="versions" class="small" hidden>历史版本</button><button id="closeDetail" class="small" aria-label="关闭详情">关闭</button></div><div class="drawer-body"><div id="detail" class="detail-body"></div><button id="detailMore" hidden>继续读取</button><details><summary>原始数据</summary><pre id="detailRaw"></pre></details></div></dialog>
<dialog id="confirm" class="confirm" aria-labelledby="confirmTitle"><h2 id="confirmTitle"></h2><p id="confirmText"></p><div class="row"><button id="confirmCancel">取消</button><button id="confirmOk" class="primary">确定</button></div></dialog>
<p id="message" class="toast" role="status" aria-live="polite" hidden></p>`;

const coreScript = String.raw`
'use strict';
const el=id=>document.getElementById(id);
const inBrowser=typeof window!=='undefined';
let csrf='',busy=false,activeView='live',currentResource='timeline',currentOperation='page',next=null,detailArgs=null,detailOperation='get',detailResource=null,blockedRequestId=null,lastStatus=null,loaded=0,queueCount=null,queueHasMore=false,budgetDirty=false,activityThrough=null,liveFollow=true,liveSnapshot=null,liveRawSnapshot=null,liveFilter='thought',liveFreshAt=0,liveRenderedSeq=-1,liveError=false,liveFeedKey='',recordFilter='all',listSummaryText='',messageTimer=null,askResolve=null;
const pretty=value=>JSON.stringify(value,null,2),num=value=>Number.isFinite(value)?new Intl.NumberFormat('zh-CN').format(value):'—';
const labels={live:'实时',overview:'总览',timeline:'意识流',notes:'便签',memory:'记忆',receipts:'动作回执',agents:'子代理',qqFailures:'待处理',settings:'设置'};
const recordViews=['timeline','notes','memory','receipts'];
const pageHints={live:'Eve 此刻在做什么，以及她写下的文字',overview:'运行状态、预算与模型用量',records:'阅读 Eve 的意识流、便签、记忆与动作回执',agents:'Eve 委派出去的独立会话',qqFailures:'发送失败或结果未知、正挡住会话的消息',settings:'运行控制、预算与外观'};
const hints={timeline:'按时间记录 Eve 的想法、收到的通知和工具调用。',notes:'Eve 自己整理的便签，打开可阅读全文和历史版本。',memory:'Eve 形成的记忆，附来源与强度。',receipts:'每个动作的执行结果。失败与送达未知会保留原始回执。',qqFailures:'这些消息发送失败或结果未知，正挡住同一会话后面的消息。取消等待不会撤回可能已送达的消息，只是让后续消息继续发送。'};
const kindNames={thought:'思考',model_start:'发起模型请求',model_complete:'模型请求完成',model_error:'模型请求异常',context_projection:'更新上下文投影',context_summary:'整理上下文',compression:'压缩上下文',upgrade_rollback:'版本回滚',self_reflection:'自己的待办浮现',notice:'收到通知',formation:'形成记忆',note:'便签更新',script_start:'开始脚本',script_complete:'脚本完成',wake:'唤醒',rest:'休息'};
function navGroup(view){return recordViews.includes(view)?'records':view;}
function time(value){if(!value)return '';const d=new Date(value);return Number.isFinite(d.getTime())?d.toLocaleString('zh-CN',{hour12:false}):String(value);}
function text(value){if(typeof value==='string')return value;if(value===undefined||value===null)return '';return pretty(value);}
function bodyText(item){const d=item?.data??item;return text(item?.content??item?.text??item?.summary??d?.text??d?.summary??d?.content??d?.response?.content??d?.reason??item?.preview??d);}
function node(tag,className,content){const n=document.createElement(tag);if(className)n.className=className;if(content!==undefined)n.textContent=content;return n;}

/* 提示条：登录前显示在登录卡片内，登录后以浮动提示出现并自动消失。 */
function message(content,tone){
  const shown=content||'';
  if(messageTimer!==null){clearTimeout(messageTimer);messageTimer=null;}
  el('loginMessage').textContent=csrf?'':shown;
  el('message').textContent=shown;el('message').hidden=!shown||!csrf;
  el('message').className='toast'+(tone==='error'?' error':'');
  if(shown&&csrf)messageTimer=setTimeout(()=>{messageTimer=null;el('message').hidden=true;},tone==='error'?9000:5000);
}
/* 确认框：须在 run() 之外等待，否则全局禁用会连确认按钮一起禁用。 */
function ask(title,body,okLabel,danger){
  return new Promise(resolve=>{
    askResolve=resolve;
    el('confirmTitle').textContent=title;el('confirmText').textContent=body;
    el('confirmOk').textContent=okLabel||'确定';el('confirmOk').className=danger?'danger':'primary';
    el('confirm').returnValue='';el('confirm').showModal();
  });
}
el('confirm').addEventListener('close',()=>{const resolve=askResolve;askResolve=null;if(resolve)resolve(el('confirm').returnValue==='ok');});
el('confirmOk').addEventListener('click',()=>el('confirm').close('ok'));
el('confirmCancel').addEventListener('click',()=>el('confirm').close(''));

/* 外观：未选择时跟随系统；选择只保存在本机浏览器。 */
const themeKey='eve.admin-theme';
function themeChoice(){try{const saved=localStorage.getItem(themeKey);return saved==='light'||saved==='dark'?saved:'auto';}catch{return 'auto';}}
function themeResolved(){const choice=themeChoice();if(choice!=='auto')return choice;return typeof matchMedia==='function'&&matchMedia('(prefers-color-scheme: dark)').matches?'dark':'light';}
function applyTheme(){const root=document.documentElement;if(root)root.setAttribute('data-theme',themeResolved());const choice=themeChoice();document.querySelectorAll('[data-theme-choice]').forEach(n=>n.setAttribute('aria-pressed',String(n.dataset.themeChoice===choice)));}
function setTheme(choice){try{if(choice==='auto')localStorage.removeItem(themeKey);else localStorage.setItem(themeKey,choice);}catch{}applyTheme();}
document.querySelectorAll('[data-theme-choice]').forEach(n=>n.addEventListener('click',()=>setTheme(n.dataset.themeChoice)));
document.querySelectorAll('[data-theme-toggle]').forEach(n=>n.addEventListener('click',()=>setTheme(themeResolved()==='dark'?'light':'dark')));
if(typeof matchMedia==='function')matchMedia('(prefers-color-scheme: dark)').addEventListener('change',applyTheme);
applyTheme();

async function api(path,body){
const requestSession=csrf,controller=new AbortController(),timer=setTimeout(()=>controller.abort(),35000);
const connectionError=()=>new Error(body&&path==='/api/control'?'连接中断，操作结果尚未确认。请刷新状态核对后再操作。':controller.signal.aborted?'管理服务响应超时，请稍后刷新。':'暂时无法连接管理服务，请稍后刷新。');
try{
let response;try{response=await fetch(path,{method:body===undefined?'GET':'POST',headers:body===undefined?{}:{'content-type':'application/json','x-eve-csrf':csrf},body:body===undefined?undefined:JSON.stringify(body),credentials:'same-origin',cache:'no-store',signal:controller.signal});}catch{throw connectionError();}
let wire;try{wire=await response.json();}catch{if(controller.signal.aborted)throw connectionError();throw new Error('管理服务未返回有效数据，请稍后刷新。');}
if(!wire.ok){if(response.status===401&&csrf===requestSession)showLogin();throw new Error(wire.error?.message||'请求失败');}return wire.value;
}finally{clearTimeout(timer);}
}
function showLogin(){clearAgentPanel();clearBrowserHistory();livePoll=null;closeLiveSocket();clearWorkspace();clearStage();csrf='';lastStatus=null;queueCount=null;activityThrough=null;budgetDirty=false;liveSnapshot=null;liveRawSnapshot=null;liveRenderedSeq=-1;liveFeedKey='';liveFreshAt=0;liveError=false;el('liveFeed').replaceChildren();el('app').hidden=true;el('login').hidden=false;el('status').textContent='';el('items').replaceChildren();el('detail').textContent='';el('detailRaw').textContent='';message('');for(const id of ['dialog','moreSheet','confirm'])if(el(id).open)el(id).close();}
function syncControls(){
  const paused=lastStatus?.supervisor?.paused??lastStatus?.paused;
  document.querySelectorAll('[data-control]').forEach(n=>{n.hidden=n.dataset.control==='pause'?paused===true:paused!==true;});
  el('resumeModel').disabled=busy||paused!==true;
}
/* 同一时间只执行一个操作；期间按钮禁用，结束后恢复。 */
async function run(work){
  if(busy)return;busy=true;
  document.querySelectorAll('button').forEach(b=>b.disabled=true);
  el('main').setAttribute('aria-busy','true');el('loadingText').textContent='正在同步…';
  try{await work();}catch(error){message(error.message,'error');}
  finally{busy=false;document.querySelectorAll('button').forEach(b=>b.disabled=false);el('main').setAttribute('aria-busy','false');el('loadingText').textContent='';syncControls();}
}

/* ── 状态与总览 ── */
function budgetWords(amount){if(!Number.isFinite(amount))return '';if(amount>=1e8)return '约 '+(amount/1e8).toFixed(amount%1e8?2:0)+' 亿 token';if(amount>=1e4)return '约 '+(amount/1e4).toFixed(amount%1e4?1:0)+' 万 token';return num(amount)+' token';}
function paintAttention(notices){
  const rows=notices.map(([tone,content,view,action])=>{
    const row=node('div','attention-item');row.setAttribute('data-tone',tone);row.append(node('span','',content));
    if(view){const go=node('button','small',action);go.addEventListener('click',()=>run(()=>navigate(view)));row.append(go);}
    return row;
  });
  el('attention').replaceChildren(...rows);
}
function paintStatus(value){
  lastStatus=value;el('status').textContent=pretty(value);
  const r=value?.runtime??{},s=value?.supervisor??value,o=value?.ownership??{},b=r.budget??value.budget??{},blocked=r.model?.blocked??null;
  blockedRequestId=blocked?.requestId??null;
  const paused=s.paused===true||b.paused===true,available=r.available;
  const state=blocked?'模型已阻塞':paused?'已暂停':available===false?'服务不可用':available===true?'运行中':'状态待确认';
  const tone=blocked||available===false?'error':paused||available!==true?'warn':'ok';
  el('liveBadge').textContent=state;el('liveBadge').className='badge'+(tone==='ok'?'':' '+tone);
  el('hero').setAttribute('data-tone',tone);
  el('greeting').textContent=blocked?'模型请求被拒绝，已停止重试':paused?'Eve 已暂停':available===true?'Eve 正在运行':'暂时无法确认运行状态';
  el('healthSummary').textContent=blocked?'上游拒绝了模型请求。请到设置中核对原因，再决定是否解除。':paused?'运行管理者已暂停当前执行。准备好后可以恢复运行。':available===true?'执行服务可用。':'请刷新，并核对管理服务与执行服务的连接。';
  el('runningHint').textContent=s.current??'未提供';
  el('modelValue').textContent=r.model?.current?.model??'—';
  el('modelHint').textContent=r.model?.current?.revision!==undefined?'配置版本 '+r.model.current.revision:'等待模型配置';
  el('recordValue').textContent=num(r.throughSeq);
  el('runtimeHealth').textContent=available===true?'可用':available===false?'不可用':'未知';
  el('ownerHealth').textContent=o.owner?.instanceId??(o.mode==='paused'?'已暂停':'未提供');
  el('upgradeHealth').textContent=s.upgrade?'更新中 · '+(s.upgrade.phase??'准备中'):s.lastOutcome?.outcome==='confirmed'?'当前版本已确认':s.lastOutcome?.outcome??'暂无更新记录';
  el('knowledgeHealth').textContent=num(r.notesThroughSeq)+' / '+num(r.memoryThroughSeq);
  el('receiptsHealth').textContent=num(r.receiptsThroughSeq);

  const amount=b.dailyTokenBudget??value.dailyTokenBudget;
  if(Number.isSafeInteger(amount)&&!budgetDirty&&document.activeElement!==el('budget')){el('budget').value=String(amount);el('budgetPreview').textContent=budgetWords(amount);}
  el('budgetValue').textContent=num(amount);el('knownValue').textContent=num(b.knownNoncachedTokens);el('reservedValue').textContent=num(b.unknownReservedTokens);el('remainingValue').textContent=num(b.safeRemaining);
  const valid=Number.isFinite(amount)&&amount>0&&Number.isFinite(b.knownNoncachedTokens)&&Number.isFinite(b.unknownReservedTokens);
  const known=valid?Math.min(1,Math.max(0,b.knownNoncachedTokens/amount)):0,reserved=valid?Math.min(1-known,Math.max(0,b.unknownReservedTokens/amount)):0;
  el('knownArc').setAttribute('stroke-dasharray',(known*365)+' 365');
  el('reservedArc').setAttribute('stroke-dasharray',(reserved*365)+' 365');el('reservedArc').setAttribute('stroke-dashoffset',String(-known*365));
  el('meterKnown').setAttribute('width',String(known*100));el('meterReserved').setAttribute('x',String(known*100));el('meterReserved').setAttribute('width',String(reserved*100));
  const percent=valid?Math.round((b.knownNoncachedTokens+b.unknownReservedTokens)/amount*100)+'%':amount===0?'停用':'—';
  el('budgetPercent').textContent=percent;el('tileBudget').textContent=percent;
  el('tileBudgetHint').textContent=Number.isFinite(b.safeRemaining)?'还可使用 '+num(b.safeRemaining):'等待预算数据';
  el('budgetChartTitle').textContent='今日预算：已知非缓存消耗 '+num(b.knownNoncachedTokens)+'，未知预留 '+num(b.unknownReservedTokens)+'，安全剩余 '+num(b.safeRemaining);
  el('budgetHint').textContent=(b.day?'记账日 '+b.day+(b.timeZone?'（'+b.timeZone+'）':'')+'。':'')+'已确认消耗只计非缓存 token；预留不等于实际消耗。';

  el('modelBlocked').hidden=!blocked;el('resumeModel').hidden=!blocked;
  el('blockedText').textContent=blocked?'模型请求已停止自动重试：'+blocked.category+'，HTTP '+blocked.httpStatus+'。请先暂停运行，核实上游原因，再明确解除。解除后仍保持暂停。':'';
  const notices=[];
  if(blocked)notices.push(['error','模型请求被上游拒绝，自动重试已停止。','settings','去处理']);
  if(available===false)notices.push(['error','执行服务当前不可用。']);
  if(paused)notices.push(['warn','Eve 当前处于暂停状态。']);
  if(b.safeRemaining===0)notices.push(['warn','今日预算已用尽，新的模型请求会等待。','settings','调整预算']);
  if(s.upgrade)notices.push(['warn','版本正在更新，等待监督确认。']);
  if(queueCount>0)notices.push(['warn','有 '+queueCount+(queueHasMore?' 条以上':' 条')+'消息挡住了会话，等待处理。','qqFailures','去处理']);
  paintAttention(notices);
  el('sync').textContent='同步于 '+new Date().toLocaleTimeString('zh-CN',{hour12:false});
  syncControls();
}
async function status(){try{paintStatus(await api('/api/status'));}catch(error){el('liveBadge').textContent='状态未同步';el('liveBadge').className='badge warn';throw error;}}
async function queueStatus(){
  try{const v=await api('/api/query',{resource:'qqFailures',operation:'page',args:{limit:32}});queueCount=v.items.length;queueHasMore=Boolean(v.next);el('queueValue').textContent=num(queueCount)+(v.next?'+':'');}
  catch(error){if(!csrf)throw error;queueCount=null;el('queueValue').textContent='未读取';}
  document.querySelectorAll('.queue-count').forEach(n=>{n.hidden=!(queueCount>0);n.textContent=String(queueCount)+(queueHasMore?'+':'');});
  if(lastStatus)paintStatus(lastStatus);
}

/* ── 导航与路由 ── */
function routeView(){if(!inBrowser)return null;const view=window.location.hash.replace(/^#\/?/,'');return Object.hasOwn(labels,view)?view:null;}
async function entered(){
  const session=csrf;const hash=await crypto.subtle.digest('SHA-256',new TextEncoder().encode(session));if(csrf!==session)return;
  initBrowserHistory(Array.from(new Uint8Array(hash),byte=>byte.toString(16).padStart(2,'0')).join(''));
  el('token').value='';el('login').hidden=true;el('app').hidden=false;message('');
  await status();await queueStatus();await navigate(routeView()??'live');
}
function initialArgs(){if(currentResource==='qqFailures'||currentResource==='notes')return {};const r=lastStatus?.runtime??{},key={timeline:'throughSeq',memory:'memoryThroughSeq',receipts:'receiptsThroughSeq'}[currentResource],through=r[key];return el('scope').value==='recent'&&Number.isSafeInteger(through)?{afterSeq:Math.max(0,through-80),throughSeq:through}:{};}
async function navigate(view){
  if(!Object.hasOwn(labels,view))view='live';
  if(view!=='live')closeLiveSocket();
  activeView=view;
  const group=navGroup(view),title=group==='records'?'记录':labels[view];
  el('pageTitle').textContent=title;el('pageHint').textContent=pageHints[group];
  document.querySelectorAll('.nav-button[data-nav]').forEach(n=>{const current=n.dataset.nav===group||n.dataset.nav==='more'&&['agents','settings'].includes(group);if(current)n.setAttribute('aria-current','page');else n.removeAttribute('aria-current');});
  document.querySelectorAll('[data-tab]').forEach(n=>{if(n.dataset.tab===view)n.setAttribute('aria-current','page');else n.removeAttribute('aria-current');});
  if(el('moreSheet').open)el('moreSheet').close();
  if(inBrowser){document.title=title+' · Eve';if(routeView()!==view)window.location.hash='/'+view;window.scrollTo(0,0);}
  const listed=group==='records'||view==='qqFailures';
  el('overview').hidden=view!=='overview';el('settings').hidden=view!=='settings';el('records').hidden=!listed;el('live').hidden=view!=='live';el('agentsPanel').hidden=view!=='agents';
  if(view==='agents'){await refreshAgents();return;}
  if(view==='live'){openLiveSocket();await pollLive(true);return;}
  if(view==='overview'){await queueStatus();await activity();return;}
  if(view==='settings')return;
  currentResource=view;currentOperation='page';el('query').value='';
  el('recordTabs').hidden=view==='qqFailures';el('searchForm').hidden=view==='qqFailures';el('scope').hidden=view==='notes';el('recordChips').hidden=view!=='timeline';
  el('recordsHint').textContent=hints[view];
  await page(initialArgs());
}

/* ── 记录列表 ── */
function recordVisible(category){return recordFilter==='all'||(recordFilter==='other'?!['thought','message','tool','error'].includes(category):category===recordFilter);}
function paintListSummary(){
  let shown=0,total=0;
  if(currentResource==='timeline')for(const card of el('items').querySelectorAll('.record')){total++;card.hidden=!recordVisible(card.dataset.category);if(!card.hidden)shown++;}
  el('listSummary').textContent=listSummaryText+(currentResource==='timeline'&&recordFilter!=='all'&&total?' · 筛选后显示 '+shown+' 条':'');
}
function empty(title,description,isError=false){const box=node('div','empty'+(isError?' error':''));box.append(node('div','empty-mark',isError?'!':'◇'),node('strong','',title),node('p','',description));el('items').append(box);}
async function page(args,append=false){
  if(append)el('items').querySelectorAll('.empty').forEach(n=>n.remove());
  if(!append){el('items').replaceChildren();loaded=0;next=null;el('more').hidden=true;el('listSummary').textContent='正在读取…';}
  let value;
  try{value=await api('/api/query',{resource:currentResource,operation:currentOperation,args});}
  catch(error){if(!append){empty('没有读取成功','点右上角的刷新重试。记录仍保留在服务端。',true);el('listSummary').textContent='读取失败';}throw error;}
  const items=Array.isArray(value?.items)?value.items:[];
  for(const item of items)renderItem(item);
  loaded+=items.length;next=value?.next??null;el('more').hidden=!next;
  listSummaryText='已加载 '+num(loaded)+' 条'+(next?'，还有更多':'，已到末尾')+(currentOperation==='search'?' · 搜索结果':'');
  paintListSummary();
  if(!loaded)empty(currentResource==='qqFailures'?'没有待处理的消息':'这个范围里没有记录',next?'可以继续加载后面的范围。':currentResource==='qqFailures'?'当前没有发送失败或结果未知的消息挡住会话。':'换个搜索词，或把范围切换到全部记录。');
}
function renderItem(item){
  if(currentResource==='timeline'){renderTimelineRecord(item);return;}
  const article=node('article','record'),head=node('div','item-head'),heading=node('div'),actions=node('div','record-actions');
  const isQueue=currentResource==='qqFailures',resource=currentResource;
  let title=item?.title??(isQueue?(item.status?.status==='unknown'?'结果未知':'发送失败'):(kindNames[item?.kind]??item?.kind??(resource==='memory'?'记忆':'记录')));
  if(item?.alias)title='@'+item.alias+' · '+title;
  heading.append(node('div','item-title',String(title)));
  const meta=node('div','meta');
  for(const part of [time(item.at??item.formedAt),item.seq!==undefined?'#'+item.seq:null,item.revision!==undefined?'版本 '+item.revision:null,item.group,item.quarantined?'未确认候选记录':null,resource==='memory'&&item.strength!==undefined?'强度 '+item.strength:null])if(part)meta.append(node('span','',String(part)));
  heading.append(meta);head.append(heading,actions);
  if(!isQueue&&item&&typeof item==='object'&&(item.id!==undefined||item.seq!==undefined)){
    const button=node('button','','打开');
    button.addEventListener('click',()=>run(()=>detail(item.id!==undefined?{id:item.id}:{seq:item.seq},false,'get',resource)));
    actions.append(button);
  }
  if(isQueue&&item?.canCancel===true&&typeof item.receiptDigest==='string'){
    const button=node('button','danger','取消等待');
    const expected={expectedActionId:item.actionId,expectedFingerprint:item.fingerprint,expectedReceiptDigest:item.receiptDigest};
    button.addEventListener('click',async()=>{
      if(!await ask('取消这条消息的等待？','原始回执会保留；可能已经送达的消息不会撤回。取消后，同一会话后面的消息才会继续发送。\n\n动作编号：'+expected.expectedActionId,'取消等待',true))return;
      run(async()=>{await api('/api/control',{action:'resolveQqFailure',args:expected});await page({});await queueStatus();message('已保留原始回执，并取消这条消息的等待。');});
    });
    actions.append(button);
  }
  article.setAttribute('data-resource',resource);article.append(head);
  const content=isQueue?'会话：'+item.conversationId+'\n原因：'+(item.status?.reason??'未提供')+'\n动作：'+item.actionId+(item.canCancel===true?'':'\n这一条正在发送、重试或结果未知，暂时不能从这里取消。'):bodyText(item);
  article.append(node('div','record-body',content.length>1400?content.slice(0,1400)+'\n… 打开后阅读完整记录':content));
  const raw=node('details');raw.append(node('summary','','原始字段'),node('pre','',pretty(item)));article.append(raw);
  el('items').append(article);
}
async function detail(args,append=false,operation='get',resource=detailResource??currentResource){const value=await api('/api/query',{resource,operation,args});detailResource=resource;detailOperation=operation;let content=typeof value.text==='string'?value.text:bodyText(value);if(operation==='versions'&&Array.isArray(value.items))content=value.items.map(v=>'版本 '+(v.revision??'—')+' · '+(v.title??'便签')+'\n'+time(v.at)+'\n'+bodyText(v)).join('\n\n');if(resource!=='notes'&&typeof value.text==='string'&&value.nextChar===undefined&&!append){try{const recorded=JSON.parse(value.text);if(recorded&&recorded.seq!==undefined&&recorded.kind)content=timelineInfo(recorded)[2]+' · '+time(recorded.at)+'\n\n'+timelineDescription(recorded);}catch{}}el('detailTitle').textContent=operation==='versions'?'便签版本记录':labels[resource]+' · 详情';if(!append){el('detail').textContent='';el('detailRaw').textContent='';}el('detail').textContent+=(append?'\n':'')+content;el('detailRaw').textContent+=(append?'\n':'')+pretty(value);detailArgs=value?.next??(value?.nextChar!==undefined?{...args,startChar:value.nextChar}:null);el('detailMore').hidden=!detailArgs;el('versions').hidden=resource!=='notes'||!args.id;el('versions').dataset.id=args.id??'';if(!el('dialog').open)el('dialog').showModal();}
async function search(){const query=el('query').value.trim();currentOperation=query?'search':'page';await page({...initialArgs(),...(query?{query}:{})});}
`;

const wiringScript = String.raw`
/* ── 事件绑定 ── */
el('loginForm').addEventListener('submit',event=>{event.preventDefault();run(async()=>{const value=await api('/api/login',{token:el('token').value});csrf=value.csrf;await entered();});});
document.querySelectorAll('[data-view]').forEach(n=>n.addEventListener('click',()=>run(()=>navigate(n.dataset.view))));
el('navMore').addEventListener('click',()=>el('moreSheet').showModal());
for(const id of ['moreSheet','dialog'])el(id).addEventListener('click',event=>{if(event.target===el(id))el(id).close();});
if(inBrowser)window.addEventListener('hashchange',()=>{const view=routeView();if(csrf&&view&&view!==activeView)run(()=>navigate(view));});
el('refresh').addEventListener('click',()=>run(async()=>{
  await status();
  if(activeView==='agents')await refreshAgents();
  else if(activeView==='live')await pollLive(true);
  else if(activeView==='overview'){await queueStatus();await activity(true);}
  else if(activeView!=='settings'){currentOperation=el('query').value.trim()?'search':'page';await page({...initialArgs(),...(currentOperation==='search'?{query:el('query').value.trim()}:{})});}
}));
document.querySelectorAll('[data-control]').forEach(n=>n.addEventListener('click',async()=>{
  const action=n.dataset.control;
  if(action==='pause'&&!await ask('暂停 Eve 的运行？','运行管理者会等当前这一步收尾后停下，之后可以随时恢复。','暂停运行'))return;
  run(async()=>{await api('/api/control',{action,args:{}});await status();message(action==='pause'?'运行已暂停。':'运行已恢复。');});
}));
el('resumeModel').addEventListener('click',async()=>{
  if(!blockedRequestId){message('请先刷新模型状态。','error');return;}
  if(!await ask('解除模型阻塞？','请确认已经核实上游拒绝的原因。解除后运行仍保持暂停，需要再手动恢复。','解除阻塞'))return;
  run(async()=>{await api('/api/control',{action:'resumeModel',args:{expectedRequestId:blockedRequestId}});await status();message('模型阻塞已解除；运行仍保持暂停，可按需恢复。');});
});
el('budget').addEventListener('input',()=>{budgetDirty=true;const amount=Number(el('budget').value);el('budgetPreview').textContent=el('budget').value===''?'':Number.isSafeInteger(amount)&&amount>=0?budgetWords(amount):'须为非负整数';});
el('budgetForm').addEventListener('submit',async event=>{
  event.preventDefault();
  const amount=Number(el('budget').value);
  if(!Number.isSafeInteger(amount)||amount<0){message('预算必须是非负整数。','error');return;}
  if(amount===0&&!await ask('把每日预算设为 0？','这会阻止新的模型请求，直到你重新调高预算。','设为 0',true))return;
  run(async()=>{await api('/api/control',{action:'budget',args:{dailyTokenBudget:amount}});budgetDirty=false;await status();message('每日预算已保存。');});
});
for(const id of ['logout','logoutSheet'])el(id).addEventListener('click',()=>run(async()=>{await api('/api/logout',{});showLogin();}));
el('searchForm').addEventListener('submit',event=>{event.preventDefault();run(search);});
el('scope').addEventListener('change',()=>run(search));
el('clear').addEventListener('click',()=>run(async()=>{el('query').value='';await search();}));
el('more').addEventListener('click',()=>run(()=>page(next,true)));
document.querySelectorAll('[data-record-filter]').forEach(n=>n.addEventListener('click',()=>{recordFilter=n.dataset.recordFilter;document.querySelectorAll('[data-record-filter]').forEach(other=>other.setAttribute('aria-pressed',String(other===n)));paintListSummary();}));
el('closeDetail').addEventListener('click',()=>el('dialog').close());
el('detailMore').addEventListener('click',()=>run(()=>detail(detailArgs,true,detailOperation)));
el('versions').addEventListener('click',()=>run(()=>detail({id:el('versions').dataset.id},false,'versions')));
if(inBrowser)window.setInterval(()=>{if(csrf&&!busy&&!document.hidden&&!el('dialog').open&&!el('confirm').open&&activeView!=='live')run(async()=>{await status();if(activeView==='agents'){await refreshAgents();}else if(activeView==='overview'){await queueStatus();await activity();}});},30000);
`;

/** 所有业务内容均以文本节点显示；不将记录作为 HTML 执行。 */
export function panelHtml(nonce: string) {
  return `<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover"><meta name="color-scheme" content="light dark"><title>Eve</title>
<style nonce="${nonce}">${panelCss}</style></head><body>
<a class="skip" href="#main">跳到主要内容</a>
${loginMarkup}
<div id="app" class="shell" hidden>${shellMarkup}
<main id="main" class="main">${topbarMarkup}
${workspaceMarkup}
${overviewMarkup}
${agentsMarkup}
${recordsMarkup}
${settingsMarkup}
</main>
${overlayMarkup}</div>
<script nonce="${nonce}">${coreScript}${usageScript}${liveScript}${wiringScript}${agentsScript}${timelineScript}${historyScript}${workspaceScript}${stageScript}${workspaceSocketScript}
run(async()=>{let session;try{session=await api('/api/session');}catch(error){showLogin();if(error.message!=='请先登录')message(error.message,'error');return;}csrf=session.csrf;await entered();});
</script></body></html>`;
}
