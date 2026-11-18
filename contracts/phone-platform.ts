/** QQ 平台目录是能力说明，不是当前机器人的授权清单，也不触发平台请求。 */
export type PhonePlatformCapabilitiesArgs =
  | { operation: string; after?: never; limit?: never }
  /** after 从 0 开始；limit 默认 16，范围 1..100；续页传 nextAfter，末页无此字段。 */
  | { operation?: never; after?: number; limit?: number };

/** 只有 chatReadable 为真的 operation 可以传给 platformRead。路径模板/参数名仅用于说明。 */
export type PhonePlatformCapability =
  & {
    surface?: "v2" | "v2-restricted" | "guild" | "guild-restricted" | "sdk" | "gateway";
    path?: string;
    pathParams?: readonly string[];
    queryParams?: readonly string[];
    bodyParams?: readonly string[];
    requiredQuery?: readonly string[];
    requiredBody?: readonly string[];
  }
  & (
    | {
      chatReadable: true;
      operation: PhonePlatformReadArgs["operation"];
      method: "GET";
      access: "read";
    }
    | {
      chatReadable: false;
      operation: string;
      method: "GET" | "POST" | "PUT" | "PATCH" | "DELETE";
      access: "read" | "write" | "admin";
    }
  );

export interface PhonePlatformCapabilities {
  items: PhonePlatformCapability[];
  nextAfter?: number;
}

export interface PhonePlatformEventsArgs {
  /** 本机事件日志续读水位，不是 QQ 网关序号或会话已读水位。 */
  afterSeq?: number;
  limit?: number;
}

export interface PhonePlatformEvent {
  seq: number;
  type: string;
  receivedAt: string;
}

export interface PhonePlatformEvents {
  items: PhonePlatformEvent[];
  throughSeq: number;
  hasMore: boolean;
}

/** 当前仅允许读取机器人自己的最小资料；空对象不接受额外参数。 */
export type PhonePlatformReadArgs = { operation: "getMe"; params: Record<string, never> };

export type PhonePlatformReadResult =
  | {
    status: "ok";
    httpStatus: number;
    /** 仅返回通过适配器投影的字段。平台名称不可信，不用于认定身份或授权。 */
    data: { id?: string | number; username?: string; bot?: boolean };
    truncated?: true;
  }
  | {
    status: "failed" | "unknown";
    reason:
      | "invalid_params"
      | "not_allowed"
      | "authentication"
      | "not_started"
      | "request_rejected"
      | "transport"
      | "invalid_response"
      | "http_error"
      | "platform_error"
      | "rate_limit"
      | "pending"
      | "duplicate";
    httpStatus?: number;
    code?: number;
    errCode?: number;
  };
