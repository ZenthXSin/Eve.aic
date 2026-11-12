import { RUNTIME_METHODS } from "../contracts/runtime.ts";
import { readRuntimeConnection } from "../plugins/runtime-client/mod.ts";

export async function composeRuntimePlugin(directory: string, configPath: string) {
  const c = await readRuntimeConnection(configPath);
  const connection = JSON.parse(await Deno.readTextFile(c.connectionFile));
  const url = new URL(connection.url);
  if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port) {
    throw new Error("执行出口仅使用本机服务");
  }
  const module = new URL("../plugins/runtime-client/mod.ts", import.meta.url);
  await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
  await Deno.writeTextFile(
    `${directory}/mod.ts`,
    `import {createRuntimeClientPlugin} from ${
      JSON.stringify(module.href)
    };\nexport default createRuntimeClientPlugin(${JSON.stringify(configPath)});\n`,
    { mode: 0o600 },
  );
  await Deno.writeTextFile(
    `${directory}/manifest.json`,
    JSON.stringify(
      {
        name: "runtime-client",
        version: "1.0.0",
        provides: Object.fromEntries(
          [...Object.keys(RUNTIME_METHODS), "upgrades"].map((key) => [key, "1.0.0"]),
        ),
        permissions: {
          net: ["127.0.0.1"],
          read: [
            configPath,
            c.connectionFile,
            c.keyFile,
            c.proofFile,
            ...(c.upgradeConnectionFile ? [c.upgradeConnectionFile] : []),
            new URL("../services/runtime/client.ts", import.meta.url).pathname,
            new URL("../plugins/runtime-client/", import.meta.url).pathname,
          ],
        },
      },
      null,
      2,
    ) + "\n",
    { mode: 0o600 },
  );
}
