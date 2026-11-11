// 工具契约：列出可用工具，执行一次调用。结果如实区分完成与失败。
import type { ToolCall, ToolSpec } from "./model.ts";

export const TOOLS = "tools";

export type { ToolCall, ToolSpec };

export interface ToolResult {
  status: "completed" | "failed";
  output: string;
}
