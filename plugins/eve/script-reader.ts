import type { ToolSpec } from "../../contracts/model.ts";
import type { ScriptHostApi } from "../../contracts/script.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { PluginContext } from "../../sdk/plugin.ts";

export const READ_SCRIPT_TOOL: ToolSpec = {
  name: "read_script",
  description:
    "直接读取原脚本的真实状态；已完成时附结果页，不启动新脚本、不重复执行动作。queued/checking/running等完成通知再读，unknown不代表失败，不可据此重发动作。大结果按同一jobId与nextChar继续读取，不要追逐读取器的新jobId。",
  parameters: {
    type: "object",
    properties: {
      jobId: { type: "string", minLength: 1 },
      startChar: { type: "integer", minimum: 0 },
      limit: { type: "integer", minimum: 1, maximum: 8192 },
    },
    required: ["jobId"],
    additionalProperties: false,
  },
};
export async function readScript(ctx: PluginContext, value: Json): Promise<Json> {
  if (
    !value || typeof value !== "object" || Array.isArray(value) ||
    Object.keys(value).some((key) => !["jobId", "startChar", "limit"].includes(key)) ||
    typeof value.jobId !== "string" || !value.jobId.trim() ||
    value.startChar !== undefined &&
      (!Number.isSafeInteger(value.startChar) || Number(value.startChar) < 0) ||
    value.limit !== undefined &&
      (!Number.isSafeInteger(value.limit) || Number(value.limit) < 1 || Number(value.limit) > 8192)
  ) {
    throw new KernelError("rejected", "需要jobId、非负startChar和1至8192的limit");
  }
  const job = await ctx.call("scripts", "status", { jobId: value.jobId }) as unknown as Awaited<
    ReturnType<ScriptHostApi["status"]>
  >;
  const result = job.status === "completed"
    ? await ctx.call("scripts", "result", {
      jobId: value.jobId,
      startChar: value.startChar ?? 0,
      limit: value.limit ?? 8192,
    })
    : undefined;
  return {
    ...job,
    ...(result === undefined ? {} : { result, resultStartChar: value.startChar ?? 0 }),
  } as unknown as Json;
}
