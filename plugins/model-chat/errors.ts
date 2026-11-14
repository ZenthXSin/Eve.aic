export class ChatModelError extends Error {
  constructor(
    readonly category:
      | "configuration"
      | "request"
      | "transport"
      | "http"
      | "provider_rejected"
      | "invalid_response",
    readonly httpStatus?: number,
    readonly timeoutPhase?: "connect" | "first_token" | "idle" | "total",
  ) {
    // 服务端正文、URL 和 fetch 原文可能含凭据，不写入错误消息或 cause。
    super(
      `模型请求未完成：${category}${httpStatus === undefined ? "" : ` (${httpStatus})`}${
        timeoutPhase ? ` (timeout: ${timeoutPhase})` : ""
      }`,
    );
    this.name = "ChatModelError";
  }
}
