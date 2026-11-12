import assert from "node:assert/strict";
import { pinSource } from "../supervisor/source.ts";

const root = new TextDecoder().decode(
  (await new Deno.Command("/usr/bin/id", {
    args: ["-u"],
    stdout: "piped",
    stderr: "null",
  }).output()).stdout,
).trim() === "0";

Deno.test({
  name: "源码校验独立 cgroup 清除成功、失败及超时的 setsid 子进程且保留同 UID 无关服务",
  ignore: !root,
  async fn() {
    const directory = await Deno.makeTempDir({
      dir: "/var/lib",
      prefix: "eve-validation-cgroup-test-",
    });
    const uid = 65534;
    const gid = 65534;
    const witness = `eve-validation-witness-${crypto.randomUUID()}.service`;
    const run = async (cmd: string, args: string[], cwd?: string) => {
      const result = await new Deno.Command(cmd, {
        args,
        cwd,
        stdout: "piped",
        stderr: "piped",
      }).output();
      assert.ok(result.success, new TextDecoder().decode(result.stderr));
      return new TextDecoder().decode(result.stdout).trim();
    };
    try {
      await Deno.chmod(directory, 0o755);
      const deno = `${directory}/deno`;
      try {
        await Deno.link(Deno.execPath(), deno);
      } catch {
        await Deno.copyFile(Deno.execPath(), deno);
      }
      await Deno.chmod(deno, 0o755);
      await run("/usr/bin/systemd-run", [
        "--quiet",
        "--collect",
        `--unit=${witness}`,
        `--property=User=${uid}`,
        `--property=Group=${gid}`,
        "/usr/bin/sleep",
        "90",
      ]);
      for (const outcome of ["success", "failure", "timeout"]) {
        const repository = `${directory}/repository-${outcome}`;
        const shared = `${directory}/shared-${outcome}`;
        await Deno.mkdir(repository, { mode: 0o755 });
        await Deno.mkdir(shared, { mode: 0o700 });
        await Deno.chown(shared, uid, gid);
        await Deno.writeTextFile(
          `${repository}/deno.json`,
          JSON.stringify({
            tasks: {
              check: "deno run --allow-all fixture.ts check",
              test: "deno run --allow-all fixture.ts test",
            },
          }),
        );
        await Deno.writeTextFile(
          `${repository}/fixture.ts`,
          `
const marker = ${JSON.stringify(shared)} + "/" + Deno.args[0] + ".pid";
new Deno.Command("/usr/bin/setsid", {
  args:["/bin/sh", "-c", 'echo $$ > "$1"; while true; do echo inherited-stdout; sleep 1; done', "fixture", marker],
  stdin:"null", stdout:"inherit", stderr:"inherit",
}).spawn().unref();
for(let i=0;i<100;i++) {
  if(await Deno.stat(marker).then(()=>true,()=>false)) break;
  await new Promise(r=>setTimeout(r,10));
}
if(${JSON.stringify(outcome)} === "failure") throw new Error("expected fixture failure");
if(${JSON.stringify(outcome)} === "timeout") await new Promise(()=>{});
`,
        );
        await run("git", ["init", "-q"], repository);
        await run("git", ["add", "."], repository);
        await run("git", [
          "-c",
          "user.name=Fixture",
          "-c",
          "user.email=fixture@local",
          "commit",
          "-qm",
          outcome,
        ], repository);
        const commit = await run("git", ["rev-parse", "HEAD"], repository);
        const chown = async (path: string) => {
          if ((await Deno.lstat(path)).isDirectory) {
            for await (const entry of Deno.readDir(path)) await chown(`${path}/${entry.name}`);
          }
          await Deno.chown(path, uid, gid);
        };
        await chown(repository);
        const started = Date.now();
        const validate = () =>
          pinSource({
            repository,
            commit,
            directory: `${directory}/artifact-${outcome}`,
            deno,
            validationIdentity: { uid, gid },
            validationTimeoutMs: 2_000,
          });
        if (outcome === "success") await validate();
        else await assert.rejects(validate, /校验失败|校验超时/);
        assert.ok(Date.now() - started < 15_000, "校验不得等待逃逸子进程持有的管道");
        let markers = 0;
        for await (const marker of Deno.readDir(shared)) {
          const pid = Number((await Deno.readTextFile(`${shared}/${marker.name}`)).trim());
          const status = await Deno.readTextFile(`/proc/${pid}/status`).catch(() => "");
          assert.ok(!status || /^State:\s+Z/m.test(status), `setsid 子进程 ${pid} 仍存活`);
          markers++;
        }
        assert.equal(markers, outcome === "success" ? 2 : 1);
        assert.equal(await run("/usr/bin/systemctl", ["is-active", witness]), "active");
        if (outcome === "success") {
          const launcher = `${directory}/private-tmp-launcher.ts`;
          await Deno.writeTextFile(
            launcher,
            `import {pinSource} from ${
              JSON.stringify(new URL("../supervisor/source.ts", import.meta.url).href)
            }; await pinSource(${
              JSON.stringify({
                repository,
                commit,
                directory: `${directory}/artifact-private-tmp`,
                deno,
                validationIdentity: { uid, gid },
                validationTimeoutMs: 5_000,
              })
            });`,
          );
          await run("/usr/bin/systemd-run", [
            "--quiet",
            "--wait",
            "--pipe",
            "--collect",
            `--unit=eve-validation-private-tmp-${crypto.randomUUID()}.service`,
            "--property=PrivateTmp=yes",
            `--setenv=DENO_DIR=${directory}/launcher-cache`,
            deno,
            "run",
            "--allow-all",
            launcher,
          ]);
          assert.equal(await run("/usr/bin/systemctl", ["is-active", witness]), "active");
        }
      }
    } finally {
      await new Deno.Command("/usr/bin/systemctl", {
        args: ["stop", witness],
        stdout: "null",
        stderr: "null",
      }).output();
      await Deno.remove(directory, { recursive: true });
    }
  },
});
