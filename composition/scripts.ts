import { readScriptConfig } from "../plugins/script-runtime/mod.ts";

/** 运行与缓存目录在私有组合中指定，清单仅授予类型检查所需的Deno执行路径。 */
export async function composeScriptPlugin(directory: string, configPath: string): Promise<void> {
  const config = await readScriptConfig(configPath);
  const module = new URL("../plugins/script-runtime/mod.ts", import.meta.url);
  await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
  await Deno.writeTextFile(
    `${directory}/mod.ts`,
    `import {createScriptPlugin} from ${
      JSON.stringify(module.href)
    };\nexport default createScriptPlugin(${JSON.stringify(configPath)});\n`,
    { mode: 0o600 },
  );
  await Deno.writeTextFile(
    `${directory}/manifest.json`,
    JSON.stringify(
      {
        name: "script-runtime",
        version: "1.0.0",
        provides: { scripts: "1.0.0", tools: "1.0.0" },
        requires: { phone: "^1.0.0" },
        optional: {
          upgrades: "^1.0.0",
          mind: "^1.0.0",
          notes: "^1.0.0",
          files: "^1.0.0",
          commands: "^1.0.0",
          memory: "^1.0.0",
          records: "^1.0.0",
        },
        permissions: {
          read: [
            configPath,
            config.directory,
            new URL("../plugins/script-runtime/", import.meta.url).pathname,
            new URL("../kernel/calls.ts", import.meta.url).pathname,
          ],
          write: [config.directory, config.cacheDirectory],
          run: [config.denoBinary],
        },
        deadlines: { enableMs: config.startupTimeoutMs ?? 300_000, disableMs: 10_000 },
      },
      null,
      2,
    ) + "\n",
    { mode: 0o600 },
  );
}
