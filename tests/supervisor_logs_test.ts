import assert from "node:assert/strict";
import { Buffer } from "node:buffer";
import { readLogFile, readServiceJournal } from "../supervisor/logs.ts";
Deno.test("完整日志：超过旧8MiB限制，跨UTF-8边界逐页读取，字节完全一致且可读追加", async () => {
  const dir = await Deno.makeTempDir({ prefix: "eve-full-logs-" });
  try {
    const path = `${dir}/test.log`,
      original = new TextEncoder().encode("完整测试日志✓\n".repeat(550000));
    assert.ok(original.length > 8388608);
    await Deno.writeFile(path, original);
    const parts: Uint8Array[] = [];
    let offset = 0;
    while (true) {
      const page = await readLogFile(path, offset, 262143) as {
        data: string;
        nextOffset: number;
        eof: boolean;
      };
      parts.push(Buffer.from(page.data, "base64"));
      offset = page.nextOffset;
      if (page.eof) break;
    }
    assert.deepEqual(Buffer.concat(parts), Buffer.from(original));
    await Deno.writeTextFile(path, "追加", { append: true });
    const page = await readLogFile(path, offset) as { data: string };
    assert.equal(Buffer.from(page.data, "base64").toString(), "追加");
    await assert.rejects(() => readLogFile(path, -1));
    await assert.rejects(() => readServiceJournal("../../etc/shadow"));
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("升级校验完整落盘：检查输出超过8MiB仍保存末尾，失败Git命令保留原始诊断", async () => {
  const { pinSource } = await import("../supervisor/source.ts");
  const dir = await Deno.makeTempDir({ prefix: "eve-validation-full-" });
  const git = async (args: string[]) => {
    const result = await new Deno.Command("git", {
      args,
      cwd: dir,
      stdout: "piped",
      stderr: "piped",
    }).output();
    assert.equal(result.code, 0, new TextDecoder().decode(result.stderr));
    return new TextDecoder().decode(result.stdout).trim();
  };
  try {
    await Deno.writeTextFile(
      `${dir}/deno.json`,
      JSON.stringify({
        tasks: {
          check: 'deno eval \'console.log("x".repeat(9000000)+"COMPLETE_LOG_TAIL")\'',
          test: "deno eval 'console.log(\"test complete\")'",
        },
      }),
    );
    await git(["init", "-q"]);
    await git(["add", "deno.json"]);
    await git([
      "-c",
      "user.name=Log Fixture",
      "-c",
      "user.email=fixture@localhost",
      "commit",
      "-qm",
      "fixture",
    ]);
    const commit = await git(["rev-parse", "HEAD"]);
    await pinSource({
      repository: dir,
      commit,
      directory: `${dir}/release`,
      deno: Deno.execPath(),
    });
    const log = await Deno.readTextFile(`${dir}/release/validation-check.log`);
    assert.ok(log.length > 9000000);
    assert.ok(log.endsWith("COMPLETE_LOG_TAIL\n"));
    await assert.rejects(() =>
      pinSource({
        repository: dir,
        commit: "f".repeat(40),
        directory: `${dir}/failure`,
        deno: Deno.execPath(),
      })
    );
    const failure = await Deno.readTextFile(`${dir}/failure/source-command.log`);
    assert.ok(failure.includes("stderr:"));
    assert.ok(failure.includes("fatal:"));
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});
