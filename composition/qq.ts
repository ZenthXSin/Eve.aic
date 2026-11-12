import { QqClient, QqRequestError } from "../adapters/qq/client.ts";
import { RestrictedFileTools } from "../services/runtime/restricted-files.ts";

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
    let attachmentReader: ((path: string) => Promise<Uint8Array>) | undefined;
    if (
      qq.mediaRoots !== undefined || qq.mediaIdentity !== undefined || qq.mediaRuntime !== undefined
    ) {
      if (
        !Array.isArray(qq.mediaRoots) || qq.mediaRoots.length < 1 ||
        qq.mediaRoots.some((path: unknown) => typeof path !== "string" || !path.startsWith("/")) ||
        !qq.mediaIdentity
      ) throw new Error();
      const files = new RestrictedFileTools(
        qq.mediaRoots[0],
        qq.mediaRoots,
        qq.mediaIdentity,
        qq.mediaRuntime,
      );
      attachmentReader = (path) => files.readBytes({ path });
    }
    return new QqClient({ appId, appSecret }, { sendScope: "private", attachmentReader });
  } catch {
    throw new QqRequestError({ category: "request" });
  }
}
