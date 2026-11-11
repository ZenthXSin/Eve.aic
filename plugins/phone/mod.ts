import { PHONE } from "../../contracts/phone.ts";
import { PHONE_METHODS } from "../../contracts/phone-rpc.ts";
import { definePlugin, type Json } from "../../sdk/plugin.ts";
import { PhoneRpcClient } from "./client.ts";

export interface PhoneConfig {
  adapterUrl: string;
  adapterKeyFile: string;
  /** 未配置时所有入站身份均为 other，绝不取第一条私信认定主人。 */
  ownerPlatformUserId?: string;
}
export async function readPhoneConfig(path: string): Promise<PhoneConfig> {
  const value = JSON.parse(await Deno.readTextFile(path)).phone;
  if (
    !value || typeof value.adapterUrl !== "string" ||
    typeof value.adapterKeyFile !== "string" || !value.adapterKeyFile.startsWith("/") ||
    (value.ownerPlatformUserId !== undefined &&
      (typeof value.ownerPlatformUserId !== "string" || value.ownerPlatformUserId.length === 0 ||
        value.ownerPlatformUserId.length > 512))
  ) throw new Error("手机配置不完整");
  return {
    adapterUrl: value.adapterUrl,
    adapterKeyFile: value.adapterKeyFile,
    ...(value.ownerPlatformUserId === undefined ? {} : {
      ownerPlatformUserId: value.ownerPlatformUserId,
    }),
  };
}

function relationships(value: Json, ownerId?: string): Json {
  if (Array.isArray(value)) return value.map((item) => relationships(item, ownerId));
  if (value !== null && typeof value === "object") {
    const mapped: Record<string, Json> = {};
    for (const [key, child] of Object.entries(value)) mapped[key] = relationships(child, ownerId);
    if (
      value.handle && value.sender && typeof value.sender === "object" &&
      !Array.isArray(value.sender) &&
      (value.direction === "incoming" || value.direction === "outgoing")
    ) {
      const sender = mapped.sender as Record<string, Json>;
      sender.relationship = value.direction === "outgoing"
        ? "self"
        : ownerId !== undefined && value.sender.platformUserId === ownerId
        ? "owner"
        : "other";
    }
    return mapped;
  }
  return value;
}

export function createPhonePlugin(configPath: string) {
  let client: PhoneRpcClient | undefined;
  let ownerId: string | undefined;
  const health = async () => {
    if (!client) throw new Error("手机尚未启用");
    const value = await client.call("health");
    if (
      value === null || typeof value !== "object" || Array.isArray(value) ||
      value.available !== true
    ) {
      throw new Error("手机适配器不可用");
    }
  };
  return definePlugin({
    load(ctx) {
      const handlers: Record<string, (args: Json) => Promise<Json>> = {};
      for (const method of PHONE_METHODS) {
        handlers[method] = async (args) => {
          if (!client) throw new Error("手机尚未启用");
          return relationships(await client.call(method, args), ownerId);
        };
      }
      ctx.provide(PHONE, handlers);
    },
    async enable(ctx) {
      const config = await readPhoneConfig(configPath);
      client = new PhoneRpcClient(
        config.adapterUrl,
        (await Deno.readTextFile(config.adapterKeyFile)).trim(),
      );
      ownerId = config.ownerPlatformUserId;
      ctx.onDisable(() => client?.close());
      await health();
    },
    async health() {
      await health();
    },
  });
}
