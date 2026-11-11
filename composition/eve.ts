import { renderScriptContracts } from "./script-docs.ts";
import { readEveConfig } from "../plugins/eve/mod.ts";

export async function writeEvePrompt(path: string): Promise<void> {
  const identity = await Deno.readTextFile(new URL("../prompts/identity.md", import.meta.url));
  const qq = await Deno.readTextFile(new URL("../prompts/qq-social.md", import.meta.url));
  const prompt = `${identity}\n\n${qq}\n\n${await renderScriptContracts()}`;
  await Deno.writeTextFile(path, prompt, { mode: 0o600 });
}
export async function composeEvePlugin(directory: string, configPath: string): Promise<void> {
  const config = await readEveConfig(configPath);
  await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
  const module = new URL("../plugins/eve/mod.ts", import.meta.url);
  await Deno.writeTextFile(
    `${directory}/mod.ts`,
    `import {createEvePlugin} from ${
      JSON.stringify(module.href)
    };\nexport default createEvePlugin(${JSON.stringify(configPath)});\n`,
    { mode: 0o600 },
  );
  await Deno.writeTextFile(
    `${directory}/manifest.json`,
    JSON.stringify(
      {
        name: "eve",
        version: "1.0.0",
        provides: { eve: "1.0.0" },
        requires: {
          model: "^1.0.0",
          tools: "^1.0.0",
          scripts: "^1.0.0",
          phone: "^1.0.0",
          mind: "^1.0.0",
        },
        permissions: {
          read: [
            configPath,
            config.promptFile,
            config.directory,
            new URL("../plugins/eve/", import.meta.url).pathname,
            new URL("../kernel/calls.ts", import.meta.url).pathname,
          ],
          write: [config.directory],
        },
        deadlines: { enableMs: 15000, disableMs: 10000 },
      },
      null,
      2,
    ) + "\n",
    { mode: 0o600 },
  );
}
export async function composeMindPlugin(directory: string, dataDirectory: string): Promise<void> {
  const module = new URL("../plugins/mind-controls/mod.ts", import.meta.url);
  await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
  await Deno.writeTextFile(
    `${directory}/mod.ts`,
    `import {createMindControls} from ${
      JSON.stringify(module.href)
    };\nexport default createMindControls(${JSON.stringify(dataDirectory)});\n`,
    { mode: 0o600 },
  );
  await Deno.writeTextFile(
    `${directory}/manifest.json`,
    JSON.stringify(
      {
        name: "mind-controls",
        version: "1.0.0",
        provides: { mind: "1.0.0" },
        permissions: {
          read: [
            dataDirectory,
            new URL("../plugins/mind-controls/", import.meta.url).pathname,
            new URL("../kernel/calls.ts", import.meta.url).pathname,
          ],
          write: [dataDirectory],
        },
      },
      null,
      2,
    ) + "\n",
    { mode: 0o600 },
  );
}
