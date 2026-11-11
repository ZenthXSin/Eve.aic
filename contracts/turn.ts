// 轮次契约：给一段输入，跑完"模型请求、工具调用、结果回传、下一轮"直到模型不再调用工具。
import type { Message } from "./model.ts";

export const TURN = "turn";

export type { Message };

export interface TurnResult {
  reply: string;
  /** 这一轮里请求了几次模型。 */
  modelRequests: number;
  /** 这一轮里执行了几次工具。 */
  toolCalls: number;
}
