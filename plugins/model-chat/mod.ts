import { MODEL, type ModelRequest } from "../../contracts/model.ts";
import { definePlugin, type Json } from "../../sdk/plugin.ts";
import { ChatModelClient, ChatModelError, type ChatModelOptions } from "./client.ts";

export interface PrivateModelConfig extends Omit<ChatModelOptions, "apiKey"> {
  apiKeyFile: string;
  protocol: "chat";
}

export async function readPrivateModelConfig(path: string): Promise<PrivateModelConfig> {
  try {
    const file = JSON.parse(await Deno.readTextFile(path));
    const model = file.model;
    if (
      model === null || typeof model !== "object" || model.protocol !== "chat" ||
      typeof model.apiKeyFile !== "string" || !model.apiKeyFile.startsWith("/")
    ) throw new Error();
    return model as PrivateModelConfig;
  } catch {
    throw new ChatModelError("configuration");
  }
}

/** 在私有组合目录生成入口调用这个工厂；源码仓库不存真实配置或密钥。 */
export function createChatModelPlugin(configPath: string) {
  let client: ChatModelClient | undefined;
  return definePlugin({
    load(ctx) {
      ctx.provide(MODEL, {
        complete: (args) => {
          if (!client) throw new ChatModelError("configuration");
          return client.complete(args as unknown as ModelRequest) as unknown as Promise<Json>;
        },
      });
    },
    async enable() {
      const config = await readPrivateModelConfig(configPath);
      let apiKey: string;
      try {
        apiKey = (await Deno.readTextFile(config.apiKeyFile)).trim();
      } catch {
        throw new ChatModelError("configuration");
      }
      client = new ChatModelClient({ ...config, apiKey });
    },
    disable() {
      client?.close();
      client = undefined;
    },
    health() {
      if (!client) throw new ChatModelError("configuration");
      // 这里只检查已启用，不为了健康探测额外花一次模型调用。
    },
  });
}
