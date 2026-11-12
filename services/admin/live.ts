import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { AdminBackend } from "./contracts.ts";
import type { ToolCall } from "../../contracts/model.ts";
import { observedPreview } from "../../sdk/script-observer.ts";
import { callFor } from "../../plugins/eve/view.ts";
import type { ConsciousEvent } from "../../plugins/eve/journal.ts";
import { projectThoughts, thoughtForCall, withoutThought } from "../../plugins/eve/thoughts.ts";

type ObjectValue = Record<string, unknown>;
const object = (value: unknown): ObjectValue =>
  value && typeof value === "object" && !Array.isArray(value) ? value as ObjectValue : {};
const string = (value: unknown) => typeof value === "string" ? value : "";
const clip = (value: unknown, size = 4000) => string(value).slice(0, size);

export interface LiveEvent {
  seq: number;
  at: string;
  kind: string;
  category: "thought" | "action" | "result" | "notice" | "error" | "system";
  title: string;
  text: string;
  truncated: boolean;
  hasText?: boolean;
  requestId?: string;
  callId?: string;
  tool?: string;
  /** 写随记的调用状态保留在 API，阅读流不重复展示成功保存动作。 */
  thoughtWrite?: boolean;
  /** Native tool previews reference their one original thought record rather than copying it. */
  nativeCalls?: {
    id: string;
    name: string;
    args: Json;
    hasThought: boolean;
    thoughtRecordSeq?: number;
    partial: boolean;
  }[];
  toolArgs?: Json;
  toolPreviewPartial?: boolean;
  nativePreviewOmitted?: number;
  toolHasThought?: boolean;
  toolReceipt?: Json;
  toolReceiptPartial?: boolean;
  wakeAt?: number;
  retryAt?: number;
}

/** 只展示实际留下的文字和动作元数据，不推断未返回的模型内容或脚本副作用。 */
export function liveEvent(value: unknown): LiveEvent {
  const e = object(value), d = object(e.data), response = object(d.response);
  const result = object(d.result), call = object(d.call), error = object(d.error);
  const event: LiveEvent = {
    seq: Number(e.seq),
    at: string(e.at),
    kind: string(e.kind),
    category: "system",
    title: string(e.kind) || "系统记录",
    text: "可打开原始记录查看详情。",
    truncated: e.truncated === true,
  };
  if (!Number.isSafeInteger(event.seq) || event.seq < 1) {
    throw new KernelError("corrupt", "实时记录序号无效");
  }
  if (typeof d.requestId === "string") event.requestId = d.requestId;
  if (typeof d.callId === "string") event.callId = d.callId;
  switch (event.kind) {
    case "model_start":
      event.category = "action";
      event.title = "模型请求已发出";
      event.text = "等待模型返回；尚无新的文字记录。";
      break;
    case "model_response": {
      event.category = "thought";
      event.title = "Eve 留下了新的文字记录";
      const calls = Array.isArray(response.toolCalls) ? response.toolCalls : [];
      const startIndex = Math.max(0, calls.length - 32);
      event.nativePreviewOmitted = startIndex;
      let remaining = 8192;
      event.nativeCalls = calls.slice(startIndex).map((_raw, offset) => {
        // Core uses this same helper: provider ids are not execution identities.
        const native = callFor(e as unknown as ConsciousEvent, startIndex + offset);
        const hasThought = thoughtForCall(native) !== undefined;
        const preview = observedPreview(
          withoutThought(native).args,
          Math.max(0, Math.min(2048, remaining)),
        );
        remaining -= JSON.stringify(preview.value).length;
        return {
          id: native.id,
          name: native.name,
          args: preview.value,
          hasThought,
          partial: preview.partial,
          ...(hasThought ? { thoughtRecordSeq: event.seq } : {}),
        };
      });
      const projected = projectThoughts({
        content: string(response.content),
        toolCalls: calls as ToolCall[],
      }).content;
      event.text = clip(projected);
      event.hasText = Boolean(event.text.trim()) && e.truncated !== true;
      if (!event.text.trim()) {
        event.text = calls.length
          ? "本次未留下文字，返回了工具调用：" + calls.map((c) => string(object(c).name)).join("、")
          : "本次没有可展示的文字。";
      }
      event.truncated ||= projected.length > 4000;
      break;
    }
    case "tool_start":
      event.category = "action";
      event.tool = string(call.name);
      {
        const native = call as unknown as ToolCall;
        event.toolHasThought = thoughtForCall(native) !== undefined;
        const preview = observedPreview(withoutThought(native).args, 2048);
        event.toolArgs = preview.value;
        event.toolPreviewPartial = preview.partial;
      }
      if (event.tool === "write_thought") event.thoughtWrite = true;
      event.title = "工具调用开始";
      event.text = event.tool === "execute_script"
        ? "正在执行脚本工具，等待调用回执。"
        : event.tool || "等待工具回执。";
      break;
    case "tool_result":
      {
        const preview = observedPreview(result, 2048);
        event.toolReceipt = preview.value;
        event.toolReceiptPartial = preview.partial;
      }
      event.category = ["failed", "unknown"].includes(string(result.status)) ? "error" : "result";
      event.title = "工具调用已返回";
      event.text = "回执状态：" + (string(result.status) || "未提供") +
        "。调用返回不等于消息已送达或后台任务已结束。";
      break;
    case "model_failure":
      event.category = "error";
      event.title = "模型请求异常";
      event.text = clip(error.message || "请求未正常完成，请核对原始记录。", 500);
      if (Number.isFinite(d.retryAt)) event.retryAt = Number(d.retryAt);
      break;
    case "recovery":
      event.category = "error";
      event.title = "宿主恢复时核对了未确认请求";
      event.text = clip(d.result, 500) || "请求结果需以恢复记录为准。";
      break;
    case "notice": {
      event.category = "notice";
      const source = string(d.source);
      const sources: Record<string, string> = {
        phone: "消息",
        scripts: "脚本",
        commands: "命令",
        memory: "记忆",
        reminder: "提醒",
        image: "图片",
        image_description: "图片描述",
        developer: "开发通知",
      };
      event.title = "收到" + (sources[source] || source || "新的") + "通知";
      event.text = Array.isArray(d.items)
        ? "本批 " + d.items.length + " 条；正文和来源可在原件中查看。"
        : "通知已进入意识日志；不代表已回复。";
      break;
    }
    case "rest":
      event.title = "进入休息等待";
      event.text = clip(d.reason, 500);
      if (Number.isFinite(d.wakeAt)) event.wakeAt = Number(d.wakeAt);
      break;
    case "wake":
      event.title = "开始下一步处理";
      event.text = clip(d.reason, 500);
      break;
    case "compression":
      event.title = "整理上下文";
      event.text = "已记录上下文摘要，完整原件保留。";
      break;
    case "context_projection":
      event.title = "调整上下文投影";
      event.text = "已记录新的固定投影边界。";
      break;
    case "context_wait":
      event.category = "error";
      event.title = "等待上下文处理";
      event.text = clip(d.reason, 500);
      break;
    case "budget_wait":
      event.category = "error";
      event.title = "等待预算余量";
      event.text = clip(d.reason, 500);
      break;
    case "channel_failure":
      event.category = "error";
      event.title = "通道连接异常";
      break;
    case "channel_recovered":
      event.title = "通道连接恢复";
      break;
    case "model_resume":
      event.title = "模型阻塞已明确解除";
      break;
  }
  if (e.truncated === true) {
    event.text = "原记录较长，摘要页未包含完整字段。请打开原件；未据此推断内容或执行结果。";
  }
  return event;
}

export function liveState(events: LiveEvent[], status: unknown) {
  const s = object(status), runtime = object(s.runtime), supervisor = object(s.supervisor);
  const budget = object(runtime.budget), model = object(runtime.model);
  const pending = new Map<string, LiveEvent>();
  let stage: LiveEvent | undefined;
  for (const e of events) {
    if (e.kind === "model_start" && e.requestId) pending.set("model:" + e.requestId, e);
    if (["model_response", "model_failure", "recovery"].includes(e.kind) && e.requestId) {
      pending.delete("model:" + e.requestId);
    }
    if (e.kind === "tool_start" && e.callId) pending.set("tool:" + e.callId, e);
    if (e.kind === "tool_result" && e.callId) pending.delete("tool:" + e.callId);
    if (
      [
        "model_start",
        "model_response",
        "model_failure",
        "tool_start",
        "tool_result",
        "rest",
        "wake",
        "budget_wait",
        "context_wait",
        "recovery",
      ].includes(e.kind)
    ) stage = e;
  }
  const inFlight = [...pending.values()].at(-1);
  let title = "等待新的执行记录", phase = "waiting", since = stage?.at ?? null;
  let description = "近期窗口中未见可确认的进行中调用；这里不据此判定后台任务已经结束。";
  if (inFlight && (!stage || inFlight.seq >= stage.seq || stage.kind === "notice")) {
    title = inFlight.kind === "model_start" ? "正在等待模型返回" : "正在等待工具回执";
    phase = inFlight.kind === "model_start" ? "model" : "tool";
    since = inFlight.at;
    description = inFlight.text;
  } else if (stage?.kind === "rest") {
    title = "休息，等待唤醒";
    phase = "rest";
    description = stage.wakeAt
      ? "记录的计划唤醒时间：" + new Date(stage.wakeAt).toISOString() +
        "；实际状态以之后的新记录为准。"
      : "等待新的通知或唤醒记录。";
  } else if (stage?.kind === "model_response") {
    title = "模型已返回，等待下一步记录";
    phase = "response";
    description = "文字与工具调用已写入日志；工具是否执行以调用记录和回执为准。";
  } else if (stage?.kind === "tool_result") {
    title = "工具已返回，等待下一步记录";
    phase = "result";
    description = stage.text;
  } else if (
    stage && ["model_failure", "recovery", "context_wait", "budget_wait"].includes(stage.kind)
  ) {
    title = stage.title;
    phase = "attention";
    description = stage.text;
  } else if (stage?.kind === "wake") {
    title = "已唤醒，等待下一步记录";
    phase = "notice";
    description = stage.text;
  }
  if (supervisor.paused === true || budget.paused === true) {
    title = "运行已暂停";
    phase = "paused";
    description = "当前由管理者暂停。下方仍可阅读暂停前的记录。";
    since = null;
  } else if (model.blocked) {
    title = "模型请求已阻塞";
    phase = "attention";
    description = "自动重试已经停止，可到运行设置检查原因。";
    since = null;
  } else if (runtime.available === false) {
    title = "执行服务不可用";
    phase = "attention";
    description = "历史记录仍可阅读；当前执行情况暂时无法确认。";
    since = null;
  }
  return {
    title,
    phase,
    description,
    since,
    requestId: inFlight?.requestId ?? null,
    callId: inFlight?.callId ?? null,
  };
}

/** 有界只读窗口。水位不变不重读正文；同一时刻的浏览器请求共用一次读取。 */
export class LiveReader {
  #events: LiveEvent[] = [];
  #through = -1;
  #pending?: Promise<Json>;
  constructor(readonly backend: AdminBackend) {}
  read(): Promise<Json> {
    if (this.#pending) return this.#pending;
    const request = this.#read();
    this.#pending = request;
    void request.finally(() => {
      if (this.#pending === request) this.#pending = undefined;
    }).catch(() => {});
    return request;
  }
  async #read(): Promise<Json> {
    const status = await this.backend.status();
    const through = object(object(status).runtime).throughSeq;
    if (!Number.isSafeInteger(through) || Number(through) < 0) {
      throw new KernelError("unavailable", "执行服务未提供有效日志水位");
    }
    const end = Number(through);
    if (end !== this.#through) {
      const start = this.#through >= 0 && end >= this.#through && end - this.#through <= 80
        ? this.#through
        : Math.max(0, end - 80);
      const events = start === this.#through ? [...this.#events] : [];
      let cursor = start;
      for (let page = 0; cursor < end && page < 10; page++) {
        const value = object(
          await this.backend.query({
            resource: "timeline",
            operation: "page",
            args: { afterSeq: cursor, throughSeq: end },
          }),
        );
        if (!Array.isArray(value.items) || !value.items.length) {
          throw new KernelError("corrupt", "实时记录窗口未能完整读取");
        }
        for (const raw of value.items) {
          const event = liveEvent(raw);
          if (event.seq !== cursor + 1 || event.seq > end) {
            throw new KernelError("corrupt", "实时记录顺序无效");
          }
          cursor = event.seq;
          events.push(event);
        }
      }
      if (cursor !== end) throw new KernelError("corrupt", "实时记录窗口超出读取上限");
      // 仅由窗口中的真实 start 关联原生随记回执；窗口外的孤立回执不猜测归属。
      const thoughtCalls = new Set(
        events.filter((e) => e.kind === "tool_start" && e.thoughtWrite && e.callId)
          .map((e) => e.callId!),
      );
      for (const event of events) {
        if (event.kind === "tool_result" && event.callId && thoughtCalls.has(event.callId)) {
          event.thoughtWrite = true;
        }
      }
      this.#events = events.slice(-80);
      this.#through = end;
    }
    const responses = [...this.#events].reverse().filter((e) => e.kind === "model_response");
    const thought = responses.find((e) => e.hasText) ?? responses[0];
    return {
      asOf: new Date().toISOString(),
      status,
      throughSeq: end,
      state: liveState(this.#events, status),
      thought: thought as unknown as Json ?? null,
      events: this.#events as unknown as Json,
      window: 80,
    };
  }
}
