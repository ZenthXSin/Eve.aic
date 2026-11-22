import type { ToolSpec } from "../../contracts/model.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { ContinuityJob, ScriptReport } from "./job-observations.ts";
import type { ConsciousEvent } from "./journal.ts";
import { activeEvents, lastActiveEvent } from "./view.ts";
import { socialContext } from "./social.ts";
import { momentContext } from "./life.ts";
import { lanternContext } from "./lanterns.ts";

export interface FocusItem {
  id: string;
  title: string;
  state: string;
  reason: string;
  next: string;
  origin?: "self" | "owner" | "shared";
  confidence?: number;
  evidence?: number[];
  expiresAt?: string;
  feedback?: {
    kind: "welcome" | "declined" | "redirected" | "uncertain";
    note: string;
    message: { conversationId: string; seq: number };
  };
}

export const FOCUS_TOOL: ToolSpec = {
  name: "set_focus",
  description:
    "维护少量近期重心，整体替换并持久保存；跨休息、重启和压缩接续。可选origin区分自己的兴趣、主人委托与共同话题；evidence引用真实意识记录编号，confidence表达把握，expiresAt表示过期。feedback只记明确针对这一项的反馈及真实入站消息句柄，不把别处一句‘好’或沉默算作赞同；重复旧依据不能提高把握。可搁置、完成、放弃或清空，不自动编任务。expectedSeq用focus.record，首次0，无变化不更新。",
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
            origin: { type: "string", enum: ["self", "owner", "shared"] },
            confidence: { type: "number", minimum: 0, maximum: 1 },
            evidence: {
              type: "array",
              maxItems: 8,
              items: { type: "integer", minimum: 1 },
            },
            expiresAt: { type: "string", description: "可选ISO过期时间" },
            feedback: {
              type: "object",
              properties: {
                kind: { type: "string", enum: ["welcome", "declined", "redirected", "uncertain"] },
                note: { type: "string", minLength: 1, maxLength: 500 },
                message: {
                  type: "object",
                  properties: {
                    conversationId: { type: "string", minLength: 1, maxLength: 256 },
                    seq: { type: "integer", minimum: 1 },
                  },
                  required: ["conversationId", "seq"],
                  additionalProperties: false,
                },
              },
              required: ["kind", "note", "message"],
              additionalProperties: false,
            },
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
      Object.keys(item).some((k) =>
        ![
          "id",
          "title",
          "state",
          "reason",
          "next",
          "origin",
          "confidence",
          "evidence",
          "expiresAt",
          "feedback",
        ].includes(k)
      ) ||
      !["active", "parked", "done", "dropped"].includes(String(item.state)) ||
      !Object.entries({ id: 64, title: 300, reason: 1000, next: 1000 }).every(([k, n]) =>
        typeof item[k] === "string" && !!(item[k] as string).trim() &&
        (item[k] as string).length <= n
      ) || ids.has(String(item.id))
    ) {
      throw new KernelError("rejected", "每项重心需要唯一id、标题、状态、原因和下一步");
    }
    ids.add(String(item.id));
    if (
      item.origin !== undefined && !["self", "owner", "shared"].includes(String(item.origin)) ||
      item.confidence !== undefined &&
        (typeof item.confidence !== "number" || !Number.isFinite(item.confidence) ||
          item.confidence < 0 || item.confidence > 1) ||
      item.evidence !== undefined && (!Array.isArray(item.evidence) || item.evidence.length > 8 ||
          !item.evidence.every((seq) => Number.isSafeInteger(seq) && Number(seq) > 0) ||
          new Set(item.evidence).size !== item.evidence.length) ||
      item.expiresAt !== undefined &&
        (typeof item.expiresAt !== "string" || !Number.isFinite(Date.parse(item.expiresAt)))
    ) throw new KernelError("rejected", "重心的来源、把握、依据或过期时间无效");
    if (item.feedback !== undefined) {
      const f = item.feedback;
      if (
        !f || typeof f !== "object" || Array.isArray(f) ||
        Object.keys(f).some((k) => !["kind", "note", "message"].includes(k)) ||
        !["welcome", "declined", "redirected", "uncertain"].includes(String(f.kind)) ||
        typeof f.note !== "string" || !f.note.trim() || f.note.length > 500 ||
        !f.message || typeof f.message !== "object" || Array.isArray(f.message) ||
        Object.keys(f.message).some((k) => !["conversationId", "seq"].includes(k)) ||
        typeof f.message.conversationId !== "string" || !f.message.conversationId.trim() ||
        f.message.conversationId.length > 256 || !Number.isSafeInteger(f.message.seq) ||
        Number(f.message.seq) < 1
      ) throw new KernelError("rejected", "反馈需要明确判断、简短说明及真实消息句柄");
    }
  }
  return args as unknown as { expectedSeq: number; items: FocusItem[] };
}

export function validateFocusEvidence(items: FocusItem[], events: ConsciousEvent[]) {
  const previous =
    (lastActiveEvent(events, "focus")?.data as unknown as { items: FocusItem[] })?.items ?? [];
  for (const item of items) {
    for (const seq of item.evidence ?? []) {
      if (!lastActiveEvent(events, events[seq - 1]?.kind ?? "", (e) => e.seq === seq)) {
        throw new KernelError("rejected", "重心依据需要未被回滚的真实意识记录编号");
      }
    }
    const old = previous.find((p) => p.id === item.id);
    const newEvidence = item.evidence?.some((seq) => !old?.evidence?.includes(seq));
    const newFeedback = item.feedback && (!old?.feedback ||
      item.feedback.message.conversationId !== old.feedback.message.conversationId ||
      item.feedback.message.seq !== old.feedback.message.seq);
    if (
      old?.confidence !== undefined && (item.confidence ?? old.confidence) > old.confidence &&
      !newEvidence && !newFeedback
    ) {
      throw new KernelError("rejected", "重复旧依据不能提高把握；有新经历或明确新反馈再调整");
    }
  }
}
export const REST_TOOL: ToolSpec = {
  name: "rest",
  description:
    "收尾并安排休息。先看能兑现的承诺和手边真想接的事；没人找你也可以看熟群的新话题、继续兴趣或玩一会，不必先找工作。工具失败先读具体回执并恢复。需要休息时留下具体原因、未完事项与醒后可执行的一步；省略wakeAt最多默认休息10分钟，可明确安排更长时间。此工具须为本轮唯一工具；无需mind.rest，不发送QQ。决定持久保存，新消息和任务完成仍可唤醒。",
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
      wakeAt: { type: "string", description: "可选未来ISO时间；省略默认最多10分钟后再看" },
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
  const active = activeEvents(events);
  for (let i = active.length - 1; i >= 0; i--) {
    const event = active[i];
    if (event.kind === "rest_review") return event;
    // A review only covers the decision immediately following it. New actions,
    // failures or external events require a fresh decision, including after restart.
    if (
      ["rest", "wake", "tool_start", "tool_result", "model_failure", "compression", "retain"]
        .includes(event.kind) ||
      event.kind === "model_response" &&
        (event.data as unknown as { response: { toolCalls: unknown[] } }).response.toolCalls
          .length ||
      event.kind === "notice" && (() => {
          const data = event.data as { source?: string; items?: Json[] };
          return data.source !== "memory" &&
            (data.source === "reminder" || !!data.items?.length ||
              ["image", "image_description"].includes(data.source ?? ""));
        })() ||
      event.kind === "perception" &&
        !!(event.data as { contextMaintenance?: Json }).contextMaintenance
    ) return undefined;
  }
}
function recentToolFailures(events: ConsciousEvent[], afterSeq: number) {
  const failures: Json[] = [];
  const active = activeEvents(events);
  for (let i = active.length - 1; i >= 0 && failures.length < 3; i--) {
    const event = active[i];
    if (event.seq <= afterSeq) break;
    if (event.kind !== "tool_result") continue;
    const data = event.data as { callId: string; result: Record<string, Json> };
    let task: Record<string, Json> | undefined;
    try {
      const value = typeof data.result?.output === "string" ? JSON.parse(data.result.output) : null;
      if (typeof value?.jobId === "string" && typeof value.status === "string") task = value;
    } catch { /* Only decode documented script summaries, not arbitrary nested data. */ }
    const status = task?.status ?? data.result?.status;
    if (!["failed", "unknown"].includes(String(status))) continue;
    failures.push({
      record: event.seq,
      callId: data.callId,
      status,
      ...(task ? { jobId: task.jobId } : {}),
      error: task?.error ?? data.result?.error ?? null,
    } as Json);
  }
  return failures.reverse();
}
export function continuityMessage(
  events: ConsciousEvent[],
  jobs: Map<string, ContinuityJob> & { readonly scriptReports?: Map<string, ScriptReport> },
) {
  const review = pendingRestReview(events);
  const rest = lastActiveEvent(events, "rest");
  const pending = [...jobs].filter(([, j]) =>
    !["completed", "failed", "cancelled", "unknown"].includes(j.status)
  );
  const unknown = [...jobs].filter(([, j]) => j.status === "unknown");
  return {
    role: "user" as const,
    content: "当前生活接续（任务状态仅为已保存的观察，不是实时状态；unknown不可盲目重跑）：\n" +
      JSON.stringify({
        focus: (() => {
          const focus = lastActiveEvent(events, "focus");
          return {
            record: focus?.seq ?? 0,
            items: ((focus?.data as { items?: FocusItem[] } | undefined)?.items ?? [])
              .filter((item) => item.state === "active" || item.state === "parked"),
            guidance:
              "自己维护的近期重心，不要求填满。自己的兴趣与别人委托分开；新反馈只调整被明确提到的那一项，旧依据不重复强化。过期、没人接或失去兴趣可搁置，完成或放弃后不反复重提。",
          };
        })(),
        pendingJobs: pending.map(([jobId, job]) => ({
          jobId,
          status: job.status,
          observation: job.observation ?? null,
        })),
        scriptReports: [...(jobs.scriptReports?.values() ?? [])],
        scriptReportGuidance:
          "scriptReports仅为脚本自述、未经服务端证明的线索；脚本永不建立completed/failed/cancelled/unknown，不据此退休任务或更新pendingJobs状态。完整脚本返回只可补充已跟踪任务的queued/checking/running展示信息，仍受终态/unknown冻结和防回退保护；未跟踪或拿不准的值只作报告，agentId/requestSeq不证明来源。",
        jobStatusGuidance:
          "pendingJobs是最近已保存的任务观察，不代表必须继续工作。queued可能仅为派工准入回执，不代表仍在排队；observation.record/recordedAt是意识记录编号和回执保存时刻，不是实时快照时刻，null表示来源时刻缺失。只利用已有完整任务回执更新，不猜进度或子任务状态，不为刷新展示轮询。",
        unknownJobCount: unknown.length,
        ...socialContext(events),
        ...(() => {
          const moment = momentContext(events), lanterns = lanternContext(events);
          return {
            ...("feeling" in moment ? { moment } : {}),
            ...(lanterns.status === "playing" ? { lanterns } : {}),
          };
        })(),
        leisure:
          "醒来有空、没急事，这次还没看群时，通常先挑一个常聊且未静音的群，看看有没有新鲜事。不等被点名，有趣就接，真惦记后续就问一句具体的。没有新内容、很晚或想休息就跳过，不反复读同一屏、不催人回。空下来也可玩、续上次的兴趣或独处。不把意愿当成已做的事。",
        recentUnknownJobs: unknown.slice(-5).map(([jobId]) => jobId),
        recentToolFailures: recentToolFailures(events, rest?.seq ?? 0).map((item) => {
          const failure = item as Record<string, Json>;
          return {
            ...failure,
            error: typeof failure.error === "string" && failure.error.length > 160
              ? failure.error.slice(0, 160) + "..."
              : failure.error,
          };
        }),
        failureRecovery:
          "调用完成不等于任务成功；以task.status和具体error为准。参数拒绝先查契约并修正，不把单个失败当成整条链路坏了。失败脚本可能已经执行部分动作，先查原jobId/actionId回执；unknown不可重放。仍可完成的部分继续或委派，确实受阻才记录原因、下一步与合理唤醒时间。",
        ownActivity:
          "先兑现答应的事。空下来可以接着玩、找人聊、自己查一个好奇的东西，也可以推进一件真实重心；不必每次把空闲变成任务。等待只阻塞对应事项。你负责交流、管理和玩，全部执行任务交给子代理。想做自主改进时选一项实际问题，交代背景、复现与验收，派编码或通用子代理实施；交付后读证据、安排审查、提交升级并核对实际生效。用现有重心和便签保留进展和句柄，不重复轮询同一结果。",
        resting:
          "该兑现的承诺能推进就先处理。想暂停时以rest作为唯一工具，留下原因、未完成事项和醒后可做的一步；省略wakeAt最多10分钟后再看。不必为了休息先派工、汇报或改写重心。",
        previousRest: rest
          ? { record: rest.seq, at: rest.at, ...rest.data as Record<string, Json> }
          : null,
        ...(review
          ? {
            beforeRest: {
              record: review.seq,
              request: review.data,
              guidance:
                "刚才没有继续动作。看有没有没处理的承诺；没有的话，可以继续好奇的事、看看群里在聊什么，也可以休息。该整理时压缩并核实applied。继续就直接行动或委派，休息就记清原因与下一步；这次只核对一次，不无休止检查。",
            },
          }
          : {}),
      }),
  };
}
