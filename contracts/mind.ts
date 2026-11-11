import type { Json } from "../kernel/protocol.ts";

export const MIND = "mind";
/** 意识流内可选的休息与提醒动作；宿主给每个请求稳定编号。 */
export interface MindApi {
  rest(args: { wakeAt?: string; text?: string }): Promise<{ requestId: string; queued: true }>;
  remind(args: { at: string; text: string }): Promise<{ requestId: string; queued: true }>;
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
  kind: "rest" | "remind" | "cancelReminder" | "compress" | "retain" | "alarm";
  args: Json;
  at: string;
}
