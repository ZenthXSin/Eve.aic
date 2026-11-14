import { RecoveryReporter } from "../sdk/recovery.ts";
import { dirname, join } from "node:path";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { OwnershipAuthority } from "./ownership.ts";
import { startOwnershipRpc } from "./rpc.ts";

/** 独立令牌仲裁入口，不调用模型，也不直接触发QQ或业务动作。 */
export async function runOwnershipService(configPath: string) {
  const config = JSON.parse(await Deno.readTextFile(configPath)).ownership;
  if (
    !config ||
    [config.directory, config.adminKeyFile, config.exitKeyFile].some((path) =>
      typeof path !== "string" || !path.startsWith("/")
    )
  ) throw new Error("守护使用独立私有绝对路径");
  const authority = new OwnershipAuthority(config.directory, config.leaseMs);
  let ready = false;
  const reporter = new RecoveryReporter(
    config.recoveryFile ?? join(dirname(config.directory), "startup/guardian.json"),
    "guardian",
    () => authority.store.recovery,
  );
  let rpc: ReturnType<typeof startOwnershipRpc> | undefined;
  let resolve!: () => void;
  const done = new Promise<void>((finish) => {
    resolve = finish;
  });
  const stop = () => resolve();
  let listening = false;
  try {
    rpc = startOwnershipRpc(
      authority,
      (await Deno.readTextFile(config.adminKeyFile)).trim(),
      (await Deno.readTextFile(config.exitKeyFile)).trim(),
      { ready: () => ready, progress: () => authority.store.recovery },
    );
    await writeDurableJson(`${config.directory}/connection.json`, { url: rpc.url });
    if (config.connectionFile) {
      if (typeof config.connectionFile !== "string" || !config.connectionFile.startsWith("/")) {
        throw new Error("守护公开连接文件需要绝对路径");
      }
      await writeDurableJson(config.connectionFile, { url: rpc.url });
      await Deno.chmod(config.connectionFile, 0o644);
    }
    Deno.addSignalListener("SIGINT", stop);
    Deno.addSignalListener("SIGTERM", stop);
    listening = true;
    await reporter.start();
    try {
      await authority.initialize();
      ready = true;
      await reporter.finish("ready");
    } catch (error) {
      await reporter.finish("failed");
      throw error;
    }
    console.log(JSON.stringify({ ownership: "ready", epoch: authority.status().epoch }));
    await done;
  } finally {
    await reporter.close();
    if (listening) {
      Deno.removeSignalListener("SIGINT", stop);
      Deno.removeSignalListener("SIGTERM", stop);
    }
    try {
      await rpc?.close();
    } finally {
      await authority.close();
    }
  }
}
if (import.meta.main) {
  if (Deno.args.length !== 1) throw new Error("参数：守护私有配置路径");
  await runOwnershipService(Deno.args[0]);
}
