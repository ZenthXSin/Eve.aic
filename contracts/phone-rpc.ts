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
  "sendBurst",
  "burstStatus",
  "waitForMessages",
  "action",
  "listSendQueue",
  "cancelSend",
  "settings",
  "download",
  "image",
  "file",
  "sendFile",
] as const;
export type PhoneMethod = typeof PHONE_METHODS[number];

/** 仅允许稳定原因码及 HTTP 状态；不携带资源 URL、签名或远端响应原文。 */
export interface PhoneAttachmentFailure {
  operation: "attachment_download";
  reason:
    | "source_rejected"
    | "redirect_rejected"
    | "redirect_invalid"
    | "connection_failed"
    | "http_rejected"
    | "empty_response"
    | "timeout"
    | "closed"
    | "body_incomplete"
    | "too_large"
    | "storage_failed";
  httpStatus?: number;
}
export function phoneAttachmentFailure(value: unknown): PhoneAttachmentFailure | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  const data = value as Record<string, unknown>;
  if (
    data.operation !== "attachment_download" || typeof data.reason !== "string" ||
    ![
      "source_rejected",
      "redirect_rejected",
      "redirect_invalid",
      "connection_failed",
      "http_rejected",
      "empty_response",
      "timeout",
      "closed",
      "body_incomplete",
      "too_large",
      "storage_failed",
    ].includes(String(data.reason)) ||
    Object.keys(data).some((key) => !["operation", "reason", "httpStatus"].includes(key)) ||
    (data.httpStatus !== undefined &&
      (data.reason !== "http_rejected" || !Number.isSafeInteger(data.httpStatus) ||
        Number(data.httpStatus) < 100 || Number(data.httpStatus) > 599))
  ) return undefined;
  return {
    operation: "attachment_download",
    reason: data.reason as PhoneAttachmentFailure["reason"],
    ...(data.httpStatus === undefined ? {} : { httpStatus: Number(data.httpStatus) }),
  };
}
export interface PhoneRpcRequest {
  method: PhoneMethod;
  args: Json;
  /** IPC 信封；由手机组合层加入，不属于 Eve 的工具参数。 */
  ownership?: { proof: OwnershipProof; actionId: string };
}
export type PhoneRpcResponse =
  | { ok: true; value: Json }
  | { ok: false; error: { category: string; attachment?: PhoneAttachmentFailure } };
