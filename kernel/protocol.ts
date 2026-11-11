// 内核与插件 worker 之间的消息协议，以及全系统共用的错误类型。

export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };

/** 调用失败的种类。remote 表示对方处理器抛错；其余由内核判定。 */
export type ErrorKind =
  | "remote" // 提供方的处理器执行了并抛出错误
  | "timeout" // 在期限内没有结果，副作用未知
  | "unavailable" // 提供方不在运行，处理器没有执行
  | "not_found" // 没有这个契约或方法
  | "rejected" // 内核在执行前拒绝（未声明依赖、清单不合法等）
  | "corrupt" // 持久化数据损坏，已保留原样
  | "storage" // 提交失败，不能宣称状态已经保存
  | "unknown"; // 已开始执行但结果未知，不能自动重放

export class KernelError extends Error {
  constructor(readonly kind: ErrorKind, message: string) {
    super(message);
    this.name = "KernelError";
  }
}

export interface WireError {
  kind: ErrorKind;
  message: string;
}

export function toWireError(error: unknown): WireError {
  if (error instanceof KernelError) return { kind: error.kind, message: error.message };
  if (error instanceof Error) return { kind: "remote", message: `${error.name}: ${error.message}` };
  return { kind: "remote", message: String(error) };
}

export interface Permissions {
  /** 允许访问的网络主机，默认无。 */
  net?: string[];
  /** 允许读取的绝对路径，默认只有自己的代码目录与公共契约。 */
  read?: string[];
  /** 允许写入的绝对路径，默认无。 */
  write?: string[];
  /** 明确授予可执行文件；脚本类型检查需要指定的 Deno，不授予任意 shell。 */
  run?: string[];
}

export interface Deadlines {
  loadMs: number;
  enableMs: number;
  probeMs: number;
  disableMs: number;
}

export interface ConfirmPolicy {
  /** 需要多少次成功的真实调用才算确认。 */
  calls: number;
  /** 确认期限；到期且没有真实流量时由内核主动探测。 */
  deadlineMs: number;
}

export interface Manifest {
  name: string;
  version: string;
  entry: string;
  /** 状态格式版本；与持久化状态不一致且插件没有 migrate 时拒绝加载。 */
  stateVersion: number;
  provides: Record<string, string>;
  requires: Record<string, string>;
  optional: Record<string, string>;
  permissions: Permissions;
  deadlines: Deadlines;
  confirm: ConfirmPolicy;
}

export type ToWorker =
  | {
    t: "init";
    manifest: Manifest;
    entry: string;
    state: Json | undefined;
    stateVersion: number | undefined;
  }
  | { t: "enable" }
  | { t: "disable" }
  | { t: "call"; id: number; contract: string; method: string; args: Json }
  | { t: "reply"; id: number; ok: true; value: Json }
  | { t: "reply"; id: number; ok: false; error: WireError }
  | { t: "probe"; id: number }
  | { t: "flush"; id: number };
// 状态提交回执与具体 setState 对应，不能用收到消息代替保存完成。
export type StateReply =
  | { t: "state_saved"; id: number; ok: true }
  | { t: "state_saved"; id: number; ok: false; error: WireError };

export type FromWorker =
  | { t: "loaded"; provides: string[] }
  | { t: "enabled" }
  | { t: "disabled" }
  | { t: "failed"; stage: string; message: string }
  | { t: "result"; id: number; ok: true; value: Json }
  | { t: "result"; id: number; ok: false; error: WireError }
  | {
    t: "call";
    id: number;
    contract: string;
    method: string;
    args: Json;
    timeoutMs?: number;
    idempotencyKey?: string;
  }
  | { t: "probed"; id: number; ok: boolean; message?: string }
  | { t: "flushed"; id: number }
  | { t: "state"; id: number; data: Json }
  | { t: "log"; message: string };
