import { appIcons, appIds, appNames } from "./apps.ts";

/**
 * 实时页的两个可视化镜头：
 * - 星图：中央光球表示 Eve 当前所处阶段，四周是应用节点；她最近操作的应用会亮起并有光流连接。
 * - 脉络：把近期窗口内的记录画成活动河流、应用使用环与节奏条。
 * 两者只读取已有快照中的真实记录与时间，不推断未发生的事。图形骨架在此静态生成，脚本只改属性。
 */
const round = (value: number) => Math.round(value * 10) / 10;

/** 宽版把节点排在左右两侧，名称写在外侧，给中央光球留出呼吸空间；窄版均匀环绕一周，名称在下方。 */
const widePlaces: [number, number][] = [
  [72, 112],
  [136, 42],
  [136, 182],
  [404, 34],
  [468, 86],
  [468, 138],
  [404, 190],
];
function constellation(kind: "wide" | "tall") {
  const wide = kind === "wide";
  const [width, height, cx, cy] = wide ? [540, 224, 270, 106] : [360, 348, 180, 166];
  const nodes = appIds.map((id, index) => {
    const angle = (-90 + index * 360 / appIds.length) * Math.PI / 180;
    const [x, y] = wide
      ? widePlaces[index]
      : [round(cx + 128 * Math.cos(angle)), round(cy + 124 * Math.sin(angle))];
    // 连线略带弧度，交替向两侧弯曲。
    const mx = (cx + x) / 2, my = (cy + y) / 2, length = Math.hypot(x - cx, y - cy) || 1;
    const bend = (index % 2 ? 1 : -1) * (wide ? 18 : 12);
    const qx = round(mx - (y - cy) / length * bend), qy = round(my + (x - cx) / length * bend);
    const name = !wide
      ? 'y="37"'
      : x < cx
      ? 'x="-29" y="4" class="star-name side-left"'
      : 'x="29" y="4" class="star-name side-right"';
    return { id, x, y, name, path: `M${cx} ${cy}Q${qx} ${qy} ${x} ${y}` };
  });
  const orb = wide ? 32 : 34;
  return `<svg class="stars stars-${kind}" viewBox="0 0 ${width} ${height}" role="group" aria-label="应用星图">
<defs><radialGradient id="orbFill-${kind}" cx="35%" cy="30%" r="80%"><stop offset="0" class="orb-stop-a"/><stop offset=".55" class="orb-stop-b"/><stop offset="1" class="orb-stop-c"/></radialGradient></defs>
<circle class="star-orbit" cx="${cx}" cy="${cy}" r="${wide ? 62 : 126}"/>${
    wide ? `<circle class="star-orbit" cx="${cx}" cy="${cy}" r="92"/>` : ""
  }
${
    nodes.map((n) =>
      `<g class="star-link" data-star-link="${n.id}"><path class="link-base" d="${n.path}"/><path class="link-flow" d="${n.path}" pathLength="100"/></g>`
    ).join("")
  }
<g class="orb" transform="translate(${cx} ${cy})">
<circle class="orb-ring ring-1" r="${orb}"/><circle class="orb-ring ring-2" r="${orb}"/><circle class="orb-ring ring-3" r="${orb}"/>
<circle class="orb-spark" r="${orb}"/>
<g class="orb-sat"><circle class="orb-sat-path" r="${orb + 13}"/><circle class="orb-sat-dot" cx="${
    orb + 13
  }" r="3.5"/><circle class="orb-sat-dot" cx="-${orb + 13}" r="2.5"/></g>
<circle class="orb-core" r="${orb}" fill="url(#orbFill-${kind})"/><ellipse class="orb-shine" cx="-${
    round(orb * .3)
  }" cy="-${round(orb * .38)}" rx="${round(orb * .42)}" ry="${round(orb * .24)}"/>
<text class="orb-name" y="5">Eve</text><text class="orb-label" y="${orb + 30}"></text></g>
${
    nodes.map((n) =>
      `<g class="star" data-star-app="${n.id}" data-app="${n.id}" role="button" tabindex="0" aria-label="查看${
        appNames[n.id]
      }" transform="translate(${n.x} ${n.y})"><circle class="star-halo" r="26"/><circle class="star-disc" r="20"/><g class="star-icon" transform="translate(-10.5 -10.5) scale(.875)">${
        appIcons[n.id]
      }</g><text ${n.name.includes("class=") ? n.name : `class="star-name" ${n.name}`}>${
        appNames[n.id]
      }</text><g class="star-count" transform="translate(16 -15)"><circle r="9"/><text y="3.5" data-star-count="${n.id}"></text></g></g>`
    ).join("")
  }</svg>`;
}

export const constellationMarkup =
  `<div class="constellation" id="constellation" data-phase="waiting">${constellation("wide")}${
    constellation("tall")
  }</div>`;

const RHYTHM_BUCKETS = 36;
export const pulseMarkup = `<section data-lens="pulse" class="pulse" aria-label="脉络">
<header class="pulse-head"><div><h2>脉络</h2><p class="hint" id="pulseRange">等待记录</p></div></header>
<div class="pulse-stats">
<div class="pulse-stat" data-tone="thought"><strong id="pulseThoughts">—</strong><span>次写下文字</span></div>
<div class="pulse-stat" data-tone="tool"><strong id="pulseTools">—</strong><span>次工具调用</span></div>
<div class="pulse-stat" data-tone="model"><strong id="pulseModelWait">—</strong><span>平均等待模型</span></div>
<div class="pulse-stat" data-tone="error"><strong id="pulseErrors">—</strong><span>条异常记录</span></div>
</div>
<div class="pulse-grid">
<div class="pulse-card pulse-river"><h3>活动河流</h3><p class="hint">每条泳道是一类记录，从左到右按时间排开。点按任意标记查看。</p>
<div class="chart-scroll"><svg id="pulseRiverChart" class="river" viewBox="0 0 900 232" role="img" aria-label="近期记录的时间分布">
${
  [["模型", 38], ["文字", 80], ["工具", 122], ["通知", 164], ["异常", 202]].map(([name, y]) =>
    `<text class="river-lane-name" x="8" y="${
      Number(y) + 4
    }">${name}</text><line class="river-lane" x1="62" x2="888" y1="${y}" y2="${y}"/>`
  ).join("")
}
<g id="pulseRiver" class="river-marks"></g></svg></div>
<div class="pulse-readout"><p id="pulseReadout" aria-live="polite">点按河流中的标记，查看那一刻发生了什么。</p><button id="pulseOpen" class="small" hidden>打开原件</button></div></div>
<div class="pulse-card pulse-usage"><h3>应用使用</h3><p class="hint">近期操作账本中各应用的操作次数。</p>
<div class="budget-viz"><div class="ring"><svg viewBox="0 0 140 140" role="img" aria-label="各应用操作次数占比"><circle class="ring-base" cx="70" cy="70" r="58"/>${
  appIds.map((id) =>
    `<circle class="usage-seg" id="pulseSeg_${id}" data-app="${id}" cx="70" cy="70" r="58" stroke-dasharray="0 365"/>`
  ).join("")
}</svg><div class="ring-label"><strong id="pulseRingTotal">—</strong><small>次操作</small></div></div><div id="pulseLegend" class="legend"></div></div></div>
</div>
<div class="pulse-card"><h3>节奏</h3><p class="hint">把这段时间等分为 ${RHYTHM_BUCKETS} 格，柱高是每格内的记录数。</p>
<svg class="rhythm" viewBox="0 0 ${
  RHYTHM_BUCKETS * 25
} 64" preserveAspectRatio="none" role="img" aria-label="记录密度">${
  Array.from(
    { length: RHYTHM_BUCKETS },
    (_, index) =>
      `<rect id="pulseBar${index}" x="${index * 25 + 3}" y="62" width="19" height="2" rx="3"/>`,
  ).join("")
}</svg></div>
</section>`;

export const stageScript = String.raw`
const stageApps=${JSON.stringify(appIds)},stageRhythmBuckets=${RHYTHM_BUCKETS};
const stagePhases={model:'等待模型返回',tool:'等待工具回执',rest:'休息中',paused:'已暂停',attention:'需要留意',response:'模型已返回',result:'工具已返回',notice:'已唤醒',waiting:'待命'};
let stageKey='',stageNoticeSeq=null,stageThoughtSeq=null,stageSpark='a',stageBannerTimer=null,stageSelectedSeq=null,stageRiverMax=-1;
function stageEach(selector,work){document.querySelectorAll(selector).forEach(work);}
function stageClock(value){const d=new Date(value);return Number.isFinite(d.getTime())?d.toLocaleTimeString('zh-CN',{hour12:false,hour:'2-digit',minute:'2-digit'}):'--:--';}
function stageSeconds(ms){if(!Number.isFinite(ms))return '—';const s=ms/1000;return s>=60?Math.floor(s/60)+' 分 '+Math.round(s%60)+' 秒':s>=10?Math.round(s)+' 秒':s.toFixed(1)+' 秒';}
function stageOperations(snapshot){return Array.isArray(snapshot?.workspace?.operations)?snapshot.workspace.operations:[];}

/* 星图与屏幕状态栏：阶段、最近操作的应用、光流方向。 */
function paintConstellation(snapshot){
  const state=snapshot.state||{},phase=stagePhases[state.phase]?state.phase:'waiting',operations=stageOperations(snapshot),current=snapshot.workspace?.currentApp||null;
  el('constellation').setAttribute('data-phase',liveError?'attention':phase);
  el('screenIsland').setAttribute('data-phase',liveError?'attention':phase);
  el('screenIslandText').textContent=liveError?'同步中断':stagePhases[phase];
  el('screenClock').textContent=stageClock(snapshot.asOf);
  stageEach('.orb-label',n=>{n.textContent=liveError?'同步中断':stagePhases[phase];});
  const counts={},recent=[];
  for(const op of operations){counts[op.appId]=(counts[op.appId]||0)+1;}
  for(let i=operations.length-1;i>=0&&recent.length<3;i--)if(!recent.includes(operations[i].appId))recent.push(operations[i].appId);
  // 进行中的操作光流由 Eve 指向应用；刚返回的由应用流回。超过半分钟的旧操作只保留高亮，不再流动。
  const pending=['running','requested','started'].includes(current?.status),age=current?.at?Date.now()-Date.parse(current.at):Infinity,flowing=pending||age<30000;
  stageEach('[data-star-count]',n=>{const count=counts[n.dataset.starCount]||0;n.textContent=count>99?'99+':String(count);n.parentNode.setAttribute('data-empty',String(!count));});
  stageEach('[data-star-app]',n=>{const id=n.dataset.starApp;n.setAttribute('data-active',String(id===current?.appId));n.setAttribute('data-recent',String(recent.includes(id)));n.setAttribute('data-viewing',String(!workspaceHome&&id===workspaceApp));});
  stageEach('[data-star-link]',n=>{const on=n.dataset.starLink===current?.appId;n.setAttribute('data-active',String(on));n.setAttribute('data-flow',on&&flowing?(pending?'out':'in'):'off');});
  let pendingNotifications=0;
  for(const c of snapshot.workspace?.apps?.qq?.conversations||[])if(Number.isSafeInteger(c.pendingNotifications)&&c.pendingNotifications>0)pendingNotifications+=c.pendingNotifications;
  stageEach('[data-app-badge]',n=>{const count=n.dataset.appBadge==='qq'?pendingNotifications:0;n.hidden=!count;n.textContent=count>99?'99+':String(count);n.title='通知水位之后的条数，不代表全部未读';});
  // 新的文字记录让光球闪一下；首帧只记水位，不补放旧动画。
  const events=snapshot.events||[];
  let thoughtSeq=-1,noticeSeq=-1,notice=null;
  for(const e of events){if(e.retained)continue;if(e.kind==='model_response'&&e.hasText)thoughtSeq=Math.max(thoughtSeq,e.seq);if(e.kind==='notice'&&e.noticeSource!=='memory'&&e.seq>noticeSeq){noticeSeq=e.seq;notice=e;}}
  if(stageThoughtSeq!==null&&thoughtSeq>stageThoughtSeq){stageSpark=stageSpark==='a'?'b':'a';el('constellation').setAttribute('data-spark',stageSpark);}
  stageThoughtSeq=Math.max(stageThoughtSeq??-1,thoughtSeq);
  if(stageNoticeSeq!==null&&notice&&noticeSeq>stageNoticeSeq)showStageBanner(notice);
  stageNoticeSeq=Math.max(stageNoticeSeq??-1,noticeSeq);
  // 主屏小组件
  el('homeNowTitle').textContent=state.title||'等待记录';el('homeNowText').textContent=state.description||'';
  const list=[];
  for(const op of operations.slice(-4).reverse()){const row=node('li');row.setAttribute('data-app',op.appId);row.append(node('i','app-dot'),node('span','',(workspaceNames[op.appId]||op.appId)+' · '+op.namespace+'.'+op.method),node('small','',workspaceStatus(op.status)+' · '+workspaceShortTime(op.startedAt||op.at)));list.push(row);}
  if(!list.length)list.push(node('li','home-empty','近期窗口还没有操作记录'));
  el('homeRecent').replaceChildren(...list);
}
/* 通知横幅：只在页面打开期间新到的通知记录上出现，几秒后收起。 */
function showStageBanner(event){
  el('screenBannerTitle').textContent=event.title;el('screenBannerText').textContent=event.text+' · '+stageClock(event.at);
  el('screenBanner').hidden=false;
  if(stageBannerTimer!==null)clearTimeout(stageBannerTimer);
  if(inBrowser)stageBannerTimer=setTimeout(()=>{stageBannerTimer=null;el('screenBanner').hidden=true;},5200);
}

/* 脉络：活动河流、应用使用环、节奏条与四个统计数。 */
function paintPulse(snapshot){
  const events=(snapshot.events||[]).filter(e=>Number.isFinite(Date.parse(e.at))),operations=stageOperations(snapshot);
  const times=events.map(e=>Date.parse(e.at)),start=times.length?Math.min(...times):0,end=times.length?Math.max(...times):0,span=Math.max(1,end-start);
  const left=70,right=880,x=at=>left+(Date.parse(at)-start)/span*(right-left);
  el('pulseRange').textContent=events.length?events.length+' 条记录 · '+stageClock(events[0].at)+' 至 '+stageClock(events.at(-1).at)+' · 只统计当前窗口，不代表全天':'当前窗口还没有记录';
  const marks=[],mark=(event,lane,shape,label)=>{
    const g=svgNode('g',{class:'river-mark river-'+lane+(event.seq>stageRiverMax&&stageRiverMax>=0?' fresh':''),tabindex:0,role:'button','aria-label':label});
    g.append(svgNode('title',{},label),shape);
    const select=()=>{stageSelectedSeq=event.seq;el('pulseReadout').textContent=time(event.at)+' · '+label;el('pulseOpen').hidden=false;el('pulseOpen').textContent='打开原件 #'+event.seq;};
    g.addEventListener('click',select);g.addEventListener('focus',select);
    marks.push(g);
  };
  const modelStarts=new Map(),toolStarts=new Map(),waits=[];
  let thoughts=0,tools=0,errors=0;
  for(const e of events){
    if(e.kind==='model_start'&&e.requestId)modelStarts.set(e.requestId,e);
    else if(e.kind==='model_response'||e.kind==='model_failure'){
      const begun=e.requestId?modelStarts.get(e.requestId):null;
      if(begun){modelStarts.delete(e.requestId);const from=x(begun.at),to=x(e.at),waited=Date.parse(e.at)-Date.parse(begun.at);if(e.kind==='model_response')waits.push(waited);mark(begun,'model',svgNode('rect',{x:from,y:33,width:Math.max(3,to-from),height:10,rx:5}),'等待模型 '+stageSeconds(waited));}
      if(e.kind==='model_response'){if(e.hasText){thoughts++;mark(e,'thought',svgNode('circle',{cx:x(e.at),cy:80,r:Math.min(11,5+e.text.length/60)}),'写下文字：'+e.text.slice(0,60)+(e.text.length>60?'…':''));}else mark(e,'quiet',svgNode('circle',{cx:x(e.at),cy:80,r:3.5}),'模型返回，未留下文字');}
    }else if(e.kind==='tool_start'){tools++;if(e.callId)toolStarts.set(e.callId,e);}
    else if(e.kind==='tool_result'){
      const begun=e.callId?toolStarts.get(e.callId):null,failed=e.category==='error';
      if(begun)toolStarts.delete(e.callId);
      const from=x((begun||e).at),to=x(e.at);
      mark(begun||e,failed?'toolfail':'tool',svgNode('rect',{x:from,y:115,width:Math.max(6,to-from),height:14,rx:4}),(begun?.tool||'工具')+' · '+e.text.slice(0,40));
    }else if(e.kind==='notice'){const cx=x(e.at);mark(e,'notice',svgNode('rect',{x:cx-5,y:159,width:10,height:10,rx:2,transform:'rotate(45 '+cx+' 164)'}),e.title);}
    if(e.category==='error'){errors++;const cx=x(e.at);mark(e,'error',svgNode('path',{d:'M'+cx+' 194 l7 13 h-14 z'}),e.title+'：'+e.text.slice(0,50));}
  }
  // 还没等到回应的请求与调用画到最右端，并保持闪动。
  for(const begun of modelStarts.values())mark(begun,'model pending',svgNode('rect',{x:x(begun.at),y:33,width:Math.max(8,right-x(begun.at)),height:10,rx:5}),'模型请求已发出，尚未见返回');
  for(const begun of toolStarts.values())mark(begun,'tool pending',svgNode('rect',{x:x(begun.at),y:115,width:Math.max(8,right-x(begun.at)),height:14,rx:4}),(begun.tool||'工具')+' 已开始，尚未见回执');
  if(events.length)marks.push(svgNode('line',{class:'river-now',x1:right,x2:right,y1:18,y2:214}));
  el('pulseRiver').replaceChildren(...marks);
  stageRiverMax=events.reduce((max,e)=>Math.max(max,e.seq),stageRiverMax);
  el('pulseThoughts').textContent=String(thoughts);el('pulseTools').textContent=String(tools);el('pulseErrors').textContent=String(errors);
  el('pulseModelWait').textContent=waits.length?stageSeconds(waits.reduce((a,b)=>a+b,0)/waits.length):'—';
  // 应用使用环
  const counts={};for(const op of operations)counts[op.appId]=(counts[op.appId]||0)+1;
  const total=operations.length,legend=[];let offset=0;
  el('pulseRingTotal').textContent=String(total);
  for(const id of stageApps){
    const share=total?(counts[id]||0)/total:0;
    el('pulseSeg_'+id).setAttribute('stroke-dasharray',(share*365)+' 365');el('pulseSeg_'+id).setAttribute('stroke-dashoffset',String(-offset*365));offset+=share;
    if(!counts[id])continue;
    const row=node('button','legend-row link');row.setAttribute('data-app',id);row.append(node('span','',''),node('strong','',counts[id]+' 次'));row.children[0].append(node('i','app-dot'),workspaceNames[id]);
    row.addEventListener('click',()=>openWorkspaceApp(id));legend.push(row);
  }
  if(!legend.length)legend.push(node('p','hint','近期窗口没有操作记录。'));
  el('pulseLegend').replaceChildren(...legend);
  // 节奏条
  const buckets=new Array(stageRhythmBuckets).fill(0);
  for(const t of times)buckets[Math.min(stageRhythmBuckets-1,Math.floor((t-start)/span*stageRhythmBuckets))]++;
  const peak=Math.max(1,...buckets);
  buckets.forEach((count,index)=>{const height=count?Math.max(5,count/peak*58):2;el('pulseBar'+index).setAttribute('height',String(height));el('pulseBar'+index).setAttribute('y',String(64-height));el('pulseBar'+index).setAttribute('data-level',count?(count/peak>.66?'high':count/peak>.33?'mid':'low'):'none');});
}
function paintStage(snapshot){
  paintConstellation(snapshot);
  const key=JSON.stringify([snapshot.throughSeq,(snapshot.events||[]).length,stageOperations(snapshot).length,stageOperations(snapshot).at(-1)?.status]);
  if(key===stageKey)return;stageKey=key;
  paintPulse(snapshot);
}
function clearStage(){stageKey='';stageNoticeSeq=null;stageThoughtSeq=null;stageSelectedSeq=null;stageRiverMax=-1;el('pulseRiver').replaceChildren();el('pulseLegend').replaceChildren();el('homeRecent').replaceChildren();el('screenBanner').hidden=true;el('pulseOpen').hidden=true;}
/* 从星图或图例点开应用：切到屏幕镜头并打开对应应用。 */
function showLens(lens){el('workspaceLayout').setAttribute('data-mobile-view',lens);stageEach('[data-workspace-tab]',n=>n.setAttribute('aria-pressed',String(n.dataset.workspaceTab===lens)));}
stageEach('[data-star-app]',n=>{const open=()=>{showLens('device');openWorkspaceApp(n.dataset.starApp);};n.addEventListener('click',open);n.addEventListener('keydown',event=>{if(event.key==='Enter'||event.key===' '){event.preventDefault();open();}});});
el('pulseOpen').addEventListener('click',()=>{if(Number.isSafeInteger(stageSelectedSeq))run(()=>detail({seq:stageSelectedSeq},false,'get','timeline'));});
el('screenBanner').addEventListener('click',()=>{el('screenBanner').hidden=true;});
`;
