import type { ToolSpec } from "../../contracts/model.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { ConsciousEvent } from "./journal.ts";
import { lastActiveEvent } from "./view.ts";

const feelings = [
  "自在",
  "高兴",
  "生气",
  "难过",
  "担心",
  "亲近",
  "反感",
  "期待",
  "好奇",
  "无聊",
  "疲倦",
  "复杂",
];
const desires = [
  "凑热闹",
  "独处",
  "玩一会",
  "创作",
  "弄明白",
  "陪伴",
  "做完手头的事",
  "休息",
  "暂时没想法",
];

export const MOMENT_TOOL: ToolSpec = {
  name: "keep_moment",
  description:
    "可选：当自己的心情或眼下想做的事确实变化，留个短暂的记号（不是日记任务）。由你选，不替别人定义情绪。只表示此刻倾向，不证明做过、也不触发发送、巡群、派工或定时唤醒。",
  parameters: {
    type: "object",
    additionalProperties: false,
    properties: {
      feeling: { type: "string", enum: feelings },
      wish: { type: "string", enum: desires },
      interest: {
        type: "string",
        maxLength: 72,
        description: "可选：自己此刻想碰的一个具体话题，不写私聊原句或他人的身份",
      },
      minutes: { type: "integer", minimum: 5, maximum: 360 },
      expectedSeq: { type: "integer", minimum: 0 },
    },
    required: ["feeling", "wish", "minutes", "expectedSeq"],
  },
};

export function momentContext(events: ConsciousEvent[], now = Date.now()) {
  const last = lastActiveEvent(events, "life_moment");
  if (!last) return { revision: 0 };
  const value = last.data as Record<string, Json>;
  if (
    typeof value.until !== "number" || !Number.isFinite(value.until) || now >= value.until ||
    !feelings.includes(String(value.feeling)) || !desires.includes(String(value.wish))
  ) return { revision: last.seq };
  return {
    revision: last.seq,
    feeling: value.feeling,
    wish: value.wish,
    ...(typeof value.interest === "string" && value.interest ? { interest: value.interest } : {}),
  };
}

export function planMoment(raw: Json, events: ConsciousEvent[], callId: string, now = Date.now()) {
  const repeat = lastActiveEvent(
    events,
    "life_moment",
    (event) => (event.data as { callId?: string }).callId === callId,
  );
  if (repeat) return { existing: repeat.seq } as const;
  if (
    !raw || typeof raw !== "object" || Array.isArray(raw) ||
    Object.keys(raw).some((key) =>
      !["feeling", "wish", "interest", "minutes", "expectedSeq"].includes(key)
    ) ||
    !feelings.includes(String(raw.feeling)) || typeof raw.feeling !== "string" ||
    !desires.includes(String(raw.wish)) || typeof raw.wish !== "string" ||
    !Number.isSafeInteger(raw.minutes) || Number(raw.minutes) < 5 || Number(raw.minutes) > 360 ||
    (raw.interest !== undefined &&
      (typeof raw.interest !== "string" || [...raw.interest].length > 72 ||
        /[\p{Cc}]/u.test(raw.interest)))
  ) {
    throw new KernelError("rejected", "当下感受参数无效");
  }
  const current = momentContext(events, now),
    interest = (raw.interest as string | undefined)?.trim() || "";
  if (raw.expectedSeq !== current.revision) {
    throw new KernelError("rejected", "当下状态已变化，请用当前版本");
  }
  if (
    "feeling" in current && current.feeling === raw.feeling && current.wish === raw.wish &&
    (current.interest ?? "") === interest
  ) return { existing: current.revision } as const;
  return {
    event: {
      callId,
      feeling: raw.feeling,
      wish: raw.wish,
      interest,
      until: now + Number(raw.minutes) * 60_000,
    },
  } as const;
}
