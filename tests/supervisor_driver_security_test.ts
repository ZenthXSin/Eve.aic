import assert from "node:assert/strict";
import { LocalInstanceDriver } from "../supervisor/driver.ts";

const root = new TextDecoder().decode(
  new Deno.Command("/usr/bin/id", { args: ["-u"], stdout: "piped" }).outputSync().stdout,
).trim() === "0";

Deno.test({
  name: "专属实例UID复用：拒绝未登记身份并清理setsid脱离进程组的后代",
  ignore: !root,
  async fn() {
    const identity = { uid: 63221, gid: 63221 };
    const exists = await new Deno.Command("/usr/bin/pgrep", {
      args: ["--uid", String(identity.uid)],
      stdout: "null",
      stderr: "null",
    }).output();
    assert.equal(exists.code, 1, "fixture专属UID已被其他进程占用，拒绝清理");
    const directory = await Deno.makeTempDir({ prefix: "eve-driver-security-" });
    const driver = new LocalInstanceDriver(Deno.execPath(), directory, [identity]);
    const child = new Deno.Command("/usr/bin/setpriv", {
      args: [
        "--reuid",
        String(identity.uid),
        "--regid",
        String(identity.gid),
        "--clear-groups",
        "/usr/bin/setsid",
        "/bin/sleep",
        "600",
      ],
      stdin: "null",
      stdout: "null",
      stderr: "null",
      clearEnv: true,
    }).spawn();
    try {
      await assert.rejects(() => driver.prepareIdentity({ uid: 65534, gid: 65534 }));
      await assert.rejects(() => driver.prepareIdentity({ uid: 0, gid: 0 }));
      // 等待setpriv完成身份切换再核对，避免只测到root包装进程。
      const deadline = Date.now() + 5000;
      let found = false;
      while (Date.now() < deadline) {
        const result = await new Deno.Command("/usr/bin/pgrep", {
          args: ["--uid", String(identity.uid)],
          stdout: "piped",
          stderr: "null",
        }).output();
        if (result.code === 0) {
          found = true;
          break;
        }
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      assert.equal(found, true);
      await driver.prepareIdentity(identity);
      assert.equal((await child.status).success, false);
      // 重复回收无副作用，且不会误杀监督进程。
      await driver.prepareIdentity(identity);
    } finally {
      try {
        child.kill("SIGKILL");
      } catch { /* 已由专属UID回收。 */ }
      await child.status;
      await Deno.remove(directory, { recursive: true });
    }
  },
});
