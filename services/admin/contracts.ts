import type { Json } from "../../kernel/protocol.ts";

export const ADMIN_READS = {
  timeline: ["page", "get", "search"],
  notes: ["page", "get", "versions", "search"],
  memory: ["page", "get", "search"],
  receipts: ["page", "get", "search"],
} as const;

export interface AdminQuery {
  resource: keyof typeof ADMIN_READS;
  operation: "page" | "get" | "versions" | "search";
  args: Record<string, Json>;
}

export type AdminControl =
  | { action: "pause" | "resume"; args: Record<string, never> }
  | { action: "resumeModel"; args: { expectedRequestId: string } }
  | { action: "budget"; args: { dailyTokenBudget: number } };

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
