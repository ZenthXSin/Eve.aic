import type { ToolSpec } from "../../contracts/model.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { ConsciousEvent } from "./journal.ts";
import { activeEvents, lastActiveEvent } from "./view.ts";

export const FOCUS_TOOL: ToolSpec = {
  name: "set_focus",
  description:
    "维护自己的近期重心，整体替换并持久保存；当前版本和原话每轮可见，跨休息、重启和压缩接续。由你选择少量在意的事，写明原因、状态与下一步；不等同于必须完成的系统任务。可搁置、完成、放弃或清空，不自动编任务。expectedSeq使用上下文里的focus.record，首次为0。无须每轮更新。",
  parameters: {
    type: "object",
    properties: {
      expectedSeq: { type: "integer", minimum: 0 },
      items: {
        type: "array",
        maxItems: 10,
        items: {
          type: "object",
          properties: {
            id: { type: "string", minLength: 1, maxLength: 64 },
            title: { type: "string", minLength: 1, maxLength: 300 },
            state: { type: "string", enum: ["active", "parked", "done", "dropped"] },
            reason: { type: "string", minLength: 1, maxLength: 1000 },
            next: { type: "string", minLength: 1, maxLength: 1000 },
          },
          required: ["id", "title", "state", "reason", "next"],
          additionalProperties: false,
        },
      },
    },
    required: ["expectedSeq", "items"],
    additionalProperties: false,
  },
};
export function focusArgs(args: Json) {
  if (
    !args || typeof args !== "object" || Array.isArray(args) ||
    Object.keys(args).some((k) => !["expectedSeq", "items"].includes(k)) ||
    !Number.isSafeInteger(args.expectedSeq) || Number(args.expectedSeq) < 0 ||
    !Array.isArray(args.items) || args.items.length > 10
  ) {
    throw new KernelError("rejected", "近期重心需要当前版本和最多10项内容");
  }
  const ids = new Set<string>();
  for (const item of args.items) {
    if (
      !item || typeof item !== "object" || Array.isArray(item) ||
      Object.keys(item).some((k) => !["id", "title", "state", "reason", "next"].includes(k)) ||
      !["active", "parked", "done", "dropped"].includes(String(item.state)) ||
      !Object.entries({ id: 64, title: 300, reason: 1000, next: 1000 }).every(([k, n]) =>
        typeof item[k] === "string" && !!(item[k] as string).trim() &&
        (item[k] as string).length <= n
      ) || ids.has(String(item.id))
    ) {
      throw new KernelError("rejected", "每项重心需要唯一id、标题、状态、原因和下一步");
    }
    ids.add(String(item.id));
  }
  return args as {
    expectedSeq: number;
    items: { id: string; title: string; state: string; reason: string; next: string }[];
  };
}
export const REST_TOOL: ToolSpec = {
  name: "rest",
  description:
    "睡前收尾并安排休息。先结合近期重心、答应的事、未读通知、真实任务回执决定是否还有想做的事；可以带着未完成事项休息。用简短判断填写原因、未完成事项与醒后下一步，不展开内部推演。此工具须为本轮唯一工具；无需再调用mind.rest，也不发送QQ消息。决定持久保存，处理完已到达的新通知后才休息；新消息和任务完成仍可唤醒。",
  parameters: {
    type: "object",
    properties: {
      reason: { type: "string", minLength: 1, maxLength: 2000 },
      unfinished: {
        type: "array",
        items: { type: "string", minLength: 1, maxLength: 2000 },
        maxItems: 20,
      },
      resume: { type: "string", minLength: 1, maxLength: 2000 },
      wakeAt: { type: "string", description: "可选ISO时间；省略沿用默认休息时长" },
    },
    required: ["reason", "unfinished", "resume"],
    additionalProperties: false,
  },
};
export function restArgs(args: Json) {
  if (
    !args || typeof args !== "object" || Array.isArray(args) ||
    Object.keys(args).some((k) => !["reason", "unfinished", "resume", "wakeAt"].includes(k))
  ) {
    throw new KernelError("rejected", "休息需要原因、未完成事项和醒后下一步");
  }
  const text = (v: Json | undefined) => typeof v === "string" && !!v.trim() && v.length <= 2000;
  if (
    !text(args.reason) || !text(args.resume) || !Array.isArray(args.unfinished) ||
    args.unfinished.length > 20 || !args.unfinished.every(text) ||
    (args.wakeAt !== undefined &&
      (typeof args.wakeAt !== "string" || !Number.isFinite(Date.parse(args.wakeAt)) ||
        Date.parse(args.wakeAt) <= Date.now()))
  ) {
    throw new KernelError(
      "rejected",
      "休息参数无效；需要简短原因、未完成事项数组、醒后下一步及可选未来时间",
    );
  }
  return args as { reason: string; unfinished: string[]; resume: string; wakeAt?: string };
}
export function pendingRestReview(events: ConsciousEvent[]) {
  const boundary = activeEvents(events).findLast((e) =>
    e.kind === "rest" || e.kind === "rest_review"
  );
  return boundary?.kind === "rest_review" ? boundary : undefined;
}
export function continuityMessage(events: ConsciousEvent[], jobs: Map<string, { status: string }>) {
  const review = pendingRestReview(events);
  const rest = lastActiveEvent(events, "rest");
  const pending = [...jobs].filter(([, j]) =>
    !["completed", "failed", "cancelled", "unknown"].includes(j.status)
  );
  const unknown = [...jobs].filter(([, j]) => j.status === "unknown");
  return {
    role: "user" as const,
    content: "当前生活接续（任务状态是回执，不代表必须继续工作；unknown不可盲目重跑）：\n" +
      JSON.stringify({
        focus: (() => {
          const focus = lastActiveEvent(events, "focus");
          return {
            record: focus?.seq ?? 0,
            items: (focus?.data as { items?: Json } | undefined)?.items ?? [],
            guidance:
              "这是自己维护的近期重心，可用set_focus改变状态和下一步；没有时不编造，也不要求填满。",
          };
        })(),
        pendingJobs: pending.map(([jobId, job]) => ({ jobId, status: job.status })),
        unknownJobCount: unknown.length,
        recentUnknownJobs: unknown.slice(-5).map(([jobId]) => jobId),
        resting:
          "准备休息时优先以rest作为本轮唯一工具，简短留下原因、未完成事项和醒后下一步；需要先行动就调用其他工具，不必每轮汇报或改写重心。",
        previousRest: rest
          ? { record: rest.seq, at: rest.at, ...rest.data as Record<string, Json> }
          : null,
        ...(review
          ? {
            beforeRest: {
              record: review.seq,
              request: review.data,
              guidance:
                "刚才准备休息。现在核对近期重心、对人的承诺、未读通知和任务回执：继续就行动，搁置就记清原因与下一步。仍想休息可调用rest一次收尾，或用简短文字留下决定；不会因有待办强迫你继续，也不会无休止追加检查。",
            },
          }
          : {}),
      }),
  };
}
