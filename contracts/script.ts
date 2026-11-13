import type { AgentApi } from "./agents.ts";
import type { LegacyReply } from "./phone-compat.ts";
import type { Json } from "../kernel/protocol.ts";
import type {
  PhoneActionReceipt,
  PhoneApi,
  PhoneBurstReceipt,
  PhoneSend,
  PhoneSendBurst,
  PhoneSendFile,
} from "./phone.ts";
import type { MindApi } from "./mind.ts";
import type { NoteApi } from "./notes.ts";
import type { CommandApi, FileApi } from "./workspace.ts";
import type { MemoryApi, RecordApi } from "./memory.ts";
import type { UpgradeApi } from "./upgrades.ts";
import type { WebApi } from "./web.ts";

export const SCRIPTS = "scripts";
/** 固定源码能力标记；可信提示生成器只读取此声明，不执行候选代码。 */
export const NATIVE_THOUGHT_CAPABILITY_VERSION = 1;
export const WEB_CAPABILITY_VERSION = 1;
/** 默认工具等待上限与后台续行能力，用于准确复原旧版本生成提示。 */
export const SCRIPT_AUTO_WAIT_CAPABILITY_VERSION = 1;
export const AGENT_CAPABILITY_VERSION = 1;
export const CONTRACT_LOOKUP_CAPABILITY_VERSION = 1;
/** 原生随记工具 write_thought 的参数，不属于脚本 tools 对象中的接口。 */
export interface NativeThoughtArguments {
  thought: string;
}
/** 原生 execute_script 的输入；thought 留在意识流，不传入脚本任务。 */
export type NativeScriptArguments = Omit<ScriptRequest, "requestId"> & {
  thought?: string;
};
export interface ScriptRequest {
  /** 原生工具调用的真实编号；意识流须先保存，不由重试生成新编号。 */
  requestId: string;
  code: string;
  /** 默认/同步最多等待5秒，随后后台继续；async立即返回；等待结束不会取消任务。 */
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
  agents: AgentApi;
  web: WebApi;
  upgrades: UpgradeApi;
  notes: NoteApi;
  files: FileApi;
  commands: CommandApi;
  memory: MemoryApi;
  records: RecordApi;
  mind: MindApi;
  phone: Omit<PhoneApi, "send" | "sendFile" | "sendBurst"> & {
    /** actionId由宿主从持久脚本编号与调用序号产生，不让代码重试制造新动作。 */
    send(args: Omit<PhoneSend, "actionId"> & LegacyReply): Promise<PhoneActionReceipt>;
    sendFile(args: Omit<PhoneSendFile, "actionId"> & LegacyReply): Promise<PhoneActionReceipt>;
    sendBurst(args: Omit<PhoneSendBurst, "actionId">): Promise<PhoneBurstReceipt>;
  };
  scripts: {
    /** 按需读取完整类型原件；按Unicode码点分页，不改变执行能力。 */
    contracts(
      args: { name: "phone-details" | "agents"; startChar?: number; limit?: number },
    ): Promise<{ text: string; characters: number; nextChar?: number }>;
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
