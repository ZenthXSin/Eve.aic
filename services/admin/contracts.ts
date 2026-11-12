import { type Json, KernelError } from "../../kernel/protocol.ts";

export const ADMIN_READS = {
  timeline: ["page", "get", "search"],
  notes: ["page", "get", "versions", "search"],
  memory: ["page", "get", "search"],
  receipts: ["page", "get", "search"],
  qqFailures: ["page"],
} as const;

export interface AdminQuery {
  resource: keyof typeof ADMIN_READS;
  operation: "page" | "get" | "versions" | "search";
  args: Record<string, Json>;
}

export type AdminControl =
  | { action: "pause" | "resume"; args: Record<string, never> }
  | { action: "resumeModel"; args: { expectedRequestId: string } }
  | { action: "resolveQqFailure"; args: ResolveFailedQq }
  | { action: "budget"; args: { dailyTokenBudget: number } };

export interface ResolveFailedQq {
  expectedActionId: string;
  expectedFingerprint: string;
  expectedReceiptDigest: string;
}

export function validateFailedResolution(value: Record<string, Json>): ResolveFailedQq {
  if (
    Object.keys(value).length !== 3 || typeof value.expectedActionId !== "string" ||
    !/^[a-zA-Z0-9_-]{1,128}$/.test(value.expectedActionId) ||
    typeof value.expectedFingerprint !== "string" ||
    !/^[a-f0-9]{64}$/.test(value.expectedFingerprint) ||
    typeof value.expectedReceiptDigest !== "string" ||
    !/^[a-f0-9]{64}$/.test(value.expectedReceiptDigest)
  ) throw new KernelError("rejected", "结束失败项需要精确动作编号、指纹与原始回执摘要");
  return value as unknown as ResolveFailedQq;
}

/** 仅由进程管理者注入。返回值必须是可展示数据，不含任何连接凭据、私钥或环境变量。 */
export interface AdminBackend {
  status(): Promise<Json>;
  query(request: AdminQuery): Promise<Json>;
  control(request: AdminControl): Promise<Json>;
}

export type AdminRequest =
  | { method: "status" }
  | { method: "query"; request: AdminQuery }
  | { method: "control"; request: AdminControl };
