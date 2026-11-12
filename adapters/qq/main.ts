// 独立适配器入口；不启动 Eve 模型循环，系统指令回执经过持久发送队列。
import { readQqTestClient } from "../../composition/qq.ts";
import { writeAtomic } from "./journal.ts";
import { startQqAdminServer } from "./admin.ts";
import { startQqPhoneAdapter } from "./service.ts";
import { readQqOwnershipFence } from "../../composition/ownership.ts";
import { RuntimeModelControlClient } from "../../services/runtime/model-control.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";

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
  const startupConfig = JSON.parse(await Deno.readTextFile(configPath));
  if (
    startupConfig.qq?.captureActivationBoundary !== undefined &&
    typeof startupConfig.qq.captureActivationBoundary !== "boolean"
  ) {
    throw new Error("QQ启动边界开关须为布尔值");
  }
  const modelControl = startupConfig.qq?.modelControl;
  const control = modelControl === undefined
    ? undefined
    : new RuntimeModelControlClient(modelControl);
  const adminControl = startupConfig.qq?.adminControl;
  let adminKey: string | undefined;
  if (adminControl !== undefined) {
    if (
      !adminControl || typeof adminControl.keyFile !== "string" ||
      !adminControl.keyFile.startsWith("/") || typeof adminControl.connectionFile !== "string" ||
      !adminControl.connectionFile.startsWith("/") ||
      [
        adminControl.keyFile,
        keyFile,
        configPath,
        startupConfig.phoneConnectionFile,
        modelControl?.connectionFile,
        modelControl?.keyFile,
      ].includes(adminControl.connectionFile)
    ) {
      throw new Error("QQ 管理出口使用独立的绝对私有连接路径");
    }
    adminKey = (await Deno.readTextFile(adminControl.keyFile)).trim();
    if (!/^[a-zA-Z0-9_-]{32,256}$/.test(adminKey) || adminKey === key) {
      throw new Error("QQ 管理密钥格式无效或未独立于手机桥密钥");
    }
    if (modelControl && adminKey === (await Deno.readTextFile(modelControl.keyFile)).trim()) {
      throw new Error("QQ 管理密钥必须独立于模型切换密钥");
    }
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
    {
      captureActivationBoundary: startupConfig.qq?.captureActivationBoundary === true,
      ...(modelControl === undefined ? {} : {
        modelCommands: { ownerPlatformUserId: modelControl.ownerPlatformUserId, control },
      }),
    },
  );
  let admin: ReturnType<typeof startQqAdminServer> | undefined;
  const stop = () => {
    void adapter.close().catch(() => {});
  };
  Deno.addSignalListener("SIGTERM", stop);
  Deno.addSignalListener("SIGINT", stop);
  try {
    if (adminKey) {
      admin = startQqAdminServer(adapter.actions, adminKey, adapter.phone);
      await writeDurableJson(adminControl.connectionFile, { url: admin.url });
    }
    // 仅连接参数；主人身份配置仍由手机组合层另行加入，不由适配器决定。
    await writeAtomic(`${directory}/phone-connection.json`, {
      phone: { adapterUrl: adapter.url, adapterKeyFile: keyFile },
    });
    const config = JSON.parse(await Deno.readTextFile(configPath));
    if (config.phoneConnectionFile) {
      if (
        typeof config.phoneConnectionFile !== "string" ||
        !config.phoneConnectionFile.startsWith("/")
      ) {
        throw new Error("手机公开连接文件需要绝对路径");
      }
      // Only the address is public. Each instance keeps its own copy of the bridge key.
      await writeDurableJson(config.phoneConnectionFile, { phone: { adapterUrl: adapter.url } });
      await Deno.chmod(config.phoneConnectionFile, 0o644);
    }
    console.log(JSON.stringify({
      adapterRpc: "ready",
      replyScope: client.sendScope,
      privateSendOnly: client.sendScope === "private",
      eveOnline: false,
    }));
    await adapter.stopped;
  } finally {
    Deno.removeSignalListener("SIGTERM", stop);
    Deno.removeSignalListener("SIGINT", stop);
    try {
      await admin?.close();
    } finally {
      await adapter.close();
    }
  }
}

if (import.meta.main) {
  if (Deno.args.length !== 2) throw new Error("参数：私有配置绝对路径、运行目录绝对路径");
  await runQqAdapter(Deno.args[0], Deno.args[1]);
}
