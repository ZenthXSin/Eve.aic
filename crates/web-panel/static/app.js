"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const phases = {
    Starting: "准备开始", Generating: "正在生成", Tools: "工具阶段", Committing: "正在保存",
    Cancelling: "取消收尾", Finished: "已结束", Blocked: "需要处理",
  };
  const turnStates = { Completed: "已完成", Pending: "未完成", Interrupted: "已中断", Failed: "失败" };
  const commits = { NotStarted: "尚未开始提交", Completed: "成功结果已保存", Failed: "失败记录已保存", Pending: "提交未完成，需处理", Unknown: "保存状态未知" };
  let token = "";
  let epoch = 0;
  let view = "tasks";
  let refreshFlight = null;
  let pollTimer = null;
  let taskLoading = false;
  let sessionLoading = false;
  let taskPages = 1;
  let tasks = [];
  let sessions = [];
  let taskCursor = null;
  let sessionCursor = null;
  let selectedTask = null;
  let selectedSession = null;
  let selectedSessionBefore = null;
  let sessionRequest = 0;
  let cancelConfirm = null;
  let cancelPending = null;
  let cancelFeedback = null;
  let judgments = [];
  let judgmentMeta = null;
  let judgmentBefore = null;
  let judgmentPages = 1;
  let judgmentLoading = false;
  let selectedJudgment = null;
  const controllers = new Set();
  const judgmentLabels = {
    result: { decided: "已判定", failed: "判断失败", dropped: "已丢弃" },
    outcome: { completed: "完成", unavailable: "不可用", protocol: "格式无效", timeout: "超时", panicked: "内部异常", dropped: "被丢弃" },
    intent: { supplement: "补充", correction: "纠正", answer: "回答", new_task: "新任务", cancel: "取消", continue: "继续", unrelated: "无关", ambiguous: "含混", pause: "暂停", resume: "恢复" },
    coverage: { complete: "完整", unsupported: "判断器不支持观察", overflow: "事件超出上限", invalid: "事件不完整", unreported: "未报告" },
    step: { rules: "明确命令规则", auxiliary: "辅助判断", primary: "主模型判断", classifier_call: "分类器调用", model_provider_call: "模型 Provider 调用" },
    fallback: { unavailable: "不可用", protocol: "格式无效", timeout: "超时", panicked: "内部异常", invalid_decision: "决定无效", ambiguous: "含混", low_confidence: "置信度低" },
    mode: { off: "仅明确命令规则", primary: "主模型自然判断", jev: "Jev 自然判断" },
  };

  class ApiError extends Error {
    constructor(message, silent = false) { super(message); this.silent = silent; }
  }

  function node(tag, className, text) {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined && text !== null) element.textContent = String(text);
    return element;
  }

  function button(text, className, action) {
    const element = node("button", `button ${className}`, text);
    element.type = "button";
    element.addEventListener("click", action);
    return element;
  }

  function text(value, fallback = "未知") {
    return typeof value === "string" && value.length ? value : fallback;
  }

  function count(value) { return Number.isSafeInteger(value) && value >= 0 ? value.toLocaleString("zh-CN") : "—"; }
  function sameSession(a, b) { return a && b && a.session_id === b.session_id && a.user_id === b.user_id; }
  function keyId(key) {
    return JSON.stringify([key.session.session_id, key.session.user_id, key.task_id, key.controller_epoch, key.generation]);
  }
  function validKey(key) {
    return key && key.session && typeof key.session.session_id === "string" && typeof key.session.user_id === "string" &&
      typeof key.task_id === "string" && Number.isSafeInteger(key.generation) && key.generation > 0 &&
      Array.isArray(key.controller_epoch) && key.controller_epoch.length === 16 &&
      key.controller_epoch.every((value) => Number.isInteger(value) && value >= 0 && value <= 255);
  }
  function validSession(key) { return key && typeof key.session_id === "string" && typeof key.user_id === "string"; }
  function canCancel(task) {
    return ["Starting", "Generating", "Tools", "Committing", "Cancelling"].includes(task.phase) &&
      task.cancel_requested === false && task.events_retired === false;
  }
  function cursorUrl(path, cursor) {
    const query = new URLSearchParams({ limit: "25" });
    if (cursor !== null) query.set("after", cursor);
    return `${path}?${query.toString()}`;
  }
  function validatePage(body, field, validate) {
    if (!body || !Array.isArray(body[field]) || !body[field].every(validate) ||
      !(body.next_cursor === null || typeof body.next_cursor === "string")) {
      throw new ApiError("服务返回的列表格式无效，请刷新重试。");
    }
    return body;
  }
  function showError(id, message) { $(id).textContent = message; $(id).hidden = !message; }
  function errorText(error) { return error instanceof ApiError ? error.message : "连接失败，请检查本地服务后重试。"; }
  function reportError(id, error) { if (token && !error.silent && error.name !== "AbortError") showError(id, errorText(error)); }

  async function api(path, body) {
    if (!token) throw new ApiError("请重新登录。", true);
    const requestEpoch = epoch;
    const controller = new AbortController();
    controllers.add(controller);
    const deadline = setTimeout(() => controller.abort(), 15000);
    try {
      const headers = { Authorization: `Bearer ${token}`, Accept: "application/json" };
      if (body !== undefined) headers["Content-Type"] = "application/json";
      const response = await fetch(path, {
        method: body === undefined ? "GET" : "POST", headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal, cache: "no-store", credentials: "omit", referrerPolicy: "no-referrer", redirect: "error",
      });
      if (epoch !== requestEpoch) throw new ApiError("访问已结束。", true);
      if (response.status === 401) {
        logout("访问令牌无效或已失效，请重新输入。");
        throw new ApiError("请重新登录。", true);
      }
      if (!response.ok) {
        const messages = {
          400: "请求无效，请刷新当前记录后重试。",
          403: "当前访问被拒绝。",
          404: "记录不存在，可能已变化，请刷新列表。",
          409: "任务状态已经变化，请刷新后重新选择。",
          413: "请求或记录超过面板的大小限制。",
          429: "请求较多，请稍后重试。",
          503: "当前服务暂不可用，请稍后刷新。",
        };
        throw new ApiError(messages[response.status] || "服务暂时无法完成请求，请稍后重试。");
      }
      const result = await response.json().catch(() => { throw new ApiError("服务返回的格式无效，请刷新重试。"); });
      if (epoch !== requestEpoch) throw new ApiError("访问已结束。", true);
      return result;
    } catch (error) {
      if (epoch !== requestEpoch) throw new ApiError("访问已结束。", true);
      if (error.name === "AbortError") throw new ApiError("请求超时，请检查服务后刷新重试。");
      throw error;
    } finally {
      clearTimeout(deadline);
      controllers.delete(controller);
    }
  }

  function emptyDetail(id, symbol, title, description) {
    const empty = node("div", "empty-detail");
    const icon = node("span", "empty-symbol", symbol);
    icon.setAttribute("aria-hidden", "true");
    empty.append(icon, node("h2", "", title), node("p", "", description));
    $(id).replaceChildren(empty);
  }

  function logout(message = "") {
    token = "";
    epoch += 1;
    sessionRequest += 1;
    for (const controller of controllers) controller.abort();
    controllers.clear();
    clearTimeout(pollTimer);
    pollTimer = null;
    refreshFlight = null;
    taskLoading = false;
    sessionLoading = false;
    taskPages = 1;
    tasks = [];
    sessions = [];
    taskCursor = null;
    sessionCursor = null;
    selectedTask = null;
    selectedSession = null;
    selectedSessionBefore = null;
    cancelConfirm = null;
    cancelPending = null;
    cancelFeedback = null;
    judgments = [];
    judgmentMeta = null;
    judgmentBefore = null;
    judgmentPages = 1;
    judgmentLoading = false;
    selectedJudgment = null;
    $("judgments-list").replaceChildren();
    $("judgment-detail").replaceChildren();
    $("judgment-count").textContent = "0";
    $("judgments-empty").textContent = "正在读取判断记录…";
    $("judgments-empty").hidden = false;
    $("judgments-more").hidden = true;
    showError("judgments-error", "");
    $("tasks-list").replaceChildren();
    $("sessions-list").replaceChildren();
    $("task-detail").replaceChildren();
    $("session-detail").replaceChildren();
    $("task-count").textContent = "0";
    $("session-count").textContent = "0";
    $("tasks-empty").textContent = "正在读取任务…";
    $("sessions-empty").textContent = "正在读取会话…";
    $("tasks-empty").hidden = false;
    $("sessions-empty").hidden = false;
    $("tasks-more").hidden = true;
    $("sessions-more").hidden = true;
    for (const id of ["global-error", "tasks-error", "sessions-error"]) showError(id, "");
    for (const name of ["received", "completed", "sent", "failed"]) $(`metric-${name}`).textContent = "—";
    $("channel-state").textContent = "等待状态";
    $("channel-detail").textContent = "正在读取当前实例";
    $("channel-dot").className = "status-dot neutral";
    $("started-at").textContent = "";
    $("console-view").hidden = true;
    $("login-view").hidden = false;
    $("access-token").value = "";
    $("login-submit").disabled = false;
    $("login-submit").textContent = "连接本地实例 →";
    showError("login-error", message);
    $("access-token").focus();
  }

  function renderStatus(body) {
    if (!body || !body.qq || typeof body.qq.ready !== "boolean" || typeof body.qq.closed !== "boolean") {
      throw new ApiError("通道状态格式无效，请刷新重试。");
    }
    const qq = body.qq;
    const failed = Boolean(qq.terminal_error);
    $("channel-state").textContent = failed ? "通道异常" : qq.closed ? "通道已关闭" : qq.ready ? "已就绪" : "正在连接";
    $("channel-dot").className = `status-dot${failed ? " bad" : qq.closed || !qq.ready ? " neutral" : ""}`;
    $("channel-detail").textContent = failed ? "通道报告终态错误，请检查本地服务。" : qq.closed ? "当前通道不再接收消息。" : qq.ready ? "已连接 QQ 消息通道。" : "等待通道完成连接。";
    for (const name of ["received", "completed", "sent", "failed"]) $(`metric-${name}`).textContent = count(qq[name]);
    const time = new Date(body.started_at_unix_ms);
    $("started-at").textContent = Number.isFinite(body.started_at_unix_ms) && !Number.isNaN(time.getTime()) ? `启动于 ${time.toLocaleString("zh-CN", { hour12: false })}` : "启动时间未知";
  }

  function badgeForTask(task) {
    const label = task.cancel_requested && task.phase !== "Finished" ? "已请求取消" : text(phases[task.phase], "状态未知");
    const style = task.phase === "Blocked" ? "bad" : task.phase === "Finished" ? "good" : task.cancel_requested ? "warning" : "active";
    return node("span", `badge ${style}`, label);
  }

  function renderTasks() {
    $("tasks-list").replaceChildren();
    for (const task of tasks) {
      const id = keyId(task.key);
      const item = node("button", `record-button${selectedTask === id ? " selected" : ""}`);
      item.type = "button";
      item.setAttribute("aria-pressed", String(selectedTask === id));
      const top = node("div", "record-topline");
      top.append(node("span", "record-name", task.key.task_id), badgeForTask(task));
      item.append(top, node("p", "record-meta", `${task.key.session.session_id} · ${task.key.session.user_id}`), node("p", "record-meta", `第 ${count(task.key.generation)} 代`));
      item.addEventListener("click", () => selectTask(id));
      $("tasks-list").append(item);
    }
    $("task-count").textContent = count(tasks.length);
    $("tasks-empty").hidden = tasks.length > 0;
    $("tasks-empty").textContent = taskLoading ? "正在读取任务…" : !$("tasks-error").hidden ? "暂时无法读取任务，请使用顶部刷新重试。" : "当前没有受管任务。新的 QQ 任务开始后会显示在这里。";
    $("tasks-more").hidden = taskCursor === null;
    $("tasks-more").disabled = taskLoading;
    renderTaskDetail();
  }

  function selectTask(id) {
    selectedTask = id;
    cancelConfirm = null;
    cancelFeedback = null;
    renderTasks();
  }

  function detailPair(label, value) {
    const pair = node("div");
    pair.append(node("dt", "", label), node("dd", "", value));
    return pair;
  }

  function renderTaskDetail() {
    if (!selectedTask) {
      emptyDetail("task-detail", "◎", "选择一个任务", "在这里查看执行状态，以及针对当前代际请求取消。");
      return;
    }
    const task = tasks.find((item) => keyId(item.key) === selectedTask);
    if (!task) {
      cancelConfirm = null;
      emptyDetail("task-detail", "◎", "任务状态已变化", "选中的代际已不在当前列表。请从列表重新选择任务。");
      return;
    }
    const heading = node("div", "detail-heading");
    heading.append(badgeForTask(task), node("h2", "", task.key.task_id), node("p", "", `会话 ${task.key.session.session_id} · 用户 ${task.key.session.user_id}`));
    const body = node("div", "detail-body");
    const grid = node("dl", "detail-grid");
    grid.append(
      detailPair("当前代际", `第 ${count(task.key.generation)} 代`),
      detailPair("执行阶段", text(phases[task.phase], "未知阶段")),
      detailPair("保存状态", text(commits[task.commit], "未知")),
      detailPair("已开始工具", task.started_tools === null || task.started_tools === undefined ? "未知" : count(task.started_tools)),
      detailPair("轮次 ID", task.turn_id === null || task.turn_id === undefined ? "尚未确定" : String(task.turn_id)),
      detailPair("取消请求", task.cancel_requested ? task.phase === "Finished" ? "已准入，本代已结束" : task.phase === "Blocked" ? "已准入，收尾需处理" : "已准入，等待收尾" : "未请求"),
    );
    body.append(grid);
    body.append(node("p", "detail-note", "取消仅请求当前代际停止。正在执行的操作可能仍需收尾，已经产生的工具副作用不会自动回滚。以刷新后的执行与保存状态为准。"));
    const cancellable = canCancel(task);
    if (!cancellable) cancelConfirm = null;
    if (cancellable && cancelConfirm !== selectedTask) {
      const actions = node("div", "detail-actions");
      const cancel = button(cancelPending === selectedTask ? "正在请求…" : "请求取消此代任务", "danger-outline", () => {
        cancelConfirm = selectedTask;
        cancelFeedback = null;
        renderTaskDetail();
      });
      cancel.disabled = cancelPending !== null;
      actions.append(cancel);
      body.append(actions);
    }
    if (cancellable && cancelConfirm === selectedTask) {
      const confirm = node("div", "confirm-box");
      confirm.append(node("p", "", `仅对“${task.key.task_id}”第 ${task.key.generation} 代请求取消，不会停止服务。`));
      const actions = node("div", "detail-actions");
      const yes = button(cancelPending === selectedTask ? "正在请求…" : "确认请求取消", "danger", () => requestCancellation(task.key));
      const no = button("返回", "secondary", () => { cancelConfirm = null; renderTaskDetail(); });
      yes.disabled = cancelPending !== null;
      no.disabled = cancelPending !== null;
      actions.append(yes, no);
      confirm.append(actions);
      body.append(confirm);
    }
    if (cancelFeedback && cancelFeedback.target === selectedTask) {
      const feedback = node("p", `action-feedback${cancelFeedback.error ? " error" : ""}`, cancelFeedback.message);
      feedback.setAttribute("role", cancelFeedback.error ? "alert" : "status");
      body.append(feedback);
    }
    $("task-detail").replaceChildren(heading, body);
  }

  async function requestCancellation(target) {
    const id = keyId(target);
    if (cancelPending !== null || cancelConfirm !== id || selectedTask !== id) return;
    const current = tasks.find((task) => keyId(task.key) === id);
    if (!current || !canCancel(current)) {
      cancelConfirm = null;
      renderTaskDetail();
      return;
    }
    const requestEpoch = epoch;
    cancelPending = id;
    renderTaskDetail();
    try {
      const body = await api("/api/cancel", { target });
      const messages = {
        requested: "取消请求已准入。任务可能仍在收尾，请查看刷新后的状态。",
        already_requested: "此代已收到取消请求，正在等待收尾。",
        already_finished: "此代已结束，没有发送新的取消请求。",
      };
      if (!body || !messages[body.status]) throw new ApiError("取消请求已发送，但响应格式无效；请刷新确认状态，不要重复操作。");
      cancelFeedback = { target: id, message: messages[body.status], error: false };
      cancelConfirm = null;
      if (body.status !== "already_finished") current.cancel_requested = true;
      else current.phase = "Finished";
    } catch (error) {
      if (epoch === requestEpoch && !error.silent) {
        cancelFeedback = { target: id, message: `${errorText(error)} 取消是否已准入需刷新确认。`, error: true };
        cancelConfirm = null;
      }
    } finally {
      if (epoch === requestEpoch) {
        cancelPending = null;
        renderTasks();
        void refresh();
      }
    }
  }

  async function loadTasks(append = false) {
    if (!token || taskLoading || (append && taskCursor === null)) return;
    const requestEpoch = epoch;
    taskLoading = true;
    $("tasks-more").disabled = true;
    try {
      let cursor = append ? taskCursor : null;
      const collected = [];
      const seenCursors = new Set();
      const pages = append ? 1 : taskPages;
      for (let page = 0; page < pages; page += 1) {
        const body = validatePage(await api(cursorUrl("/api/tasks", cursor)), "tasks", (task) => task && validKey(task.key));
        collected.push(...body.tasks);
        cursor = body.next_cursor;
        if (cursor === null) break;
        if (seenCursors.has(cursor)) throw new ApiError("任务分页游标重复，请刷新重试。");
        seenCursors.add(cursor);
      }
      if (epoch !== requestEpoch) return;
      const merged = append ? [...tasks, ...collected] : collected;
      tasks = [...new Map(merged.map((task) => [keyId(task.key), task])).values()];
      taskCursor = cursor;
      if (append) taskPages += 1;
      showError("tasks-error", "");
    } catch (error) {
      reportError("tasks-error", error);
      if (token && tasks.length === 0) $("tasks-empty").textContent = "暂时无法读取任务，请使用顶部刷新重试。";
      throw error;
    } finally {
      if (epoch === requestEpoch) {
        taskLoading = false;
        renderTasks();
      }
    }
  }

  function renderSessions() {
    $("sessions-list").replaceChildren();
    for (const session of sessions) {
      const selected = sameSession(selectedSession, session.key);
      const item = node("button", `record-button${selected ? " selected" : ""}`);
      item.type = "button";
      item.setAttribute("aria-pressed", String(selected));
      const top = node("div", "record-topline");
      top.append(node("span", "record-name", session.key.session_id), node("span", "badge", `${count(session.turn_count)} 轮`));
      item.append(top, node("p", "record-meta", `用户 ${session.key.user_id}`), node("p", "record-meta", `修订 ${count(session.revision)}`));
      item.addEventListener("click", () => { void loadSession(session.key); });
      $("sessions-list").append(item);
    }
    $("session-count").textContent = count(sessions.length);
    $("sessions-empty").hidden = sessions.length > 0;
    $("sessions-empty").textContent = sessionLoading ? "正在读取会话…" : !$("sessions-error").hidden ? "暂时无法读取会话，请使用顶部刷新重试。" : "当前没有保存的会话。";
    $("sessions-more").hidden = sessionCursor === null;
    $("sessions-more").disabled = sessionLoading;
  }

  async function loadSessions(append = false) {
    if (!token || sessionLoading || (append && sessionCursor === null)) return;
    const requestEpoch = epoch;
    sessionLoading = true;
    $("sessions-more").disabled = true;
    try {
      const body = validatePage(await api(cursorUrl("/api/sessions", append ? sessionCursor : null)), "sessions", (session) => session && validSession(session.key));
      if (epoch !== requestEpoch) return;
      const merged = append ? [...sessions, ...body.sessions] : body.sessions;
      sessions = [...new Map(merged.map((session) => [JSON.stringify([session.key.session_id, session.key.user_id]), session])).values()];
      sessionCursor = body.next_cursor;
      showError("sessions-error", "");
    } catch (error) { reportError("sessions-error", error); }
    finally { if (epoch === requestEpoch) { sessionLoading = false; renderSessions(); } }
  }

  async function loadSession(key, before = null) {
    const request = ++sessionRequest;
    const requestEpoch = epoch;
    selectedSession = { session_id: key.session_id, user_id: key.user_id };
    selectedSessionBefore = before;
    renderSessions();
    emptyDetail("session-detail", "▤", "正在读取会话", "正在读取已保存的轮次与当前任务状态。");
    try {
      const body = await api("/api/session", { key: selectedSession, before, limit: 25 });
      if (request !== sessionRequest || requestEpoch !== epoch) return;
      if (!body || !body.session || !sameSession(body.session.key, key) || !Array.isArray(body.session.turns) ||
        body.session.turns.length > 25 || !(body.session.next_before === null ||
          (Number.isSafeInteger(body.session.next_before) && body.session.next_before > 0 &&
            (before === null || body.session.next_before < before)))) {
        throw new ApiError("服务返回的会话格式无效，请重新选择会话。");
      }
      renderSessionDetail(body, before);
    } catch (error) {
      if (request === sessionRequest && requestEpoch === epoch && !error.silent) {
        emptyDetail("session-detail", "▤", "未能读取会话", errorText(error));
        const actions = node("div", "detail-actions");
        actions.append(button("重新读取", "secondary", () => { void loadSession(key, before); }));
        if (before !== null) actions.append(button("回到最新", "secondary", () => { void loadSession(key); }));
        $("session-detail").firstChild.append(actions);
      }
    }
  }

  function renderSessionDetail(body, before) {
    const session = body.session;
    const heading = node("div", "detail-heading");
    const pageLabel = before === null ? "最新记录" : "较早记录";
    heading.append(node("span", "section-kicker", `保存记录 · 修订 ${count(session.revision)}`), node("h2", "", session.key.session_id), node("p", "", `用户 ${session.key.user_id} · ${pageLabel}，本页 ${count(session.turns.length)} 轮 · 每页最多 25 轮`));
    const actions = node("div", "detail-actions");
    actions.append(button("刷新此页", "secondary", () => { void loadSession(session.key, before); }));
    if (session.next_before !== null) {
      actions.append(button("更早轮次", "secondary", () => { void loadSession(session.key, session.next_before); }));
    }
    if (before !== null) actions.append(button("回到最新", "secondary", () => { void loadSession(session.key); }));
    heading.append(actions);
    $("session-detail").replaceChildren(heading);
    if (body.control && validKey(body.control.key)) {
      const control = body.control;
      const box = node("div", "session-control");
      box.append(node("p", "", `读取时任务：${text(phases[control.phase], "未知")} · 第 ${count(control.key.generation)} 代`));
      box.append(button("查看任务", "secondary", () => {
        if (!tasks.some((task) => keyId(task.key) === keyId(control.key))) tasks.unshift(control);
        switchView("tasks");
        selectTask(keyId(control.key));
        void refresh();
      }));
      $("session-detail").append(box);
    }
    const conversation = node("div", "conversation");
    if (!session.turns.length) conversation.append(node("p", "empty-list", before === null ? "此会话暂时没有保存的轮次。" : "没有更早的已保存轮次，可回到最新记录。"));
    for (const turn of session.turns) {
      if (!turn || !turn.status || typeof turn.status.state !== "string") continue;
      const entry = node("article", "turn");
      const header = node("div", "turn-header");
      const state = turn.status.state;
      header.append(node("span", "", `轮次 ${String(turn.id ?? "未知")}`), node("span", `badge ${state === "Completed" ? "good" : state === "Failed" ? "bad" : "warning"}`, text(turnStates[state], "未知状态")));
      entry.append(header);
      if (typeof turn.input === "string") appendMessage(entry, "user", "用户", turn.input, turn.input_truncated === true);
      if (Array.isArray(turn.status.messages)) {
        for (const message of turn.status.messages) {
          if (message && (message.role === "assistant" || message.role === "Assistant") && typeof message.text === "string") {
            appendMessage(entry, "assistant", "Eve", message.text, message.truncated === true);
          }
        }
      }
      if (state !== "Completed") {
        const notes = { Pending: "该轮次尚未保存完成，不能据此确认执行结果。", Interrupted: "该轮次已中断，不会在此面板自动重放。", Failed: "该轮次执行或保存失败；此处不展示底层错误正文。" };
        entry.append(node("p", "turn-note", notes[state] || "此轮状态尚未识别，请检查本地运行记录。"));
      }
      conversation.append(entry);
    }
    $("session-detail").append(conversation);
  }

  function appendMessage(parent, role, label, content, truncated = false) {
    const message = node("div", `message ${role}`);
    message.append(node("span", "message-label", label), node("p", "message-text", content));
    if (truncated) message.append(node("p", "truncation-note", "仅显示前 8192 字节，完整记录保留。"));
    parent.append(message);
  }

  function label(map, value, fallback = "未知") { return typeof value === "string" && map[value] ? map[value] : fallback; }
  function micros(value) {
    if (!Number.isSafeInteger(value) || value < 0) return "未知";
    return value < 1000 ? `${value} 微秒` : `${(value / 1000).toLocaleString("zh-CN", { maximumFractionDigits: 1 })} 毫秒`;
  }
  function clock(value) {
    const time = new Date(value);
    return Number.isSafeInteger(value) && !Number.isNaN(time.getTime()) ? time.toLocaleTimeString("zh-CN", { hour12: false }) : "时间未知";
  }
  function validJudgment(item) {
    return item && Number.isSafeInteger(item.sequence) && item.sequence > 0 && typeof item.result === "string" &&
      Array.isArray(item.intents) && Array.isArray(item.steps) && Array.isArray(item.fallbacks) && typeof item.coverage === "string";
  }
  function judgmentBadge(item) {
    const style = item.result === "decided" ? "good" : item.result === "failed" ? "bad" : "warning";
    return node("span", `badge ${style}`, label(judgmentLabels.result, item.result));
  }
  function judgmentSummary(item) {
    if (item.result === "decided") return item.intents.map((intent) => label(judgmentLabels.intent, intent)).join("、") || "无意图";
    if (item.result === "failed") return `原因：${label(judgmentLabels.outcome, item.failure)}`;
    return "调用方在结束前丢弃，不代表远端已停止";
  }

  async function loadJudgments(more = false) {
    if (judgmentLoading) return;
    judgmentLoading = true;
    const requestEpoch = epoch;
    renderJudgments();
    try {
      const query = new URLSearchParams({ limit: "25" });
      if (more && judgmentBefore !== null) query.set("before", String(judgmentBefore));
      const body = await api(`/api/judgments?${query.toString()}`);
      if (epoch !== requestEpoch) return;
      if (!body || !Array.isArray(body.items) || !body.items.every(validJudgment) ||
        !(body.next_before === null || Number.isSafeInteger(body.next_before))) {
        throw new ApiError("判断记录格式无效，请刷新重试。");
      }
      judgments = more ? judgments.concat(body.items) : body.items;
      judgmentPages = more ? judgmentPages + 1 : 1;
      judgmentBefore = body.next_before;
      judgmentMeta = body;
      showError("judgments-error", "");
    } catch (error) {
      if (epoch !== requestEpoch) return;
      if (error instanceof ApiError && error.message.startsWith("当前服务暂不可用")) {
        showError("judgments-error", "当前实例未提供判断诊断。");
      } else reportError("judgments-error", error);
    } finally {
      if (epoch === requestEpoch) {
        judgmentLoading = false;
        renderJudgments();
      }
    }
  }

  function renderJudgments() {
    $("judgments-list").replaceChildren();
    for (const item of judgments) {
      const button = node("button", `record-button${selectedJudgment === item.sequence ? " selected" : ""}`);
      button.type = "button";
      button.setAttribute("aria-pressed", String(selectedJudgment === item.sequence));
      const top = node("div", "record-topline");
      top.append(node("span", "record-name", `第 ${count(item.sequence)} 次 · ${clock(item.finished_at_unix_ms)}`), judgmentBadge(item));
      button.append(top, node("p", "record-meta", judgmentSummary(item)), node("p", "record-meta", `本机耗时 ${micros(item.elapsed_micros)} · 观察${label(judgmentLabels.coverage, item.coverage)}`));
      button.addEventListener("click", () => { selectedJudgment = item.sequence; renderJudgments(); });
      $("judgments-list").append(button);
    }
    if (judgmentMeta) {
      const mode = label(judgmentLabels.mode, judgmentMeta.mode);
      const evicted = judgmentMeta.evicted > 0 ? `，已移出最早的 ${count(judgmentMeta.evicted)} 次` : "";
      $("judgments-scope").textContent = `${mode}。本进程共记录 ${count(judgmentMeta.recorded_total)} 次，最多保留最近 ${count(judgmentMeta.capacity)} 次${evicted}；只保存在内存中，重启后清空。`;
    }
    $("judgment-count").textContent = count(judgments.length);
    $("judgments-empty").hidden = judgments.length > 0;
    $("judgments-empty").textContent = judgmentLoading ? "正在读取判断记录…" : !$("judgments-error").hidden ? "暂时无法读取判断记录。" : "本次启动后还没有消息判断。明确命令或自然判断发生后会显示在这里。";
    $("judgments-more").hidden = judgmentBefore === null;
    $("judgments-more").disabled = judgmentLoading;
    renderJudgmentDetail();
  }

  function renderJudgmentDetail() {
    if (selectedJudgment === null) {
      emptyDetail("judgment-detail", "◇", "选择一次判断", "查看阶段、本地调用尝试、回退原因与耗时。不显示消息正文或身份。");
      return;
    }
    const item = judgments.find((entry) => entry.sequence === selectedJudgment);
    if (!item) {
      emptyDetail("judgment-detail", "◇", "记录已不在当前列表", "该判断可能已被更新的记录移出。请从列表重新选择。");
      return;
    }
    const heading = node("div", "detail-heading");
    heading.append(judgmentBadge(item), node("h2", "", `第 ${count(item.sequence)} 次判断`), node("p", "", `结束于 ${clock(item.finished_at_unix_ms)}（本机时钟）`));
    const body = node("div", "detail-body");
    const grid = node("dl", "detail-grid");
    grid.append(
      detailPair("结果", judgmentSummary(item)),
      detailPair("本机耗时", micros(item.elapsed_micros)),
      detailPair("观察覆盖", label(judgmentLabels.coverage, item.coverage)),
      detailPair("回退", item.fallbacks.length ? item.fallbacks.map((reason) => label(judgmentLabels.fallback, reason)).join("、") : "无"),
    );
    const counts = item.counts;
    if (counts) {
      grid.append(
        detailPair("阶段开始次数", `规则 ${count(counts.rules)} · 辅助 ${count(counts.auxiliary)} · 主模型 ${count(counts.primary)}`),
        detailPair("本地调用尝试", `分类器 ${count(counts.classifier_calls)} · 模型 Provider ${count(counts.model_provider_calls)}`),
      );
    } else {
      grid.append(detailPair("计数", "观察不完整，不显示计数"));
    }
    body.append(grid);
    const steps = node("ol", "judgment-steps");
    for (const step of item.steps) {
      const row = node("li", "judgment-step");
      const outcome = step.outcome === null ? "未观察到结束" : label(judgmentLabels.outcome, step.outcome);
      row.append(node("span", "record-name", label(judgmentLabels.step, step.name)), node("span", "record-meta", `${step.kind === "attempt" ? "调用尝试" : "阶段"} · ${outcome} · ${step.elapsed_micros === null ? "耗时未知" : micros(step.elapsed_micros)}`));
      steps.append(row);
    }
    if (item.steps.length) body.append(steps);
    body.append(node("p", "truncation-note", "调用尝试是本机适配器的调用次数，不代表网络请求、远端收到的请求、token 或费用；耗时是本机经过时间。"));
    $("judgment-detail").replaceChildren(heading, body);
  }

  function switchView(next) {
    view = next;
    $("tasks-view").hidden = next !== "tasks";
    $("sessions-view").hidden = next !== "sessions";
    $("judgments-view").hidden = next !== "judgments";
    $("page-title").textContent = next === "tasks" ? "任务状态" : next === "sessions" ? "会话记录" : "判断诊断";
    for (const item of document.querySelectorAll("[data-view]")) {
      const active = item.dataset.view === next;
      item.classList.toggle("active", active);
      if (active) item.setAttribute("aria-current", "page");
      else item.removeAttribute("aria-current");
    }
    if (next === "sessions" && sessions.length === 0) void loadSessions();
    if (next === "judgments") void loadJudgments();
  }

  async function refresh() {
    if (!token) return;
    if (refreshFlight) return refreshFlight;
    const requestEpoch = epoch;
    $("refresh-button").disabled = true;
    $("refresh-state").textContent = "正在刷新…";
    const flight = (async () => {
      const results = await Promise.allSettled([
        api("/api/status").then(renderStatus),
        loadTasks(),
        // 停在判断页且未加载更早记录时刷新首页；判断错误显示在本页，不影响全局状态。
        view === "judgments" && judgmentPages === 1 ? loadJudgments() : Promise.resolve(),
      ]);
      if (epoch !== requestEpoch) return;
      const failure = results.find((result) => result.status === "rejected");
      if (failure) {
        reportError("global-error", failure.reason);
        $("refresh-state").textContent = "刷新失败";
      } else {
        showError("global-error", "");
        $("refresh-state").textContent = `更新于 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`;
      }
    })();
    refreshFlight = flight;
    try { await flight; }
    finally {
      if (epoch === requestEpoch) {
        refreshFlight = null;
        $("refresh-button").disabled = false;
        schedulePoll();
      }
    }
  }

  function schedulePoll() {
    clearTimeout(pollTimer);
    pollTimer = null;
    if (!token || document.hidden) return;
    pollTimer = setTimeout(() => { void refresh(); }, 3000);
  }

  $("login-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if ($("login-submit").disabled) return;
    const entered = $("access-token").value;
    $("access-token").value = "";
    if (!/^[\x21-\x7e]{32,256}$/.test(entered)) {
      showError("login-error", "访问令牌须为 32 至 256 个可见 ASCII 字符，不包含空白。");
      return;
    }
    token = entered;
    epoch += 1;
    const requestEpoch = epoch;
    $("login-submit").disabled = true;
    $("login-submit").textContent = "正在连接…";
    showError("login-error", "");
    try {
      const body = await api("/api/status");
      if (epoch !== requestEpoch) return;
      renderStatus(body);
      $("login-view").hidden = true;
      $("console-view").hidden = false;
      emptyDetail("task-detail", "◎", "选择一个任务", "在这里查看执行状态，以及针对当前代际请求取消。");
      emptyDetail("session-detail", "▤", "选择一段会话", "仅展示用户与助手文字，工具参数和结果不在此展示。");
      switchView("tasks");
      void refresh();
    } catch (error) {
      if (epoch === requestEpoch && !error.silent) logout(errorText(error));
    } finally {
      if (epoch === requestEpoch) {
        $("login-submit").disabled = false;
        $("login-submit").textContent = "连接本地实例 →";
      }
    }
  });

  $("logout-button").addEventListener("click", () => logout());
  $("refresh-button").addEventListener("click", () => {
    void refresh();
    if (view === "sessions") {
      void loadSessions();
      if (selectedSession) void loadSession(selectedSession, selectedSessionBefore);
    }
    if (view === "judgments" && judgmentPages > 1) void loadJudgments();
  });
  $("tasks-more").addEventListener("click", () => { void loadTasks(true).catch(() => {}); });
  $("sessions-more").addEventListener("click", () => { void loadSessions(true); });
  $("judgments-more").addEventListener("click", () => { void loadJudgments(true); });
  for (const item of document.querySelectorAll("[data-view]")) item.addEventListener("click", () => switchView(item.dataset.view));
  document.addEventListener("visibilitychange", () => {
    clearTimeout(pollTimer);
    if (document.hidden) {
      if (token) $("refresh-state").textContent = "页面隐藏，已暂停刷新";
    } else if (token) void refresh();
  });
  window.addEventListener("pagehide", () => logout());
})();
