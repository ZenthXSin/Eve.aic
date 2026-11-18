import { readQqTestClient } from "../composition/qq.ts";
import { discoverPlatform, qqOperationRoutes } from "../adapters/qq/platform-bridge.ts";
import {
  createQqPlatformOperator,
  type QqPlatformConfirm,
} from "../adapters/qq/platform-client.ts";
import { platformDigest, readPrivatePlatformFile } from "../adapters/qq/platform-tool.ts";

export function terminalConfirmation(
  fileDigest: string,
  interactive = () => Deno.stdin.isTerminal() && Deno.stdout.isTerminal(),
  ask = (message: string) => prompt(message),
): QqPlatformConfirm {
  return ({ operation, method, template, access, digest }) => {
    if (!interactive()) return false;
    const challenge = `${operation} ${digest.slice(0, 12)}`;
    return ask(
      `请核对已审阅请求文件的 SHA-256：${fileDigest}\n${access} ${method} ${template}\n仅确认本次请求请输入 ${challenge}`,
    ) === challenge;
  };
}

async function run(args: string[]) {
  if (args[0] === "list" || args[0] === "describe") {
    const detail = args[0] === "describe";
    if (args.length > 2 || (detail && !args[1])) throw new TypeError("invalid catalogue request");
    return discoverPlatform(
      detail ? { operation: args[1] } : { after: Number(args[1] ?? 0), limit: 50 },
    );
  }
  if (
    Deno.uid() !== 0 || args.length !== 2 || !["read", "write", "admin"].includes(args[0]) ||
    !args[1].startsWith("/")
  ) {
    throw new TypeError(
      "usage: list [after] | describe NAME | read|write|admin /absolute/private/request.json",
    );
  }
  const text = await readPrivatePlatformFile(args[1], 0, 262144);
  const raw: unknown = JSON.parse(text);
  let client: Awaited<ReturnType<typeof readQqTestClient>> | undefined;
  try {
    const invoke = createQqPlatformOperator({
      catalog: qqOperationRoutes,
      request: async (method, path, body) => {
        client = await readQqTestClient("/etc/eve-rebuild/qq.json");
        return await client.platformRequest(method, path, body);
      },
      directory: "/root/.local/share/eve-rebuild-runtime/qq",
      access: args[0] as "read" | "write" | "admin",
    });
    return await invoke(raw, terminalConfirmation(platformDigest(text)));
  } finally {
    client?.close();
  }
}

if (import.meta.main) {
  try {
    const value = await run(Deno.args);
    console.log(JSON.stringify(value));
    if ("status" in value && value.status !== "ok") Deno.exitCode = 1;
  } catch {
    console.error("QQ 平台请求无效、权限不足或未确认；请用 list / describe 核对参数。");
    Deno.exitCode = 1;
  }
}
