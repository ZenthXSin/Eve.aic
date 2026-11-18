const sections = {
  timeline: ["活动", "Eve 留下的时间线与生活记录"],
  notes: ["便签", "按来源阅读已保存的便签"],
  memory: ["记忆", "形成过的记忆，不是对事实的独立证实"],
  agents: ["帮手", "会话状态与已有的任务消息"],
  receipts: ["回执", "区分请求、已经确认的结果与未知结果"],
  qqConversations: ["QQ 会话", "只读，不改变手机已读状态"],
  qqFailures: ["发送异常", "失败队首；查看不会重试或修改回执"],
  workspace: ["工作区", "最近脚本的操作摘要，不执行命令"],
};
const journal = new Set(["timeline", "receipts", "memory"]);
const labels = {
  model_start: "开始准备模型请求",
  model_response: "模型已返回",
  model_failure: "模型请求失败",
  focus: "更新重心",
  rest: "休息",
  wake: "醒来",
  notice: "收到通知",
  tool_start: "开始工具调用",
  tool_result: "工具回执",
  life_moment: "当下心情",
  leisure_lanterns: "九宫熄灯",
  request: "请求已登记",
  result: "原始回执",
  response: "回复",
  formation: "记忆形成",
  context_state: "更新上下文",
};
function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = String(text);
  if (className) node.className = className;
  return node;
}
function button(label, handler) {
  const node = element("button", label);
  node.type = "button";
  node.addEventListener("click", handler);
  return node;
}
function clipped(value, max = 240) {
  if (typeof value === "object" && value) value = value.preview ?? value.text ?? value.value;
  if (typeof value !== "string" && typeof value !== "number") return "";
  const text = String(value);
  return text.length > max ? text.slice(0, max) + "…" : text;
}
function date(value) {
  const when = new Date(value ?? NaN);
  return Number.isFinite(when.getTime()) ? when.toLocaleString("zh-CN", { hour12: false }) : "";
}
function records(value) {
  return Array.isArray(value.items)
    ? value.items
    : Array.isArray(value.messages)
    ? value.messages
    : Array.isArray(value.matches)
    ? value.matches
    : [];
}
function cardData(resource, row) {
  const data = row.data && typeof row.data === "object" ? row.data : {};
  const seq = row.seq ?? row.recordSeq ?? row.sequence ?? row.handle?.seq;
  const id = clipped(row.id ?? row.agentId ?? row.actionId ?? row.conversationId ?? data.id);
  const reading = resource === "qqHistory" || resource === "agentHistory";
  const title = resource === "qqConversations"
    ? clipped(row.remark ?? row.platformName ?? row.alias ?? row.conversationId)
    : resource === "agents"
    ? clipped(row.config?.name ?? id)
    : reading
    ? clipped(row.sender?.displayName ?? (row.direction === "outgoing" ? "发出" : "收到"))
    : resource === "workspace"
    ? clipped([row.namespace, row.method].filter(Boolean).join(" · "))
    : clipped(row.title ?? labels[row.kind] ?? row.kind ?? id) || "记录";
  let text = clipped(
    row.summary ?? row.text ??
      (typeof row.preview === "string" ? (reading ? row.preview : null) : row.preview?.text) ??
      row.body?.text ?? row.body?.preview ??
      (typeof row.body === "string" ? row.body : null),
  );
  if (resource === "timeline" || resource === "receipts") {
    text = [
      clipped(data.requestId ?? data.callId ?? data.id),
      clipped(data.call?.name ?? data.namespace ?? data.source),
      clipped(data.result?.status),
    ].filter(Boolean).join(" · ");
    if (row.kind === "life_moment") {
      text = [data.feeling, data.wish, data.interest].map((value) => clipped(value)).filter(Boolean)
        .join(" · ");
    }
    if (row.kind === "leisure_lanterns") {
      text = `${
        ({ playing: "进行中", left: "已放下", won: "全灭通关" })[data.status] ?? "状态未知"
      } · ${Number.isInteger(data.moves) ? data.moves : "—"} 步`;
    }
  }
  if (resource === "qqConversations") {
    text = [
      clipped(row.kind),
      `${row.pendingNotifications ?? "—"} 条本机提醒`,
      row.settings?.muted ? "已静音" : "",
    ].filter(Boolean).join(" · ");
  }
  if (resource === "agents") {
    text = [clipped(row.state), clipped(row.config?.role), clipped(row.latest?.preview)].filter(
      Boolean,
    ).join(" · ");
  }
  const meta = [
    seq != null ? `#${seq}` : null,
    date(row.at ?? row.formedAt ?? row.updatedAt ?? row.time?.at),
    clipped(row.status ?? row.delivery?.state),
    row.truncated || row.partial ? "部分记录" : "",
    row.quarantined ? "回滚区 · 未确认" : "",
  ].filter(Boolean).join(" · ");
  return { title, text, meta, id };
}
export async function mount(root, ctx) {
  if (!document.querySelector('link[href="/studio/explore.css"]')) {
    const sheet = element("link");
    sheet.rel = "stylesheet";
    sheet.href = "/studio/explore.css";
    document.head.append(sheet);
  }
  const section = ctx.route.split("/")[1] || "timeline",
    resource = Object.hasOwn(sections, section) ? section : "timeline";
  const area = element("section", undefined, "archive-page"),
    nav = element("nav", undefined, "archive-nav");
  nav.setAttribute("aria-label", "记录类别");
  for (const [key, [name]] of Object.entries(sections)) {
    const link = element("a", name);
    link.href = `#explore/${key}`;
    if (key === resource) link.setAttribute("aria-current", "page");
    nav.append(link);
  }
  const title = element("h1", sections[resource][0]),
    summary = element("p", sections[resource][1], "archive-sub"),
    tools = element("form", undefined, "archive-tools");
  const feed = element("div", undefined, "archive-list"),
    feedback = element("p", "", "archive-status"),
    pager = element("div", undefined, "archive-pager");
  feedback.setAttribute("role", "status");
  feed.setAttribute("aria-label", "记录列表");
  area.append(
    element("p", "EVE / 档案室", "archive-kicker"),
    title,
    summary,
    nav,
    tools,
    feedback,
    feed,
    pager,
  );
  root.replaceChildren(area);
  let trail = [], active = 0, current, fromStart = false, keyword = "", mode = resource, target;
  const query = (operation, args, res = mode) =>
    ctx.api("/api/query", { resource: res === "agentHistory" ? "agents" : res, operation, args });
  async function details(row, article) {
    const box = element("div", "正在读取…", "archive-detail");
    article.append(box);
    try {
      const reply = await query(
        "get",
        mode === "notes" || mode === "memory"
          ? { id: row.id, limit: 2048 }
          : { seq: row.seq, limit: 2048 },
      );
      if (!box.isConnected || ctx.signal.aborted) return;
      const note = reply.note ?? reply;
      if (mode === "notes" || mode === "agentHistory") {
        let page = reply;
        const show = () => {
          box.replaceChildren(
            element("p", clipped(page.text ?? note.summary, 2048) || "没有可展示的正文"),
          );
          if (Number.isSafeInteger(page.nextChar)) {
            const more = button("继续阅读原文", async () => {
              more.disabled = true;
              try {
                const locator = mode === "notes"
                  ? { id: row.id, revision: note.revision }
                  : { seq: row.seq };
                page = await query("get", { ...locator, startChar: page.nextChar, limit: 2048 });
                if (box.isConnected && !ctx.signal.aborted) show();
              } catch (error) {
                if (box.isConnected && !ctx.signal.aborted) {
                  box.append(element("p", error.message));
                  more.disabled = false;
                }
              }
            });
            box.append(more);
          }
        };
        show();
      } else if (mode === "memory") {
        const sourceIds = Array.isArray(row.sources)
          ? row.sources.slice(0, 12).map((source) => source.recordSeq).filter(Number.isSafeInteger)
          : [];
        box.textContent = [
          clipped(row.text, 2048),
          sourceIds.length
            ? `依据记录：${sourceIds.map((seq) => `#${seq}`).join("、")}`
            : "此条预览未包含来源列表",
          reply.digest ? `形成记录摘要 ${clipped(reply.digest, 64)}` : "",
        ].filter(Boolean).join("\n");
      } else {box.textContent = [
          `原记录 #${reply.seq}`,
          reply.digest ? `摘要 ${clipped(reply.digest, 64)}` : "",
          Number.isInteger(reply.characters) ? `长度 ${reply.characters} 字符` : "",
          "默认不展开原始模型参数与工具凭据。",
        ].filter(Boolean).join("\n");}
    } catch (error) {
      if (box.isConnected && !ctx.signal.aborted) box.textContent = error.message;
    }
  }
  const back = button("返回列表", () => {
    mode = resource;
    target = null;
    trail = [];
    void load(null);
  });
  const search = element("input");
  search.type = "search";
  search.maxLength = 160;
  search.placeholder = "查找原记录";
  search.setAttribute("aria-label", "检索文字");
  const find = element("button", "搜索");
  find.type = "submit";
  if (["notes", "timeline", "memory", "receipts"].includes(resource)) tools.append(search, find);
  tools.addEventListener("submit", (event) => {
    event.preventDefault();
    keyword = search.value.trim();
    trail = [];
    void load(null);
  });
  if (journal.has(resource)) {
    tools.append(
      button("从头浏览", () => {
        fromStart = true;
        keyword = "";
        search.value = "";
        trail = [];
        void load(null);
      }),
      button("最近记录", () => {
        fromStart = false;
        keyword = "";
        search.value = "";
        trail = [];
        void load(null);
      }),
    );
  }
  tools.append(button("刷新列表", () => {
    trail = [];
    void load(null);
  }));
  async function load(cursor) {
    const flight = ++active;
    feedback.textContent = "正在读取…";
    feed.setAttribute("aria-busy", "true");
    pager.replaceChildren();
    try {
      let args = cursor ?? {};
      if (!cursor) {
        if (mode === "qqHistory") args = { conversationId: target };
        else if (mode === "agentHistory") args = { agentId: target, limit: 10 };
        else if (mode === "qqFailures") args = { limit: 16 };
        else if (mode === "agents") args = { limit: 12 };
        else if (keyword) args = { query: keyword };
        else if (journal.has(mode) && !fromStart) {
          const status = await ctx.api("/api/status");
          const head = status?.runtime
            ?.[
              ({
                timeline: "throughSeq",
                memory: "memoryThroughSeq",
                receipts: "receiptsThroughSeq",
              })[mode]
            ];
          if (Number.isSafeInteger(head) && head >= 0) {
            args = { afterSeq: Math.max(0, head - 24), throughSeq: head };
          }
        }
      }
      const data = await query(keyword && mode === resource ? "search" : "page", args);
      if (ctx.signal.aborted || active !== flight) return;
      current = args;
      const rows = records(data);
      feed.replaceChildren();
      for (const row of rows) {
        const { title: name, text, meta } = cardData(mode, row),
          article = element("article", undefined, "archive-card");
        article.append(element("h2", name), element("p", meta, "archive-meta"));
        if (text) article.append(element("p", text));
        if (mode === "qqConversations" || mode === "agents") {
          article.append(button(mode === "agents" ? "查看会话消息" : "查看聊天记录", () => {
            target = row.conversationId ?? row.agentId ?? row.id;
            mode = mode === "agents" ? "agentHistory" : "qqHistory";
            trail = [];
            void load(null);
          }));
        } else if (
          (mode === "notes" || mode === "memory") && row.id ||
          ["timeline", "receipts", "agentHistory"].includes(mode) && Number.isInteger(row.seq)
        ) {
          const open = button("核对原记录", () => {
            open.disabled = true;
            void details(row, article);
          });
          article.append(open);
        }
        feed.append(article);
      }
      let next = data.next ?? null;
      if (data.older) {
        next = mode === "qqConversations"
          ? { cursor: data.older }
          : mode === "qqHistory"
          ? { conversationId: target, cursor: data.older }
          : mode === "agentHistory"
          ? { agentId: target, ...data.older }
          : next;
      }
      feedback.textContent = rows.length
        ? `本页 ${rows.length} 条${next ? " · 还有记录" : " · 已到此页范围末尾"}${
          data.partial ? " · 工作区预览不完整" : ""
        }`
        : next
        ? "本批未匹配，可继续下一页。"
        : "此范围没有记录。";
      if (mode !== resource) pager.append(back);
      if (trail.length) {
        pager.append(button("上一页", () => {
          void load(trail.pop());
        }));
      }
      if (next) {
        pager.append(button("下一页", () => {
          trail.push(current);
          if (trail.length > 64) trail.shift();
          void load(next);
        }));
      }
    } catch (error) {
      if (!ctx.signal.aborted && active === flight) {
        feedback.textContent = `未能读取：${error.message}`;
        feed.replaceChildren();
        if (mode !== resource) pager.append(back);
        pager.append(button("重试读取", () => void load(cursor)));
      }
    } finally {
      if (active === flight) feed.removeAttribute("aria-busy");
    }
  }
  await load(null);
}
