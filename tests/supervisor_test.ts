import assert from "node:assert/strict";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import {
  type BackendControl,
  type GuardianControl,
  type InstanceDriver,
  type InstanceSpec,
  Supervisor,
} from "../supervisor/controller.ts";
import type { OwnershipProof } from "../contracts/ownership.ts";
import type { ModelResponse } from "../contracts/model.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { pinSource, verifySource } from "../supervisor/source.ts";
import { InstanceClient } from "../supervisor/client.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

async function fixture(
  options: { handoverMs?: number; confirmationMs?: number; activationMs?: number } = {},
) {
  const directory = await Deno.makeTempDir({ prefix: "eve-supervisor-test-" });
  let now = 1000;
  const authority = new OwnershipAuthority(`${directory}/guardian`, 60000, () => now);
  await authority.initialize();
  const instances: InstanceSpec[] = [];
  for (const [index, id] of ["old", "new"].entries()) {
    const credentialFile = `${directory}/${id}.credential`;
    await Deno.writeTextFile(credentialFile, `instance_${id}_credential_01234567890123456789`);
    instances.push({
      id,
      credentialFile,
      source: {
        commit: String(index + 1).repeat(40),
        directory: `${directory}/source-${id}`,
        digest: "",
        checkedAt: new Date().toISOString(),
        files: [],
      },
      configFile: `${directory}/${id}.json`,
      keyFile: `${directory}/${id}.key`,
      connectionFile: `${directory}/${id}.connection`,
      proofFile: `${directory}/${id}.proof`,
    });
  }
  const states = new Map<string, string>();
  const proofs = new Map<string, OwnershipProof>();
  let response: ModelResponse = { content: "观察尚未决定", toolCalls: [] };
  let completeAt: number | undefined;
  let seq = 20;
  const markers: unknown[] = [];
  const preparation: string[] = [];
  const driver: InstanceDriver = {
    ensure(instance) {
      if (!states.has(instance.id) || states.get(instance.id) === "dead") {
        states.set(instance.id, "standby");
      }
      return Promise.resolve();
    },
    async call<T>(instance: InstanceSpec, method: string, args: Json = {}) {
      if (states.get(instance.id) === "dead") throw new KernelError("unavailable", "process dead");
      let value: unknown = null;
      if (method === "status") {
        value = {
          state: states.get(instance.id),
          instanceId: instance.id,
          sourceCommit: instance.source.commit,
          error: null,
          eve: { healthy: true, running: states.get(instance.id) === "active" },
          plugins: ["runtime", "phone", "scripts", "mind", "eve"].map((name) => ({
            name,
            status: "running",
            version: "1.0.0",
          })),
        };
      }
      if (method === "activate") {
        await authority.actorStatus(proofs.get(instance.id)!);
        if (instance.id === "new") now += options.activationMs ?? 0;
        assert.ok(
          ![...states.entries()].some(([id, state]) => id !== instance.id && state === "active"),
        );
        states.set(instance.id, "active");
      }
      if (method === "deactivate") {
        states.set(instance.id, "standby");
        preparation.push(`${instance.id}:deactivate`);
      }
      if (method === "prepareObserver" && options.handoverMs !== undefined) {
        assert.equal(states.get(instance.id), "standby");
        await authority.renew(proofs.get(instance.id)!);
        preparation.push(`${instance.id}:prepareObserver`);
      }
      if (method === "observerRequest" || method === "trialRequest") {
        value = { messages: [{ role: "user", content: JSON.stringify(args) }], tools: [] };
      }
      if (method === "observerResponse") {
        value = (args as unknown as { response: ModelResponse }).response.toolCalls;
      }
      return value as T;
    },
    proof(instance, proof) {
      proofs.set(instance.id, proof);
      return Promise.resolve();
    },
    terminate(instance) {
      states.set(instance.id, "dead");
      return Promise.resolve();
    },
    async handover(old, candidate) {
      if (options.handoverMs === undefined) return;
      assert.equal(states.get(old.id), "standby");
      assert.equal(states.get(candidate.id), "standby");
      preparation.push(`${old.id}:handover`);
      now += options.handoverMs;
      await assert.rejects(() => authority.renew(proofs.get(old.id)!), /有效在线租约/);
    },
  };
  const backend: BackendControl = {
    quiesce: () => Promise.resolve(),
    watermark: () => Promise.resolve(seq++),
    rollback(args) {
      if (!markers.some((item) => JSON.stringify(item) === JSON.stringify(args))) {
        markers.push(args);
      }
      return Promise.resolve();
    },
    complete: () => {
      if (completeAt !== undefined) now = completeAt;
      return Promise.resolve(response);
    },
    saveNote: () => Promise.resolve({ saved: true }),
  };
  const supervisorOptions = {
    directory: `${directory}/supervisor`,
    instances,
    driver,
    backend,
    confirmationMs: options.confirmationMs ?? 5000,
    now: () => now,
    guardian: {
      status: () => Promise.resolve(authority.status()),
      register: (args) => authority.register(args),
      grant: (args) => authority.grant(args),
      pause: (args) => authority.pause(args),
    } satisfies GuardianControl,
  };
  let supervisor = new Supervisor(supervisorOptions);
  await supervisor.initialize();
  await supervisor.bootstrap("old");
  return {
    directory,
    authority,
    states,
    proofs,
    markers,
    preparation,
    supervisor: () => supervisor,
    setResponse(value: ModelResponse) {
      response = value;
    },
    completeAt(value: number) {
      completeAt = value;
    },
    setNow(value: number) {
      now = value;
    },
    async restart() {
      await supervisor.close();
      supervisor = new Supervisor(supervisorOptions);
      await supervisor.initialize();
    },
    async close() {
      await supervisor.close();
      await authority.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}
Deno.test("双版本监督：慢准备后首次观察获得完整确认窗口，重启和试聊不续期", async () => {
  const f = await fixture({ activationMs: 4500 });
  try {
    f.setResponse({
      content: "先试聊",
      toolCalls: [{ id: "trial1", name: "trial_candidate", args: { text: "检查新版" } }],
    });
    await f.supervisor().upgrade("new");
    assert.equal(f.supervisor().state.upgrade?.phase, "observing");
    assert.equal(f.supervisor().state.upgrade?.deadline, 10500);
    assert.equal(f.supervisor().state.upgrade?.step, 1);
    f.setNow(8000);
    await f.restart();
    assert.equal(f.supervisor().state.upgrade?.deadline, 10500);
    await f.supervisor().tick();
    assert.equal(f.supervisor().state.upgrade?.step, 2);
    assert.equal(f.supervisor().state.upgrade?.deadline, 10500);
    f.setNow(10500);
    await f.supervisor().tick();
    assert.equal(f.supervisor().state.lastOutcome?.outcome, "rolled_back");
    assert.equal(f.supervisor().state.lastOutcome?.reason, "confirmation_timeout");
  } finally {
    await f.close();
  }
});
Deno.test("双版本监督：准备窗口到期不能通过进入观察重置期限", async () => {
  for (const activationMs of [5000, 5001]) {
    const f = await fixture({ activationMs });
    try {
      f.setResponse({
        content: "即使预置同意也不能掩盖准备超时",
        toolCalls: [{
          id: "yes",
          name: "confirm_upgrade",
          args: { accept: true, reason: "健康" },
        }],
      });
      await f.supervisor().upgrade("new");
      assert.equal(f.supervisor().state.current, "old");
      assert.equal(f.supervisor().state.lastOutcome?.outcome, "rolled_back");
      assert.equal(f.supervisor().state.lastOutcome?.reason, "preparation_timeout");
    } finally {
      await f.close();
    }
  }
});
Deno.test("双版本监督：完整新观察窗口内可确认，到期或迟到的模型接受必须回退", async () => {
  for (const responseAt of [10499, 10500, 10501]) {
    const f = await fixture({ activationMs: 4500 });
    try {
      f.completeAt(responseAt);
      f.setResponse({
        content: "决定",
        toolCalls: [{
          id: "yes",
          name: "confirm_upgrade",
          args: { accept: true, reason: "新版健康" },
        }],
      });
      await f.supervisor().upgrade("new");
      const accepted = responseAt < 10500;
      assert.equal(f.supervisor().state.current, accepted ? "new" : "old");
      assert.equal(
        f.supervisor().state.lastOutcome?.outcome,
        accepted ? "confirmed" : "rolled_back",
      );
      if (!accepted) assert.equal(f.supervisor().state.lastOutcome?.reason, "timeout");
    } finally {
      await f.close();
    }
  }
});
Deno.test("双版本监督：先在有效旧租约内完成快照，账本交接超过租期仍安全授权候选", async () => {
  for (const pendingExit of [false, true]) {
    const f = await fixture({ handoverMs: 60001, confirmationMs: 120000 });
    try {
      const oldProof = f.proofs.get("old")!;
      const permit = pendingExit
        ? await f.authority.begin({
          permitId: "c".repeat(64),
          proof: oldProof,
          resource: "command",
          actionId: "handover_pending_exit",
          requestHash: "d".repeat(64),
        })
        : undefined;
      f.setResponse({
        content: "新版健康",
        toolCalls: [{
          id: "confirm_slow_handover",
          name: "confirm_upgrade",
          args: { accept: true, reason: "快照与账本完整，候选独占租约" },
        }],
      });
      await f.supervisor().upgrade("new");
      assert.deepEqual(f.preparation.slice(0, 3), [
        "old:deactivate",
        "old:prepareObserver",
        "old:handover",
      ]);
      if (permit) {
        assert.equal(f.supervisor().state.upgrade?.phase, "draining");
        assert.equal(f.authority.status().owner?.instanceId, "old");
        assert.equal(f.states.get("new"), "standby");
        await f.authority.finish(permit.id, "completed");
        await f.supervisor().tick();
      }
      assert.equal(f.supervisor().state.current, "new");
      assert.equal(f.supervisor().state.lastOutcome?.outcome, "confirmed");
      assert.equal(f.authority.status().owner?.instanceId, "new");
      assert.ok(f.authority.status().epoch > oldProof.epoch);
      await assert.rejects(() => f.authority.renew(oldProof), /有效在线租约/);
    } finally {
      await f.close();
    }
  }
});
Deno.test("双版本监督：旧实例明确确认与机械健康同时满足才成功，重启保留结果", async () => {
  const f = await fixture();
  try {
    f.setResponse({
      content: "新版实际正常",
      toolCalls: [{
        id: "confirm1",
        name: "confirm_upgrade",
        args: { accept: true, reason: "观察健康" },
      }],
    });
    await f.supervisor().upgrade("new");
    assert.equal(f.supervisor().state.current, "new");
    assert.equal(f.supervisor().state.lastOutcome?.outcome, "confirmed");
    assert.equal(f.states.get("old"), "dead");
    assert.equal(f.authority.status().owner?.instanceId, "new");
    const stale = f.proofs.get("old")!;
    await assert.rejects(() => f.authority.renew(stale));
    await f.restart();
    assert.equal(f.supervisor().state.lastOutcome?.outcome, "confirmed");
    f.states.set("new", "dead");
    await f.supervisor().tick();
    assert.equal(f.supervisor().state.current, "old");
    assert.equal(f.authority.status().owner?.instanceId, "old");
  } finally {
    await f.close();
  }
});
Deno.test("双版本监督：缺少理由、空理由和冲突的多重确认均回退", async () => {
  for (
    const args of [
      [{ accept: true }],
      [{ accept: true, reason: " " }],
      [{ accept: true, reason: "通过" }, { accept: false, reason: "拒绝" }],
    ]
  ) {
    const f = await fixture();
    try {
      f.setResponse({
        content: "待校验决定",
        toolCalls: args.map((value, index) => ({
          id: `confirm${index}`,
          name: "confirm_upgrade",
          args: value,
        })),
      });
      await f.supervisor().upgrade("new");
      assert.equal(f.supervisor().state.current, "old");
      assert.equal(f.supervisor().state.lastOutcome?.outcome, "rolled_back");
    } finally {
      await f.close();
    }
  }
});
Deno.test("双版本监督：稳定升级编号重试采用原结果，复用当前UID被拒绝", async () => {
  const f = await fixture();
  try {
    f.supervisor().instance("old").identity = { uid: 63231, gid: 63231 };
    f.supervisor().instance("new").identity = { uid: 63231, gid: 63232 };
    await assert.rejects(() => f.supervisor().upgrade("new", "upgrade_same_uid"));
    delete f.supervisor().instance("new").identity;
    f.setResponse({
      content: "确认",
      toolCalls: [{ id: "yes", name: "confirm_upgrade", args: { accept: true, reason: "健康" } }],
    });
    const id = "upgrade_stable_fixture";
    assert.equal(await f.supervisor().upgrade("new", id), id);
    assert.equal(await f.supervisor().upgrade("new", id), id);
    assert.equal(f.supervisor().state.lastOutcome?.upgradeId, id);
  } finally {
    await f.close();
  }
});
Deno.test("双版本监督：超时与旧观察进程崩溃都会回切，未确认写入保留来源标记", async () => {
  for (const reason of ["timeout", "observer_crash"]) {
    const f = await fixture();
    try {
      await f.supervisor().upgrade("new");
      assert.equal(f.supervisor().state.upgrade?.phase, "observing");
      const candidateEpoch = f.authority.status().epoch;
      await f.restart();
      if (reason === "timeout") f.setNow(7000);
      else f.states.set("old", "dead");
      await f.supervisor().tick();
      assert.equal(f.supervisor().state.current, "old");
      assert.equal(f.supervisor().state.lastOutcome?.outcome, "rolled_back");
      assert.ok(f.authority.status().epoch > candidateEpoch);
      assert.equal(f.markers.length, 1);
      assert.equal((f.markers[0] as { sourceCommit: string }).sourceCommit, "2".repeat(40));
    } finally {
      await f.close();
    }
  }
});
Deno.test("双版本监督：待决出口阻止交接；持久暂停阻止自动接管，显式恢复才醒来", async () => {
  const f = await fixture();
  try {
    const permit = await f.authority.begin({
      permitId: "b".repeat(64),
      proof: f.proofs.get("old")!,
      resource: "command",
      actionId: "long_command",
      requestHash: "a".repeat(64),
    });
    await f.supervisor().upgrade("new");
    assert.equal(f.supervisor().state.upgrade?.phase, "draining");
    assert.equal(f.authority.status().owner?.instanceId, "old");
    await f.authority.finish(permit.id, "completed");
    await f.supervisor().tick();
    assert.equal(f.supervisor().state.current, "new");
    await f.supervisor().pause();
    await f.restart();
    await f.supervisor().tick();
    assert.equal(f.authority.status().mode, "paused");
    assert.ok(f.supervisor().state.paused);
    await f.supervisor().resume();
    assert.equal(f.supervisor().state.current, "old");
    assert.equal(f.authority.status().mode, "active");
  } finally {
    await f.close();
  }
});
Deno.test("双版本监督：拒绝信号不会变成成功，观察模型超时后的迟到确认也不能成功", async () => {
  for (const accepted of [false, true]) {
    const f = await fixture();
    try {
      f.setResponse({
        content: "决定",
        toolCalls: [{
          id: "c",
          name: "confirm_upgrade",
          args: { accept: accepted, reason: "测试" },
        }],
      });
      if (accepted) {
        f.completeAt(7000);
        await f.supervisor().upgrade("new");
        assert.equal(f.supervisor().state.lastOutcome?.outcome, "rolled_back");
      } else {
        await f.supervisor().upgrade("new");
        assert.equal(f.supervisor().state.lastOutcome?.outcome, "rolled_back");
        assert.ok(f.supervisor().state.lastOutcome?.reason.startsWith("old_eve_rejected"));
      }
    } finally {
      await f.close();
    }
  }
});
Deno.test("固定源码：真实 Git 提交和机械任务产物校验，篡改/新增文件均拒绝", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-supervisor-source-" });
  const run = async (args: string[]) => {
    const out = await new Deno.Command("git", {
      args,
      cwd: directory,
      stdout: "piped",
      stderr: "piped",
    }).output();
    assert.ok(out.success, new TextDecoder().decode(out.stderr));
    return new TextDecoder().decode(out.stdout).trim();
  };
  try {
    await run(["init"]);
    await Deno.writeTextFile(
      `${directory}/deno.json`,
      JSON.stringify({ tasks: { check: "deno check main.ts", test: "deno test main.ts" } }),
    );
    await Deno.writeTextFile(
      `${directory}/main.ts`,
      'Deno.test("pinned mechanical replay",()=>{if(1+1!==2)throw new Error("bad")});',
    );
    await run(["add", "deno.json", "main.ts"]);
    await run([
      "-c",
      "user.name=Fixture",
      "-c",
      "user.email=fixture@local",
      "commit",
      "-m",
      "pinned",
    ]);
    const commit = await run(["rev-parse", "HEAD"]);
    const artifact = await pinSource({
      repository: directory,
      commit,
      directory: `${directory}/artifact`,
      deno: Deno.execPath(),
    });
    await verifySource(artifact);
    await Deno.writeTextFile(`${artifact.directory}/extra.ts`, "new");
    await assert.rejects(() => verifySource(artifact));
    await Deno.remove(`${artifact.directory}/extra.ts`);
    await Deno.writeTextFile(`${artifact.directory}/main.ts`, "tampered");
    await assert.rejects(() => verifySource(artifact));
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
});
Deno.test("实例RPC客户端：每次读取连接并传独立控制凭据，拒绝非本机地址", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-supervisor-client-" });
  const key = "instance_control_fixture_01234567890123456789";
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (req) => {
    assert.equal(req.headers.get("authorization"), `Bearer ${key}`);
    return Response.json({ ok: true, value: await req.json() });
  });
  const config = { connectionFile: `${directory}/connection.json`, keyFile: `${directory}/key` };
  const client = new InstanceClient(config.connectionFile, config.keyFile);
  try {
    await Deno.writeTextFile(config.keyFile, key);
    await writeDurableJson(config.connectionFile, {
      url: `http://127.0.0.1:${server.addr.port}/instance`,
    });
    assert.deepEqual(await client.call("status"), { method: "status", args: {} });
    await writeDurableJson(config.connectionFile, { url: "http://example.com:80/instance" });
    await assert.rejects(() => client.call("status"));
  } finally {
    client.close();
    await server.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});

const validationRootCheck = await new Deno.Command("/usr/bin/id", {
  args: ["-u"],
  stdout: "piped",
  stderr: "null",
}).output();
Deno.test({
  name: "候选真实机械校验使用非root身份，Git低UID仓库可读、管理员秘密不可读且环境不继承",
  ignore: new TextDecoder().decode(validationRootCheck.stdout).trim() !== "0",
  async fn() {
    const directory = await Deno.makeTempDir({ dir: "/tmp", prefix: "eve-supervisor-uid-" });
    const identity = { uid: 65534, gid: 65534 };
    try {
      await Deno.chmod(directory, 0o755);
      await Deno.mkdir(`${directory}/bin`, { mode: 0o755 });
      await Deno.chmod(`${directory}/bin`, 0o755);
      const deno = `${directory}/bin/deno`;
      await Deno.copyFile(Deno.execPath(), deno);
      await Deno.chmod(deno, 0o755);
      await Deno.writeTextFile(`${directory}/guardian.secret`, "fixture_root_secret", {
        mode: 0o600,
      });
      const repository = `${directory}/repository`;
      await Deno.mkdir(repository, { mode: 0o755 });
      await Deno.writeTextFile(
        `${repository}/deno.json`,
        JSON.stringify({
          tasks: {
            check: "deno run --allow-all verify.ts",
            test: "deno run --allow-all verify.ts",
          },
        }),
      );
      await Deno.writeTextFile(
        `${repository}/verify.ts`,
        `if(Deno.uid()===0)throw new Error("root validation");\nlet denied=false;try{await Deno.readTextFile(${
          JSON.stringify(`${directory}/guardian.secret`)
        });}catch{denied=true;}if(!denied)throw new Error("secret leaked");if(Deno.env.get("EVE_SUPERVISOR_SECRET"))throw new Error("env leaked");`,
      );
      const git = async (args: string[]) => {
        const out = await new Deno.Command("git", {
          args,
          cwd: repository,
          stdout: "piped",
          stderr: "piped",
        }).output();
        assert.ok(out.success, new TextDecoder().decode(out.stderr));
        return new TextDecoder().decode(out.stdout).trim();
      };
      await git(["init"]);
      await git(["add", "."]);
      await git([
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@local",
        "commit",
        "-m",
        "restricted validation",
      ]);
      const commit = await git(["rev-parse", "HEAD"]);
      const chown = async (path: string) => {
        const stat = await Deno.lstat(path);
        if (stat.isDirectory) {
          for await (const entry of Deno.readDir(path)) await chown(`${path}/${entry.name}`);
        }
        await Deno.chown(path, identity.uid, identity.gid);
      };
      await chown(repository);
      Deno.env.set("EVE_SUPERVISOR_SECRET", "fixture_env_secret");
      const artifact = await pinSource({
        repository,
        commit,
        directory: `${directory}/artifact`,
        deno,
        validationIdentity: identity,
      }).catch(async (error) => {
        throw new Error(
          `${error.kind}: ${await Deno.readTextFile(`${directory}/artifact/validation-check.log`)
            .catch(() => "no validation log")}`,
        );
      });
      await verifySource(artifact);
    } finally {
      Deno.env.delete("EVE_SUPERVISOR_SECRET");
      await Deno.remove(directory, { recursive: true });
    }
  },
});
