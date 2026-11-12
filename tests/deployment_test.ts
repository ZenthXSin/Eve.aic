import assert from "node:assert/strict";
import {
  buildDeploymentPlan,
  type DeploymentOptions,
  stageDeployment,
} from "../deployment/plan.ts";
import { fingerprint } from "../kernel/calls.ts";
import type { Json } from "../kernel/protocol.ts";
const existing = {
  model: {
    protocol: "chat",
    baseUrl: "https://model.invalid/v1",
    model: "fixture",
    apiKeyFile: "/private/model.key",
    sessionId: "eve",
    maxOutputTokens: 100,
    timeoutMs: 1000,
  },
  qq: {
    sandbox: false,
    replyScope: "private",
    appIdFile: "/private/qq-id",
    appSecretFile: "/private/qq-secret",
  },
  phone: {
    adapterUrl: "http://127.0.0.1:1234/phone",
    adapterKeyFile: "/legacy/bridge-key",
    ownerPlatformUserId: "verified-owner",
  },
};
async function fixture() {
  const root = await Deno.makeTempDir({ prefix: "eve-deployment-" });
  await Deno.mkdir(`${root}/source`);
  const options: DeploymentOptions = {
    privateDirectory: `${root}/private`,
    runtimeDirectory: `${root}/runtime`,
    qqDirectory: `${root}/original-qq`,
    workspaceDirectory: `${root}/workspace`,
    deno: "/usr/bin/false",
    source: {
      commit: "a".repeat(40),
      directory: `${root}/source`,
      digest: await fingerprint([] as Json),
      files: [],
      checkedAt: new Date().toISOString(),
    },
    instanceIdentities: [{ uid: 60002, gid: 60002 }, { uid: 60003, gid: 60003 }],
    commandIdentity: {
      uid: Number(
        new TextDecoder().decode(
          new Deno.Command("/usr/bin/id", { args: ["-u"] }).outputSync().stdout,
        ),
      ) || 60001,
      gid: Number(
        new TextDecoder().decode(
          new Deno.Command("/usr/bin/id", { args: ["-g"] }).outputSync().stdout,
        ),
      ) || 60001,
    },
  };
  return { root, options };
}
Deno.test({
  name: "deployment separates instance config from privileged keys and stages no service changes",
  ignore: Number(
    new TextDecoder().decode(
      new Deno.Command("/usr/bin/id", { args: ["-u"] }).outputSync().stdout,
    ),
  ) !== 0,
  fn: async () => {
    const { root, options } = await fixture();
    try {
      await Deno.writeTextFile(
        `${root}/phone.key`,
        "fixture_phone_bridge_key_01234567890123456789\n",
      );
      const plan = buildDeploymentPlan({
        ...existing,
        phone: { ...existing.phone, adapterKeyFile: `${root}/phone.key` },
      }, options);
      assert.equal(plan.units.length, 5);
      for (const u of plan.units) {
        assert.match(u.content, /KillMode=control-group/);
        assert.match(u.content, /NoNewPrivileges=yes/);
        assert.match(u.content, /ProtectSystem=strict/);
      }
      const instance = JSON.parse(plan.files.find((f) => f.path.endsWith("/config.json"))!.content);
      assert.equal(instance.model, undefined);
      assert.equal(instance.qq, undefined);
      assert.equal(instance.ownership, undefined);
      assert(!JSON.stringify(instance).includes("guardian-admin"));
      assert(!JSON.stringify(instance).includes("guardian-exit"));
      assert.equal(instance.phone.ownerPlatformUserId, "verified-owner");
      assert.equal(instance.phone.ownershipProofFile, instance.runtime.proofFile);
      const runtime = JSON.parse(plan.files.find((f) => f.path.endsWith("/runtime.json"))!.content);
      assert.deepEqual(runtime.runtimeService.commandIdentity, options.commandIdentity);
      assert.equal(runtime.runtimeService.semantic, undefined);
      const adapter = JSON.parse(plan.files.find((f) => f.path.endsWith("/qq.json"))!.content);
      assert.equal(adapter.qq.captureActivationBoundary, true);
      assert.deepEqual(adapter.qq.mediaIdentity, options.commandIdentity);
      assert.deepEqual(adapter.qq.mediaRoots, [options.workspaceDirectory]);
      assert.equal(
        adapter.qq.mediaRuntime.workerPath,
        `${options.source.directory}/services/runtime/files-worker.ts`,
      );
      const before = structuredClone(existing);
      await stageDeployment(plan, options.source, `${root}/units`);
      assert.deepEqual(existing, before);
      const privateConfig = await Deno.stat(`${options.privateDirectory}/runtime.json`);
      assert.equal(privateConfig.mode! & 0o777, 0o600);
      const settings = JSON.parse(
        await Deno.readTextFile(`${options.privateDirectory}/runtime-settings.json`),
      );
      assert.equal(settings.paused, true);
      assert.equal(settings.dailyTokenBudget, 200_000_000);
      const own = options.instanceIdentities[0], other = options.instanceIdentities[1];
      const read = async (identity: { uid: number; gid: number }, path: string) =>
        await new Deno.Command("/usr/bin/setpriv", {
          args: [
            "--reuid",
            String(identity.uid),
            "--regid",
            String(identity.gid),
            "--clear-groups",
            "/bin/cat",
            path,
          ],
          stdout: "piped",
          stderr: "piped",
        }).output();
      assert.equal((await read(own, instance.runtime.keyFile)).success, true);
      assert.equal((await read(other, instance.runtime.keyFile)).success, false);
      assert.equal(
        (await read(own, `${options.privateDirectory}/guardian-admin.key`)).success,
        false,
      );
      assert.equal((await read(options.commandIdentity, instance.runtime.keyFile)).success, false);
      const ownConfig = plan.files.find((f) => f.path.endsWith("/config.json"))!.path;
      const mutate = async (args: string[]) =>
        await new Deno.Command("/usr/bin/setpriv", {
          args: ["--reuid", String(own.uid), "--regid", String(own.gid), "--clear-groups", ...args],
          stdout: "piped",
          stderr: "piped",
        }).output();
      assert.equal(
        (await mutate(["/bin/sh", "-c", 'printf tampered > "$1"', "sh", ownConfig])).success,
        false,
      );
      assert.equal((await mutate(["/bin/mv", ownConfig, `${ownConfig}.renamed`])).success, false);
      const configStat = await Deno.stat(ownConfig);
      assert.equal(configStat.uid, 0);
      assert.equal(configStat.gid, own.gid);
      assert.equal(configStat.mode! & 0o777, 0o440);
      assert.equal((await read(own, ownConfig)).success, true);
      assert.equal((await Deno.stat(`${options.privateDirectory}/instances`)).mode! & 0o777, 0o711);
      assert.equal((await Deno.stat(`${options.runtimeDirectory}/public`)).mode! & 0o777, 0o755);
      assert.equal(
        (await Deno.readTextFile(plan.prompt.path)).length > 100,
        true,
      );
      await assert.rejects(
        () => stageDeployment(plan, options.source, `${root}/units`),
        Deno.errors.AlreadyExists,
      );
    } finally {
      await Deno.remove(root, { recursive: true });
    }
  },
});
Deno.test("deployment rejects root command identity, unbound owner, public QQ and unsafe paths", async () => {
  const { root, options } = await fixture();
  try {
    assert.throws(() =>
      buildDeploymentPlan(existing, { ...options, commandIdentity: { uid: 0, gid: 0 } })
    );
    assert.throws(() => buildDeploymentPlan({ ...existing, phone: {} }, options));
    assert.throws(() =>
      buildDeploymentPlan(existing, {
        ...options,
        instanceIdentities: [{ uid: 60002, gid: 60002 }, { uid: 60003, gid: 60002 }],
      })
    );
    assert.throws(() =>
      buildDeploymentPlan({ ...existing, qq: { ...existing.qq, replyScope: "group" } }, options)
    );
    assert.throws(() =>
      buildDeploymentPlan(existing, { ...options, privateDirectory: "/tmp/../root" })
    );
    assert.throws(() =>
      buildDeploymentPlan(existing, { ...options, runtimeDirectory: "/tmp/x%h" })
    );
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});
Deno.test("deployment CLI dry run reads existing config without creating private/runtime state or leaking owner", async () => {
  const { root, options } = await fixture();
  try {
    await Deno.writeTextFile(`${root}/existing.json`, JSON.stringify(existing));
    await Deno.writeTextFile(
      `${root}/options.json`,
      JSON.stringify({
        ...options,
        existingConfigFile: `${root}/existing.json`,
        unitDirectory: `${root}/units`,
      }),
    );
    const before = [...Deno.readDirSync(root)].map((e) => e.name).sort();
    const output = await new Deno.Command(Deno.execPath(), {
      args: [
        "run",
        "--allow-read",
        new URL("../deployment/main.ts", import.meta.url).pathname,
        "--dry-run",
        `${root}/options.json`,
      ],
      stdout: "piped",
      stderr: "piped",
    }).output();
    assert.equal(output.success, true, new TextDecoder().decode(output.stderr));
    const text = new TextDecoder().decode(output.stdout);
    assert.equal(JSON.parse(text).mode, "dry-run");
    assert(!text.includes("verified-owner"));
    assert(!text.includes("model.key"));
    assert.deepEqual([...Deno.readDirSync(root)].map((e) => e.name).sort(), before);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});
Deno.test("workspace clone keeps independent commit history without private origin or object alternates", async () => {
  const { prepareWorkspaceRepository } = await import("../deployment/workspace.ts");
  const { root, options } = await fixture();
  try {
    const git = async (args: string[], cwd?: string) => {
      const r = await new Deno.Command("git", { args, cwd, stdout: "piped", stderr: "piped" })
        .output();
      assert.equal(r.success, true, new TextDecoder().decode(r.stderr));
      return new TextDecoder().decode(r.stdout).trim();
    };
    const repository = `${root}/original`, target = `${root}/working`;
    await git(["init", "-b", "main", repository]);
    await Deno.writeTextFile(`${repository}/source.ts`, "export const value = 1;\n");
    await git(["add", "source.ts"], repository);
    await git([
      "-c",
      "user.name=Fixture",
      "-c",
      "user.email=fixture@localhost",
      "commit",
      "-m",
      "fixture",
    ], repository);
    const commit = await git(["rev-parse", "HEAD"], repository);
    await prepareWorkspaceRepository({
      repository,
      branch: "main",
      commit,
      target,
      ...options.commandIdentity,
    });
    const config = await Deno.readTextFile(`${target}/.git/config`);
    assert(!config.includes(repository));
    assert(!config.includes('[remote "origin"]'));
    await assert.rejects(
      () => Deno.stat(`${target}/.git/objects/info/alternates`),
      Deno.errors.NotFound,
    );
    assert.equal(await Deno.readTextFile(`${target}/.git/refs/heads/main`), commit + "\n");
    assert.equal(await Deno.readTextFile(`${repository}/source.ts`), "export const value = 1;\n");
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});
