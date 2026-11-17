import assert from "node:assert/strict";
import { renderScriptContracts } from "../composition/script-docs.ts";

Deno.test("精简工具提示：常用输入与实际脚本类型兼容，完整契约按需读取", async () => {
  const prompt = await renderScriptContracts();
  assert.ok(prompt.length < 7500);
  assert.equal(prompt.includes("export interface ModelApi"), false);
  assert.equal(prompt.includes("export interface ScriptHostApi"), false);
  const blocks = [...prompt.matchAll(/```ts\n([\s\S]*?)\n```/g)];
  assert.equal(blocks.length, 1);
  const code = blocks[0][1];
  const checks: string[] = [];
  let namespace = "";
  for (const line of code.split("\n")) {
    const section = line.match(/^ {2}(\w+): \{$/);
    if (section) namespace = section[1];
    const method = line.match(/^ {4}(\w+)\(/);
    if (method) {
      const compact = `CoreTools["${namespace}"]["${method[1]}"]`;
      const actual = `ScriptApi["${namespace}"]["${method[1]}"]`;
      checks.push(
        `type Check${checks.length} = Assert<Parameters<${compact}> extends Parameters<${actual}> ? true : false>;`,
      );
    }
  }
  assert.ok(checks.length >= 30);
  const directory = await Deno.makeTempDir({ prefix: "eve-compact-contracts-" });
  try {
    const path = `${directory}/contract-check.ts`;
    await Deno.writeTextFile(
      path,
      `import type { ScriptApi } from ${
        JSON.stringify(new URL("../contracts/script.ts", import.meta.url).href)
      };\n${code}\ntype Assert<T extends true> = T;\n${checks.join("\n")}\n`,
    );
    const checked = await new Deno.Command(Deno.execPath(), {
      args: ["check", "--no-config", path],
      stdout: "piped",
      stderr: "piped",
    }).output();
    assert.equal(checked.success, true, new TextDecoder().decode(checked.stderr));
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});
