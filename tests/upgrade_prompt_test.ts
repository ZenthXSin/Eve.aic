import assert from "node:assert/strict";
import { renderScriptContracts } from "../composition/script-docs.ts";
import { fingerprint } from "../kernel/calls.ts";
import type { Json } from "../kernel/protocol.ts";
import { refreshUpgradePrompt } from "../supervisor/prompt.ts";
import type { SourceArtifact } from "../supervisor/source.ts";

async function fixture(directory: string, version: string): Promise<SourceArtifact> {
  await Deno.mkdir(`${directory}/contracts`, { recursive: true });
  await Deno.mkdir(`${directory}/prompts`);
  const contents: Record<string, string> = { "prompts/qq-social.md": `QQ ${version}\n` };
  for (
    const name of ["phone", "model", "script", "mind", "notes", "workspace", "memory", "upgrades"]
  ) {
    contents[`contracts/${name}.ts`] = `export interface ${name} { ${version}: string; }\n`;
  }
  // A candidate generator is code, not a privileged deployment entrypoint.
  await Deno.mkdir(`${directory}/composition`);
  contents["composition/eve.ts"] = `throw new Error("candidate code executed");`;
  const files: SourceArtifact["files"] = [];
  for (const [path, content] of Object.entries(contents)) {
    await Deno.writeTextFile(`${directory}/${path}`, content);
    files.push({
      path,
      sha256: [
        ...new Uint8Array(
          await crypto.subtle.digest("SHA-256", new TextEncoder().encode(content)),
        ),
      ].map((b) => b.toString(16).padStart(2, "0")).join(""),
      executable: false,
    });
  }
  return {
    commit: "a".repeat(40),
    directory,
    checkedAt: new Date().toISOString(),
    files,
    digest: await fingerprint(files as unknown as Json),
  };
}

Deno.test("升级提示仅刷新固定原件能力，完整保留身份定制，不执行候选代码", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-upgrade-prompt-" });
  try {
    const active = await fixture(`${directory}/old`, "old");
    const candidate = await fixture(`${directory}/new`, "alias");
    const identity = "我是 Eve。\n\n这里是主人定制的身份和个人设定。\n";
    const oldContracts = await renderScriptContracts((p) =>
      Deno.readTextFile(`${active.directory}/${p}`)
    );
    const prompt = `${identity}\n\nQQ old\n\n\n${oldContracts}`;
    const result = await refreshUpgradePrompt(prompt, active, candidate);
    assert.ok(result.startsWith(`${identity}\n\nQQ alias\n\n\n`));
    assert.ok(result.includes("interface notes { alias: string; }"));
    assert.ok(!result.includes("old: string"));
    for (
      const invalid of [
        "only a custom identity",
        `${prompt}\ncustom suffix`,
        `${prompt}${prompt.slice(identity.length)}`,
        prompt.slice(identity.length + 2),
      ]
    ) {
      await assert.rejects(() => refreshUpgradePrompt(invalid, active, candidate), /生成区/);
    }
    await Deno.writeTextFile(`${candidate.directory}/contracts/notes.ts`, "changed after pin");
    await assert.rejects(() => refreshUpgradePrompt(prompt, active, candidate), /摘要变化/);
    candidate.files = candidate.files.filter((file) => file.path !== "contracts/notes.ts");
    await assert.rejects(() => refreshUpgradePrompt(prompt, active, candidate), /缺少/);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});
