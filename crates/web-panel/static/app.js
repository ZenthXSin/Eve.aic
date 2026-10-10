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
  let goals = [];
  let goalMeta = null;
  let goalCursor = null;
  let goalPages = 1;
  let goalLoading = false;
  let selectedGoal = null;
  let goalRequest = 0;
  let memoryScopes = [];
  let memoryAfter = null;
  let memoryLoading = false;
  let selectedMemory = null;
  let memoryRequest = 0;
  let evidenceRequest = 0;
  let pluginsRequest = 0;
  let pluginPagesRequest = 0;
  let pluginPageRequest = 0;
  let pluginActionPending = false;
  let selectedPluginPage = null;
  let pluginPageDirty = false;
  let pluginPageSaving = false;
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
  const goalLabels = {
    status: { ready: "待执行", waiting: "等待中", executing: "执行中", completed: "执行记录已验证", cancelled: "已取消", blocked: "需要处理" },
    source: { user: "用户", environment: "环境观察", tool: "工具", inference: "推断", internal: "内部" },
    visibility: { public: "公开", user: "仅该用户", internal: "内部" },
    block: { interrupted: "执行中断", unknown_commit: "提交状态未知", feedback_save_failed: "反馈保存失败", invalidated: "输入已变化而失效" },
    commit: { not_started: "尚未开始提交", completed: "成功结果已保存", failed: "失败记录已保存", pending: "提交未完成，需处理", unknown: "保存状态未知" },
    event: { external_input: "外部输入", state_changed: "状态变化", drive_evaluated: "派生评估", agenda_selected: "议程选择", feedback: "执行反馈" },
    draft: { saved: "草稿已保存", not_saved: "没有已保存草稿", unavailable: "草稿正文不可用" },
  };
  const memoryLabels = {
    status: { confirmed: "已确认", revoked: "已撤销" },
    kind: { user_statement: "用户明确声明", completed_interaction: "已完成对话", missing: "来源记录缺失" },
  };
  const learningLabels = {
    job: { running: "进行中", completed: "已完成", failed: "失败", interrupted: "已中断" },
    failure: { provider: "模型服务失败", invalid_output: "输出格式无效", timeout: "超时", cancelled: "已取消" },
    action: { confirm: "新增确认", update: "更新偏好", defer: "暂缓自动保存", reject: "拒绝自动保存" },
    reason: {
      eligible: "满足来源门槛", evidence_threshold: "自评或真实来源数量未达门槛", expired: "已过首次确认期限",
      policy_denied: "替换策略未授权此动作", duplicate: "已有规范化等价偏好", revoked_conflict: "与用户撤销记录冲突，不自动恢复",
      manual_conflict: "与用户手动确认或更正冲突，保留手动选择", ambiguous_conflict: "存在多个或不明确的更新目标",
      stale_evidence: "候选来源修订未晚于当前偏好来源", explicit_revision_update: "明确偏好键已有更新的真实来源",
      already_linked: "候选已有可核对的保存历史",
    },
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
          409: "记录或配置已经变化，请刷新后重新操作。",
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
    goals = [];
    goalMeta = null;
    goalCursor = null;
    goalPages = 1;
    goalLoading = false;
    selectedGoal = null;
    goalRequest += 1;
    memoryScopes = [];
    memoryAfter = null;
    memoryLoading = false;
    selectedMemory = null;
    memoryRequest += 1;
    evidenceRequest += 1;
    pluginsRequest += 1;
    pluginPagesRequest += 1;
    pluginPageRequest += 1;
    selectedPluginPage = null;
    pluginPageDirty = false;
    pluginPageSaving = false;
    pluginActionPending = false;
    for (const id of ["plugins-list", "plugin-operations", "plugin-pages-list", "plugin-page-detail"]) $(id).replaceChildren();
    $("plugins-feedback").textContent = "";
    showError("plugins-error", "");
    showError("plugin-pages-error", "");
    $("memory-list").replaceChildren();
    $("memory-detail").replaceChildren();
    $("memory-count").textContent = "0";
    $("memory-empty").textContent = "正在读取记忆作用域…";
    $("memory-empty").hidden = false;
    $("memory-more").hidden = true;
    showError("memory-error", "");
    $("goals-list").replaceChildren();
    $("goal-detail").replaceChildren();
    $("goal-count").textContent = "0";
    $("goals-empty").textContent = "正在读取认知目标…";
    $("goals-empty").hidden = false;
    $("goals-more").hidden = true;
    showError("goals-error", "");
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

  function validGoal(item) {
    return item && typeof item.id === "string" && item.id.length > 0 && Number.isSafeInteger(item.revision) &&
      typeof item.status === "string" && typeof item.description === "string" && Number.isSafeInteger(item.reflections);
  }
  function goalBadge(status) {
    const style = status === "blocked" ? "bad" : status === "completed" ? "good" : status === "executing" ? "active" : status === "cancelled" ? "" : "warning";
    return node("span", `badge ${style}`.trim(), label(goalLabels.status, status));
  }
  function goalDate(value) {
    const time = new Date(value);
    return Number.isSafeInteger(value) && value > 0 && !Number.isNaN(time.getTime()) ? time.toLocaleString("zh-CN", { hour12: false }) : "时间未知";
  }
  function goalSource(item) { return `${label(goalLabels.source, item.source_kind)} · ${text(item.source_channel, "未知通道")}`; }
  function goalOwner(item) {
    const scope = label(goalLabels.visibility, item.visibility);
    return item.visibility === "user" && typeof item.owner === "string" ? `${scope}（${item.owner}）` : scope;
  }

  async function loadGoals(append = false) {
    if (!token || goalLoading || (append && goalCursor === null)) return;
    goalLoading = true;
    const requestEpoch = epoch;
    renderGoals();
    try {
      let cursor = append ? goalCursor : null;
      const collected = [];
      const seen = new Set();
      let meta = null;
      const pages = append ? 1 : goalPages;
      for (let page = 0; page < pages; page += 1) {
        const body = await api(cursorUrl("/api/goals", cursor));
        if (!body || !Array.isArray(body.items) || !body.items.every(validGoal) ||
          !(body.next_cursor === null || typeof body.next_cursor === "string") || !Number.isSafeInteger(body.revision)) {
          throw new ApiError("目标列表格式无效，请刷新重试。");
        }
        meta = body;
        collected.push(...body.items);
        cursor = body.next_cursor;
        if (cursor === null) break;
        if (seen.has(cursor)) throw new ApiError("目标分页游标重复，请刷新重试。");
        seen.add(cursor);
      }
      if (epoch !== requestEpoch) return;
      const merged = append ? [...goals, ...collected] : collected;
      goals = [...new Map(merged.map((item) => [item.id, item])).values()];
      goalCursor = cursor;
      goalMeta = meta;
      if (append) goalPages += 1;
      showError("goals-error", "");
    } catch (error) {
      if (epoch !== requestEpoch) return;
      if (error instanceof ApiError && error.message.startsWith("当前服务暂不可用")) {
        showError("goals-error", "当前实例未开启认知（需以 --cognition 启动），或认知状态暂时无法读取。");
      } else reportError("goals-error", error);
    } finally {
      if (epoch === requestEpoch) {
        goalLoading = false;
        renderGoals();
      }
    }
  }

  function renderGoals() {
    $("goals-list").replaceChildren();
    for (const item of goals) {
      const selected = selectedGoal === item.id;
      const entry = node("button", `record-button${selected ? " selected" : ""}`);
      entry.type = "button";
      entry.setAttribute("aria-pressed", String(selected));
      const top = node("div", "record-topline");
      top.append(node("span", "record-name", item.description + (item.description_truncated ? "…" : "")), goalBadge(item.status));
      const extra = item.reflection_of ? `反思子目标，父目标 ${item.reflection_of} 不在当前状态中` : `反思草稿 ${count(item.reflections)} 份`;
      entry.append(top, node("p", "record-meta", `${goalSource(item)} · 版本 ${count(item.revision)}`), node("p", "record-meta", `${goalOwner(item)} · ${extra}`));
      entry.addEventListener("click", () => { void loadGoal(item.id); });
      $("goals-list").append(entry);
    }
    if (goalMeta) $("goals-scope").textContent = `认知状态修订 ${count(goalMeta.revision)}。只读查看已保存的待办、状态与反思草稿；不能在此修改目标或触发规划。`;
    $("goal-count").textContent = count(goals.length);
    $("goals-empty").hidden = goals.length > 0;
    $("goals-empty").textContent = goalLoading ? "正在读取认知目标…" : !$("goals-error").hidden ? "暂时无法读取认知目标。" : "还没有保存的认知目标。用户通过 /goal 保存待办后会显示在这里。";
    $("goals-more").hidden = goalCursor === null;
    $("goals-more").disabled = goalLoading;
  }

  async function loadGoal(id) {
    const request = ++goalRequest;
    const requestEpoch = epoch;
    selectedGoal = id;
    renderGoals();
    emptyDetail("goal-detail", "◈", "正在读取目标", "正在读取目标状态、来源记录与反思草稿。");
    try {
      const body = await api(`/api/goal?${new URLSearchParams({ id }).toString()}`);
      if (request !== goalRequest || requestEpoch !== epoch) return;
      if (!body || !validGoal(body.goal) || body.goal.id !== id || !Array.isArray(body.reflections) || !Array.isArray(body.events) ||
        !body.budget || typeof body.reflection_check !== "string") {
        throw new ApiError("服务返回的目标格式无效，请重新选择目标。");
      }
      renderGoalDetail(body);
    } catch (error) {
      if (request === goalRequest && requestEpoch === epoch && !error.silent) {
        emptyDetail("goal-detail", "◈", "未能读取目标", errorText(error));
        const actions = node("div", "detail-actions");
        actions.append(button("重新读取", "secondary", () => { void loadGoal(id); }));
        $("goal-detail").firstChild.append(actions);
      }
    }
  }

  function renderGoalDetail(body) {
    const goal = body.goal;
    const heading = node("div", "detail-heading");
    const title = Array.from(goal.description);
    const short = title.length > 60 ? `${title.slice(0, 60).join("")}…` : goal.description;
    heading.append(goalBadge(goal.status), node("h2", "", short), node("p", "", `目标 ${goal.id} · 版本 ${count(goal.revision)} · 认知状态修订 ${count(body.revision)}`));
    const actions = node("div", "detail-actions");
    actions.append(button("刷新此目标", "secondary", () => { void loadGoal(goal.id); }));
    if (goal.reflection_of) actions.append(button("查看父目标", "secondary", () => { void loadGoal(goal.reflection_of); }));
    heading.append(actions);
    const detail = node("div", "detail-body");
    if (short !== goal.description) detail.append(node("p", "goal-text", goal.description));
    if (goal.description_truncated) detail.append(node("p", "truncation-note", "仅显示前 8192 字节，完整记录保留。"));
    const grid = node("dl", "detail-grid");
    const budget = body.budget;
    grid.append(
      detailPair("状态", label(goalLabels.status, goal.status)),
      detailPair("来源", goalSource(goal)),
      detailPair("可见范围", goalOwner(goal)),
      detailPair("优先级", count(goal.priority)),
      detailPair("预算", `模型请求 ≤ ${count(budget.max_model_requests)} · 工具 ≤ ${count(budget.max_tool_calls)} · 尝试 ≤ ${count(budget.max_attempts)} · 超时 ${count(budget.timeout_ms)} 毫秒`),
      detailPair("有效期", body.expires_at_ms === null ? "未设置" : goalDate(body.expires_at_ms)),
    );
    if (body.wait_reason) grid.append(detailPair("等待原因", body.wait_reason));
    if (body.block_reason) grid.append(detailPair("阻塞原因", label(goalLabels.block, body.block_reason)));
    if (body.execution) grid.append(detailPair("执行记录", `任务 ${body.execution.task_id} · 轮次 ${body.execution.turn_id === null ? "尚未确定" : String(body.execution.turn_id)}`));
    if (body.feedback) {
      grid.append(detailPair("执行反馈", `${label(goalLabels.commit, body.feedback.commit)} · ${body.feedback.verification_met ? "满足验证条件" : "未满足验证条件"}`));
    }
    detail.append(grid);
    detail.append(node("p", "detail-note", goal.reflection_of
      ? `这是目标 ${goal.reflection_of} 的反思子目标。完成只说明草稿已保存并通过结构校验，父目标与现实目标都没有因此完成。`
      : "“执行记录已验证”只表示该目标记录满足自身验证条件；反思草稿是模型建议，不代表现实目标完成。"));

    const drafts = node("section", "goal-section");
    drafts.append(node("h3", "", `反思草稿（${count(body.reflections.length)}）`));
    if (body.reflection_check === "inconsistent") {
      drafts.append(node("p", "inline-error", "当前修订的派生记录残缺或矛盾，没有草稿被标为当前；请检查本地认知状态。"));
    }
    if (!body.reflections.length) drafts.append(node("p", "muted", "此目标还没有反思子目标。"));
    for (const item of body.reflections) {
      if (!item || typeof item.goal_id !== "string") continue;
      const card = node("article", "reflection-card");
      const top = node("div", "record-topline");
      const version = Number.isSafeInteger(item.parent_revision) ? `对应目标版本 ${count(item.parent_revision)}` : "对应版本未知";
      top.append(node("span", "record-name", version), node("span", `badge ${item.current ? "good" : ""}`.trim(), item.current ? "当前草稿" : "历史草稿"));
      card.append(top, node("p", "record-meta", `${label(goalLabels.status, item.status)} · ${label(goalLabels.draft, item.draft_state)} · ${item.goal_id}`));
      if (item.draft && typeof item.draft.summary === "string") {
        card.append(node("p", "", item.draft.summary), node("p", "", `建议下一步：${text(item.draft.next_step, "未提供")}`),
          node("p", "record-meta", `需要用户补充信息：${item.draft.needs_user_input ? "是" : "否"}`));
      } else if (item.draft_state === "unavailable") {
        card.append(node("p", "turn-note", "子目标标记为已保存，但会话结果缺失或格式不一致；不显示正文，也不视为已保存。"));
      }
      if (!item.current) card.append(node("p", "truncation-note", "历史草稿不作为当前建议。"));
      drafts.append(card);
    }
    drafts.append(node("p", "truncation-note", "草稿是模型建议，尚未验证；原待办仍保持未完成，需用户确认后再行动。"));
    detail.append(drafts);

    const events = node("section", "goal-section");
    events.append(node("h3", "", `来源记录（最近 ${count(body.events.length)} 条）`));
    if (!body.events.length) events.append(node("p", "muted", "没有与此目标直接关联的来源记录。"));
    const list = node("ol", "judgment-steps");
    for (const item of body.events) {
      if (!item || typeof item.summary !== "string") continue;
      const row = node("li", "judgment-step");
      row.append(node("span", "record-name", label(goalLabels.event, item.kind)), node("span", "record-meta", `${label(goalLabels.source, item.source_kind)} · ${text(item.source_channel, "未知通道")} · ${goalDate(item.at_ms)}`));
      const summary = node("p", "event-summary", item.summary);
      row.append(summary);
      if (item.summary_truncated) row.append(node("p", "truncation-note", "仅显示前 2048 字节。"));
      list.append(row);
    }
    if (body.events.length) events.append(list);
    if (body.events_omitted > 0) events.append(node("p", "truncation-note", `另有 ${count(body.events_omitted)} 条更早记录未列出。`));
    events.append(node("p", "truncation-note", "用户反馈与文件片段是保存时的外部数据，未经验证。"));
    detail.append(events);
    $("goal-detail").replaceChildren(heading, detail);
  }

  function validScope(scope) {
    return scope && typeof scope.channel === "string" && typeof scope.session_id === "string" && typeof scope.user_id === "string";
  }
  function scopeId(scope) { return JSON.stringify([scope.channel, scope.session_id, scope.user_id]); }
  function memoryBadge(status) {
    return node("span", `badge ${status === "confirmed" ? "good" : ""}`.trim(), label(memoryLabels.status, status));
  }

  async function loadMemoryScopes(append = false) {
    if (!token || memoryLoading || (append && memoryAfter === null)) return;
    memoryLoading = true;
    const requestEpoch = epoch;
    renderMemoryScopes();
    try {
      const body = await api("/api/memory/scopes", append ? { after: memoryAfter, limit: 25 } : { limit: 25 });
      if (epoch !== requestEpoch) return;
      if (!body || !Array.isArray(body.items) || !body.items.every((item) => item && validScope(item.scope)) ||
        !(body.next_after === null || validScope(body.next_after))) {
        throw new ApiError("记忆作用域格式无效，请刷新重试。");
      }
      const merged = append ? [...memoryScopes, ...body.items] : body.items;
      memoryScopes = [...new Map(merged.map((item) => [scopeId(item.scope), item])).values()];
      memoryAfter = body.next_after;
      showError("memory-error", "");
    } catch (error) {
      if (epoch !== requestEpoch) return;
      if (error instanceof ApiError && error.message.startsWith("当前服务暂不可用")) {
        showError("memory-error", "当前实例未开启交互记忆（需以 --memory 启动），或记忆状态暂时无法读取。");
      } else reportError("memory-error", error);
    } finally {
      if (epoch === requestEpoch) {
        memoryLoading = false;
        renderMemoryScopes();
      }
    }
  }

  function renderMemoryScopes() {
    $("memory-list").replaceChildren();
    for (const item of memoryScopes) {
      const id = scopeId(item.scope);
      const selected = selectedMemory === id;
      const entry = node("button", `record-button${selected ? " selected" : ""}`);
      entry.type = "button";
      entry.setAttribute("aria-pressed", String(selected));
      const top = node("div", "record-topline");
      top.append(node("span", "record-name", `用户 ${item.scope.user_id}`), node("span", "badge", `${count(item.confirmed)} 条生效`));
      entry.append(top, node("p", "record-meta", `${item.scope.channel} · 会话 ${item.scope.session_id}`),
        node("p", "record-meta", `已撤销 ${count(item.revoked)} · 来源 ${count(item.evidence)} · 修订 ${count(item.revision)}`));
      entry.addEventListener("click", () => { void loadMemory(item.scope); });
      $("memory-list").append(entry);
    }
    $("memory-count").textContent = count(memoryScopes.length);
    $("memory-empty").hidden = memoryScopes.length > 0;
    $("memory-empty").textContent = memoryLoading ? "正在读取记忆作用域…" : !$("memory-error").hidden ? "暂时无法读取记忆。" : "还没有保存的记忆。用户通过 /remember 保存偏好或完成对话后会显示在这里。";
    $("memory-more").hidden = memoryAfter === null;
    $("memory-more").disabled = memoryLoading;
  }

  async function loadMemory(scope) {
    const request = ++memoryRequest;
    evidenceRequest += 1;
    const requestEpoch = epoch;
    selectedMemory = scopeId(scope);
    renderMemoryScopes();
    emptyDetail("memory-detail", "✦", "正在读取记忆", "正在读取偏好与版本历史。");
    try {
      const body = await api("/api/memory/scope", { scope });
      if (request !== memoryRequest || requestEpoch !== epoch) return;
      if (!body || !validScope(body.scope) || scopeId(body.scope) !== scopeId(scope) || !Array.isArray(body.preferences)) {
        throw new ApiError("服务返回的记忆格式无效，请重新选择。");
      }
      renderMemoryDetail(body);
    } catch (error) {
      if (request === memoryRequest && requestEpoch === epoch && !error.silent) {
        emptyDetail("memory-detail", "✦", "未能读取记忆", errorText(error));
        const actions = node("div", "detail-actions");
        actions.append(button("重新读取", "secondary", () => { void loadMemory(scope); }));
        $("memory-detail").firstChild.append(actions);
      }
    }
  }

  function renderMemoryDetail(body) {
    const heading = node("div", "detail-heading");
    const effective = body.preferences.filter((item) => item && item.effective).length;
    heading.append(node("span", "section-kicker", `作用域修订 ${count(body.revision)}`), node("h2", "", `用户 ${body.scope.user_id}`),
      node("p", "", `${body.scope.channel} · 会话 ${body.scope.session_id} · 偏好 ${count(body.preferences.length)} 条，生效 ${count(effective)} 条 · 来源 ${count(body.evidence)} 条`));
    const actions = node("div", "detail-actions");
    actions.append(button("刷新此作用域", "secondary", () => { void loadMemory(body.scope); }));
    heading.append(actions);
    const detail = node("div", "detail-body");
    detail.append(node("p", "detail-note", "只有“已确认”且为最新版本的偏好会进入对话上下文。撤销和更正都保留历史与来源，不删除记录。"));
    if (!body.preferences.length) detail.append(node("p", "muted", "此作用域还没有偏好，只有交互来源。"));
    const evidenceBox = node("section", "goal-section");
    evidenceBox.id = "memory-evidence";
    for (const item of body.preferences) {
      if (!item || typeof item.id !== "string" || !Array.isArray(item.history)) continue;
      const card = node("article", "reflection-card");
      const top = node("div", "record-topline");
      top.append(node("span", "record-name", item.id), memoryBadge(item.status));
      card.append(top, node("p", "", item.text));
      if (item.text_truncated) card.append(node("p", "truncation-note", "仅显示前 4096 字节。"));
      card.append(node("p", "record-meta", item.effective ? `版本 ${count(item.revision)} · 当前生效` : `版本 ${count(item.revision)} · 已撤销，不再进入对话上下文`));
      const versions = node("ol", "judgment-steps");
      for (const version of item.history) {
        if (!version || typeof version.evidence_id !== "string") continue;
        const row = node("li", "judgment-step");
        row.append(node("span", "record-name", `版本 ${count(version.revision)}${version.current ? "（最新）" : ""} · ${label(memoryLabels.status, version.status)}`),
          node("span", "record-meta", `${label(memoryLabels.kind, version.evidence_kind)} · ${goalDate(version.at_ms)}`));
        row.append(node("p", "event-summary", version.text));
        if (version.text_truncated) row.append(node("p", "truncation-note", "仅显示前 512 字节。"));
        if (version.evidence_kind !== "missing") {
          const open = button("查看来源", "secondary", () => { void loadEvidence(body.scope, version.evidence_id); });
          open.setAttribute("aria-label", `查看版本 ${version.revision} 的来源`);
          row.append(open);
        }
        versions.append(row);
      }
      card.append(versions);
      detail.append(card);
    }
    const learningBox = node("section", "goal-section");
    learningBox.id = "memory-learning";
    learningBox.append(node("h3", "", "学习候选"), node("p", "muted", "正在读取偏好提炼记录…"));
    const autonomyBox = node("section", "goal-section");
    autonomyBox.id = "memory-autonomy";
    autonomyBox.append(node("h3", "", "自主学习"), node("p", "muted", "正在读取兴趣、知识、实践、技能与邀请…"));
    detail.append(learningBox, autonomyBox, evidenceBox);
    $("memory-detail").replaceChildren(heading, detail);
    void loadLearning(body.scope, memoryRequest);
    void loadAutonomy(body.scope, memoryRequest);
  }

  const autonomyLabels = {
    interest: { active: "进行中", withdrawn: "已撤回" },
    practice: { running: "运行中", verified: "已验证", unverified: "未验证", not_applicable: "不适用", failed: "失败", interrupted: "中断" },
    invitation: { composing: "撰写中", pending: "等待时机", delivering: "发送中", delivered: "已送达", unknown: "结果未知", cancelled: "已取消", failed: "失败", interrupted: "中断" },
    feedback: { request: "提出了想法", interested: "有兴趣", declined: "不需要", bad_timing: "当时不方便", unrelated: "没有回应" },
  };

  async function loadAutonomy(scope, request) {
    const requestEpoch = epoch;
    try {
      const body = await api("/api/memory/autonomy", { scope });
      if (request !== memoryRequest || requestEpoch !== epoch || !$("memory-autonomy")) return;
      if (!body || !["interests", "knowledge", "practice", "skills", "invitations"].every((key) => Array.isArray(body[key]))) {
        throw new ApiError("自主学习记录格式无效，请刷新重试。");
      }
      renderAutonomy(body);
    } catch (error) {
      if (request !== memoryRequest || requestEpoch !== epoch || error.silent || !$("memory-autonomy")) return;
      const unavailable = error instanceof ApiError && error.message.startsWith("当前服务暂不可用");
      $("memory-autonomy").replaceChildren(node("h3", "", "自主学习"),
        node("p", unavailable ? "muted" : "inline-error", unavailable ? "未开启兴趣学习（需以 --interest-learning 启动），或记录暂时无法读取。" : errorText(error)));
    }
  }

  function renderAutonomy(body) {
    const box = $("memory-autonomy");
    const parts = [node("h3", "", "自主学习")];
    const list = (title, rows) => {
      parts.push(node("h4", "", title));
      if (!rows.length) { parts.push(node("p", "muted", "暂无记录。")); return; }
      const container = node("div", "record-list");
      for (const [name, meta] of rows) {
        const row = node("div", "record-row");
        row.append(node("span", "record-name", name), node("span", "record-meta", meta));
        container.append(row);
      }
      parts.push(container);
    };
    list("兴趣", body.interests.map((item) => [`${text(item.topic)}（${label(autonomyLabels.interest, item.status)}）`,
      `原话：${item.quotes.map((quote) => `“${text(quote)}”`).join("；") || "—"} · ${goalDate(item.updated_at_ms)}`]));
    list("领域知识", body.knowledge.map((item) => [text(item.statement),
      `${item.source_quoted ? "附来源原文" : "未验证推测"}${item.version ? ` · 版本 ${text(item.version)}` : ""}${item.url ? ` · ${text(item.url)}` : ""}`]));
    list("实践", body.practice.map((item) => [`${item.follow_up ? "后续创作" : "学习目标"} · ${label(autonomyLabels.practice, item.status)}`,
      `尝试 ${count(item.attempts)} 次 · 探测 ${count(item.probes_passed)}/${count(item.probes_total)} 通过${item.runtime_version ? ` · ${text(item.runtime_version)}` : ""} · ${goalDate(item.started_at_ms)}`]));
    list("技能", body.skills.map((item) => [`${text(item.name)}${item.enabled ? `（启用第 ${count(item.enabled)} 版）` : "（已停用）"}`,
      `${count(item.versions)} 个版本 · 后台调用 ${count(item.task_verified)}/${count(item.task_invocations)} 通过 · 对话调用 ${count(item.tool_verified)}/${count(item.tool_calls)} 通过`]));
    list("主动邀请", body.invitations.map((item) => [`${label(autonomyLabels.invitation, item.status)}${item.feedback ? ` · 回应：${label(autonomyLabels.feedback, item.feedback)}` : ""}`,
      `${item.text ? text(item.text) : "—"}${item.quote ? ` · 原话“${text(item.quote)}”` : ""}${item.delivered_at_ms ? ` · ${goalDate(item.delivered_at_ms)}` : ""}`]));
    box.replaceChildren(...parts);
  }

  async function loadLearning(scope, request) {
    const requestEpoch = epoch;
    try {
      const body = await api("/api/memory/learning", { scope });
      if (request !== memoryRequest || requestEpoch !== epoch || !$("memory-learning")) return;
      if (!body || !Array.isArray(body.jobs) || !Array.isArray(body.candidates) || typeof body.autonomous !== "boolean") {
        throw new ApiError("学习记录格式无效，请刷新重试。");
      }
      renderLearning(body);
    } catch (error) {
      if (request !== memoryRequest || requestEpoch !== epoch || error.silent || !$("memory-learning")) return;
      const unavailable = error instanceof ApiError && error.message.startsWith("当前服务暂不可用");
      $("memory-learning").replaceChildren(node("h3", "", "学习候选"),
        node("p", unavailable ? "muted" : "inline-error", unavailable ? "未开启偏好提炼（需以 --memory-learning 或 --self-learning 启动），或学习记录暂时无法读取、核对。" : errorText(error)));
    }
  }

  function renderLearning(body) {
    const box = $("memory-learning");
    box.replaceChildren(node("h3", "", `学习候选（${count(body.candidates.length)}）`));
    box.append(node("p", "detail-note", body.autonomous
      ? "自主学习：宿主按证据策略自动确认候选；与用户手动更正、撤销冲突时保留手动选择。"
      : "手动模式：候选需用户在 QQ 中发送 /accept-memory 候选ID 确认后才会保存。"));
    const tally = {};
    for (const job of body.jobs) if (job && typeof job.status === "string") tally[job.status] = (tally[job.status] || 0) + 1;
    const failures = body.jobs.filter((job) => job && job.failure).map((job) => label(learningLabels.failure, job.failure));
    const parts = Object.entries(tally).map(([status, value]) => `${label(learningLabels.job, status)} ${count(value)}`);
    box.append(node("p", "record-meta", `提炼批次 ${count(body.jobs.length)} 次${parts.length ? `：${parts.join("、")}` : ""}${failures.length ? `（失败原因：${failures.join("、")}）` : ""} · 学习决策共 ${count(body.decisions_total)} 条`));
    if (!body.candidates.length) box.append(node("p", "muted", "还没有提炼出候选偏好。"));
    for (const item of body.candidates) {
      if (!item || typeof item.id !== "string" || !Array.isArray(item.decisions)) continue;
      const card = node("article", "reflection-card");
      const top = node("div", "record-topline");
      const saved = item.saved && typeof item.saved.preference_id === "string" ? item.saved : null;
      const state = saved ? (saved.effective ? "已保存，生效中" : "已保存，后被撤销") : item.expired ? "已过期，未保存" : "待确认";
      top.append(node("span", "record-name", item.id), node("span", `badge ${saved ? (saved.effective ? "good" : "") : item.expired ? "" : "warning"}`.trim(), state));
      card.append(top, node("p", "", item.text));
      card.append(node("p", "record-meta", `模型自评 ${count(item.confidence)}（不是校准概率）· 引用对话 ${count(item.evidence_ids.length)} 条 · 生成于 ${goalDate(item.created_at_ms)} · 确认期限 ${goalDate(item.expires_at_ms)}`));
      card.append(node("p", "record-meta", saved ? `实际保存：偏好 ${saved.preference_id}，当前版本 ${count(saved.revision)}（按记忆历史核对）` : "实际保存：记忆历史中没有该候选对应的确认或更新。"));
      if (item.decisions.length) {
        const list = node("ol", "judgment-steps");
        for (const decision of item.decisions) {
          if (!decision || typeof decision.action !== "string") continue;
          const target = decision.action === "update" && typeof decision.update_preference === "string" ? `（目标 ${decision.update_preference} 版本 ${count(decision.update_revision)}）` : "";
          const row = node("li", "judgment-step");
          row.append(node("span", "record-name", `#${count(decision.sequence)} ${label(learningLabels.action, decision.action)}${target}`),
            node("span", "record-meta", `${label(learningLabels.reason, decision.reason)} · 策略 ${text(decision.policy_version)} · 读取记忆版本 ${count(decision.memory_revision)} · ${goalDate(decision.at_ms)}`));
          list.append(row);
        }
        card.append(list);
        if (item.decisions_total > item.decisions.length) card.append(node("p", "truncation-note", `另有 ${count(item.decisions_total - item.decisions.length)} 条更早决策未列出。`));
      } else {
        card.append(node("p", "muted", "尚无自动学习决策；手动确认不会伪造自动决策。"));
      }
      box.append(card);
    }
    box.append(node("p", "truncation-note", "决策记录是提交前的意图，是否真正保存以记忆历史为准；候选正文是模型提炼结果，不等于用户已确认。"));
  }

  async function loadEvidence(scope, id) {
    const request = ++evidenceRequest;
    const requestEpoch = epoch;
    const box = $("memory-evidence");
    if (!box) return;
    box.replaceChildren(node("h3", "", "来源"), node("p", "muted", "正在读取来源…"));
    box.scrollIntoView({ block: "nearest" });
    try {
      const body = await api("/api/memory/evidence", { scope, id });
      if (request !== evidenceRequest || requestEpoch !== epoch || !$("memory-evidence")) return;
      if (!body || body.id !== id || typeof body.user_text !== "string" || !Array.isArray(body.references)) {
        throw new ApiError("来源格式无效，请重新打开。");
      }
      const target = $("memory-evidence");
      target.replaceChildren(node("h3", "", `来源 ${body.id}`),
        node("p", "record-meta", `${label(memoryLabels.kind, body.kind)} · ${goalDate(body.at_ms)}${body.turn_id === null ? "" : ` · 对话轮次 ${body.turn_id}`}`));
      const conversation = node("div", "conversation");
      appendMessage(conversation, "user", body.kind === "user_statement" ? "用户声明" : "用户输入", body.user_text);
      if (body.user_text_truncated) conversation.append(node("p", "truncation-note", "仅显示前 2048 字节，完整记录保留。"));
      if (typeof body.assistant_text === "string") {
        appendMessage(conversation, "assistant", "Eve（当时回复）", body.assistant_text);
        if (body.assistant_text_truncated) conversation.append(node("p", "truncation-note", "仅显示前 2048 字节，完整记录保留。"));
      }
      target.append(conversation);
      const refs = body.references.map((ref) => `${ref.preference_id} 版本 ${ref.revision}${ref.effective ? "（生效）" : ref.current ? "（最新，未生效）" : "（历史）"}`);
      target.append(node("p", "record-meta", `引用此来源的偏好版本：${refs.join("、") || "无"}${body.references_total > body.references.length ? `，另有 ${count(body.references_total - body.references.length)} 条未列出` : ""}`));
      target.append(node("p", "truncation-note", "助手回复是当时的历史内容，不是已核实的事实；用户声明也只代表用户当时的说法。"));
    } catch (error) {
      if (request === evidenceRequest && requestEpoch === epoch && !error.silent && $("memory-evidence")) {
        $("memory-evidence").replaceChildren(node("h3", "", "来源"), node("p", "inline-error", errorText(error)));
      }
    }
  }

  async function loadPlugins() {
    const request = ++pluginsRequest;
    const requestEpoch = epoch;
    try {
      const [body, operations] = await Promise.all([api("/api/plugins"), api("/api/plugins/operations")]);
      if (request !== pluginsRequest || requestEpoch !== epoch) return;
      if (!body || typeof body.instance !== "string" || !Array.isArray(body.items) || !Array.isArray(operations)) {
        throw new ApiError("插件列表格式无效。");
      }
      $("plugins-list").replaceChildren();
      const reasons = { host_bound: "宿主已绑定此插件，调整后需重启程序。", protected_dependency: "宿主固定插件依赖它，不能单独停止。", failed: "插件启动或收尾失败，需要处理后重启程序。" };
      const states = { Registered: "尚未启动", Starting: "正在启动", Active: "运行中", Stopping: "正在停止", Stopped: "已停止", Failed: "失败" };
      for (const plugin of body.items) {
        const card = node("article", "plugin-card");
        card.append(node("h3", "", plugin.id), node("p", "record-meta", `版本 ${plugin.version} · ${states[plugin.state] || plugin.state}`),
          node("p", "muted", `依赖：${plugin.dependencies.join("、") || "无"} · 权限：${plugin.permissions.join("、") || "无"}`));
        if (plugin.reason) card.append(node("p", "detail-note", reasons[plugin.reason] || "当前状态不能操作。"));
        if (plugin.can_start || plugin.can_stop) {
          const action = plugin.can_stop ? "stop" : "start";
          const control = button(action === "stop" ? "停止插件" : "启动插件", action === "stop" ? "danger-outline" : "secondary", async () => {
            if (pluginActionPending) return;
            pluginActionPending = true;
            control.disabled = true;
            showError("plugins-error", "");
            try {
              const receipt = await api("/api/plugins/action", { instance: body.instance, plugin_id: plugin.id, expected_state: plugin.state, action });
              if (requestEpoch !== epoch) return;
              $("plugins-feedback").textContent = `已提交操作 ${receipt.id}，完成状态请查看右侧记录。`;
              await loadPlugins();
            } catch (error) { if (requestEpoch === epoch) reportError("plugins-error", error); }
            finally { if (requestEpoch === epoch) { pluginActionPending = false; control.disabled = false; } }
          });
          control.disabled = pluginActionPending;
          card.append(control);
        }
        $("plugins-list").append(card);
      }
      const box = $("plugin-operations");
      box.replaceChildren();
      if (!operations.length) box.append(node("p", "muted", "本次面板访问还没有提交插件操作。"));
      const statesOfOperation = { running: "进行中", completed: "已完成", failed: "失败，需处理", interrupted: "中断，需处理" };
      for (const operation of operations) {
        const card = node("article", "plugin-card");
        card.append(node("h3", "", `${operation.id} · ${operation.action === "stop" ? "停止" : "启动"} ${operation.plugin_id}`), node("p", "muted", statesOfOperation[operation.state] || "未知状态"));
        if (operation.state !== "running") {
          const remove = button("移除记录", "secondary", async () => {
            remove.disabled = true;
            try { await api("/api/plugins/ack", { id: operation.id }); if (requestEpoch === epoch) await loadPlugins(); }
            catch (error) { if (requestEpoch === epoch) reportError("plugins-error", error); }
            finally { if (requestEpoch === epoch) remove.disabled = false; }
          });
          card.append(remove);
        }
        box.append(card);
      }
      showError("plugins-error", "");
    } catch (error) { if (request === pluginsRequest && requestEpoch === epoch) reportError("plugins-error", error); }
  }

  async function loadPluginPages() {
    const request = ++pluginPagesRequest;
    const requestEpoch = epoch;
    try {
      const pages = await api("/api/plugin-pages");
      if (request !== pluginPagesRequest || requestEpoch !== epoch) return;
      if (!Array.isArray(pages)) throw new ApiError("插件页面目录格式无效。");
      const list = $("plugin-pages-list");
      list.replaceChildren();
      if (!pages.length) list.append(node("p", "empty-list", "当前运行的插件未提供页面。"));
      for (const link of pages) {
        const open = button(link.page.title, "secondary", () => {
          if (pluginPageSaving || pluginPageDirty) { showError("plugin-pages-error", "请先保存修改，或在右侧重新读取以放弃编辑，再切换页面。"); return; }
          void openPluginPage(link);
        });
        const card = node("article", "plugin-card");
        card.append(open, node("p", "record-meta", link.plugin_id));
        list.append(card);
      }
      showError("plugin-pages-error", "");
      // 定时刷新目录，保留右侧尚未保存的表单。
    } catch (error) { if (request === pluginPagesRequest && requestEpoch === epoch) reportError("plugin-pages-error", error); }
  }

  async function openPluginPage(link, feedback = "") {
    const request = ++pluginPageRequest;
    const requestEpoch = epoch;
    try {
      const page = await api("/api/plugin-pages/read", { plugin_id: link.plugin_id, page_id: link.page.id });
      if (request !== pluginPageRequest || requestEpoch !== epoch) return;
      if (!page || !Array.isArray(page.fields) || !Number.isSafeInteger(page.revision) || typeof page.instance !== "string") throw new ApiError("插件页面格式无效。");
      selectedPluginPage = link;
      pluginPageDirty = false;
      const form = node("form", "plugin-form");
      form.append(node("h2", "", page.descriptor.title), node("p", "muted", page.descriptor.description), node("p", "record-meta", `插件 ${link.plugin_id} · 配置修订 ${page.revision}`));
      const notice = node("p", "detail-note", feedback);
      notice.setAttribute("role", "status");
      const errorBox = node("p", "inline-error");
      errorBox.setAttribute("role", "alert");
      form.append(notice, errorBox);
      const inputs = [];
      for (const [index, field] of page.fields.entries()) {
        if (!field.kind || !["boolean", "text", "integer"].includes(field.kind.type)) throw new ApiError("不支持的插件字段类型。");
        const row = node("div", "plugin-field");
        const label = node("label", "", field.label);
        const input = node("input");
        input.id = `plugin-field-${index}`;
        label.htmlFor = input.id;
        const initial = field.override_value === null ? field.value : field.override_value;
        if (field.kind.type === "boolean") { input.type = "checkbox"; input.checked = initial === true; }
        else {
          input.type = field.kind.type === "integer" ? "number" : "text";
          input.value = initial === null ? "" : String(initial);
          if (field.kind.type === "integer") {
            input.step = "1";
            if (field.kind.minimum !== null) input.min = String(field.kind.minimum);
            if (field.kind.maximum !== null) input.max = String(field.kind.maximum);
          } else input.maxLength = 8192;
          input.required = true;
          // 字符串可以为空；只有数字需要非空。
          if (field.kind.type === "text") input.required = false;
        }
        const reset = node("input");
        reset.type = "checkbox";
        const resetLabel = node("label", "reset-field");
        resetLabel.append(reset, document.createTextNode("恢复默认（移除文件覆盖）"));
        const item = { field, input, reset, changed: false };
        const changed = () => { item.changed = true; pluginPageDirty = true; };
        input.addEventListener("input", changed);
        input.addEventListener("change", changed);
        reset.addEventListener("change", () => { input.disabled = reset.checked; changed(); });
        const source = { override: "文件覆盖", environment: "启动时环境配置", default: "默认值" }[field.source] || field.source;
        row.append(label, input, resetLabel, node("p", "small-text", `${source}${field.restart_required ? " · 修改需重启" : " · 修改用于新请求"}`));
        if (field.restart_required) row.append(node("p", "small-text", `当前生效：${field.value === null ? "未设置" : String(field.value)}`));
        form.append(row);
        inputs.push(item);
      }
      const actions = node("div", "plugin-actions");
      const save = node("button", "button primary", "保存修改");
      save.type = "submit";
      const reload = button("重新读取（放弃编辑）", "secondary", () => { if (!pluginPageSaving) void openPluginPage(link); });
      actions.append(save, reload);
      form.append(actions);
      form.addEventListener("submit", async (event) => {
        event.preventDefault();
        if (pluginPageSaving) return;
        const values = Object.create(null);
        for (const item of inputs.filter((entry) => entry.changed)) {
          let value = item.reset.checked ? null : item.field.kind.type === "boolean" ? item.input.checked : item.input.value;
          if (value !== null && item.field.kind.type === "integer") {
            value = Number(value);
            if (!item.input.value.trim() || !Number.isSafeInteger(value)) { errorBox.textContent = "整数配置必须填写有效整数。"; return; }
          }
          values[item.field.id] = value;
        }
        if (!Object.keys(values).length) { notice.textContent = "没有需要保存的修改。"; return; }
        pluginPageSaving = true;
        save.disabled = true;
        reload.disabled = true;
        errorBox.textContent = "";
        try {
          const result = await api("/api/plugin-pages/save", { plugin_id: link.plugin_id, page_id: link.page.id, instance: page.instance, expected_revision: page.revision, values });
          if (requestEpoch !== epoch || request !== pluginPageRequest) return;
          pluginPageDirty = false;
          await openPluginPage(link, `已保存修订 ${result.revision}。${result.restart_required.length ? `以下字段重启后生效：${result.restart_required.join("、")}` : "新请求使用更新后的配置。"}`);
        } catch (error) {
          if (requestEpoch === epoch && request === pluginPageRequest && !error.silent) errorBox.textContent = `${errorText(error)} 修改仍保留；先重新读取确认保存状态，再决定是否重试。`;
        } finally {
          if (requestEpoch === epoch) { pluginPageSaving = false; save.disabled = false; reload.disabled = false; }
        }
      });
      $("plugin-page-detail").replaceChildren(form);
      showError("plugin-pages-error", "");
    } catch (error) { if (request === pluginPageRequest && requestEpoch === epoch) reportError("plugin-pages-error", error); }
  }

  function switchView(next) {
    view = next;
    $("tasks-view").hidden = next !== "tasks";
    $("sessions-view").hidden = next !== "sessions";
    $("judgments-view").hidden = next !== "judgments";
    $("goals-view").hidden = next !== "goals";
    $("memory-view").hidden = next !== "memory";
    $("plugins-view").hidden = next !== "plugins";
    $("plugin-pages-view").hidden = next !== "plugin-pages";
    $("page-title").textContent = { tasks: "任务状态", sessions: "会话记录", judgments: "判断诊断", goals: "认知目标", memory: "记忆偏好", plugins: "插件管理", "plugin-pages": "插件页面" }[next];
    for (const item of document.querySelectorAll("[data-view]")) {
      const active = item.dataset.view === next;
      item.classList.toggle("active", active);
      if (active) item.setAttribute("aria-current", "page");
      else item.removeAttribute("aria-current");
    }
    if (next === "sessions" && sessions.length === 0) void loadSessions();
    if (next === "judgments") void loadJudgments();
    if (next === "goals") void loadGoals();
    if (next === "memory" && memoryScopes.length === 0) void loadMemoryScopes();
    if (next === "plugins") void loadPlugins();
    if (next === "plugin-pages") void loadPluginPages();
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
        view === "goals" ? loadGoals() : Promise.resolve(),
        view === "plugins" ? loadPlugins() : Promise.resolve(),
        view === "plugin-pages" ? loadPluginPages() : Promise.resolve(),
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
      emptyDetail("memory-detail", "✦", "选择一个作用域", "查看已确认和已撤销的偏好、版本历史，以及每个版本的来源。来源正文需单独打开。");
      emptyDetail("goal-detail", "◈", "选择一个目标", "查看状态、来源记录，以及当前和历史反思草稿。草稿是未验证的建议，不代表目标完成。");
      emptyDetail("plugin-page-detail", "▦", "选择一个插件页面", "在这里修改配置、恢复默认值并查看生效方式。");
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
    if (view === "goals" && selectedGoal !== null) void loadGoal(selectedGoal);
    if (view === "memory") {
      void loadMemoryScopes();
      const current = memoryScopes.find((item) => scopeId(item.scope) === selectedMemory);
      if (current) void loadMemory(current.scope);
    }
  });
  $("tasks-more").addEventListener("click", () => { void loadTasks(true).catch(() => {}); });
  $("sessions-more").addEventListener("click", () => { void loadSessions(true); });
  $("judgments-more").addEventListener("click", () => { void loadJudgments(true); });
  $("goals-more").addEventListener("click", () => { void loadGoals(true); });
  $("memory-more").addEventListener("click", () => { void loadMemoryScopes(true); });
  for (const item of document.querySelectorAll("[data-view]")) item.addEventListener("click", () => switchView(item.dataset.view));
  document.addEventListener("visibilitychange", () => {
    clearTimeout(pollTimer);
    if (document.hidden) {
      if (token) $("refresh-state").textContent = "页面隐藏，已暂停刷新";
    } else if (token) void refresh();
  });
  window.addEventListener("pagehide", () => logout());
})();
