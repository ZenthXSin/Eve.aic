import assert from "node:assert/strict";
import { composeEvePlugin } from "../composition/eve.ts";
import { composeScriptPlugin } from "../composition/scripts.ts";
import { readEveConfig } from "../plugins/eve/mod.ts";
import { readScriptConfig } from "../plugins/script-runtime/mod.ts";

Deno.test("长历史启动：意识流与脚本默认五分钟，可配置且拒绝无效计时期限", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-startup-config-" });
  const path = `${directory}/config.json`;
  const eve = {
    directory: `${directory}/mind`,
    promptFile: `${directory}/prompt.md`,
    sourceCommit: "a".repeat(40),
    modelTimeoutMs: 1000,
    maxOutputTokens: 1000,
    pollMs: 100,
    defaultSleepMs: 1000,
    dailyTokenBudget: 10000,
  };
  const scripts = {
    directory: `${directory}/scripts`,
    denoBinary: Deno.execPath(),
    cacheDirectory: `${directory}/cache`,
    defaultTimeoutMs: 1000,
  };
  const write = (value: unknown) => Deno.writeTextFile(path, JSON.stringify(value));
  try {
    for (const startupTimeoutMs of [undefined, 450_000]) {
      await write({ eve: { ...eve, startupTimeoutMs }, scripts: { ...scripts, startupTimeoutMs } });
      assert.equal((await readEveConfig(path)).startupTimeoutMs, startupTimeoutMs);
      assert.equal((await readScriptConfig(path)).startupTimeoutMs, startupTimeoutMs);
      await composeEvePlugin(`${directory}/eve-plugin`, path);
      await composeScriptPlugin(`${directory}/script-plugin`, path);
      for (const plugin of ["eve-plugin", "script-plugin"]) {
        const manifest = JSON.parse(
          await Deno.readTextFile(`${directory}/${plugin}/manifest.json`),
        );
        assert.equal(manifest.deadlines.enableMs, startupTimeoutMs ?? 300_000);
      }
    }
    for (const startupTimeoutMs of [0, -1, 1.5, "300000", null, 2_147_483_648]) {
      await write({ eve: { ...eve, startupTimeoutMs }, scripts: { ...scripts, startupTimeoutMs } });
      await assert.rejects(readEveConfig(path));
      await assert.rejects(readScriptConfig(path));
    }
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});
