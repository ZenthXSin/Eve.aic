// 测试用的假模型：不联网。用户说"回声 X"就调用 echo 工具，拿到结果后转述；其余原样应答。
import type { Message, ModelRequest, ModelResponse } from "../../contracts/model.ts";
import { MODEL } from "../../contracts/model.ts";
import { definePlugin, type Json } from "../../sdk/plugin.ts";

function complete(request: ModelRequest): ModelResponse {
  const last = request.messages.at(-1) as Message | undefined;
  if (!last) return { content: "（没有输入）", toolCalls: [] };
  if (last.role === "tool") {
    return { content: `工具说：${last.content}`, toolCalls: [] };
  }
  const match = /^回声\s*(.+)$/.exec(last.content);
  if (match && request.tools.some((tool) => tool.name === "echo")) {
    const turn = request.messages.filter((message) => message.role === "user").length;
    return {
      content: "",
      toolCalls: [{ id: `call-${turn}`, name: "echo", args: { text: match[1] } }],
    };
  }
  const turns = request.messages.filter((message) => message.role === "user").length;
  return { content: `第 ${turns} 轮，你说：${last.content}`, toolCalls: [] };
}

export default definePlugin({
  load(ctx) {
    ctx.provide(MODEL, {
      complete: (args) => complete(args as unknown as ModelRequest) as unknown as Json,
    });
  },
});
