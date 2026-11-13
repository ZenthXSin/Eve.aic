import { PHONE } from "../../contracts/phone.ts";
import { PHONE_METHODS } from "../../contracts/phone-rpc.ts";
import { definePlugin, type Json, KernelError } from "../../sdk/plugin.ts";
import { PhoneRpcClient } from "./client.ts";
import type { OwnershipProof } from "../../contracts/ownership.ts";
import type { ModelImage } from "../../contracts/model.ts";
import { fingerprint } from "../../kernel/calls.ts";

export interface PhoneConfig {
  adapterUrl: string;
  /** 适配器重启可更换端口，只从公开连接文件读取URL，不覆盖实例密钥。 */
  connectionFile?: string;
  /** 生产接通同模型的客观图片描述服务；缺服务时报错，不回传原图。 */
  describeImages?: boolean;
  adapterKeyFile: string;
  /** 未配置时所有入站身份均为 other，绝不取第一条私信认定主人。 */
  ownerPlatformUserId?: string;
  /** 每个实例独立，由组合层写入；手机 worker 不读取守护控制/出口密钥。 */
  ownershipProofFile?: string;
}
export async function readPhoneConfig(path: string): Promise<PhoneConfig> {
  const value = JSON.parse(await Deno.readTextFile(path)).phone;
  if (
    !value || typeof value.adapterUrl !== "string" ||
    typeof value.adapterKeyFile !== "string" || !value.adapterKeyFile.startsWith("/") ||
    (value.connectionFile !== undefined &&
      (typeof value.connectionFile !== "string" || !value.connectionFile.startsWith("/"))) ||
    (value.describeImages !== undefined && typeof value.describeImages !== "boolean") ||
    (value.ownershipProofFile !== undefined &&
      (typeof value.ownershipProofFile !== "string" ||
        !value.ownershipProofFile.startsWith("/"))) ||
    (value.ownerPlatformUserId !== undefined &&
      (typeof value.ownerPlatformUserId !== "string" || value.ownerPlatformUserId.length === 0 ||
        value.ownerPlatformUserId.length > 512))
  ) throw new Error("手机配置不完整");
  return {
    adapterUrl: value.adapterUrl,
    adapterKeyFile: value.adapterKeyFile,
    ...(value.connectionFile === undefined ? {} : { connectionFile: value.connectionFile }),
    ...(value.describeImages === undefined ? {} : { describeImages: value.describeImages }),
    ...(value.ownershipProofFile === undefined
      ? {}
      : { ownershipProofFile: value.ownershipProofFile }),
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
    if (Array.isArray(value.mentions)) {
      mapped.mentions = value.mentions.map((item) => {
        if (!item || typeof item !== "object" || Array.isArray(item)) return item;
        return {
          ...item,
          relationship: item.platformUserId === ownerId
            ? "owner"
            : item.relationship === "self"
            ? "self"
            : "other",
        };
      });
    }
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
          if (method === "image") {
            if (
              !args || typeof args !== "object" || Array.isArray(args) ||
              (args.mode !== undefined && args.mode !== "describe" && args.mode !== "original")
            ) throw new KernelError("rejected", "看图模式必须是describe或original");
            const { mode = "describe", ...attachmentArgs } = args;
            const raw = await client.call("image", attachmentArgs) as unknown as {
              image: ModelImage;
              download: Json;
            };
            if (!raw?.image?.provenance || typeof raw.image.dataUrl !== "string") {
              throw new Error("手机图片原件未确认");
            }
            if (mode === "original") {
              return {
                selectedMode: "original",
                image: raw.image as unknown as Json,
                provenance: raw.image.provenance as unknown as Json,
                download: raw.download,
              };
            }
            const described = await ctx.call("model", "describeImage", {
              image: raw.image,
              _actionId: `phone_image_${await fingerprint(
                raw.image.provenance as unknown as Json,
              )}`,
            } as unknown as Json, { timeoutMs: 180_000 }) as {
              description?: Json;
              provenance?: Json;
            };
            if (typeof described?.description !== "string" || !described.description.trim()) {
              throw new Error("模型未返回图片描述");
            }
            return {
              selectedMode: "describe",
              description: described.description,
              provenance: raw.image.provenance as unknown as Json,
              download: raw.download,
            };
          }
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
        120_000,
        config.ownershipProofFile
          ? async () =>
            JSON.parse(await Deno.readTextFile(config.ownershipProofFile!)) as OwnershipProof
          : undefined,
        config.connectionFile
          ? async () => {
            const connection = JSON.parse(await Deno.readTextFile(config.connectionFile!));
            // 仅接入点可由公开连接更新；密钥和主人映射仍来自本实例私有配置。
            return connection?.phone?.adapterUrl;
          }
          : undefined,
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
