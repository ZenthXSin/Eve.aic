import assert from "node:assert/strict";
import { CommandSessions } from "../services/runtime/commands.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { FileTools } from "../services/runtime/files.ts";

Deno.test("执行权限：未显式开启管理员模式不能配置root身份", () => {
  const files = new FileTools("/tmp");
  assert.throws(() => new CommandSessions("/tmp/unused", files, { uid: 0, gid: 0 }));
  assert.throws(() => new CommandSessions("/tmp/unused", files, { uid: 1, gid: 1 }, false, true));
  assert.throws(() => new CommandSessions("/tmp/unused", files, undefined, false, true));
});

Deno.test("执行权限：管理员文件出口可读写工作区外的绝对路径，默认模式仍按工作区校验", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-runtime-access-" });
  const workspace = `${directory}/workspace`;
  await Deno.mkdir(workspace);
  const path = `${directory}/outside.txt`;
  const options = {
    directory: `${directory}/data`,
    workspace,
    settingsFile: `${directory}/settings.json`,
    maxOutputTokens: 10,
    model: { complete: () => Promise.reject(new Error("unused")), close() {} },
  };
  const restricted = new RuntimeEngine(options);
  const admin = new RuntimeEngine({
    ...options,
    accessMode: "unrestricted",
    commandIdentity: { uid: 0, gid: 0 },
  });
  try {
    await assert.rejects(
      restricted.files.write({ path, text: "outside", expectedSha256: "absent" }),
    );
    const saved = await admin.files.write({ path, text: "搬家检查", expectedSha256: "absent" });
    assert.equal((await admin.files.read({ path })).text, "搬家检查");
    await admin.files.write({ path, text: "已确认", expectedSha256: saved.sha256 });
    assert.equal(await Deno.readTextFile(path), "已确认");
    assert.equal(await admin.files.path("/"), "/");
    assert.throws(() => new RuntimeEngine({ ...options, accessMode: "unrestricted" }));
  } finally {
    await restricted.close();
    await admin.close();
    await Deno.remove(directory, { recursive: true });
  }
});

const root = Deno.build.os === "linux" && new TextDecoder().decode(
      (await new Deno.Command("/usr/bin/id", { args: ["-u"] }).output()).stdout,
    ).trim() === "0";
Deno.test({
  name: "执行权限：管理员命令真实以root运行且保留主机能力和独立cgroup",
  ignore: !root,
  async fn() {
    const directory = await Deno.makeTempDir({ prefix: "eve-root-command-" });
    const files = new FileTools(directory, ["/"]);
    const commands = new CommandSessions(
      `${directory}/sessions`,
      files,
      { uid: 0, gid: 0 },
      true,
      true,
    );
    try {
      await commands.initialize();
      const job = await commands.start({
        cwd: "/",
        cmd:
          `python3 -c 'import os,json,pathlib; s=dict(x.split(":",1) for x in pathlib.Path("/proc/self/status").read_text().splitlines() if ":" in x); print(json.dumps({"uid":os.getuid(),"gid":os.getgid(),"caps":s["CapEff"].strip(),"noNewPrivs":s["NoNewPrivs"].strip(),"home":os.environ["HOME"],"cgroup":pathlib.Path("/proc/self/cgroup").read_text()}))'`,
      }, crypto.randomUUID());
      await job.done;
      const result = await commands.poll({ sessionId: job.sessionId });
      assert.equal(result.exitCode, 0, result.stderr.text);
      const actual = JSON.parse(result.stdout.text);
      assert.equal(actual.uid, 0);
      assert.equal(actual.gid, 0);
      assert.notEqual(BigInt(`0x${actual.caps}`), 0n);
      assert.equal(actual.noNewPrivs, "0");
      assert.equal(actual.home, "/root");
      assert.ok(actual.cgroup.includes(`eve-command-${job.sessionId}.service`));
    } finally {
      await commands.close();
      await files.close();
      await Deno.remove(directory, { recursive: true });
    }
  },
});
