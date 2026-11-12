import type { Json } from "../kernel/protocol.ts";
import type { OwnershipProof } from "./ownership.ts";

/** 设备/存储/执行出口独立于内核存活；模型上下文不接触 IPC 凭据。 */
export interface RuntimeRequest {
  namespace: string;
  method: string;
  args: Json;
  ownership: { proof: OwnershipProof; actionId: string };
}
export const RUNTIME_METHODS = {
  timeline: ["page", "append"],
  records: ["page", "text"],
  notes: ["create", "update", "get", "list", "versions", "search"],
  files: ["read", "write", "replace", "list", "search"],
  commands: ["start", "poll", "write", "cancel", "completions"],
  memory: ["remember", "recall", "used", "list", "get"],
  model: ["complete", "result", "budget", "describeImage"],
} as const;
