import type { Json } from "../kernel/protocol.ts";

export const MIND = "mind";
/** 意识流内可选的休息与提醒动作；宿主给每个请求稳定编号。 */
export interface MindApi {
  rest(args: { wakeAt?: string; text?: string }): Promise<{ requestId: string; queued: true }>;
  remind(args: { at: string; text: string }): Promise<{ requestId: string; queued: true }>;
  /** 无模型调用的条件提醒。code 是 async 函数体，return boolean；可读 facts 与只读 tools。
   * tools: files.read/list/search、commands.poll、notes.get/list/search、records.page/text。
   * 检查使用隔离 worker；失败保留提醒并在 status 中报告，下次继续检查。 */
  watch(
    args: { code: string; text: string; everyMs?: number },
  ): Promise<{ requestId: string; queued: true }>;
  cancelReminder(args: { reminderId: string }): Promise<{ requestId: string; queued: true }>;
  compress(
    args: { throughSeq: number; summary: string },
  ): Promise<{ requestId: string; queued: true }>;
  retain(
    args: {
      responseSeq: number;
      toolIndex: number;
      mode: "raw" | "summary" | "omit";
      summary?: string;
    },
  ): Promise<{ requestId: string; queued: true }>;
  status(): Promise<Json>;
}
export interface MindEvent {
  seq: number;
  requestId: string;
  kind:
    | "rest"
    | "remind"
    | "watch"
    | "cancelReminder"
    | "compress"
    | "retain"
    | "alarm"
    | "condition_error"
    | "condition_recovered";
  args: Json;
  at: string;
}
