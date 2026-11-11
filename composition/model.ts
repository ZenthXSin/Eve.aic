import { readPrivateModelConfig } from "../plugins/model-chat/mod.ts";

/** 只写无密钥的插件入口和清单，授予模型 worker 读取指定配置与密钥的权限。 */
export async function composeModelPlugin(directory: string, configPath: string): Promise<void> {
  const config = await readPrivateModelConfig(configPath);
  const module = new URL("../plugins/model-chat/mod.ts", import.meta.url);
  const codeDir = new URL("../plugins/model-chat/", import.meta.url).pathname;
  await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
  await Deno.writeTextFile(
    `${directory}/mod.ts`,
    `import { createChatModelPlugin } from ${JSON.stringify(module.href)};\n` +
      `export default createChatModelPlugin(${JSON.stringify(configPath)});\n`,
    { mode: 0o600 },
  );
  await Deno.writeTextFile(
    `${directory}/manifest.json`,
    JSON.stringify(
      {
        name: "model-chat",
        version: "1.0.0",
        provides: { model: "1.0.0" },
        permissions: {
          net: [new URL(config.baseUrl).host],
          read: [configPath, config.apiKeyFile, codeDir],
        },
      },
      null,
      2,
    ) + "\n",
    { mode: 0o600 },
  );
}
