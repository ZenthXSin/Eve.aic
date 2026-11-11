import type { Json } from "../kernel/protocol.ts";
import type { OwnershipProof } from "./ownership.ts";

/** 与适配器通讯的窄接口；认证信息不在工具参数、返回值或模型上下文里。 */
export const PHONE_METHODS = [
  "conversations",
  "open",
  "history",
  "message",
  "text",
  "search",
  "notifications",
  "send",
  "action",
] as const;
export type PhoneMethod = typeof PHONE_METHODS[number];
export interface PhoneRpcRequest {
  method: PhoneMethod;
  args: Json;
  /** IPC 信封；由手机组合层加入，不属于 Eve 的工具参数。 */
  ownership?: { proof: OwnershipProof; actionId: string };
}
export type PhoneRpcResponse =
  | { ok: true; value: Json }
  | { ok: false; error: { category: string } };
