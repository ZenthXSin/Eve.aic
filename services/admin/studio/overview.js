const numbers = new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 });
const clocks = new Intl.DateTimeFormat("zh-CN", {
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hour12: false,
});
const valueText = (value) => Number.isFinite(value) ? numbers.format(value) : "未知";
const duration = (value) =>
  !Number.isFinite(value)
    ? "未知"
    : value > 0 && value < .01
    ? "< 0.01 ms"
    : `${numbers.format(value)} ms`;
const clock = (value) => Number.isFinite(value) ? clocks.format(value) : "时间未知";
const stages = new Map(Object.entries({
  pendingScanMs: "扫描待执行工具",
  noticesMs: "汇集通知",
  toolsListMs: "读取工具清单",
  recallMs: "记忆检索",
  selfContextMs: "读取自我便签",
  contextProjectionMs: "组装上下文",
  stateUpdateMs: "写入轮次状态",
}));
const eventNames = new Map(Object.entries({
  model_start: "开始模型轮次",
  model_response: "收到模型回复",
  model_failure: "模型请求失败",
  model_resume: "恢复模型请求",
  tool_start: "开始执行工具",
  tool_result: "收到工具结果",
  notice: "收到通知",
  perception: "写入感知",
  rest: "进入休息",
  rest_review: "检查休息条件",
  budget_wait: "等待预算",
  context_wait: "等待上下文处理",
  context_projection: "记录上下文投影",
  compression: "压缩上下文",
  upgrade_rollback: "记录升级回退",
  self_reflection: "记录自我反思",
}));
const modes = new Map(
  Object.entries({ active: "持有中", draining: "排空中", idle: "空闲", paused: "已暂停" }),
);
const pauseStates = new Map(Object.entries({
  paused: "已暂停",
  unpaused: "未暂停",
  inconsistent: "暂停标记不一致",
  unknown: "暂停状态未知",
}));
let mountNumber = 0;

function loadStyle() {
  const href = new URL("./overview.css", import.meta.url).href;
  if ([...document.querySelectorAll('link[rel="stylesheet"]')].some((link) => link.href === href)) {
    return;
  }
  const link = document.createElement("link");
  link.rel = "stylesheet";
  link.href = href;
  document.head.append(link);
}

function element(tag, content, className) {
  const node = document.createElement(tag);
  if (content != null) node.textContent = content;
  if (className) node.className = className;
  return node;
}

function timeNode(at) {
  const node = element("time", clock(at));
  if (Number.isFinite(at)) node.dateTime = new Date(at).toISOString();
  return node;
}

function svgNode(tag, attributes) {
  const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
  for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, String(value));
  return node;
}

function plot(host, points, format, baseline) {
  const stamps = points.map((point) => point.at), values = points.map((point) => point.value);
  const first = Math.min(...stamps), last = Math.max(...stamps);
  const low = baseline ? 0 : Math.min(...values), high = Math.max(...values);
  const svg = svgNode("svg", {
    viewBox: "0 0 600 124",
    preserveAspectRatio: "none",
    "aria-hidden": true,
  });
  for (const grid of [8, 62, 116]) {
    svg.append(svgNode("line", { x1: 5, x2: 595, y1: grid, y2: grid, class: "ov-gridline" }));
  }
  for (const point of points) {
    const cx = last === first ? 300 : 6 + 588 * (point.at - first) / (last - first);
    const cy = high === low ? 62 : 116 - 108 * (point.value - low) / (high - low);
    svg.append(svgNode("circle", { cx, cy, r: 3.6, class: "ov-point" }));
  }
  const scale = element("div", null, "ov-scale");
  scale.append(
    element(
      "span",
      high === low ? `纵轴恒值 ${format(low)}` : `纵轴 ${format(low)} — ${format(high)}`,
    ),
  );
  const range = element("div", null, "ov-range");
  range.append(timeNode(first));
  if (last !== first) range.append(timeNode(last));
  host.replaceChildren(scale, svg, range);
}

const markup = `
  <header class="ov-head">
    <div><p class="ov-kicker">运行监测</p><h1>运行概览</h1></div>
    <div class="ov-sync"><div class="ov-refresh-controls"><label><input type="checkbox" data-part="auto" checked> 自动刷新（15 秒）</label><button type="button" class="ov-button" data-part="refresh">刷新概览</button></div>
      <span data-part="stamp">尚未采样</span></div>
  </header>
  <p class="ov-message" data-part="message" role="status" aria-live="polite" aria-atomic="true"></p>
  <div class="ov-vitals" role="group" aria-label="运行摘要">
    <article><h2>执行服务</h2><strong data-part="available">等待读取</strong><p data-part="instance">实例未知</p></article>
    <article><h2>所有权</h2><strong data-part="mode">等待读取</strong><p data-part="permits">活动许可未知</p></article>
    <article><h2>意识记录水位</h2><strong data-part="watermark">等待读取</strong><p data-part="receipts">回执水位未知</p></article>
  </div>
  <section class="ov-panel ov-performance" aria-label="性能采样">
    <div class="ov-title-row"><div><p class="ov-kicker">时序观测</p><h2>性能采样</h2></div>
      <label class="ov-select">观察序列<select data-part="series">
        <optgroup label="事件计时"><option value="preparation">轮次准备耗时</option><option value="model">模型回执间隔</option><option value="tool">工具执行耗时</option></optgroup>
        <optgroup label="状态采样"><option value="throughSeq">意识记录水位</option><option value="receiptsThroughSeq">动作回执水位</option></optgroup>
      </select></label></div>
    <p class="ov-note" data-part="coverage">正在读取采样窗口。</p>
    <div class="ov-performance-grid"><div class="ov-chart-area">
      <div class="ov-readout"><span data-part="metricOneLabel">P50</span><strong data-part="metricOne">—</strong>
        <span data-part="metricTwoLabel">P95</span><strong data-part="metricTwo">—</strong><span class="ov-count" data-part="sampleCount">样本数未知</span></div>
      <figure><div class="ov-plot" data-part="plot"></div><figcaption data-part="caption">等待可验证的采样。</figcaption></figure>
      <details class="ov-values" data-part="details"><summary>查看本序列数值</summary><div data-part="values"></div></details>
    </div><aside class="ov-stages"><h3>准备步骤耗时</h3><p>按已上报样本的 P50 排序；并行步骤不可相加。</p><ol data-part="stages"></ol></aside></div>
  </section>
  <div class="ov-pair"><section class="ov-panel" aria-label="日预算"><p class="ov-kicker">Token 账本</p><h2>日预算</h2>
    <p class="ov-note" data-part="day">账期未知</p><p class="ov-budget-value"><strong data-part="remaining">—</strong> 安全余量</p>
    <meter data-part="budgetMeter" aria-label="安全计入量占日额度" hidden></meter><p class="ov-note" data-part="budgetRatio"></p>
    <dl class="ov-facts"><div><dt>日额度</dt><dd data-part="limit">未知</dd></div><div><dt>已知非缓存用量</dt><dd data-part="known">未知</dd></div>
    <div><dt>未知用量的预留</dt><dd data-part="reserved">未知</dd></div><div><dt>安全计入量（含预留）</dt><dd data-part="total">未知</dd></div></dl>
  </section><section class="ov-panel" aria-label="待处理项"><p class="ov-kicker">需要核对</p><h2>待处理项</h2>
    <dl class="ov-facts ov-pending"><div><dt>模型阻塞</dt><dd data-part="blocked">未知</dd></div><div><dt>所有权请求</dt><dd data-part="ownershipRequest">未知</dd></div>
      <div><dt>升级流程</dt><dd data-part="upgrade">未知</dd></div><div><dt>QQ 失败队首</dt><dd data-part="queue">未知</dd></div></dl>
    <p class="ov-note" data-part="queueStamp">只检查失败队首页，不代表所有消息的待办量。</p>
  </section></div>
  <div class="ov-pair"><section class="ov-panel" aria-label="近期活动"><p class="ov-kicker">意识时间线</p><h2>近期活动</h2>
    <p class="ov-note">最多列出窗口内最新 8 条，仅含事件类型和时间。</p><ol class="ov-activity" data-part="activity"></ol>
  </section><section class="ov-panel ov-actions" aria-label="运行控制及快捷入口"><p class="ov-kicker">操作台</p><h2>运行控制</h2>
    <p data-part="pauseState">正在核对监督器与预算的暂停标记。</p><button type="button" class="ov-button ov-control" data-part="control" disabled>暂停 / 恢复</button>
    <p class="ov-note" data-part="outcome" role="status"></p><nav class="ov-shortcuts" aria-label="快捷入口"><h3>快捷入口</h3>
      <a href="#chat" data-route="chat">打开对话</a><a href="#explore" data-route="explore">探索原始记录</a><a href="#settings" data-route="settings">检查设置</a></nav>
  </section></div>
  <dialog class="ov-dialog" data-part="dialog"><h2 data-part="dialogTitle"></h2><p data-part="dialogText"></p>
    <div><button type="button" class="ov-button" data-part="cancel" autofocus>取消</button><button type="button" class="ov-button ov-control" data-part="confirm"></button></div></dialog>`;

const seriesNotes = {
  preparation: "轮次起点上报的准备耗时。",
  model: "同一请求从起点到成功回复的记录时间差，包含框架开销，不是供应商纯耗时。",
  tool: "工具结果上报的执行耗时。",
  throughSeq: "管理后端被访问时观测到的意识记录序号，不是每秒事件数。",
  receiptsThroughSeq: "管理后端被访问时观测到的动作回执序号，不是每秒动作数。",
};

function showSeries(parts, data) {
  const key = parts.series.value, metric = data.performance[key];
  const points = metric
    ? metric.points
    : data.history.filter((point) => Number.isFinite(point[key]))
      .map((point) => ({ at: point.at, value: point[key] }));
  const format = metric ? duration : valueText, unavailable = metric && data.timeline.read !== "ok";
  parts.sampleCount.textContent = unavailable ? "样本数未知" : `${points.length} 个样本`;
  parts.metricOneLabel.textContent = metric ? "P50" : "首个观测";
  parts.metricTwoLabel.textContent = metric ? "P95" : "末个观测";
  parts.metricOne.textContent = metric ? duration(metric.p50) : valueText(points[0]?.value);
  parts.metricTwo.textContent = metric
    ? metric.p95 !== null ? duration(metric.p95) : unavailable ? "未知" : "不足 20 样本"
    : valueText(points.at(-1)?.value);
  const empty = unavailable
    ? "本次未读到时间线，耗时未知。"
    : "尚无本序列的有效样本；不按 0 绘图。";
  if (points.length) plot(parts.plot, points, format, !!metric);
  else parts.plot.replaceChildren(element("p", empty, "ov-empty"));
  parts.caption.textContent = `${seriesNotes[key]}${
    points.length === 1 ? "仅 1 点，不能判断趋势。" : "横轴为实际时间，不补点。"
  }`;
  if (!parts.details.open) {
    parts.values.replaceChildren();
    return;
  }
  if (!points.length) {
    parts.values.replaceChildren(element("p", empty));
    return;
  }
  const table = element("table");
  table.append(element("caption", `${parts.series.selectedOptions[0].textContent} · 新样本在前`));
  const header = table.createTHead().insertRow();
  for (const text of ["本地时间", metric ? "事件序号" : "状态采样", metric ? "耗时" : "水位"]) {
    const cell = element("th", text);
    cell.scope = "col";
    header.append(cell);
  }
  const body = table.createTBody();
  for (const point of points.toReversed()) {
    const row = body.insertRow();
    row.insertCell().append(timeNode(point.at));
    row.insertCell().textContent = metric ? `#${valueText(point.seq)}` : "观测值";
    row.insertCell().textContent = format(point.value);
  }
  parts.values.replaceChildren(table);
}

function showOverview(parts, data) {
  const { status, budget, timeline, pending } = data, queue = pending.qqFailures;
  parts.stamp.textContent = `采样 ${clock(data.sampledAt)} · 本地时间`;
  parts.available.textContent = status.available === true
    ? "可用"
    : status.available === false
    ? "未就绪"
    : "未知";
  parts.available.dataset.tone = status.available === true ? "ready" : "attention";
  parts.instance.textContent = `${status.instance ?? "实例未知"} · ${
    status.recovery ?? pauseStates.get(status.paused) ?? "暂停状态未知"
  }`;
  parts.mode.textContent = modes.get(status.ownershipMode) ?? status.ownershipMode ?? "未知";
  parts.permits.textContent = `活动许可 ${valueText(status.activePermits)}`;
  parts.watermark.textContent = valueText(status.throughSeq);
  parts.receipts.textContent = `动作回执水位 ${valueText(status.receiptsThroughSeq)}`;
  parts.coverage.textContent = timeline.read === "ok"
    ? timeline.count
      ? `最近连续记录 #${timeline.fromSeq}–#${timeline.throughSeq}，共 ${timeline.count} 条；${timeline.truncated} 条只有截断元数据。此窗口不是全量历史。`
      : "时间线水位为 0，还没有事件。"
    : timeline.read === "not_read"
    ? "缺少可用运行状态或时间线水位，本次未读取事件。"
    : "时间线读取不完整；本次不展示局部页为完整采样。";
  showSeries(parts, data);
  const measured = data.performance.stages.filter((stage) => stage.count > 0);
  parts.stages.replaceChildren(...measured.map((stage) => {
    const item = element("li"), name = element("span", stages.get(stage.key) ?? stage.key);
    const amount = element("span", `${duration(stage.p50)} · ${stage.count} 样本`);
    const bar = element("meter");
    bar.max = measured[0].p50 || 1;
    bar.value = stage.p50;
    bar.setAttribute("aria-hidden", "true");
    item.append(name, amount, bar);
    return item;
  }));
  if (!measured.length) parts.stages.append(element("li", "没有可计量的准备步骤，无法判断瓶颈。"));
  parts.day.textContent = `账期 ${budget.day ?? "未知"} · ${budget.timeZone ?? "时区未知"}`;
  for (
    const [part, field] of Object.entries({
      remaining: "safeRemaining",
      limit: "dailyTokenBudget",
      known: "knownNoncachedTokens",
      reserved: "unknownReservedTokens",
      total: "safetyTotal",
    })
  ) parts[part].textContent = valueText(budget[field]);
  const hasRatio = budget.dailyTokenBudget > 0 && Number.isFinite(budget.safetyTotal);
  parts.budgetMeter.hidden = !hasRatio;
  if (hasRatio) {
    parts.budgetMeter.max = budget.dailyTokenBudget;
    parts.budgetMeter.value = Math.min(budget.dailyTokenBudget, budget.safetyTotal);
  }
  parts.budgetRatio.textContent = hasRatio
    ? `安全计入量占日额度的 ${valueText(100 * budget.safetyTotal / budget.dailyTokenBudget)}%`
    : budget.dailyTokenBudget === 0
    ? "日额度为 0，不计算占比。"
    : "缺少额度或安全计入量，不计算占比。";
  parts.blocked.textContent = pending.modelBlocked === false
    ? "未阻塞"
    : pending.modelBlocked === true
    ? `${pending.requestId}${pending.httpStatus == null ? "" : ` · HTTP ${pending.httpStatus}`}`
    : "未知";
  parts.ownershipRequest.textContent = pending.ownershipRequest === true
    ? "等待处理"
    : pending.ownershipRequest === false
    ? "无待处理请求"
    : "未知";
  parts.upgrade.textContent = pending.upgrading === false
    ? "无进行中升级"
    : pending.upgradePhase ?? "未知";
  parts.queue.textContent = queue.read === "ok"
    ? queue.more ? `至少 ${queue.count} 项（有下一页）` : `${queue.count} 项（无下一页）`
    : queue.read === "not_read"
    ? "未读取"
    : "不可读取，数量未知";
  parts.queueStamp.textContent = `仅检查失败队首页${
    queue.sampledAt == null
      ? ""
      : ` · ${queue.read === "ok" ? "采样" : "尝试读取"} ${clock(queue.sampledAt)}`
  }。`;
  parts.activity.replaceChildren(...timeline.recent.map((event) => {
    const row = element("li");
    row.append(
      element("span", `#${event.seq}`, "ov-seq"),
      element(
        "span",
        `${eventNames.get(event.kind) ?? event.kind ?? "未知事件"}${
          event.truncated ? " · 已截断" : ""
        }`,
      ),
      timeNode(event.at),
    );
    return row;
  }));
  if (!timeline.recent.length) {
    parts.activity.append(
      element("li", timeline.read === "ok" ? "尚无记录。" : "活动未知，本次未取得完整窗口。"),
    );
  }
  parts.pauseState.textContent = `${pauseStates.get(status.paused) ?? "状态未知"} · 当前模型 ${
    status.model ?? "未知"
  }。`;
}

export async function mount(root, ctx) {
  if (ctx.signal.aborted) return;
  loadStyle();
  const pane = element("section", null, "ovw");
  pane.setAttribute("aria-label", "运行概览");
  pane.innerHTML = markup;
  root.replaceChildren(pane);
  const parts = Object.fromEntries(
    [...pane.querySelectorAll("[data-part]")].map((node) => [node.dataset.part, node]),
  );
  const prefix = `studio-ov-${++mountNumber}`;
  parts.dialog.setAttribute("aria-labelledby", parts.dialogTitle.id = `${prefix}-title`);
  parts.dialog.setAttribute("aria-describedby", parts.dialogText.id = `${prefix}-explain`);
  const life = new AbortController(), listen = { signal: life.signal };
  let data = null, reading = null, timer, observer, writing = false, failed = false;
  let verification = null, proposed = null, pageShown = true, inView = true;
  const active = () =>
    !life.signal.aborted && !ctx.signal.aborted && pageShown && inView &&
    document.visibilityState === "visible" && pane.isConnected && root.contains(pane) &&
    !pane.closest('[hidden], [inert], [aria-hidden="true"]') && (pane.checkVisibility?.() ?? true);
  const action = () =>
    failed || writing || verification || !data?.status.available
      ? null
      : data.status.paused === "paused"
      ? "resume"
      : data.status.paused === "unpaused"
      ? "pause"
      : null;
  const verb = (choice) => choice === "pause" ? "暂停运行" : "恢复运行";
  function buttons() {
    parts.refresh.disabled = !!reading || writing;
    parts.refresh.textContent = reading ? "读取中…" : "刷新概览";
    parts.confirm.disabled = parts.control.disabled = !!reading || !action();
    parts.control.textContent = writing
      ? "正在提交…"
      : verification
      ? "等待新快照"
      : action()
      ? verb(action())
      : "暂停 / 恢复不可用";
  }
  function stopRead() {
    clearTimeout(timer);
    reading?.abort();
    reading = null;
  }
  function closeDialog() {
    proposed = null;
    if (parts.dialog.open) parts.dialog.close();
  }
  function schedule() {
    clearTimeout(timer);
    if (active() && !writing && parts.auto.checked) timer = setTimeout(() => refresh(), 15_000);
  }
  async function refresh(manual = false) {
    if (!active() || reading || writing) return;
    clearTimeout(timer);
    const request = new AbortController();
    reading = request;
    buttons();
    try {
      const next = await ctx.api("/api/overview", undefined, { signal: request.signal });
      if (reading !== request || !active()) return;
      if (next?.schema !== 1 || !Number.isFinite(next.sampledAt)) throw new Error("概览格式不匹配");
      const previousAt = data?.sampledAt;
      showOverview(parts, next);
      data = next;
      failed = false;
      if (verification) {
        if (verification.firstAt === null) verification.firstAt = next.sampledAt;
        else if (next.sampledAt !== verification.firstAt && next.status.read === "ok") {
          const state = pauseStates.get(next.status.paused) ?? "状态未知";
          parts.outcome.textContent = next.status.paused === verification.expected
            ? `控制后的新采样：${state}。`
            : `新采样为「${state}」，与操作目标不同，请核对。`;
          verification = null;
        }
      }
      if (proposed && proposed !== action()) closeDialog();
      const notice = next.status.read !== "ok"
        ? "未取得执行服务状态；未知项不按正常或零值处理。"
        : manual
        ? previousAt === next.sampledAt ? "已检查，仍为共享缓存中的同一份采样。" : "已刷新采样。"
        : "";
      if (parts.message.textContent !== notice) parts.message.textContent = notice;
    } catch {
      if (reading !== request || !active()) return;
      failed = true;
      closeDialog();
      parts.message.textContent = data
        ? "刷新失败；下方保留上次读数，不代表当前状态。"
        : "概览读取失败，可手动重试。";
    } finally {
      if (reading === request) {
        reading = null;
        buttons();
        schedule();
      }
    }
  }
  async function submit(choice) {
    if (!choice || choice !== action() || reading || !active()) return;
    stopRead();
    writing = true;
    verification = { firstAt: null, expected: choice === "pause" ? "paused" : "unpaused" };
    closeDialog();
    buttons();
    let confirmed = false;
    try {
      const result = await ctx.api("/api/control", { action: choice, args: {} }, listen);
      const target = choice === "pause";
      confirmed = result?.supervisor?.paused === target &&
        result?.runtime?.budget?.paused === target;
    } catch {
      confirmed = false;
    }
    writing = false;
    if (life.signal.aborted) return;
    const message = confirmed
      ? `后端已确认${verb(choice)}；等待新采样核对，可手动刷新。`
      : "控制结果未确认，不会自动重发；请刷新核对新采样。";
    parts.outcome.textContent = message;
    buttons();
    if (active()) {
      ctx.notify(message);
      await refresh();
    }
  }
  parts.control.addEventListener("click", () => {
    const choice = action();
    if (!choice || reading || !active()) return;
    proposed = choice;
    parts.dialogTitle.textContent = `确认${verb(choice)}？`;
    const explanation = choice === "pause"
      ? "这会暂停整个实例的运行和新模型请求，影响所有管理员。"
      : "这会允许实例继续运行并消耗日预算，不会单独解除被拒绝的模型请求。";
    parts.dialogText.textContent = `${explanation} 取消不会更改状态。`;
    parts.confirm.textContent = `确认${verb(choice)}`;
    if (typeof parts.dialog.showModal === "function") {
      parts.dialog.showModal();
      parts.cancel.focus();
    } else if (globalThis.confirm(`${parts.dialogTitle.textContent}\n${explanation}`)) {
      void submit(choice);
    } else closeDialog();
  }, listen);
  parts.confirm.addEventListener("click", () => submit(proposed), listen);
  parts.cancel.addEventListener("click", closeDialog, listen);
  parts.dialog.addEventListener("cancel", () => {
    proposed = null;
  }, listen);
  parts.refresh.addEventListener("click", () => refresh(true), listen);
  parts.auto.addEventListener("change", () => {
    if (parts.auto.checked) void refresh();
    else clearTimeout(timer);
  }, listen);
  parts.series.addEventListener("change", () => {
    if (data) showSeries(parts, data);
  }, listen);
  parts.details.addEventListener("toggle", () => {
    if (data) showSeries(parts, data);
  }, listen);
  for (const link of pane.querySelectorAll("a[data-route]")) {
    link.addEventListener("click", (event) => {
      if (event.button || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
      event.preventDefault();
      ctx.navigate(link.dataset.route);
    }, listen);
  }
  function visibility() {
    if (active()) {
      if (!data || parts.auto.checked) void refresh();
    } else {
      stopRead();
      closeDialog();
      buttons();
    }
  }
  document.addEventListener("visibilitychange", visibility, listen);
  globalThis.addEventListener("pagehide", () => {
    pageShown = false;
    visibility();
  }, listen);
  globalThis.addEventListener("pageshow", () => {
    pageShown = true;
    visibility();
  }, listen);
  if (typeof IntersectionObserver === "function") {
    const bounds = pane.getBoundingClientRect();
    inView = bounds.bottom > 0 && bounds.top < globalThis.innerHeight;
    observer = new IntersectionObserver(([entry]) => {
      if (entry.isIntersecting !== inView) {
        inView = entry.isIntersecting;
        visibility();
      }
    });
    observer.observe(pane);
  }
  function dispose() {
    life.abort();
    stopRead();
    observer?.disconnect();
    closeDialog();
    pane.remove();
  }
  ctx.signal.addEventListener("abort", dispose, { ...listen, once: true });
  await refresh();
  return dispose;
}
