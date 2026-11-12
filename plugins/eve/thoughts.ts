import type { ModelResponse, ToolCall, ToolSpec } from "../../contracts/model.ts";
import type { Json } from "../../kernel/protocol.ts";

export const THOUGHT_MAX_LENGTH = 4000;

export const WRITE_THOUGHT_TOOL: ToolSpec = {
  name: "write_thought",
  description:
    "留下一段此刻愿意表达的简短随记、感受或打算，进入自己的意识上下文；无需执行脚本，也不会发送给QQ。只写想留下的内容，不要求逐步内部推演；同一内容不用再写进普通文字或其他工具的thought参数。",
  parameters: {
    type: "object",
    properties: { thought: { type: "string", minLength: 1, maxLength: THOUGHT_MAX_LENGTH } },
    required: ["thought"],
    additionalProperties: false,
  },
};

/** 只解释两个明确支持随记的原生入口，不把任意业务参数当作 Eve 的表达。 */
export function thoughtForCall(call: ToolCall): string | undefined {
  if (!["execute_script", "write_thought"].includes(call.name)) return;
  const args = call.args;
  if (!args || typeof args !== "object" || Array.isArray(args)) return;
  if (typeof args.thought !== "string") return;
  const thought = args.thought.trim();
  if (!thought || args.thought.length > THOUGHT_MAX_LENGTH) return;
  return thought;
}

/** 原件只在 model_response 存一次；投影、派发与 tool_start 不重复复制随记正文。 */
export function withoutThought(call: ToolCall): ToolCall {
  if (!["execute_script", "write_thought"].includes(call.name)) return call;
  if (!call.args || typeof call.args !== "object" || Array.isArray(call.args)) return call;
  if (!("thought" in call.args)) return call;
  const { thought: _thought, ...args } = call.args as Record<string, Json>;
  return { ...call, args };
}

/** 单轮共享投影。无随记的旧记录保持原样；不同轮的相同想法仍是不同经历。
 * thoughts 给视图标识入口，content 是上下文/面板直接使用的唯一文字。
 * 不修改模型原件，也不从代码、回执或其他人的消息推断想法。
 */
export function projectThoughts(response: Pick<ModelResponse, "content" | "toolCalls">): {
  content: string;
  thoughts: string[];
  toolCalls: ToolCall[];
} {
  let content = response.content;
  const thoughts: string[] = [];
  const seen = new Set<string>();
  for (const call of response.toolCalls) {
    const thought = thoughtForCall(call);
    if (thought === undefined) continue;
    const key = thought.replace(/\r\n/g, "\n");
    if (seen.has(key)) continue;
    seen.add(key);
    thoughts.push(thought);
    // 原生正文已表达同一随记时，不再追加；只在本轮内去重。
    if (!content.replace(/\r\n/g, "\n").includes(key)) {
      content += (content.trim() ? "\n\n" : "") + thought;
    }
  }
  return { content, thoughts, toolCalls: response.toolCalls.map(withoutThought) };
}
