import assert from "node:assert/strict";
import { writeEvePrompt } from "../composition/eve.ts";
import { renderScriptContracts } from "../composition/script-docs.ts";
import { fingerprint } from "../kernel/calls.ts";
import type { Json } from "../kernel/protocol.ts";
import { refreshUpgradePrompt } from "../supervisor/prompt.ts";
import type { SourceArtifact } from "../supervisor/source.ts";

async function fixture(
  directory: string,
  version: string,
  nativeThoughts = false,
  splitMechanics = false,
): Promise<SourceArtifact> {
  await Deno.mkdir(`${directory}/contracts`, { recursive: true });
  await Deno.mkdir(`${directory}/prompts`);
  const contents: Record<string, string> = { "prompts/qq-social.md": `QQ ${version}\n` };
  if (splitMechanics) {
    contents["prompts/qq-social.md"] =
      `<!-- eve-qq-prompt-layout: social-mechanics-v1 -->\nQQ ${version}\n`;
    contents["prompts/qq-mechanics.md"] = `mechanics ${version}\n`;
  }
  for (
    const name of ["phone", "model", "script", "mind", "notes", "workspace", "memory", "upgrades"]
  ) {
    contents[`contracts/${name}.ts`] = `export interface ${name} { ${version}: string; }\n`;
  }
  if (nativeThoughts) {
    contents["contracts/script.ts"] = "export const NATIVE_THOUGHT_CAPABILITY_VERSION = 1;\n" +
      contents["contracts/script.ts"];
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

// Frozen generated preamble from 6107d62: changing it silently would reject existing prompts.
const legacyPreamble =
  "唯一原生工具 execute_script 的 code 是异步函数正文。当前参数名 tools，类型为 ScriptApi；可 await 和 return。\n" +
  "脚本默认异步返回持久任务编号，具体动作编号由宿主生成。不要把同一任务重新写成新调用来猜结果；查 scripts.status/receipt 或 phone.action。\n" +
  "mode:sync 等待完成后，原生回执的 result 是脚本 return 值的 JSON 文本页；result.truncated 为 true 时保留原 jobId，按 nextChar 分页读取。tools.scripts.result 的 limit 为 1 至 8192；用 return await tools.scripts.result(...) 将该页交回主意识。\n" +
  "upgrades.request 只提交完整 Git commit，独立监督校验并由旧 Eve 明确确认；回执 actionId 用于 upgrades.status，不以 submitted 冒充升级完成。\n" +
  "以下为当前契约原件；PhoneApi.send/sendFile 在 ScriptApi 中省去 actionId，由宿主提供。ScriptHostApi 供宿主领取任务/通知，不是 tools 内的额外对象。\n\n";

Deno.test("升级提示：610版固定生成说明可刷新到新能力，并保留当前人格原文", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-upgrade-legacy-prompt-" });
  try {
    const active = await fixture(`${directory}/old`, "legacy610");
    const candidate = await fixture(`${directory}/new`, "newStatus", true);
    const identity = await Deno.readTextFile(new URL("../prompts/identity.md", import.meta.url));
    const generated = await renderScriptContracts((p) =>
      Deno.readTextFile(`${active.directory}/${p}`)
    );
    const sections = generated.slice(generated.indexOf("接口来源：contracts/"));
    // 使用610冻结前言独立比对，防止新版renderer悄悄改变旧源码生成区。
    assert.equal(generated, legacyPreamble + sections);
    const prompt = `${identity}\n\nQQ legacy610\n\n\n${legacyPreamble}${sections}`;
    const result = await refreshUpgradePrompt(prompt, active, candidate);
    assert.ok(result.startsWith(`${identity}\n\nQQ newStatus`));
    assert.ok(result.includes("interface upgrades { newStatus: string; }"));
    assert.equal(result.includes("legacy610: string"), false);
    assert.ok(result.includes("原生工具有 execute_script 与 write_thought。"));
    assert.ok(result.includes("execute_script 可选参数 thought"));
    assert.ok(result.includes("同一内容不重复记录"));
    assert.equal(result.includes("唯一原生工具 execute_script"), false);
    assert.equal(result.indexOf(identity), 0);
    assert.equal(result.lastIndexOf(identity), 0);
    // 新版同样能再升级；身份前缀不因能力标记改变而丢失或重复。
    const next = await fixture(`${directory}/next`, "next", true);
    const refreshed = await refreshUpgradePrompt(result, candidate, next);
    assert.ok(refreshed.startsWith(`${identity}\n\nQQ next`));
    assert.equal(refreshed.includes("newStatus: string"), false);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("升级提示：旧社交生成区迁移至社交与机制分区，再升级仍逐字保留定制前缀", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-upgrade-split-prompt-" });
  try {
    const legacy = await fixture(`${directory}/old`, "old", true);
    const candidate = await fixture(`${directory}/new`, "new", true, true);
    const next = await fixture(`${directory}/next`, "next", true, true);
    const prefix = "自定义人格\r\n\r\n原来是谁就还是谁。\n  保留空白与称呼  \n";
    const render = async (source: SourceArtifact) => {
      const social = await Deno.readTextFile(`${source.directory}/prompts/qq-social.md`);
      const mechanics = source.files.some((file) => file.path === "prompts/qq-mechanics.md")
        ? `${await Deno.readTextFile(`${source.directory}/prompts/qq-mechanics.md`)}\n\n`
        : "";
      const contracts = await renderScriptContracts((path) =>
        Deno.readTextFile(`${source.directory}/${path}`)
      );
      return `${social}\n\n${mechanics}${contracts}`;
    };
    const oldPrompt = `${prefix}\n\n${await render(legacy)}`;
    const expected = `${prefix}\n\n${await render(candidate)}`;
    const migrated = await refreshUpgradePrompt(oldPrompt, legacy, candidate);
    assert.equal(migrated, expected);
    assert.equal(migrated.slice(0, prefix.length), prefix);
    const refreshed = await refreshUpgradePrompt(migrated, candidate, next);
    assert.equal(refreshed, `${prefix}\n\n${await render(next)}`);
    assert.equal(refreshed.includes("mechanics new"), false);
    assert.equal(refreshed.includes("QQ old"), false);
    for (const ambiguous of [migrated + "\nextra", migrated + "\n\n" + await render(candidate)]) {
      await assert.rejects(() => refreshUpgradePrompt(ambiguous, candidate, next), /生成区/);
    }
    await Deno.writeTextFile(`${next.directory}/prompts/qq-mechanics.md`, "mutated mechanisms");
    await assert.rejects(() => refreshUpgradePrompt(migrated, candidate, next), /摘要变化/);
    next.files = next.files.filter((file) => file.path !== "prompts/qq-mechanics.md");
    await assert.rejects(() => refreshUpgradePrompt(migrated, candidate, next), /缺少 QQ 机制/);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("初次组成提示按人格、社交、机制、实际契约排序，完整保留人格原文", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-compose-split-prompt-" });
  try {
    const path = `${directory}/prompt.md`;
    const identity = await Deno.readTextFile(new URL("../prompts/identity.md", import.meta.url));
    const social = await Deno.readTextFile(new URL("../prompts/qq-social.md", import.meta.url));
    const mechanics = await Deno.readTextFile(
      new URL("../prompts/qq-mechanics.md", import.meta.url),
    );
    await writeEvePrompt(path);
    const result = await Deno.readTextFile(path);
    assert.equal(
      result,
      `${identity}\n\n${social}\n\n${mechanics}\n\n${await renderScriptContracts()}`,
    );
    assert.equal(result.indexOf(identity), 0);
    assert.equal(result.lastIndexOf(identity), 0);
    assert.equal(result.lastIndexOf(mechanics), identity.length + social.length + 4);
    assert.equal(social.includes("blockedBy"), false);
    assert.equal(social.includes("receiptDigest"), false);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("升级人格：仅精确旧原件可迁移，新人格摘要验真；自定义前缀逐字保留", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-identity-migration-" });
  try {
    const old = await fixture(`${directory}/old`, "old"),
      next = await fixture(`${directory}/next`, "next");
    const add = async (source: SourceArtifact, identity: string) => {
      const path = "prompts/identity.md", bytes = new TextEncoder().encode(identity);
      await Deno.writeTextFile(`${source.directory}/${path}`, identity);
      source.files.push({
        path,
        sha256: [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))].map((b) =>
          b.toString(16).padStart(2, "0")
        ).join(""),
        executable: false,
      });
    };
    await add(old, "旧人格\n");
    await add(next, "新版人格，学感兴趣的事\n");
    const suffix = `\n\nQQ old\n\n\n${await renderScriptContracts((p) =>
      Deno.readTextFile(`${old.directory}/${p}`)
    )}`;
    assert.ok(
      (await refreshUpgradePrompt("旧人格\n" + suffix, old, next)).startsWith(
        "新版人格，学感兴趣的事\n",
      ),
    );
    assert.ok(
      (await refreshUpgradePrompt("主人的定制\r\n  原字不改\n" + suffix, old, next)).startsWith(
        "主人的定制\r\n  原字不改\n",
      ),
    );
    await Deno.writeTextFile(`${next.directory}/prompts/identity.md`, "被改的原件");
    await assert.rejects(() => refreshUpgradePrompt("旧人格\n" + suffix, old, next), /摘要变化/);
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});
