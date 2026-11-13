import type { ScriptRequest } from "../../contracts/script.ts";
import { SCRIPTS } from "../../contracts/script.ts";
import { type ToolCall, type ToolResult, TOOLS, type ToolSpec } from "../../contracts/tools.ts";
import { definePlugin, type Json } from "../../sdk/plugin.ts";
import { type ScriptConfig, ScriptRuntime } from "./runtime.ts";

export const SCRIPT_TOOL: ToolSpec = {
  name: "execute_script",
  description:
    "执行一份 TypeScript 脚本；使用 tools 的类型接口调用能力。默认最多等待5秒，未完成就返回原任务编号并在后台并行继续；mode:async可立即返回，mode:sync也最多等待5秒；每个具体动作独立保存真实回执。脚本正文可 await 或 return。已完成任务的回执含 result 返回值 JSON 文本页（最多8192字符）；若 truncated，使用原 jobId 执行 return await tools.scripts.result({jobId,startChar:nextChar,limit:2048}) 读取后续原件页，异步任务完成后也这样读取。",
  parameters: {
    type: "object",
    properties: {
      code: { type: "string" },
      mode: { type: "string", enum: ["async", "sync"] },
      timeoutMs: { type: "integer", minimum: 1 },
      thought: {
        type: "string",
        minLength: 1,
        maxLength: 4000,
        description:
          "可选：此刻愿意留下的简短随记、感受或打算；由意识流记录进上下文，不属于脚本或QQ消息。同一内容不用在普通文字或write_thought重复写，不要求逐步内部推演。",
      },
    },
    required: ["code"],
    additionalProperties: false,
  },
};
export async function readScriptConfig(path: string): Promise<ScriptConfig> {
  const value = JSON.parse(await Deno.readTextFile(path)).scripts;
  if (
    !value ||
    [value.directory, value.denoBinary, value.cacheDirectory].some((v) =>
      typeof v !== "string" || !v.startsWith("/")
    ) ||
    !Number.isSafeInteger(value.defaultTimeoutMs) || value.defaultTimeoutMs < 1 ||
    (value.startupTimeoutMs !== undefined &&
      (!Number.isSafeInteger(value.startupTimeoutMs) || value.startupTimeoutMs < 1 ||
        value.startupTimeoutMs > 2_147_483_647))
  ) {
    throw new Error("脚本配置需要绝对目录、Deno路径与有效期限");
  }
  return {
    directory: value.directory,
    denoBinary: value.denoBinary,
    cacheDirectory: value.cacheDirectory,
    defaultTimeoutMs: value.defaultTimeoutMs,
    ...(value.startupTimeoutMs === undefined ? {} : { startupTimeoutMs: value.startupTimeoutMs }),
  };
}
export function createScriptPlugin(configPath: string) {
  let runtime: ScriptRuntime | undefined;
  return definePlugin({
    load(ctx) {
      const current = () => {
        if (!runtime) throw new Error("脚本运行时未启用");
        return runtime;
      };
      ctx.provide(SCRIPTS, {
        start: (args) =>
          current().start(args as unknown as ScriptRequest) as unknown as Promise<Json>,
        contracts: (args) => current().contracts(args as never) as unknown as Promise<Json>,
        status: (args) => current().status(args as { jobId: string }) as unknown as Json,
        observations: (args) =>
          current().observations(args as { jobId: string; afterSeq?: number }) as unknown as Json,
        images: (args) =>
          current().images(args as { jobId: string; afterSeq?: number }) as unknown as Json,
        cancel: (args) => current().cancel(args as { jobId: string }) as unknown as Promise<Json>,
        result: (args) =>
          current().result(
            args as { jobId: string; startChar?: number; limit?: number },
          ) as unknown as Json,
        completions: (args) =>
          current().completions((args ?? {}) as { afterSeq?: number }) as unknown as Json,
        receipts: (args) =>
          current().receipts(args as { jobId: string; afterSeq?: number }) as unknown as Json,
        receipt: (args) =>
          current().receipt(
            args as {
              jobId: string;
              seq: number;
              section: "args" | "result" | "error";
              startChar?: number;
              limit?: number;
            },
          ) as unknown as Json,
      });
      ctx.provide(TOOLS, {
        list: () => [SCRIPT_TOOL] as unknown as Json,
        execute: async (args) => {
          const call = args as unknown as ToolCall;
          if (call?.name !== "execute_script") {
            return {
              status: "failed",
              output: "当前只有 execute_script",
            } as ToolResult as unknown as Json;
          }
          const data = call.args as unknown as Omit<ScriptRequest, "requestId">;
          const job = await current().start({
            code: data.code,
            ...(data.mode === undefined ? {} : { mode: data.mode }),
            ...(data.timeoutMs === undefined ? {} : { timeoutMs: data.timeoutMs }),
            requestId: call.id,
          });
          // start 只返回任务摘要；必须把已持久化的真实 return 值交给原生工具调用方。
          // 按工具说明读取2048字符原件页时，包裹后的返回值仍能完整展示。
          const result = job.status === "completed"
            ? current().result({ jobId: job.jobId, limit: 8192 })
            : undefined;
          return {
            status: "completed",
            output: JSON.stringify({ ...job, ...(result === undefined ? {} : { result }) }),
          };
        },
      });
    },
    async enable(ctx) {
      runtime = new ScriptRuntime(ctx, await readScriptConfig(configPath));
      ctx.onDisable(() => runtime?.close());
      await runtime.initialize();
    },
    health() {
      if (!runtime?.healthy) throw new Error("脚本运行时不可用");
    },
  });
}
