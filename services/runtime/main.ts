import { readPrivateModelConfig } from "../../plugins/model-chat/mod.ts";
import { ChatModelClient } from "../../plugins/model-chat/client.ts";
import { FileOwnershipExit } from "../../composition/ownership.ts";
import { OwnershipFence } from "../../sdk/ownership-fence.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";
import { RuntimeEngine } from "./engine.ts";
import { LocalSemantic } from "./embedding.ts";
import { startRuntimeRpc } from "./rpc.ts";
import { startRuntimeAdmin } from "./admin.ts";

export async function runRuntimeService(configPath: string) {
  const config = JSON.parse(await Deno.readTextFile(configPath));
  const c = config.runtimeService;
  if (
    !c ||
    [
      c.directory,
      c.workspace,
      c.settingsFile,
      c.keyFile,
      c.guardianConnectionFile,
      c.exitKeyFile,
      c.adminKeyFile,
      c.adminConnectionFile,
    ]
      .some((p) => typeof p !== "string" || !p.startsWith("/"))
  ) throw new Error("独立执行服务需要完整私有路径");
  if (
    !c.commandIdentity || !Number.isSafeInteger(c.commandIdentity.uid) ||
    c.commandIdentity.uid < 1 ||
    !Number.isSafeInteger(c.commandIdentity.gid) || c.commandIdentity.gid < 1
  ) {
    throw new Error("生产命令出口必须配置独立非 root 用户");
  }
  if (c.commandUseSystemd !== true) throw new Error("生产命令出口必须启用逐命令进程组监督");
  const model = await readPrivateModelConfig(configPath);
  const client = new ChatModelClient({
    ...model,
    apiKey: (await Deno.readTextFile(model.apiKeyFile)).trim(),
  });
  const semantic = c.semantic
    ? new LocalSemantic(c.semantic.connectionFile, c.semantic.keyFile, `${c.directory}/vectors`)
    : undefined;
  const engine = new RuntimeEngine({
    directory: c.directory,
    workspace: c.workspace,
    allowedRoots: c.allowedRoots,
    settingsFile: c.settingsFile,
    model: client,
    maxOutputTokens: model.maxOutputTokens,
    modelName: model.model,
    semantic,
    commandIdentity: c.commandIdentity,
    commandUseSystemd: true,
  });
  const fence = new OwnershipFence(
    `${c.directory}/ownership`,
    "runtime",
    new FileOwnershipExit(c.guardianConnectionFile, c.exitKeyFile),
  );
  await engine.initialize();
  let rpc: Awaited<ReturnType<typeof startRuntimeRpc>> | undefined;
  let admin: ReturnType<typeof startRuntimeAdmin> | undefined;
  let resolve!: () => void;
  const done = new Promise<void>((r) => resolve = r), stop = () => resolve();
  try {
    await fence.initialize();
    rpc = await startRuntimeRpc(engine, (await Deno.readTextFile(c.keyFile)).trim(), fence);
    const adminKey = (await Deno.readTextFile(c.adminKeyFile)).trim();
    if (adminKey === (await Deno.readTextFile(c.keyFile)).trim()) {
      throw new Error("管理与实例凭据必须独立");
    }
    admin = startRuntimeAdmin(engine, adminKey, () => rpc!.quiesce());
    await writeDurableJson(`${c.directory}/connection.json`, { url: rpc.url });
    if (c.connectionFile) {
      if (typeof c.connectionFile !== "string" || !c.connectionFile.startsWith("/")) {
        throw new Error("执行服务公开连接文件需要绝对路径");
      }
      await writeDurableJson(c.connectionFile, { url: rpc.url });
      await Deno.chmod(c.connectionFile, 0o644);
    }
    await writeDurableJson(c.adminConnectionFile, { url: admin.url });
    Deno.addSignalListener("SIGTERM", stop);
    Deno.addSignalListener("SIGINT", stop);
    await done;
  } finally {
    Deno.removeSignalListener("SIGTERM", stop);
    Deno.removeSignalListener("SIGINT", stop);
    if (admin) await admin.close();
    if (rpc) await rpc.close();
    else {
      await fence.close();
      await engine.close();
    }
  }
}
if (import.meta.main) {
  if (Deno.args.length !== 1) throw new Error("参数：私有配置绝对路径");
  await runRuntimeService(Deno.args[0]);
}
