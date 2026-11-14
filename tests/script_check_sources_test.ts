import assert from "node:assert/strict";
import { ScriptCheckSources } from "../plugins/script-runtime/check-sources.ts";

Deno.test("脚本检查缓存：相同源码共享稳定URL，导入保留原路径，在途文件不清除", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-check-sources-" });
  const sources = new ScriptCheckSources(directory);
  try {
    const [first, second] = await Promise.all([
      sources.acquire("/original/a.ts", "return 1;", "return 1;"),
      sources.acquire("/original/b.ts", "return 1;", "return 1;"),
    ]);
    assert.equal(first.path, second.path);
    for (let i = 0; i < 70; i++) {
      const source = await sources.acquire(
        `/original/${i}.ts`,
        `return ${i + 2};`,
        `return ${i + 2};`,
      );
      await source.release();
    }
    assert.equal(await Deno.readTextFile(first.path), "return 1;");
    await first.release();
    await second.release();
    assert.ok(Array.from(Deno.readDirSync(sources.directory)).length <= 64);
    for (const code of ['return import("./local.ts");', '/// <reference path="./local.ts" />']) {
      const source = await sources.acquire("/original/import.ts", code, code);
      assert.equal(source.path, "/original/import.ts");
      await source.release();
    }
    await sources.close();
    await assert.rejects(Deno.stat(sources.directory), Deno.errors.NotFound);
  } finally {
    await sources.close().catch(() => {});
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本检查缓存：被改写的检查原件拒绝复用并保留诊断", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-check-corrupt-" });
  const sources = new ScriptCheckSources(directory);
  try {
    const source = await sources.acquire("/original/a.ts", "return 1;", "return 1;");
    await source.release();
    await Deno.writeTextFile(source.path, "return 2;");
    await assert.rejects(
      sources.acquire("/original/b.ts", "return 1;", "return 1;"),
      /检查缓存源码不符/,
    );
    await sources.close();
    assert.equal(await Deno.readTextFile(source.path), "return 2;");
  } finally {
    await sources.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("脚本检查缓存：每次仍运行Deno检查，契约改变使成功缓存失效", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-check-dependencies-" });
  const sources = new ScriptCheckSources(`${directory}/cache`);
  try {
    const api = `${directory}/api.ts`;
    await Deno.writeTextFile(api, "export interface Api { ok(): number }");
    const body = "return tools.ok();";
    const code = `import type { Api } from ${JSON.stringify(new URL(`file://${api}`).href)};
      export default async function run(tools: Api) { ${body} }`;
    const source = await sources.acquire(`${directory}/job.ts`, code, body);
    const check = () =>
      new Deno.Command(Deno.execPath(), {
        args: ["check", "--no-config", "--no-lock", "--no-remote", "--no-npm", source.path],
        clearEnv: true,
        env: { DENO_DIR: `${directory}/cache` },
        stdout: "null",
        stderr: "piped",
      }).output();
    assert.equal((await check()).success, true);
    assert.equal((await check()).success, true);
    await Deno.writeTextFile(api, "export interface Api { changed(): number }");
    const invalidated = await check();
    assert.equal(invalidated.success, false);
    assert.match(new TextDecoder().decode(invalidated.stderr), /ok/);
    await source.release();
  } finally {
    await sources.close();
    await Deno.remove(directory, { recursive: true });
  }
});
