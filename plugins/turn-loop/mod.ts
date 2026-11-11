// 最基本的轮次循环：模型请求、工具调用、结果回传、下一轮，直到模型不再调用工具。
// 只依赖 model 与 tools 两个契约；对话历史作为插件状态保存，换版和重启后延续。
import type { Message, ModelResponse } from "../../contracts/model.ts";
import { MODEL } from "../../contracts/model.ts";
import { type ToolResult, TOOLS, type ToolSpec } from "../../contracts/tools.ts";
import { TURN, type TurnResult } from "../../contracts/turn.ts";
import { definePlugin, type Json, type PluginContext } from "../../sdk/plugin.ts";

const MAX_MODEL_REQUESTS = 8;

interface State {
  history: Message[];
}

async function run(ctx: PluginContext, input: string): Promise<TurnResult> {
  // 在副本上推进，整轮成功才提交；失败的输入不进入完成历史。
  const history = [...(ctx.getState<State & Json>()?.history ?? [])];
  history.push({ role: "user", content: input });
  const tools = await ctx.call<ToolSpec[] & Json>(TOOLS, "list");
  let modelRequests = 0;
  let toolCalls = 0;
  while (true) {
    if (modelRequests >= MAX_MODEL_REQUESTS) {
      throw new Error(`模型连续请求了 ${MAX_MODEL_REQUESTS} 次仍在调用工具，这一轮中止`);
    }
    modelRequests++;
    const response = await ctx.call<ModelResponse & Json>(MODEL, "complete", {
      messages: history,
      tools,
    } as unknown as Json);
    history.push({ role: "assistant", content: response.content, toolCalls: response.toolCalls });
    if (response.toolCalls.length === 0) {
      await ctx.setState({ history } as unknown as Json);
      return { reply: response.content, modelRequests, toolCalls };
    }
    for (const call of response.toolCalls) {
      toolCalls++;
      let result: ToolResult;
      try {
        result = await ctx.call<ToolResult & Json>(TOOLS, "execute", call as unknown as Json);
      } catch (error) {
        // 工具没有给出结果：如实告诉模型，不伪造成功。
        const kind = (error as { kind?: string }).kind;
        if (kind === "timeout" || kind === "unknown") {
          // 尚未确定是否执行，不能转换成可安全重试的失败。
          throw error;
        }
        result = { status: "failed", output: `工具调用出错：${(error as Error).message}` };
      }
      history.push({
        role: "tool",
        toolCallId: call.id,
        content: result.status === "completed" ? result.output : `失败：${result.output}`,
      });
    }
  }
}

export default definePlugin({
  load(ctx) {
    let tail: Promise<unknown> = Promise.resolve();
    ctx.provide(TURN, {
      run: (args) => {
        const input = (args as { input?: unknown } | null)?.input;
        if (typeof input !== "string") throw new Error("run 需要字符串参数 input");
        const pending = tail.then(() => run(ctx, input));
        tail = pending.catch(() => {});
        return pending as unknown as Promise<Json>;
      },
      history: () => (ctx.getState<State & Json>()?.history ?? []) as unknown as Json,
    });
  },
});
