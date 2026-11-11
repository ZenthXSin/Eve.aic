// 最小的工具提供者：只有一个 echo。脚本工具落地前用它验证轮次循环。
import { type ToolCall, type ToolResult, TOOLS, type ToolSpec } from "../../contracts/tools.ts";
import { definePlugin, type Json } from "../../sdk/plugin.ts";

const SPECS: ToolSpec[] = [{
  name: "echo",
  description: "原样返回 text",
  parameters: {
    type: "object",
    properties: { text: { type: "string" } },
    required: ["text"],
    additionalProperties: false,
  },
}];

function execute(call: ToolCall): ToolResult {
  if (call.name !== "echo") return { status: "failed", output: `没有工具 ${call.name}` };
  const text = (call.args as { text?: unknown } | null)?.text;
  if (typeof text !== "string") return { status: "failed", output: "echo 需要字符串参数 text" };
  return { status: "completed", output: text };
}

export default definePlugin({
  load(ctx) {
    ctx.provide(TOOLS, {
      list: () => SPECS as unknown as Json,
      execute: (args) => execute(args as unknown as ToolCall) as unknown as Json,
    });
  },
});
