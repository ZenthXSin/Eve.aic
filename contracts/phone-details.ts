/** 发送与队列原始回执：sent才表示送达，unknown不自动重发；细节可用scripts.contracts读取。 */
export interface PhoneActionReceipt {
  actionId: string;
  state: "running" | "blocked" | "sent" | "failed" | "unknown" | "cancelled";
  /** 逻辑结束保留原始失败/未尝试记录；绝不表示已发送。 */
  resolution?: {
    kind: "failed_retained" | "unknown_retained" | "queued_cancelled";
    actor: "admin" | "eve";
    at: string;
  };
  /** 同会话未成功的队首动作；不得用新动作越过。 */
  blockedBy?: string;
  attempted?: boolean;
  /** 仅多次真实尝试时提供；仅在确认尚未提交或明确拒绝时自动重试原主动消息。 */
  attemptCount?: number;
  platformMessageId?: string;
  reason?: "interrupted";
  error?: {
    category: string;
    httpStatus?: number;
    code?: number;
    traceId?: string;
    /** 受控传输诊断，不含资源地址、凭据或原始异常文本。 */
    transportReason?: "timeout" | "aborted" | "network";
    /** 包括建连/等待响应头，或读取响应正文；不证明请求是否已经送达。 */
    phase?: "headers" | "body";
    elapsedMs?: number;
    retryAfterMs?: number;
    failureStage?: "auth" | "upload" | "message";
  };
}

export interface PhoneBurstReceipt {
  actionId: string;
  conversationId: string;
  /** 每条均为独立真实动作；unknown/blocked可用该子编号检查和取消。 */
  items: { index: number; actionId: string; receipt: PhoneActionReceipt }[];
  /** 只有所有子消息实际sent才为true；取消不算送达。 */
  complete: boolean;
}

export interface PhoneBurstStatus {
  actionId: string;
  conversationId: string;
  complete: boolean;
  items: { index: number; actionId: string; receipt: PhoneActionReceipt | null }[];
}
