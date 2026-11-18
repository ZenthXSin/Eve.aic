import type { QqIngressOptions } from "./receiver.ts";
import { createQqWebhookHandler, type QqWebhookConfig, webhookSettings } from "./webhook.ts";

type IngressConfig = Omit<QqWebhookConfig, "enabled"> & {
  mode?: string;
  intents?: number;
  port?: number;
};

async function readIdentity(path: string) {
  if (typeof path !== "string" || !path.startsWith("/")) throw new TypeError("Invalid QQ identity");
  const value = (await Deno.readTextFile(path)).trim();
  if (!value) throw new TypeError("Invalid QQ identity");
  return value;
}

export async function configuredIngress(qq: {
  ingress?: IngressConfig;
  appIdFile: string;
  appSecretFile: string;
}): Promise<QqIngressOptions> {
  const config = qq.ingress;
  if (config === undefined) return {};
  if (!config || typeof config !== "object" || Array.isArray(config)) {
    throw new TypeError("Invalid QQ ingress");
  }
  const only = (...allowed: string[]) => Object.keys(config).every((key) => allowed.includes(key));
  if (config.mode === undefined || config.mode === "websocket") {
    const { intents = 1 << 25 } = config;
    if (
      !only("mode", "intents") || !Number.isSafeInteger(intents) || intents < 0 ||
      intents > 0xffff_ffff
    ) {
      throw new TypeError("Invalid QQ WebSocket subscription");
    }
    return { intents };
  }
  const { port } = config;
  if (
    config.mode !== "webhook" ||
    !only(
      "mode",
      "port",
      "path",
      "maxBodyBytes",
      "maxClockSkewSeconds",
      "bodyTimeoutMs",
      "maxInFlight",
      "unsignedRegistrationUntilUnixSeconds",
    ) ||
    typeof port !== "number" || !Number.isInteger(port) || port < 1024 || port > 65535
  ) throw new TypeError("Invalid local QQ Webhook listener");
  const callback = { ...webhookSettings(config), enabled: true };
  const [appId, secret] = await Promise.all([
    readIdentity(qq.appIdFile),
    readIdentity(qq.appSecretFile),
  ]);

  return {
    webhook: async (signal, accept, listening) => {
      if (signal.aborted) return;
      const writing = new Set<Promise<void>>();
      const handler = createQqWebhookHandler({
        appId,
        secret,
        signal,
        config: callback,
        onDispatch: async (event) => {
          const durable = accept(event);
          writing.add(durable);
          try {
            await durable;
            return "persisted";
          } finally {
            writing.delete(durable);
          }
        },
      });
      const server = Deno.serve(
        { hostname: "127.0.0.1", port, signal, onListen: listening },
        handler,
      );
      try {
        await server.finished;
      } finally {
        await Promise.allSettled(writing);
      }
    },
  };
}
