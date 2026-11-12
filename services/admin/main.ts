import { writeDurableJson } from "../../sdk/durable-json.ts";
import { AdminControlClient } from "./client.ts";
import { startAdminServer } from "./server.ts";

export async function runAdminService(configPath: string) {
  const config = JSON.parse(await Deno.readTextFile(configPath));
  const c = config.admin;
  if (
    !c ||
    [c.directory, c.tokenFile, c.backendConnectionFile, c.backendKeyFile].some((p) =>
      typeof p !== "string" || !p.startsWith("/")
    )
  ) {
    throw new Error("管理服务需要完整私有路径");
  }
  await Deno.mkdir(c.directory, { recursive: true, mode: 0o700 });
  const server = await startAdminServer(
    new AdminControlClient(c.backendConnectionFile, c.backendKeyFile),
    {
      token: (await Deno.readTextFile(c.tokenFile)).replace(/\r?\n$/, ""),
      publicOrigin: c.publicOrigin,
      port: c.port,
    },
  );
  let stop!: () => void;
  const done = new Promise<void>((resolve) => stop = resolve);
  Deno.addSignalListener("SIGTERM", stop);
  Deno.addSignalListener("SIGINT", stop);
  try {
    await writeDurableJson(`${c.directory}/connection.json`, { url: server.url });
    await done;
  } finally {
    Deno.removeSignalListener("SIGTERM", stop);
    Deno.removeSignalListener("SIGINT", stop);
    await server.close();
  }
}

if (import.meta.main) {
  if (Deno.args.length !== 1) throw new Error("参数：私有配置绝对路径");
  await runAdminService(Deno.args[0]);
}
