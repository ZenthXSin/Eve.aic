import { readPhoneConfig } from "../plugins/phone/mod.ts";

/** 手机 worker 只读本机连接配置与密钥，不授予 QQ 凭据、原件或写日志权限。 */
export async function composePhonePlugin(directory: string, configPath: string): Promise<void> {
  const config = await readPhoneConfig(configPath);
  const module = new URL("../plugins/phone/mod.ts", import.meta.url);
  await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
  await Deno.writeTextFile(
    `${directory}/mod.ts`,
    `import {createPhonePlugin} from ${JSON.stringify(module.href)};\n` +
      `export default createPhonePlugin(${JSON.stringify(configPath)});\n`,
    { mode: 0o600 },
  );
  await Deno.writeTextFile(
    `${directory}/manifest.json`,
    JSON.stringify(
      {
        name: "phone",
        version: "1.0.0",
        provides: { phone: "1.0.0" },
        permissions: {
          net: [new URL(config.adapterUrl).host],
          read: [
            configPath,
            config.adapterKeyFile,
            ...(config.ownershipProofFile ? [config.ownershipProofFile] : []),
            new URL("../plugins/phone/", import.meta.url).pathname,
          ],
        },
      },
      null,
      2,
    ) + "\n",
    { mode: 0o600 },
  );
}
