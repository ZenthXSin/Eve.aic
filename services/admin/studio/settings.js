const path = "/api/settings";
const scalar = (value) => ["string", "number", "boolean"].includes(typeof value);
const modes = new Map([
  ["hot", "即时生效"],
  ["live", "即时生效"],
  ["immediate", "即时生效"],
  ["restart", "重启后生效"],
  ["restart-required", "重启后生效"],
  ["readonly", "只读"],
  ["read-only", "只读"],
  ["external", "外部管理"],
  ["restricted", "受限"],
]);
const scopes = new Map([
  ["supervisor", "监督进程"],
  ["instance", "实例"],
  ["runtime", "运行时"],
  ["qq", "QQ"],
  ["guardian", "守护进程"],
  ["admin", "管理服务"],
  ["budget", "预算"],
  ["pause", "暂停状态"],
]);
const kinds = new Map([
  ["boolean", "boolean"],
  ["bool", "boolean"],
  ["integer", "integer"],
  ["int", "integer"],
  ["number", "integer"],
  ["string", "text"],
  ["text", "text"],
  ["url", "url"],
  ["textarea", "multiline"],
  ["multiline", "multiline"],
  ["select", "select"],
  ["enum", "select"],
]);

function readSheet(data) {
  if (
    !(typeof data?.revision === "string" || Number.isSafeInteger(data?.revision)) ||
    !Array.isArray(data.items)
  ) {
    throw new Error("settings-format");
  }
  const keys = new Set();
  const items = data.items.map((item) => {
    if (typeof item?.key !== "string" || !item.key || keys.has(item.key)) {
      throw new Error("settings-key");
    }
    keys.add(item.key);
    const type = String(item.type ?? "").toLowerCase();
    const secret = item.secret === true || item.sensitive === true || item.redacted === true ||
      /secret|password|credential|private.?key|api.?key|access.?token|auth.?token|bot.?token|(?:^|[._-])token$/i
        .test(item.key) ||
      /^(secret|password|token)$/.test(type);
    const options = !secret && Array.isArray(item.options)
      ? item.options.flatMap((option) => {
        const value = option?.value ?? option;
        if (!scalar(value)) return [];
        return [{ value, label: typeof option?.label === "string" ? option.label : String(value) }];
      })
      : [];
    const kind = secret ? "secret" : options.length ? "select" : kinds.get(type) ?? "unknown";
    return {
      key: item.key,
      label: typeof item.label === "string" ? item.label : item.key,
      group: typeof item.group === "string" && item.group ? item.group : "通用",
      description: typeof item.description === "string" ? item.description : "",
      reason: typeof item.reason === "string" ? item.reason : "",
      scope: typeof item.scope === "string" ? item.scope : "",
      isSet: typeof item.isSet === "boolean" ? item.isSet : null,
      mode: typeof item.mode === "string" ? item.mode : "",
      kind,
      options,
      editable: item.editable === true && item.mode !== "restricted" && kind !== "secret" &&
        kind !== "unknown",
      required: item.required !== false,
      value: !secret && scalar(item.value) ? item.value : null,
      min: Number.isFinite(item.min) ? item.min : null,
      max: Number.isFinite(item.max) ? item.max : null,
      minLength: Number.isSafeInteger(item.minLength) ? item.minLength : 0,
      maxLength: Number.isSafeInteger(item.maxLength) ? Math.min(item.maxLength, 128) : 128,
    };
  });
  return { revision: data.revision, items };
}

function display(item) {
  if (item.kind === "secret") {
    return item.isSet === null ? "不显示原值" : item.isSet ? "已设置（不显示原值）" : "未设置";
  }
  if (item.value === null) return "未提供";
  if (item.kind === "select") {
    return item.options.find((option) => option.value === item.value)?.label ?? "未列出的选项";
  }
  if (typeof item.value === "boolean") return item.value ? "开启" : "关闭";
  return String(item.value) || "（空）";
}

function fieldState({ item, control }) {
  if (!item.editable) return { dirty: false };
  let value = control.value, error = "";
  if (item.kind === "boolean") {
    value = control.checked;
  } else if (item.kind === "select") {
    const option = item.options.find((entry) => String(entry.value) === value);
    if (!option) error = "请选择列表中的一项。";
    else value = option.value;
  } else if (item.kind === "number" || item.kind === "integer") {
    const digits = value.trim();
    value = /^[-+]?(?:\d+(?:\.\d*)?|\.\d+)$/.test(digits) ? Number(digits) : NaN;
    if (!Number.isFinite(value)) error = "请输入有效数字。";
    else if (item.kind === "integer" && !Number.isSafeInteger(value)) {
      error = "请输入有效范围内的整数。";
    } else if (item.min !== null && value < item.min) error = `不能小于 ${item.min}。`;
    else if (item.max !== null && value > item.max) error = `不能大于 ${item.max}。`;
  } else if (item.required && !value.trim()) {
    error = "此项不能为空。";
  } else if (
    value.length < item.minLength || (item.maxLength !== null && value.length > item.maxLength)
  ) {
    error = item.maxLength === null
      ? `至少需要 ${item.minLength} 个字符。`
      : `长度须在 ${item.minLength} 到 ${item.maxLength} 个字符之间。`;
  } else if (/[\u0000-\u001f\u007f]/u.test(value)) {
    error = "不能包含换行或控制字符。";
  } else if (item.kind === "url" && value && !URL.canParse(value)) {
    error = "请输入包含协议的完整地址。";
  }
  return { value, dirty: value !== (item.value ?? ""), error };
}

export async function mount(root, ctx) {
  if (ctx.signal?.aborted) return;
  const doc = root.ownerDocument;
  const page = doc.createElement("section");
  page.className = "eve-settings";
  page.lang = "zh-CN";
  page.setAttribute("aria-labelledby", "est-title");
  page.innerHTML = `
    <link rel="stylesheet" href="/studio/settings.css">
    <header class="est-head"><div><p class="est-kicker">工作台 · 配置</p><h1 id="est-title">设置</h1>
      <p class="est-intro">按分组查看当前配置。只提交你修改的项目。</p></div>
      <button class="est-btn" type="button" data-ui="refresh">重新读取</button></header>
    <div class="est-layout"><form class="est-card" novalidate autocomplete="off" aria-labelledby="est-form-title">
      <h2 id="est-form-title">配置项目</h2>
      <div class="est-tools" data-ui="tools" hidden><label for="est-search">查找项目</label>
        <input type="search" id="est-search" data-ui="filter" placeholder="输入名称、分组或键名" autocomplete="off" spellcheck="false">
        <button class="est-btn" type="button" data-ui="clear" hidden>清除筛选</button></div>
      <p class="est-hint" data-ui="matches" role="status" hidden></p><div data-ui="groups"></div>
      <p class="est-hint" data-ui="empty">正在读取配置…</p>
      <div class="est-source" data-ui="picker" hidden><label for="est-scope">本次保存来源</label><select id="est-scope" data-ui="scope"></select>
        <p class="est-hint">每次只保存一个来源；其他来源的输入会保留。</p></div>
      <div class="est-actions"><button class="est-btn est-primary" type="submit" data-ui="save" disabled>保存更改</button>
        <button class="est-btn" type="button" data-ui="reset" disabled>撤销更改</button></div>
      <p class="est-feedback" role="status" aria-live="polite" aria-atomic="true" data-ui="feedback"></p>
    </form><aside class="est-card est-summary" aria-labelledby="est-summary-title">
      <p class="est-kicker">本次编辑</p><h2 id="est-summary-title">更改摘要</h2><dl>
        <div><dt>可编辑项目</dt><dd data-ui="editable">—</dd></div><div><dt>只读项目</dt><dd data-ui="readonly">—</dd></div>
        <div><dt>待保存</dt><dd data-ui="dirty">—</dd></div></dl><p class="est-hint">重新读取会保留未保存的输入。撤销更改仅恢复本页表单，不修改服务端。</p>
    </aside></div>`;
  root.replaceChildren(page);

  const controller = new AbortController(), { signal } = controller;
  const ui = Object.fromEntries(
    Array.from(page.querySelectorAll("[data-ui]"), (node) => [node.dataset.ui, node]),
  );
  const rows = new Map(), groups = new Map();
  let revision = null, busy = "", recheck = false, sequence = 0;

  function dispose() {
    controller.abort();
    ctx.signal?.removeEventListener("abort", dispose);
    rows.clear();
    groups.clear();
    page.remove();
  }
  ctx.signal?.addEventListener("abort", dispose, { once: true });

  function element(tag, className) {
    const node = doc.createElement(tag);
    if (className) node.className = className;
    return node;
  }
  function text(node, value) {
    if (node.textContent !== value) node.textContent = value;
  }
  function tell(message, tone = "") {
    text(ui.feedback, message);
    ui.feedback.dataset.tone = tone;
  }

  function makeRow(item) {
    const block = element("div", "est-field");
    const header = element("div", "est-label"), label = element(item.editable ? "label" : "span");
    const badge = element("small", "est-badge"),
      hint = element("p", "est-hint"),
      error = element("p", "est-error");
    const id = `est-field-${++sequence}`;
    const control = element(
      !item.editable
        ? "p"
        : item.kind === "select"
        ? "select"
        : item.kind === "multiline"
        ? "textarea"
        : "input",
    );
    label.id = `${id}-label`;
    control.id = id;
    hint.id = `${id}-hint`;
    error.id = `${id}-error`;
    error.hidden = true;
    if (item.editable) {
      label.htmlFor = id;
      control.name = item.key;
      control.setAttribute("aria-describedby", `${hint.id} ${error.id}`);
      if (control.tagName === "INPUT") {
        control.type = item.kind === "boolean"
          ? "checkbox"
          : item.kind === "integer"
          ? "number"
          : "text";
        if (item.kind === "integer") {
          control.inputMode = "numeric";
          control.step = "1";
        }
        control.spellcheck = false;
      }
    } else {
      control.className = "est-value";
      control.setAttribute("aria-labelledby", label.id);
    }
    header.append(label, badge);
    if (item.kind === "boolean" && item.editable) header.prepend(control);
    block.append(header, hint);
    if (control.parentNode !== header) block.append(control);
    block.append(error);
    return { item, block, label, badge, hint, control, error, edits: 0, options: null };
  }

  function fill(row) {
    const { item, control } = row;
    if (!item.editable) return text(control, display(item));
    if (item.kind === "boolean") {
      control.checked = item.value === true;
      return;
    }
    const value = item.value === null ? "" : String(item.value);
    if (control.value !== value) control.value = value;
  }

  function refreshControls() {
    let dirty = 0, editable = 0;
    const byScope = new Map();
    for (const row of rows.values()) {
      const state = fieldState(row);
      if (row.item.editable) editable++;
      if (state.dirty) {
        dirty++;
        const scope = byScope.get(row.item.scope) ?? { count: 0, invalid: false };
        scope.count++;
        if (state.error) scope.invalid = true;
        byScope.set(row.item.scope, scope);
      }
      const message = state.dirty ? state.error || "" : "";
      text(row.error, message);
      row.error.hidden = !message;
      if (row.item.editable) row.control.setAttribute("aria-invalid", String(Boolean(message)));
    }
    if (revision !== null) {
      text(ui.editable, String(editable));
      text(ui.readonly, String(rows.size - editable));
      text(ui.dirty, String(dirty));
    }
    const choices = [...byScope.keys()];
    if (
      choices.length !== ui.scope.options.length ||
      choices.some((value, index) => ui.scope.options[index].value !== value)
    ) {
      const previous = ui.scope.value;
      ui.scope.replaceChildren(...choices.map((value) => {
        const option = element("option");
        option.value = value;
        text(option, scopes.get(value) ?? (value || "通用"));
        return option;
      }));
      if (byScope.has(previous)) ui.scope.value = previous;
    }
    ui.picker.hidden = choices.length < 2;
    ui.scope.disabled = Boolean(busy);
    const selected = byScope.get(ui.scope.value);
    ui.refresh.disabled = Boolean(busy);
    ui.save.disabled = Boolean(
      busy || !selected || selected.invalid || selected.count > 32 || recheck,
    );
    ui.reset.disabled = Boolean(busy || !dirty);
    text(
      ui.save,
      busy === "save"
        ? "正在保存…"
        : choices.length > 1
        ? `保存所选来源（${selected.count} 项）`
        : "保存更改",
    );
    return dirty;
  }

  function filterRows() {
    const query = ui.filter.value.trim().toLowerCase();
    const visible = new Set();
    let matches = 0;
    for (const { item, block } of rows.values()) {
      block.hidden = ![item.label, item.group, item.key].some((part) =>
        part.toLowerCase().includes(query)
      );
      if (!block.hidden) {
        visible.add(item.group);
        matches++;
      }
    }
    for (const [group, fieldset] of groups) fieldset.hidden = !visible.has(group);
    ui.tools.hidden = !rows.size;
    ui.clear.hidden = !query;
    ui.matches.hidden = !query || !rows.size;
    text(
      ui.matches,
      matches
        ? `找到 ${matches} 项。筛选不改变待保存的内容。`
        : "没有匹配的项目。可以更换关键词或清除筛选。",
    );
  }

  function accept(sheet, sentEdits = null) {
    const seen = new Set();
    for (const item of sheet.items) {
      let group = groups.get(item.group);
      if (!group) {
        group = element("fieldset");
        const legend = element("legend");
        text(legend, item.group);
        group.append(legend);
        ui.groups.append(group);
        groups.set(item.group, group);
      }
      let row = rows.get(item.key);
      if (row && (row.item.kind !== item.kind || row.item.editable !== item.editable)) {
        row.block.remove();
        row = null;
      }
      const keep = row && item.editable &&
        (sentEdits?.has(item.key) ? row.edits !== sentEdits.get(item.key) : fieldState(row).dirty);
      if (!row) {
        row = makeRow(item);
        rows.set(item.key, row);
      }
      row.item = item;
      if (item.kind === "integer" && item.editable) {
        row.control.min = item.min === null ? "" : String(item.min);
        row.control.max = item.max === null ? "" : String(item.max);
      }
      text(row.label, item.label);
      const mode = modes.get(item.mode) ?? item.mode;
      text(
        row.badge,
        [...new Set([!item.editable ? "只读" : "", mode].filter(Boolean))].join(" · "),
      );
      row.badge.hidden = !row.badge.textContent;
      const bounds = (item.kind === "integer" || item.kind === "number") &&
          (item.min !== null || item.max !== null)
        ? [
          item.min !== null ? `最小 ${item.min}` : "",
          item.max !== null ? `最大 ${item.max}` : "",
        ].filter(Boolean).join("，")
        : "";
      text(row.hint, [item.description, item.reason, bounds].filter(Boolean).join(" · "));
      row.hint.hidden = !row.hint.textContent;
      if (item.editable && item.kind === "select" && JSON.stringify(item.options) !== row.options) {
        const selected = row.control.value;
        row.control.replaceChildren(...item.options.map((option) => {
          const choice = element("option");
          choice.value = String(option.value);
          text(choice, option.label);
          return choice;
        }));
        row.options = JSON.stringify(item.options);
        if (keep) row.control.value = selected;
      }
      if (!keep) fill(row);
      if (row.block.parentNode !== group) group.append(row.block);
      seen.add(item.key);
    }
    for (const [key, row] of rows) {
      if (!seen.has(key)) {
        row.block.remove();
        rows.delete(key);
      }
    }
    for (const [name, group] of groups) {
      if (group.childElementCount === 1) {
        group.remove();
        groups.delete(name);
      }
    }
    revision = sheet.revision;
    recheck = false;
    ui.empty.hidden = rows.size > 0;
    text(ui.empty, "当前没有可展示的配置项目。");
    filterRows();
  }

  async function load() {
    if (busy || signal.aborted) return;
    busy = "read";
    refreshControls();
    tell("正在读取设置…");
    try {
      const sheet = readSheet(await ctx.api(path, undefined, { signal }));
      if (signal.aborted) return;
      accept(sheet);
      tell(
        refreshControls() ? "已读取当前版本，保留了你的更改。请核对后保存。" : "已读取当前版本。",
      );
    } catch {
      if (signal.aborted) return;
      if (revision === null) text(ui.empty, "尚未读取到配置项目。");
      tell("读取失败。请检查连接或登录状态，再重新读取。未覆盖你的输入。", "error");
    } finally {
      busy = "";
      if (!signal.aborted) refreshControls();
    }
  }

  function edited(event) {
    const row = rows.get(event.target.name);
    if (!row || row.control !== event.target) return;
    row.edits++;
    const dirty = refreshControls();
    if (!busy && !recheck) tell(dirty ? `有 ${dirty} 项尚未保存。` : "没有待保存的更改。");
  }
  const form = page.querySelector("form");
  ui.filter.addEventListener("input", filterRows, { signal });
  ui.filter.addEventListener("keydown", (event) => {
    if (event.key === "Enter") event.preventDefault();
  }, { signal });
  ui.clear.addEventListener("click", () => {
    ui.filter.value = "";
    filterRows();
    ui.filter.focus();
  }, { signal });
  form.addEventListener("input", edited, { signal });
  form.addEventListener("change", edited, { signal });
  ui.scope.addEventListener("change", refreshControls, { signal });
  ui.refresh.addEventListener("click", load, { signal });
  ui.reset.addEventListener("click", () => {
    if (busy) return;
    for (const row of rows.values()) fill(row);
    refreshControls();
    tell(recheck ? "本页更改已撤销。请重新读取当前版本。" : "本页更改已撤销，未向服务端提交。");
  }, { signal });
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (busy || recheck || revision === null || signal.aborted) return;
    const changes = Object.create(null), sentEdits = new Map();
    for (const [key, row] of rows) {
      const state = fieldState(row);
      if (!state.dirty || row.item.scope !== ui.scope.value) continue;
      if (state.error) {
        tell("请先修正标出的项目。", "error");
        row.control.focus();
        return;
      }
      changes[key] = state.value;
      sentEdits.set(key, row.edits);
    }
    if (!sentEdits.size) return;
    if (sentEdits.size > 32) {
      tell("每次最多保存 32 项，请先减少本次更改。", "error");
      return;
    }
    busy = "save";
    refreshControls();
    tell("正在保存更改…");
    let acknowledged = false;
    try {
      const result = await ctx.api(path, { revision, changes }, { signal });
      if (signal.aborted) return;
      acknowledged = true;
      const sheet = readSheet(
        Array.isArray(result?.items) ? result : await ctx.api(path, undefined, { signal }),
      );
      if (signal.aborted) return;
      accept(sheet, sentEdits);
      tell(
        refreshControls() ? "已保存本次更改。其余输入仍待保存。" : "已保存，配置版本已更新。",
        "success",
      );
      ctx.notify?.("设置已保存");
    } catch (error) {
      if (signal.aborted) return;
      recheck = true;
      const conflict = error?.status === 409 || error?.status === 412 || error?.code === "conflict";
      tell(
        conflict
          ? "配置版本已变化。请重新读取，核对保留的输入后再保存。"
          : acknowledged
          ? "已接收保存请求，但无法读回当前版本。请重新读取后核对。"
          : "未能确认保存结果。输入已保留，请重新读取后核对。",
        "error",
      );
    } finally {
      busy = "";
      if (!signal.aborted) refreshControls();
    }
  }, { signal });
  await load();
  return dispose;
}
