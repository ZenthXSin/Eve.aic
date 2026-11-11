// 幂等调用的事实记录与编号。状态快照回滚不回滚已执行的动作。
import { type Json, KernelError, type WireError } from "./protocol.ts";

export interface CallRecord {
  schema: 1;
  id: string;
  caller: string;
  contract: string;
  method: string;
  key: string;
  request: string;
  status: "started" | "completed" | "failed" | "unknown";
  updatedAt: string;
  value?: Json;
  error?: WireError;
}

export function canonical(value: Json): string {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number" && Number.isFinite(value)) return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (typeof value === "object" && value !== null) {
    return `{${
      Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`)
        .join(",")
    }}`;
  }
  throw new KernelError("rejected", "调用参数必须是有限 JSON 值");
}

export async function fingerprint(value: Json): Promise<string> {
  const bytes = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical(value)));
  return Array.from(new Uint8Array(bytes), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function validateCallRecord(value: unknown): value is CallRecord {
  if (!value || typeof value !== "object") return false;
  const r = value as CallRecord;
  if (
    r.schema !== 1 || !/^[a-f0-9]{64}$/.test(r.id) || !/^[a-f0-9]{64}$/.test(r.request) ||
    [r.caller, r.contract, r.method, r.key, r.updatedAt].some((v) => typeof v !== "string" || !v) ||
    !["started", "completed", "failed", "unknown"].includes(r.status)
  ) return false;
  if (r.status === "completed") {
    if (!("value" in r) || r.error) return false;
    try {
      canonical(r.value!);
    } catch {
      return false;
    }
  } else if (r.status !== "started") {
    if (
      !r.error || typeof r.error.message !== "string" ||
      ![
        "remote",
        "timeout",
        "unknown",
        "unavailable",
        "not_found",
        "rejected",
        "corrupt",
        "storage",
      ].includes(r.error.kind)
    ) return false;
  }
  return true;
}
