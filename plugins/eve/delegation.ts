import type { AgentJob, AgentRole } from "../../contracts/agents.ts";
import type { ToolSpec } from "../../contracts/model.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { PluginContext } from "../../sdk/plugin.ts";

export const DELEGATE_TOOL: ToolSpec = {
  name: "delegate_task",
  description:
    "把工作交给子代理，直接返回已持久准入的jobId/agentId；无需写脚本，不等待工作完成。默认general跟随Eve当前模型与凭据，它能继续派coding/review/investigation并等完整交付后汇总；三类专用子代理不能再派工。日常操作、巡检、资料整理、创作交付等用general；专业任务也可直接按role选择。默认使用对应常驻会话，同会话排队；独立任务需并行时newSession:true另开同类实例。追问用原agentId。交代目标、背景/路径、授权范围、验收与真实证据；回复会唤醒Eve，由她管理结果与下一步。",
  parameters: {
    type: "object",
    properties: {
      text: { type: "string", minLength: 1 },
      role: { type: "string", enum: ["general", "coding", "investigation", "review"] },
      agentId: { type: "string", minLength: 1 },
      newSession: { type: "boolean" },
    },
    required: ["text"],
    additionalProperties: false,
  },
};

export async function delegateTask(ctx: PluginContext, value: Json, callId: string) {
  if (
    !value || typeof value !== "object" || Array.isArray(value) ||
    Object.keys(value).some((k) => !["text", "role", "agentId", "newSession"].includes(k)) ||
    typeof value.text !== "string" || !value.text.trim() ||
    value.role !== undefined &&
      !["general", "coding", "investigation", "review"].includes(String(value.role)) ||
    value.agentId !== undefined && (typeof value.agentId !== "string" || !value.agentId.trim()) ||
    value.newSession !== undefined && typeof value.newSession !== "boolean" ||
    value.agentId !== undefined && (value.newSession === true || value.role !== undefined)
  ) throw new KernelError("rejected", "派工需要任务正文；原agentId与新会话/角色不能混用");
  const role = (value.role ?? "general") as AgentRole;
  let agentId = value.agentId as string | undefined;
  if (!agentId) {
    if (value.newSession) {
      const agent = await ctx.call("agents", "create", {
        role,
        _actionId: `${callId}_agent`,
      }) as { agentId: string };
      agentId = agent.agentId;
    } else agentId = `agent_${role}`;
  }
  // Original tool call IDs determine both admissions. Crash recovery cannot
  // create another helper or repeat an uncertain task under a fresh action ID.
  const job = await ctx.call("agents", "send", {
    agentId,
    text: value.text,
    _actionId: `${callId}_task`,
  }) as unknown as AgentJob;
  return job;
}
