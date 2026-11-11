import { QqClient, QqRequestError } from "../adapters/qq/client.ts";

export async function readQqTestClient(configPath: string): Promise<QqClient> {
  try {
    const config = JSON.parse(await Deno.readTextFile(configPath));
    const qq = config.qq;
    if (
      !qq || qq.sandbox !== false || qq.replyScope !== "private" ||
      typeof qq.appIdFile !== "string" || !qq.appIdFile.startsWith("/") ||
      typeof qq.appSecretFile !== "string" || !qq.appSecretFile.startsWith("/")
    ) throw new Error();
    const appId = (await Deno.readTextFile(qq.appIdFile)).trim();
    const appSecret = (await Deno.readTextFile(qq.appSecretFile)).trim();
    return new QqClient({ appId, appSecret }, { sendScope: "private" });
  } catch {
    throw new QqRequestError({ category: "request" });
  }
}
