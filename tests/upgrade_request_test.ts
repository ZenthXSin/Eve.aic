import assert from "node:assert/strict";
import {
  type UpgradeOptions,
  type UpgradeReceipt,
  UpgradeRequests,
} from "../supervisor/upgrades.ts";
import {
  type GuardianControl,
  type InstanceDriver,
  type InstanceSpec,
  Supervisor,
} from "../supervisor/controller.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { OwnershipRpcClient, startOwnershipRpc } from "../guardian/rpc.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { fingerprint } from "../kernel/calls.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import type { OwnershipProof } from "../contracts/ownership.ts";
import type { ModelRequest, ModelResponse } from "../contracts/model.ts";
import { renderScriptContracts } from "../composition/script-docs.ts";

const key = (name: string) => `upgrade_request_fixture_${name}_012345678901234567890123456789`;
async function git(directory: string, args: string[]) {
  const result = await new Deno.Command("git", {
    args,
    cwd: directory,
    stdout: "piped",
    stderr: "piped",
  }).output();
  if (!result.success) throw new Error(new TextDecoder().decode(result.stderr));
  return new TextDecoder().decode(result.stdout).trim();
}

for (const requestedBy of ["instance", "administrator"] as const) {
  Deno.test({
    name:
      `源码升级申请（${requestedBy}）：真实守护鉴权、低UID机械验证、只读配置分组、稳定动作恢复及两版确认`,
    ignore: new TextDecoder().decode(
      new Deno.Command("/usr/bin/id", { args: ["-u"], stdout: "piped" }).outputSync().stdout,
    ).trim() !== "0",
    async fn() {
      const directory = await Deno.makeTempDir({
        dir: "/var/lib",
        prefix: "eve-upgrade-request-fixture-",
      });
      await Deno.chmod(directory, 0o755);
      const deno = `${directory}/deno`;
      await Deno.link(Deno.execPath(), deno);
      // 机械验证使用 systemd User，需要像生产 eve-exec 一样有已注册的低权限账户。
      const commandIdentity = { uid: 65534, gid: 65534 },
        oldIdentity = { uid: 63011, gid: 63011 },
        nextIdentity = { uid: 63012, gid: 63012 };
      const repository = `${directory}/repository`;
      await Deno.mkdir(repository, { mode: 0o755 });
      await Deno.chmod(repository, 0o755);
      await Deno.writeTextFile(`${repository}/main.ts`, "export const fixture = true;\n", {
        mode: 0o644,
      });
      await Deno.writeTextFile(
        `${repository}/fixture_test.ts`,
        `Deno.test("validation uses isolated uid", () => { if (Deno.uid() !== ${commandIdentity.uid}) throw new Error("unexpected root validation"); });\n`,
        { mode: 0o644 },
      );
      await Deno.writeTextFile(
        `${repository}/deno.json`,
        JSON.stringify({
          tasks: { check: "deno check main.ts", test: "deno test --allow-sys=uid fixture_test.ts" },
        }),
        { mode: 0o644 },
      );
      await Deno.mkdir(`${repository}/prompts`);
      await Deno.mkdir(`${repository}/contracts`);
      const promptFiles = [
        "prompts/qq-social.md",
        ...[
          "phone",
          "model",
          "script",
          "mind",
          "notes",
          "workspace",
          "memory",
          "upgrades",
          "web",
        ].map((name) => `contracts/${name}.ts`),
      ];
      const promptManifest = [];
      for (const path of promptFiles) {
        const bytes = await Deno.readFile(new URL(`../${path}`, import.meta.url));
        await Deno.writeFile(`${repository}/${path}`, bytes, { mode: 0o644 });
        const sha256 = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
          .map((byte) => byte.toString(16).padStart(2, "0")).join("");
        promptManifest.push({ path, sha256, executable: false });
      }
      for (const name of ["main.ts", "fixture_test.ts", "deno.json"]) {
        await Deno.chmod(`${repository}/${name}`, 0o644);
      }
      await git(repository, ["init", "-q"]);
      await git(repository, ["add", "."]);
      await git(repository, [
        "-c",
        "user.name=Upgrade Fixture",
        "-c",
        "user.email=fixture@localhost",
        "commit",
        "-qm",
        "low uid candidate",
      ]);
      const commit = await git(repository, ["rev-parse", "HEAD"]);
      const ownRepository = async (path: string) => {
        const stat = await Deno.lstat(path);
        await Deno.chown(path, commandIdentity.uid, commandIdentity.gid);
        if (stat.isDirectory) {
          for await (const entry of Deno.readDir(path)) {
            await ownRepository(`${path}/${entry.name}`);
          }
        }
      };
      await ownRepository(repository);
      for (const name of ["artifacts", "private", "instances", "old"]) {
        await Deno.mkdir(`${directory}/${name}`, { mode: 0o755 });
        await Deno.chmod(`${directory}/${name}`, 0o755);
      }
      const oldPrivate = `${directory}/old`;
      await Deno.writeTextFile(`${oldPrivate}/credential.key`, key("old"), { mode: 0o440 });
      await Deno.chown(`${oldPrivate}/credential.key`, 0, oldIdentity.gid);
      await Deno.writeTextFile(`${oldPrivate}/runtime.key`, key("runtime"));
      await Deno.writeTextFile(`${oldPrivate}/phone.key`, key("phone"));
      const identity = "你是 Eve，升级测试固定身份。\n定制身份保留。\n";
      const originalPrompt = `${identity}\n\n${await Deno.readTextFile(
        `${repository}/prompts/qq-social.md`,
      )}\n\n${await renderScriptContracts()}`;
      await Deno.writeTextFile(`${oldPrivate}/prompt.md`, originalPrompt);
      const old: InstanceSpec = {
        id: "old",
        identity: oldIdentity,
        source: {
          commit: "a".repeat(40),
          directory: repository,
          digest: await fingerprint(promptManifest),
          checkedAt: new Date().toISOString(),
          files: promptManifest,
        },
        configFile: `${oldPrivate}/config.json`,
        connectionFile: `${oldPrivate}/connection.json`,
        keyFile: `${oldPrivate}/control.key`,
        credentialFile: `${oldPrivate}/credential.key`,
        proofFile: `${oldPrivate}/proof.json`,
      };
      await writeDurableJson(old.configFile, {
        instance: {
          id: "old",
          sourceCommit: old.source.commit,
          directory: `${directory}/old-runtime`,
        },
        runtime: { keyFile: `${oldPrivate}/runtime.key` },
        phone: { adapterKeyFile: `${oldPrivate}/phone.key` },
        eve: { promptFile: `${oldPrivate}/prompt.md` },
        mindDirectory: `${directory}/old-mind`,
        scripts: {},
      });
      await Deno.chmod(old.configFile, 0o440);
      await Deno.chown(old.configFile, 0, oldIdentity.gid);
      const authority = new OwnershipAuthority(`${directory}/guardian`, 60_000);
      await authority.initialize();
      const rpc = startOwnershipRpc(authority, key("admin"), key("exit"));
      const guardianConnectionFile = `${directory}/guardian.json`;
      await writeDurableJson(guardianConnectionFile, { url: rpc.url });
      const guardianCall = async <T>(method: string, args: unknown = {}): Promise<T> => {
        const client = new OwnershipRpcClient(rpc.url, key("admin"));
        try {
          return await client.call<T>(method, args);
        } finally {
          client.close();
        }
      };
      const guardian: GuardianControl = {
        status: () => guardianCall("status"),
        register: (args) => guardianCall("register", args),
        grant: (args) => guardianCall("grant", args),
        pause: (args) => guardianCall("pause", args),
      };
      const states = new Map<string, string>();
      const driver: InstanceDriver = {
        ensure(instance) {
          if (!states.has(instance.id)) states.set(instance.id, "standby");
          return Promise.resolve();
        },
        async proof(instance, proof) {
          await writeDurableJson(instance.proofFile, proof);
        },
        terminate(instance) {
          states.set(instance.id, "stopped");
          return Promise.resolve();
        },
        call<T>(instance: InstanceSpec, method: string, args: Json = {}) {
          let value: unknown = null;
          if (method === "activate") states.set(instance.id, "active");
          else if (method === "deactivate") states.set(instance.id, "standby");
          else if (method === "status") {
            value = {
              state: states.get(instance.id) ?? "standby",
              sourceCommit: instance.source.commit,
              instanceId: instance.id,
              error: null,
              eve: { healthy: true, running: states.get(instance.id) === "active" },
              plugins: ["eve", "phone", "runtime", "scripts", "mind"].map((name) => ({
                name,
                version: "1.0.0",
                status: "running",
              })),
            };
          } else if (method === "prepareObserver") value = { throughSeq: 0 };
          else if (method === "observerRequest") {
            value = {
              messages: [{ role: "user", content: "observe actual candidate" }],
              tools: [],
            } satisfies ModelRequest;
          } else if (method === "observerResponse") {
            value = (args as unknown as { response: ModelResponse }).response.toolCalls;
          }
          return Promise.resolve(value as T);
        },
      };
      const supervisor = new Supervisor({
        directory: `${directory}/supervisor`,
        instances: [old],
        driver,
        guardian,
        backend: {
          quiesce: () => Promise.resolve(),
          watermark: () => Promise.resolve(0),
          rollback: () => Promise.resolve(),
          complete: () =>
            Promise.resolve({
              content: "我确认候选",
              toolCalls: [{
                id: "confirm",
                name: "confirm_upgrade",
                args: { accept: true, reason: "候选机械验证通过" },
              }],
            }),
          saveNote: () => Promise.resolve(null),
        },
      });
      await supervisor.initialize();
      await supervisor.bootstrap("old");
      const originalProof: OwnershipProof = JSON.parse(await Deno.readTextFile(old.proofFile));
      const options: UpgradeOptions = {
        directory: `${directory}/upgrade`,
        repository,
        artifactDirectory: `${directory}/artifacts`,
        instancePrivateDirectory: `${directory}/private`,
        instanceRuntimeDirectory: `${directory}/instances`,
        guardianConnectionFile,
        deno,
        commandIdentity,
        instanceIdentities: [oldIdentity, nextIdentity],
      };
      let upgrades = new UpgradeRequests(options, supervisor, key("supervisor-control"));
      await upgrades.initialize();
      const requestId = "stable_upgrade_action";
      const actionId = requestedBy === "administrator" ? `administrator_${requestId}` : requestId;
      const data = (proof: OwnershipProof, method: string, args: Json, id = actionId) => ({
        method,
        args,
        ownership: { proof: proof as unknown as Json, actionId: id },
      });
      try {
        await assert.rejects(
          () => upgrades.invoke(data(originalProof, "request", { commit }), "Bearer forged"),
          (error) => error instanceof KernelError && error.kind === "rejected",
        );
        for (
          const credential of [
            null,
            "Bearer forged",
            `Bearer ${key("admin")}`,
            `Bearer ${originalProof.credential}`,
          ]
        ) {
          await assert.rejects(
            () => upgrades.requestAdministrator({ commit, requestId }, credential),
            (error) => error instanceof KernelError && error.kind === "rejected",
          );
        }
        for (
          const forged of [
            data(originalProof, "request", { commit, requestedBy: "administrator" }),
            { ...data(originalProof, "request", { commit }), requestedBy: "administrator" },
            data(originalProof, "request", { commit }, "administrator_forged"),
          ]
        ) {
          await assert.rejects(
            () => upgrades.invoke(forged, `Bearer ${originalProof.credential}`),
            (error) => error instanceof KernelError && error.kind === "rejected",
          );
        }
        const submit = (value: string) =>
          requestedBy === "administrator"
            ? upgrades.requestAdministrator(
              { commit: value, requestId },
              `Bearer ${key("supervisor-control")}`,
            )
            : upgrades.invoke(
              data(originalProof, "request", { commit: value }),
              `Bearer ${originalProof.credential}`,
            );
        const receipt = await submit(commit) as unknown as UpgradeReceipt;
        assert.equal(receipt.actionId, actionId);
        assert.equal(receipt.requestedBy, requestedBy);
        await upgrades.close();
        upgrades = new UpgradeRequests(options, supervisor, key("supervisor-control"));
        await upgrades.initialize();
        const active = supervisor.instance(supervisor.state.current!);
        const proof: OwnershipProof = JSON.parse(await Deno.readTextFile(active.proofFile));
        const status = await upgrades.invoke(
          data(proof, "status", { actionId }, "read_status"),
          `Bearer ${proof.credential}`,
        ) as unknown as UpgradeReceipt;
        assert.equal(status.state, "submitted", JSON.stringify(status));
        assert.equal(status.outcome, "confirmed");
        assert.equal(status.requestedBy, requestedBy);
        assert.equal(status.reason, "候选机械验证通过");
        const latest = await upgrades.invoke(
          data(proof, "status", {}, "read_latest"),
          `Bearer ${proof.credential}`,
        );
        assert.deepEqual(latest, status);
        assert.notEqual(active.id, old.id);
        assert.equal(active.identity?.uid, nextIdentity.uid);
        assert.equal(supervisor.options.instances.length, 2);
        const retried = await (requestedBy === "administrator"
          ? upgrades.requestAdministrator(
            { commit, requestId },
            `Bearer ${key("supervisor-control")}`,
          )
          : upgrades.invoke(
            data(proof, "request", { commit }),
            `Bearer ${proof.credential}`,
          )) as unknown as UpgradeReceipt;
        assert.equal(retried.requestedBy, requestedBy);
        assert.equal(retried.instanceId, active.id);
        assert.equal(supervisor.options.instances.length, 2);
        await assert.rejects(
          () =>
            requestedBy === "administrator"
              ? upgrades.requestAdministrator(
                { commit: "b".repeat(40), requestId },
                `Bearer ${key("supervisor-control")}`,
              )
              : upgrades.invoke(
                data(proof, "request", { commit: "b".repeat(40) }),
                `Bearer ${proof.credential}`,
              ),
          /不能换提交/,
        );
        await assert.rejects(
          () =>
            upgrades.invoke(
              data(originalProof, "status", { actionId }, "read_stale_status"),
              `Bearer ${originalProof.credential}`,
            ),
          (error) => error instanceof KernelError && error.kind === "unavailable",
        );
        const candidateConfig = JSON.parse(await Deno.readTextFile(active.configFile));
        assert.equal(await Deno.readTextFile(candidateConfig.eve.promptFile), originalPrompt);
        for (
          const path of [
            active.configFile,
            active.keyFile,
            active.credentialFile,
            candidateConfig.runtime.keyFile,
            candidateConfig.phone.adapterKeyFile,
            candidateConfig.eve.promptFile,
          ]
        ) {
          const stat = await Deno.stat(path);
          assert.equal(stat.uid, 0);
          assert.equal(stat.gid, nextIdentity.gid);
          assert.equal((stat.mode ?? 0) & 0o777, 0o440);
        }
        assert.equal(
          (await Deno.stat(`${options.instancePrivateDirectory}/${active.id}`)).gid,
          nextIdentity.gid,
        );
        assert.equal(
          (await Deno.stat(`${options.instanceRuntimeDirectory}/${active.id}`)).uid,
          nextIdentity.uid,
        );
      } catch (error) {
        // 固定 fixture 源码的验证输出不含业务凭据，失败时保留到测试日志后再清理目录。
        for await (const artifact of Deno.readDir(options.artifactDirectory)) {
          if (!artifact.isDirectory) continue;
          for (const task of ["check", "test"]) {
            try {
              console.error(
                `fixture validation ${task}: ${await Deno.readTextFile(
                  `${options.artifactDirectory}/${artifact.name}/validation-${task}.log`,
                )}`,
              );
            } catch (failure) {
              if (!(failure instanceof Deno.errors.NotFound)) throw failure;
            }
          }
        }
        throw error;
      } finally {
        await upgrades.close();
        await supervisor.close();
        await rpc.close();
        await authority.close();
        await Deno.remove(directory, { recursive: true });
      }
    },
  });
}

Deno.test("管理员升级：严格独立密钥与参数，准备中断恢复不重跑校验且保留来源", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-admin-upgrade-recovery-" });
  const options: UpgradeOptions = {
    directory,
    repository: `${directory}/repository`,
    artifactDirectory: `${directory}/artifacts`,
    instancePrivateDirectory: `${directory}/private`,
    instanceRuntimeDirectory: `${directory}/runtime`,
    guardianConnectionFile: `${directory}/guardian.json`,
    deno: Deno.execPath(),
    commandIdentity: { uid: 65534, gid: 65534 },
    instanceIdentities: [{ uid: 63011, gid: 63011 }, { uid: 63012, gid: 63012 }],
  };
  const supervisor = {
    options: { instances: [] },
    state: { upgrade: null, lastOutcome: null, current: null, paused: false },
    stageUpgrade() {
      throw new Error("interrupted validation must not restart");
    },
  } as unknown as Supervisor;
  const receipt = {
    actionId: "administrator_recovery",
    requestedBy: "administrator",
    state: "preparing",
    commit: "a".repeat(40),
    instanceId: "candidate_interrupted",
  };
  const journal = new FactJournal(`${directory}/requests`);
  await journal.initialize();
  await journal.append("receipt", receipt);
  await journal.close();
  const upgrades = new UpgradeRequests(options, supervisor, key("supervisor-control"));
  try {
    await upgrades.initialize();
    const auth = `Bearer ${key("supervisor-control")}`;
    const disabled = new UpgradeRequests(options, supervisor);
    await assert.rejects(
      () => disabled.requestAdministrator({ commit: receipt.commit, requestId: "recovery" }, auth),
      /独立监督控制凭据/,
    );
    for (
      const args of [
        { commit: "HEAD", requestId: "recovery" },
        { commit: receipt.commit, requestId: "a".repeat(65) },
        { commit: receipt.commit, requestId: "recovery", requestedBy: "administrator" },
        { commit: receipt.commit, requestId: "recovery", ownership: {} },
      ] as Json[]
    ) await assert.rejects(() => upgrades.requestAdministrator(args, auth), /完整Git提交/);
    const recovered = await upgrades.requestAdministrator({
      commit: receipt.commit,
      requestId: "recovery",
    }, auth) as unknown as UpgradeReceipt;
    assert.equal(recovered.state, "failed");
    assert.equal(recovered.error, "validation_interrupted");
    assert.equal(recovered.requestedBy, "administrator");
    assert.equal(recovered.actionId, receipt.actionId);
    assert.deepEqual(
      await upgrades.requestAdministrator({ commit: receipt.commit, requestId: "recovery" }, auth),
      recovered,
    );
    await assert.rejects(
      () => upgrades.requestAdministrator({ commit: "b".repeat(40), requestId: "recovery" }, auth),
      /不能换提交/,
    );
    await upgrades.close();
    await assert.rejects(
      () => upgrades.requestAdministrator({ commit: receipt.commit, requestId: "recovery" }, auth),
      /已关闭/,
    );
  } finally {
    await upgrades.close();
    await Deno.remove(directory, { recursive: true });
  }
});
