// 独立适配器入口，沿用本机私有 QQ 配置；不启动 Eve 模型或发送聊天消息。
import { readQqTestClient } from "../../composition/qq.ts";
import { writeAtomic } from "./journal.ts";
import { startQqPhoneAdapter } from "./service.ts";
import { readQqOwnershipFence } from "../../composition/ownership.ts";

export async function runQqAdapter(configPath: string, directory: string): Promise<void> {
  if (!configPath.startsWith("/") || !directory.startsWith("/")) {
    throw new Error("使用绝对私有路径");
  }
  await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
  const keyFile = `${directory}/bridge-key`;
  let key: string;
  try {
    const stat = await Deno.lstat(keyFile);
    if (!stat.isFile || stat.isSymlink || stat.size > 256) throw new Error("连接密钥文件损坏");
    key = (await Deno.readTextFile(keyFile)).trim();
  } catch (error) {
    if (!(error instanceof Deno.errors.NotFound)) throw error;
    key = Array.from(
      crypto.getRandomValues(new Uint8Array(32)),
      (n) => n.toString(16).padStart(2, "0"),
    ).join("");
    await Deno.writeTextFile(keyFile, key + "\n", { createNew: true, mode: 0o600 });
  }
  const client = await readQqTestClient(configPath);
  const fence = await readQqOwnershipFence(configPath, directory);
  const adapter = await startQqPhoneAdapter(
    client,
    directory,
    key,
    (status) =>
      console.log(JSON.stringify({ adapter: "qq", gateway: status.status, code: status.code })),
    fence,
  );
  const stop = () => {
    void adapter.close().catch(() => {});
  };
  Deno.addSignalListener("SIGTERM", stop);
  Deno.addSignalListener("SIGINT", stop);
  try {
    // 仅连接参数；主人身份配置仍由手机组合层另行加入，不由适配器决定。
    await writeAtomic(`${directory}/phone-connection.json`, {
      phone: { adapterUrl: adapter.url, adapterKeyFile: keyFile },
    });
    console.log(JSON.stringify({ adapterRpc: "ready", privateSendOnly: true, eveOnline: false }));
    await adapter.stopped;
  } finally {
    Deno.removeSignalListener("SIGTERM", stop);
    Deno.removeSignalListener("SIGINT", stop);
    await adapter.close();
  }
}

if (import.meta.main) {
  if (Deno.args.length !== 2) throw new Error("参数：私有配置绝对路径、运行目录绝对路径");
  await runQqAdapter(Deno.args[0], Deno.args[1]);
}
