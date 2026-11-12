import { RUNTIME_METHODS } from "../../contracts/runtime.ts";
import { RuntimeClient, type RuntimeConnection } from "../../services/runtime/client.ts";
import { definePlugin, type Json, KernelError } from "../../sdk/plugin.ts";
import { UPGRADE_METHODS, UPGRADES } from "../../contracts/upgrades.ts";
import { UpgradeClient } from "./upgrades.ts";

export interface RuntimePluginConnection extends RuntimeConnection {
  upgradeConnectionFile?: string;
}
export async function readRuntimeConnection(path: string): Promise<RuntimePluginConnection> {
  const config = JSON.parse(await Deno.readTextFile(path)).runtime;
  if (
    !config ||
    [config.connectionFile, config.keyFile, config.proofFile].some((v) =>
      typeof v !== "string" || !v.startsWith("/")
    ) || (config.upgradeConnectionFile !== undefined &&
      (typeof config.upgradeConnectionFile !== "string" ||
        !config.upgradeConnectionFile.startsWith("/")))
  ) throw new KernelError("rejected", "执行出口连接不完整");
  return config;
}
export function createRuntimeClientPlugin(configPath: string) {
  let client: RuntimeClient | undefined;
  let upgrades: UpgradeClient | undefined;
  return definePlugin({
    load(ctx) {
      ctx.provide(
        UPGRADES,
        Object.fromEntries(UPGRADE_METHODS.map((method) => [
          method,
          async (args: Json) => {
            if (!upgrades) throw new KernelError("unavailable", "独立升级监督未连接");
            const data = args === null ? {} : structuredClone(args);
            if (typeof data !== "object" || Array.isArray(data)) {
              throw new KernelError("rejected", "升级参数必须为对象");
            }
            const id = data._actionId;
            if (typeof id !== "string" || !id || id.length > 256) {
              throw new KernelError("rejected", "升级动作编号必须由宿主提供");
            }
            delete data._actionId;
            return await upgrades.call(method, data, id);
          },
        ])),
      );
      for (const [namespace, methods] of Object.entries(RUNTIME_METHODS)) {
        ctx.provide(
          namespace,
          Object.fromEntries(methods.map((method) => [method, async (args: Json) => {
            if (!client) throw new KernelError("unavailable", "独立执行服务未连接");
            const data = args === null ? {} : structuredClone(args);
            if (typeof data !== "object" || Array.isArray(data)) {
              throw new KernelError("rejected", "工具参数必须为对象");
            }
            const id = typeof data._actionId === "string"
              ? data._actionId
              : `read_${crypto.randomUUID()}`;
            delete data._actionId;
            // 只有在线内核模型桥主动声明真实意识活动；隔离调用直接走RuntimeClient。
            if (namespace === "model" && method === "complete") data._purpose = "online";
            return await client.call(namespace, method, data, id);
          }])),
        );
      }
    },
    async enable(ctx) {
      const config = await readRuntimeConnection(configPath);
      client = new RuntimeClient(config);
      if (config.upgradeConnectionFile) {
        upgrades = new UpgradeClient({
          connectionFile: config.upgradeConnectionFile,
          proofFile: config.proofFile,
        });
        ctx.onDisable(() => upgrades?.close());
      }
      ctx.onDisable(() => client?.close());
      const health = await client.call("health", "status") as {
        available: boolean;
        ownershipFenced: boolean;
      };
      if (!health.available || !health.ownershipFenced) {
        throw new KernelError("unavailable", "独立出口未受所有权保护");
      }
    },
    health() {
      if (!client) throw new KernelError("unavailable", "执行出口未连接");
    },
  });
}
