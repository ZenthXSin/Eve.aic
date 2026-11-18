const path = "/api/chat";
const limit = 4096;
const markerKey = "eve.studio.chat.unconfirmed-id.v1";
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const states = new Map([
  ["queued", "排队中"],
  ["processing", "处理中"],
  ["answered", "已回复"],
  ["failed", "处理失败"],
  ["unknown", "结果未知"],
  ["sent", "已发送"],
]);
const timeFormat = new Intl.DateTimeFormat("zh-CN", {
  month: "numeric",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
});

function makeId(crypto) {
  if (crypto.randomUUID) return crypto.randomUUID();
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 15) | 64;
  bytes[8] = (bytes[8] & 63) | 128;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${
    hex.slice(20)
  }`;
}

function checkDraft(raw) {
  if (!raw.trim()) return "请先输入消息内容。";
  if (raw.length > limit) return `每条消息最多 ${limit} 个字符。`;
  if (/[\uD800-\uDFFF]/u.test(raw) || /\p{Cc}/u.test(raw.replace(/[\t\r\n]/g, ""))) {
    return "请移除消息中无效的字符或控制字符。";
  }
  return "";
}

function publicMessage(item) {
  if (
    (item?.role !== "user" && item?.role !== "assistant") || typeof item.text !== "string" ||
    !Number.isSafeInteger(item.seq) || item.seq < 1 || typeof item.id !== "string" || !item.id ||
    !states.has(item.status)
  ) return null;
  const at = typeof item.at === "string" ? new Date(item.at) : null;
  return {
    key: item.id,
    id: item.id,
    role: item.role,
    text: item.text,
    seq: item.seq,
    at: at && !Number.isNaN(at.valueOf()) ? at : null,
    status: item.status,
  };
}

function readPage(data) {
  if (data?.mode !== "isolated" || !Array.isArray(data.messages)) throw new Error("chat-format");
  return {
    entries: data.messages.map(publicMessage).filter(Boolean),
    description: typeof data.description === "string"
      ? data.description
      : "仅限本 Web 会话，不共享主意识记忆，也不会调用工具或发送 QQ 消息。",
    before: Number.isSafeInteger(data.nextBefore) && data.nextBefore > 0 ? data.nextBefore : null,
  };
}

export async function mount(root, ctx) {
  if (ctx.signal?.aborted) return;
  const doc = root.ownerDocument, win = doc.defaultView;
  const page = doc.createElement("section");
  page.className = "eve-chat";
  page.lang = "zh-CN";
  page.setAttribute("aria-labelledby", "ech-title");
  page.innerHTML = `
    <link rel="stylesheet" href="/studio/chat.css">
    <header class="ech-head"><div><p class="ech-kicker">工作台 · 对话</p><h1 id="ech-title">和 Eve 聊天</h1>
      <p class="ech-intro">发送一条消息，在这里查看发送结果和回复。</p></div>
      <button type="button" class="ech-btn" data-ui="refresh">刷新记录</button></header>
    <details class="ech-about"><summary>这是怎样的私有会话？</summary><p data-ui="about">仅限本 Web 会话，不共享主意识记忆，也不会调用工具或发送 QQ 消息。</p></details>
    <section class="ech-card" aria-labelledby="ech-history-title">
      <div class="ech-bar"><h2 id="ech-history-title">对话记录</h2><span class="ech-badge" role="status" data-ui="sync">正在读取…</span></div>
      <div class="ech-pager"><button type="button" class="ech-btn" data-ui="older" hidden>加载更早记录</button></div>
      <div class="ech-viewport" data-ui="viewport" role="region" aria-label="可滚动的对话记录" tabindex="0">
        <p class="ech-empty" data-ui="empty">正在读取会话记录…</p><ol class="ech-log" data-ui="log" aria-label="消息列表"></ol></div>
      <p class="ech-hint" data-ui="historyNote" role="status"></p>
    </section>
    <section class="ech-recovery" data-ui="recovery" aria-labelledby="ech-recovery-title" hidden>
      <h2 id="ech-recovery-title">发送进度</h2><p data-ui="progress" role="status" aria-live="polite"></p>
      <div class="ech-actions"><button type="button" class="ech-btn" data-ui="retry" hidden>用原编号重试</button>
        <button type="button" class="ech-btn" data-ui="check">核对记录</button>
        <button type="button" class="ech-btn" data-ui="dismiss" hidden>结束核对</button>
        <button type="button" class="ech-btn" data-ui="cancel" hidden>继续核对</button></div></section>
    <form class="ech-compose ech-card" novalidate autocomplete="off">
      <div class="ech-bar"><label for="ech-message">消息内容</label><span class="ech-count" id="ech-count" data-ui="count">0 / 4096</span></div>
      <p class="ech-turn" data-ui="turn" role="status" hidden></p>
      <textarea id="ech-message" data-ui="input" rows="3" maxlength="4096" placeholder="写下想对 Eve 说的话…" dir="auto" spellcheck="false" aria-describedby="ech-count ech-help ech-error"></textarea>
      <p class="ech-error" id="ech-error" data-ui="error" hidden></p>
      <div class="ech-bottom"><p class="ech-hint" id="ech-help">回车换行；Ctrl / ⌘ + 回车发送。</p>
        <button class="ech-btn ech-primary" type="submit" data-ui="send" disabled>发送消息</button></div>
      <p class="ech-feedback" data-ui="feedback" role="status" aria-live="polite" aria-atomic="true"></p>
    </form>`;
  root.replaceChildren(page);

  const controller = new AbortController(), { signal } = controller;
  const ui = Object.fromEntries(
    Array.from(page.querySelectorAll("[data-ui]"), (node) => [node.dataset.ui, node]),
  );
  const form = page.querySelector("form");
  const nodes = new Map();
  let records = [], cursor = null, paginated = false, serverBusy = false, generation = 0;
  let timer = null, loading = "", queuedRead = false, loaded = false, failures = 0;
  let intent = null, posting = false, confirming = false, composing = false, draftEdits = 0;
  try {
    const id = win.sessionStorage.getItem(markerKey);
    if (id && uuid.test(id)) intent = { id, text: null, phase: "restored", edit: null };
  } catch {
    intent = null;
  }

  function remember(id) {
    try {
      win.sessionStorage.setItem(markerKey, id);
    } catch {
      return;
    }
  }
  function forget(id) {
    try {
      if (win.sessionStorage.getItem(markerKey) === id) win.sessionStorage.removeItem(markerKey);
    } catch {
      return;
    }
  }
  function dispose() {
    controller.abort();
    ctx.signal?.removeEventListener("abort", dispose);
    win.clearTimeout(timer);
    intent = null;
    records = [];
    nodes.clear();
    ui.input.value = "";
    page.remove();
  }
  ctx.signal?.addEventListener("abort", dispose, { once: true });
  function text(node, value) {
    if (node.textContent !== value) node.textContent = value;
  }
  function tell(message, tone = "") {
    text(ui.feedback, message);
    ui.feedback.dataset.tone = tone;
  }

  function controls() {
    const validation = checkDraft(ui.input.value);
    const error = ui.input.value.length > 0 && ui.input.value.trim() ? validation : "";
    text(ui.error, error);
    ui.error.hidden = !error;
    ui.input.setAttribute("aria-invalid", String(Boolean(error)));
    text(ui.count, `${ui.input.value.length} / ${limit}`);
    ui.send.disabled = Boolean(
      !loaded || serverBusy || composing || posting || intent || validation,
    );
    text(ui.send, posting ? "正在发送…" : "发送消息");
    ui.refresh.disabled = Boolean(loading);
    ui.older.hidden = !cursor;
    ui.older.disabled = Boolean(loading);
    ui.recovery.hidden = !intent;
    if (!intent) return;
    const uncertain = intent.phase === "uncertain" || intent.phase === "restored";
    ui.retry.hidden = !uncertain || !intent.text || confirming;
    ui.retry.disabled = posting || !loaded || serverBusy;
    ui.check.disabled = Boolean(loading);
    ui.dismiss.hidden = intent.phase === "sending" || posting;
    ui.cancel.hidden = !confirming;
    text(ui.dismiss, confirming ? "确认结束核对" : "结束核对");
    const progress = confirming
      ? "原请求可能已经送达。结束只会移除本页的核对标记，不会撤回或重发消息。"
      : {
        sending: "正在发送，请勿重复提交。你可以继续编辑输入框。",
        uncertain: "发送结果未确认。请先核对记录；重试只会提交原消息和原编号。",
        restored: "发现上次尚未核对的发送编号。消息原文未保存在浏览器中，不会自动重发。",
        conflict: "此编号已有不同的正文，不会覆盖或重发。请查看记录后结束核对。",
      }[intent.phase];
    text(ui.progress, progress);
  }

  function draw(older) {
    const view = ui.viewport;
    const top = view.scrollTop, height = view.scrollHeight;
    const atBottom = height - top - view.clientHeight < 44;
    const seen = new Set();
    let position = ui.log.firstElementChild;
    for (const record of records) {
      let row = nodes.get(record.key);
      if (!row) {
        const entry = doc.createElement("li"),
          meta = doc.createElement("div"),
          author = doc.createElement("strong");
        const time = doc.createElement("time"),
          state = doc.createElement("span"),
          body = doc.createElement("p");
        entry.className = `ech-entry ech-${record.role}`;
        meta.className = "ech-meta";
        state.className = "ech-state";
        body.className = "ech-text";
        body.dir = "auto";
        text(author, record.role === "user" ? "你" : "Eve");
        meta.append(author, time, state);
        entry.append(meta, body);
        row = { entry, time, state, body };
        nodes.set(record.key, row);
      }
      text(row.body, record.text);
      text(row.state, states.get(record.status));
      row.time.hidden = !record.at;
      if (record.at) {
        row.time.dateTime = record.at.toISOString();
        text(row.time, timeFormat.format(record.at));
      }
      if (row.entry !== position) ui.log.insertBefore(row.entry, position);
      position = row.entry.nextElementSibling;
      seen.add(record.key);
    }
    for (const [key, row] of nodes) {
      if (!seen.has(key)) {
        row.entry.remove();
        nodes.delete(key);
      }
    }
    ui.empty.hidden = records.length > 0;
    text(ui.empty, "还没有可展示的消息。可以从下方发起对话。");
    if (older) view.scrollTop = top + (view.scrollHeight - height);
    else if (atBottom || !loaded) view.scrollTop = view.scrollHeight;
  }

  function clearSentDraft(active) {
    if (
      active.edit === draftEdits && !composing && active.text !== null &&
      ui.input.value.trim() === active.text
    ) {
      ui.input.value = "";
      draftEdits++;
    }
  }
  function reconcile() {
    if (!intent) return;
    const match = records.find((record) => record.role === "user" && record.id === intent.id);
    if (!match) return;
    if (intent.text !== null && match.text !== intent.text) {
      intent.phase = "conflict";
      return;
    }
    clearSentDraft(intent);
    forget(intent.id);
    intent = null;
    confirming = false;
    tell("已在会话记录中确认这条消息。", "success");
  }
  function turnStatus() {
    const latest = records.findLast((record) => record.role === "user");
    serverBusy = latest?.status === "queued" || latest?.status === "processing";
    const message = {
      queued: "上一条已排队。Eve 会先处理当前动作和新通知；你可以先写下一条。",
      processing: "Eve 正在处理上一条消息。输入框仍可编辑。",
      failed: "上一条消息处理失败，没有可展示的回复；不会自动重发。",
      unknown: "上一条消息的处理结果未知；不会自动重发。",
    }[latest?.status] ?? "";
    text(ui.turn, message);
    ui.turn.hidden = !message;
  }
  function schedule() {
    win.clearTimeout(timer);
    if (doc.hidden || signal.aborted || loading || (!failures && !intent && !serverBusy)) return;
    const delay = failures ? Math.min(30000, 4000 * 2 ** Math.min(4, failures)) : 9000;
    timer = win.setTimeout(() => read(), delay);
  }

  async function read(older = false) {
    if (signal.aborted || (older && !cursor)) return;
    if (loading) {
      if (!older) queuedRead = true;
      return;
    }
    win.clearTimeout(timer);
    loading = older ? "older" : "latest";
    const before = cursor;
    const started = generation;
    controls();
    try {
      const result = readPage(
        await ctx.api(older ? `${path}?before=${before}` : path, undefined, { signal }),
      );
      if (signal.aborted) return;
      if (!older && generation !== started) {
        queuedRead = true;
        return;
      }
      const previous = new Map(records.map((record) => [record.key, record]));
      if (!older && !paginated) previous.clear();
      const incoming = new Map(result.entries.map((record) => [record.key, record]));
      const merged = older
        ? new Map([...incoming, ...previous])
        : new Map([...previous, ...incoming]);
      records = [...merged.values()];
      records.sort((left, right) => left.seq - right.seq);
      if (older || !paginated) cursor = older && result.before >= before ? null : result.before;
      if (older) paginated = true;
      draw(older);
      loaded = true;
      failures = 0;
      text(ui.about, result.description);
      text(ui.sync, "已同步");
      ui.sync.dataset.tone = "ok";
      text(ui.historyNote, older ? cursor ? "已载入更早的记录。" : "已到最早记录。" : "");
      reconcile();
      turnStatus();
    } catch {
      if (signal.aborted) return;
      failures++;
      text(ui.sync, "读取失败");
      ui.sync.dataset.tone = "error";
      if (!loaded) text(ui.empty, "暂时无法读取会话记录。");
      text(
        ui.historyNote,
        older ? "未能读取更早记录，可以重试。" : "连接或会话暂不可用。稍后会重试，也可手动刷新。",
      );
    } finally {
      loading = "";
      if (!signal.aborted) {
        controls();
        if (queuedRead && !doc.hidden) {
          queuedRead = false;
          void read();
        } else schedule();
      }
    }
  }

  async function post(active) {
    if (posting || signal.aborted || !active.text) return;
    posting = true;
    confirming = false;
    active.phase = "sending";
    controls();
    tell("");
    try {
      const result = await ctx.api(`${path}/send`, { id: active.id, text: active.text }, {
        signal,
      });
      if (signal.aborted || intent !== active) return;
      const receipt = publicMessage(result?.message);
      if (
        result?.mode !== "isolated" || receipt?.role !== "user" || receipt.id !== active.id ||
        receipt.text !== active.text
      ) {
        throw new Error("chat-receipt");
      }
      generation++;
      records = [...records.filter((record) => record.key !== receipt.key), receipt].sort((
        left,
        right,
      ) => left.seq - right.seq);
      draw(false);
      reconcile();
      turnStatus();
    } catch (error) {
      if (signal.aborted || intent !== active) return;
      const known = new Map([[429, "上一条私聊尚未处理。请等处理结束后再发送。"], [
        507,
        "会话存储已满，无法接收新消息。输入已保留。",
      ], [400, "服务端未接收消息。请检查输入后重试。"]]);
      if (error?.status === 409 || error?.code === "conflict") active.phase = "conflict";
      else if (known.has(error?.status)) {
        forget(active.id);
        intent = null;
        tell(known.get(error.status), "error");
      } else active.phase = "uncertain";
    } finally {
      posting = false;
      if (!signal.aborted) {
        controls();
        void read();
      }
    }
  }

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!loaded || serverBusy || posting || intent || composing || signal.aborted) return;
    const error = checkDraft(ui.input.value);
    if (error) {
      tell(error, "error");
      ui.input.focus();
      return;
    }
    let id;
    try {
      id = makeId(win.crypto);
    } catch {
      tell("无法生成安全的发送编号，未发送。", "error");
      return;
    }
    intent = { id, text: ui.input.value.trim(), edit: draftEdits, phase: "sending" };
    remember(id);
    void post(intent);
  }, { signal });
  ui.input.addEventListener("input", () => {
    draftEdits++;
    controls();
  }, { signal });
  ui.input.addEventListener("compositionstart", () => {
    composing = true;
    controls();
  }, { signal });
  ui.input.addEventListener("compositionend", () => {
    composing = false;
    controls();
  }, { signal });
  ui.input.addEventListener("keydown", (event) => {
    if (
      event.key !== "Enter" || (!event.ctrlKey && !event.metaKey) || event.repeat ||
      event.isComposing || composing || event.keyCode === 229
    ) return;
    event.preventDefault();
    if (!ui.send.disabled) form.requestSubmit();
  }, { signal });
  ui.refresh.addEventListener("click", () => read(), { signal });
  ui.older.addEventListener("click", () => read(true), { signal });
  ui.check.addEventListener("click", () => read(), { signal });
  ui.retry.addEventListener("click", () => {
    if (loaded && intent?.phase === "uncertain") void post(intent);
  }, { signal });
  ui.dismiss.addEventListener("click", () => {
    if (posting || !intent) return;
    if (confirming) {
      forget(intent.id);
      intent = null;
      confirming = false;
      tell("已结束核对，未重发或撤回任何消息。");
    } else confirming = true;
    controls();
  }, { signal });
  ui.cancel.addEventListener("click", () => {
    confirming = false;
    controls();
  }, { signal });
  doc.addEventListener("visibilitychange", () => {
    win.clearTimeout(timer);
    if (!doc.hidden) void read();
  }, { signal });
  win.addEventListener("online", () => {
    if (!doc.hidden) void read();
  }, { signal });
  controls();
  await read();
  return dispose;
}
