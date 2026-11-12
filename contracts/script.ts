import type { Json } from "../kernel/protocol.ts";
import type { PhoneActionReceipt, PhoneApi, PhoneSend, PhoneSendFile } from "./phone.ts";
import type { MindApi } from "./mind.ts";
import type { NoteApi } from "./notes.ts";
import type { CommandApi, FileApi } from "./workspace.ts";
import type { MemoryApi, RecordApi } from "./memory.ts";
import type { UpgradeApi } from "./upgrades.ts";

export const SCRIPTS = "scripts";
export interface ScriptRequest {
  /** 原生工具调用的真实编号；意识流须先保存，不由重试生成新编号。 */
  requestId: string;
  code: string;
  mode?: "async" | "sync";
  timeoutMs?: number;
}
export type ScriptStatus =
  | "queued"
  | "checking"
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "unknown";
export interface ScriptSummary {
  jobId: string;
  status: ScriptStatus;
  scriptStarted: boolean;
  actions: number;
  startedAt: string;
  finishedAt?: string;
  error?: { kind: string; message: string };
}
export interface ScriptApi {
  upgrades: UpgradeApi;
  notes: NoteApi;
  files: FileApi;
  commands: CommandApi;
  memory: MemoryApi;
  records: RecordApi;
  mind: MindApi;
  phone: Omit<PhoneApi, "send" | "sendFile"> & {
    /** actionId由宿主从持久脚本编号与调用序号产生，不让代码重试制造新动作。 */
    send(args: Omit<PhoneSend, "actionId">): Promise<PhoneActionReceipt>;
    sendFile(args: Omit<PhoneSendFile, "actionId">): Promise<PhoneActionReceipt>;
  };
  scripts: {
    status(args: { jobId: string }): Promise<ScriptSummary>;
    cancel(args: { jobId: string }): Promise<ScriptSummary>;
    result(args: { jobId: string; startChar?: number; limit?: number }): Promise<{
      text: string;
      characters: number;
      truncated: boolean;
      nextChar?: number;
    }>;
    receipt(
      args: {
        jobId: string;
        seq: number;
        section: "args" | "result" | "error";
        startChar?: number;
        limit?: number;
      },
    ): Promise<{
      text: string;
      characters: number;
      truncated: boolean;
      nextChar?: number;
    }>;
  };
}
export interface ScriptCompletion {
  seq: number;
  job: ScriptSummary;
}
export interface ScriptHostApi {
  start(args: ScriptRequest): Promise<ScriptSummary>;
  status(args: { jobId: string }): Promise<ScriptSummary>;
  cancel(args: { jobId: string }): Promise<ScriptSummary>;
  result(args: { jobId: string; startChar?: number; limit?: number }): Promise<{
    text: string;
    characters: number;
    truncated: boolean;
    nextChar?: number;
  }>;
  completions(args?: { afterSeq?: number }): Promise<{
    items: ScriptCompletion[];
    throughSeq: number;
    hasMore: boolean;
  }>;
  receipts(args: { jobId: string; afterSeq?: number }): Promise<{
    items: {
      seq: number;
      actionId: string;
      namespace: string;
      method: string;
      status: string;
      error?: Json;
    }[];
    throughSeq: number;
    hasMore: boolean;
  }>;
  receipt(
    args: {
      jobId: string;
      seq: number;
      section: "args" | "result" | "error";
      startChar?: number;
      limit?: number;
    },
  ): Promise<{
    text: string;
    characters: number;
    truncated: boolean;
    nextChar?: number;
  }>;
}
