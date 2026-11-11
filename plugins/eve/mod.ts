import { definePlugin, type Json, KernelError } from "../../sdk/plugin.ts";
import { type EveConfig, EveRuntime } from "./runtime.ts";
export async function readEveConfig(path: string): Promise<EveConfig> {
  const value = JSON.parse(await Deno.readTextFile(path)).eve;
  if (
    !value ||
    [value.directory, value.promptFile].some((v) => typeof v !== "string" || !v.startsWith("/")) ||
    !(/^[a-f0-9]{40}$/.test(value.sourceCommit)) ||
    [
      value.modelTimeoutMs,
      value.maxOutputTokens,
      value.pollMs,
      value.defaultSleepMs,
      value.dailyTokenBudget,
    ].some((v) => !Number.isSafeInteger(v) || v < 1)
  ) throw new KernelError("rejected", "意识流配置不完整");
  if (value.timeZone !== undefined && typeof value.timeZone !== "string") {
    throw new KernelError("rejected", "预算时区无效");
  }
  try {
    new Intl.DateTimeFormat("en-CA", { timeZone: value.timeZone ?? "Asia/Shanghai" }).format();
  } catch {
    throw new KernelError("rejected", "预算时区无效");
  }
  return value;
}
export function createEvePlugin(configPath: string) {
  let runtime: EveRuntime | undefined;
  return definePlugin({
    load(ctx) {
      const current = () => {
        if (!runtime) throw new KernelError("unavailable", "意识流尚未启用");
        return runtime;
      };
      ctx.provide("eve", {
        start: () => {
          current().start();
          return null;
        },
        pulse: async () => {
          await current().pulse();
          return current().status() as unknown as Json;
        },
        status: () => current().status() as unknown as Json,
        read: (args) => current().read((args ?? {}) as { afterSeq?: number }) as unknown as Json,
      });
    },
    async enable(ctx) {
      runtime = new EveRuntime(ctx, await readEveConfig(configPath));
      ctx.onDisable(() => runtime?.close());
      await runtime.initialize();
    },
    health() {
      if (!runtime?.status().healthy) throw new KernelError("unavailable", "意识流不健康");
    },
  });
}
