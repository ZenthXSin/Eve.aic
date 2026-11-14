// The SDK's real generated API methods execute here; Eve supplies its HTTP transport.
import { GroupInternal, GuildInternal } from "./internal.mjs";
export const QQ_THIRD_PARTY_SDK = {
  sdk: "@satorijs/adapter-qq",
  version: "4.14.1",
  commit: "fa5ec61ac75c4fc3b204b5e2d01442f4bb29f410",
  source: "https://github.com/satorijs/satori",
} as const;
export const QQ_API_ORIGIN = "https://api.sgroup.qq.com";
export const QQ_TOKEN_ORIGIN = "https://bots.qq.com";
export const QQ_REQUEST_TIMEOUT_MS = 30_000;
export const QQ_UPLOAD_TIMEOUT_MS = 120_000;
export const QQ_TOKEN_TIMEOUT_MS = 10_000;

export async function sdkRequest<T>(
  operation:
    | "getMe"
    | "getGateway"
    | "sendPrivateMessage"
    | "sendMessage"
    | "sendFilePrivate"
    | "sendFileGuild"
    | "uploadPreparePrivate"
    | "uploadPrepareGuild"
    | "uploadPartFinishPrivate"
    | "uploadPartFinishGuild",
  args: unknown[],
  transport: (path: string, init: RequestInit) => Promise<T & { data: Record<string, unknown> }>,
): Promise<T> {
  let receipt: T | undefined;
  const http = Object.assign(async (path: string, config: { method: string; data?: unknown }) => {
    const result = await transport(path, {
      method: config.method,
      ...(config.data === undefined ? {} : { body: JSON.stringify(config.data) }),
    });
    receipt = result;
    return { data: result.data, headers: new Headers() };
  }, { isError: () => false });
  // Payload debug logging is intentionally disabled: credentials never enter a tool log.
  const bot = { logger: { debug() {} } };
  const api = operation === "getMe"
    ? new GuildInternal(bot, () => http)
    : new GroupInternal(bot, () => http);
  await (api as unknown as Record<string, (...args: unknown[]) => Promise<unknown>>)[operation](
    ...args,
  );
  if (!receipt) throw new Error("Satori SDK did not produce an HTTP receipt");
  return receipt;
}
