import type { AgentJob } from "../../contracts/agents.ts";
import type { Message, ToolSpec } from "../../contracts/model.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";

export const AGENT_DELEGATE_TOOL: ToolSpec = {
  name: "delegate_task",
  description:
    "通用子代理可把专业工作派给coding/review/investigation，默认coding，三类帮手不能再派工。仅等持久准入，返回jobId/agentId；保留各角色专用模型配置、默认high。默认复用自己对应角色的帮手会话，同会话排队；newSession:true并行多开，追问用自己帮手的原agentId。可指定reasoningEffort及name/instructions。说明目标、背景/路径、授权范围和验收证据。需要结果用wait_for_agents一次等待；交付先回到你，由你核对并向Eve汇总。",
  parameters: {
    type: "object",
    properties: {
      text: { type: "string", minLength: 1 },
      role: { type: "string", enum: ["coding", "review", "investigation"] },
      agentId: { type: "string", minLength: 1 },
      newSession: { type: "boolean" },
      reasoningEffort: {
        type: "string",
        enum: ["none", "minimal", "low", "medium", "high", "xhigh"],
      },
      name: { type: "string", minLength: 1 },
      instructions: { type: "string" },
    },
    required: ["text"],
    additionalProperties: false,
  },
};
export const AGENT_WAIT_TOOL: ToolSpec = {
  name: "wait_for_agents",
  description:
    "等待本任务派出的子任务交付，一次返回完整原回复和真实状态；不轮询、不额外调用模型。jobIds省略则等本任务全部子任务；可以一次等待多个jobId。等待只挂起你的任务，其他会话和Eve继续运行；取消或重启仍沿原句柄接续。",
  parameters: {
    type: "object",
    properties: {
      jobIds: {
        type: "array",
        items: { type: "string", minLength: 1 },
        minItems: 1,
        uniqueItems: true,
      },
    },
    additionalProperties: false,
  },
};

export function delegationArgs(value: Json): Record<string, Json> {
  if (
    !value || typeof value !== "object" || Array.isArray(value) ||
    Object.keys(value).some((k) =>
      !Object.hasOwn(AGENT_DELEGATE_TOOL.parameters!.properties as object, k)
    ) ||
    typeof value.text !== "string" || !value.text.trim() ||
    value.role !== undefined &&
      !["coding", "review", "investigation"].includes(String(value.role)) ||
    value.agentId !== undefined && (typeof value.agentId !== "string" || !value.agentId.trim()) ||
    value.newSession !== undefined && typeof value.newSession !== "boolean" ||
    value.agentId !== undefined &&
      ["role", "reasoningEffort", "name", "instructions"].some((k) => value[k] !== undefined) ||
    value.agentId !== undefined && value.newSession === true
  ) throw new KernelError("rejected", "派工需要正文，原agentId不能与新会话配置混用");
  return value;
}

export interface AgentDelivery extends AgentJob {
  text?: string;
}
export interface AgentDeliveries {
  schema: "eve.agent_children.v1";
  origin: "runtime";
  cancelled: boolean;
  items: AgentDelivery[];
}
/** Only runtime joins and wait_for_agents receipts count as consumed deliveries. */
export function agentDeliveries(message: Message): AgentDeliveries | undefined {
  try {
    const value = JSON.parse(message.content);
    if (
      value?.schema === "eve.agent_children.v1" && value.origin === "runtime" &&
      typeof value.cancelled === "boolean" && Array.isArray(value.items) &&
      value.items.every((x: AgentDelivery) =>
        typeof x.jobId === "string" && typeof x.status === "string"
      )
    ) return value;
  } catch { /* Ordinary text is not a delivery envelope. */ }
}
export function consumedChildren(transcript: Message[]): Map<string, number> {
  const consumed = new Map<string, number>();
  const waits = new Set<string>();
  for (const message of transcript) {
    if (message.role === "assistant") {
      for (const call of message.toolCalls ?? []) {
        if (call.name === "wait_for_agents") waits.add(call.id);
      }
    } else if (
      message.role === "user" || message.role === "tool" && waits.has(message.toolCallId!)
    ) {
      for (const item of agentDeliveries(message)?.items ?? []) {
        if (item.replySeq) consumed.set(item.jobId, item.replySeq);
      }
    }
  }
  return consumed;
}
