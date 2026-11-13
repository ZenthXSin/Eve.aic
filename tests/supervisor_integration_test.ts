import assert from "node:assert/strict";
import { fingerprint } from "../kernel/calls.ts";
import { type GuardianControl, type InstanceSpec, Supervisor } from "../supervisor/controller.ts";
import { pinSource } from "../supervisor/source.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { OwnershipRpcClient, startOwnershipRpc } from "../guardian/rpc.ts";
import { FileOwnershipExit } from "../composition/ownership.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { startRuntimeRpc } from "../services/runtime/rpc.ts";
import { RuntimeClient } from "../services/runtime/client.ts";
import { RuntimeAdminClient, startRuntimeAdmin } from "../services/runtime/admin.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import type { Json } from "../kernel/protocol.ts";
import { KernelError } from "../kernel/protocol.ts";
import type { ModelRequest, ModelResponse } from "../contracts/model.ts";
import { ScriptRuntime } from "../plugins/script-runtime/runtime.ts";
import { ScriptStore } from "../plugins/script-runtime/store.ts";
import type { PluginContext } from "../sdk/plugin.ts";
import { activeEvents } from "../plugins/eve/view.ts";

const root = new URL("..", import.meta.url).pathname.replace(/\/$/, "");
const token = (name: string) => `fixture_supervisor_${name}_012345678901234567890123456789`;
const isRoot = new TextDecoder().decode(
  new Deno.Command("/usr/bin/id", { args: ["-u"], stdout: "piped" }).outputSync().stdout,
).trim() === "0";
const identities = [{ uid: 63121, gid: 63121 }, { uid: 63122, gid: 63122 }];
async function command(program: string, args: string[], cwd: string) {
  const result = await new Deno.Command(program, { args, cwd, stdout: "piped", stderr: "piped" })
    .output();
  if (!result.success) throw new Error(new TextDecoder().decode(result.stderr));
  return new TextDecoder().decode(result.stdout).trim();
}
async function copy(directory: string, destination: string) {
  await Deno.mkdir(destination, { recursive: true });
  for await (const entry of Deno.readDir(directory)) {
    if (entry.isDirectory) await copy(`${directory}/${entry.name}`, `${destination}/${entry.name}`);
    else if (entry.isFile) {
      await Deno.copyFile(`${directory}/${entry.name}`, `${destination}/${entry.name}`);
    } else throw new Error("test source must not contain links");
  }
}
async function fixture(accept: boolean, replace = false) {
  const occupied = await command("/bin/ps", ["-eo", "uid="], root);
  assert.ok(
    !occupied.split("\n").some((line) =>
      identities.some((identity) => identity.uid === Number(line.trim()))
    ),
    "集成测试的专属 UID 已被占用，不能回收未知进程",
  );
  const directory = await Deno.makeTempDir({
    dir: "/var/lib",
    prefix: "eve-supervisor-integration-",
  });
  await Deno.chmod(directory, 0o755);
  const deno = `${directory}/deno`;
  await Deno.link(Deno.execPath(), deno);
  for (const name of ["private", "instances"]) {
    await Deno.mkdir(`${directory}/${name}`, { mode: 0o755 });
    await Deno.chmod(`${directory}/${name}`, 0o755);
  }
  const repository = `${directory}/repository`;
  await Deno.mkdir(repository);
  for (
    const name of [
      "runtime",
      "supervisor",
      "kernel",
      "sdk",
      "contracts",
      "composition",
      "plugins",
      "services",
      "guardian",
      "prompts",
    ]
  ) {
    await copy(`${root}/${name}`, `${repository}/${name}`);
  }
  await Deno.copyFile(`${root}/deno.json`, `${repository}/deno.json`);
  await command("git", ["init", "-q"], repository);
  await command("git", ["add", "."], repository);
  await command("git", [
    "-c",
    "user.name=Eve Integration Fixture",
    "-c",
    "user.email=fixture@localhost",
    "commit",
    "-qm",
    "old fixed source",
  ], repository);
  const oldCommit = await command("git", ["rev-parse", "HEAD"], repository);
  await Deno.writeTextFile(`${repository}/VERSION`, "candidate fixture version\n");
  await command("git", ["add", "VERSION"], repository);
  await command("git", [
    "-c",
    "user.name=Eve Integration Fixture",
    "-c",
    "user.email=fixture@localhost",
    "commit",
    "-qm",
    "candidate fixed source",
  ], repository);
  const candidateCommit = await command("git", ["rev-parse", "HEAD"], repository);
  const artifacts = await Promise.all(
    [oldCommit, candidateCommit].map((commit, index) =>
      pinSource({
        repository,
        commit,
        directory: `${directory}/artifact-${index}`,
        deno: Deno.execPath(),
        validate: false,
      })
    ),
  );
  await writeDurableJson(`${directory}/settings.json`, {
    dailyTokenBudget: 200_000_000,
    paused: false,
    timeZone: "Asia/Shanghai",
  });
  for (const name of ["guardian-admin", "guardian-exit", "runtime", "runtime-admin", "phone"]) {
    await Deno.writeTextFile(`${directory}/${name}.key`, token(name), { mode: 0o600 });
  }
  let authority = new OwnershipAuthority(`${directory}/guardian`, 60_000);
  await authority.initialize();
  let guardianRpc = startOwnershipRpc(authority, token("guardian-admin"), token("guardian-exit"));
  const guardianConnectionFile = `${directory}/guardian.json`;
  await writeDurableJson(guardianConnectionFile, { url: guardianRpc.url });
  await Deno.chmod(guardianConnectionFile, 0o644);
  const guardianCall = async <T>(method: string, args: unknown = {}): Promise<T> => {
    const connection = JSON.parse(await Deno.readTextFile(guardianConnectionFile));
    const rpc = new OwnershipRpcClient(connection.url, token("guardian-admin"));
    try {
      return await rpc.call<T>(method, args);
    } finally {
      rpc.close();
    }
  };
  const guardian: GuardianControl = {
    status: () => guardianCall("status"),
    register: (args) => guardianCall("register", args),
    grant: (args) => guardianCall("grant", args),
    pause: (args) => guardianCall("pause", args),
  };
  let observations = 0, onlineCalls = 0, phoneSeq = 0;
  const modelRequests: ModelRequest[] = [];
  const engine = new RuntimeEngine({
    directory: `${directory}/runtime`,
    workspace: `${directory}/workspace`,
    settingsFile: `${directory}/settings.json`,
    maxOutputTokens: 100,
    model: {
      complete(request) {
        modelRequests.push(request);
        const observer = request.tools.some((tool) => tool.name === "confirm_upgrade");
        let response: ModelResponse;
        if (observer) {
          observations++;
          phoneSeq++;
          response = observations === 1 ? { content: "我要再看一轮新版的运行。", toolCalls: [] } : {
            content: accept ? "我确认新版正常。" : "我拒绝这次升级。",
            toolCalls: [{
              id: "upstream-repeat",
              name: "confirm_upgrade",
              args: { accept, reason: accept ? "我已观察两轮，机械状态正常" : "我希望保留旧版" },
            }],
          };
        } else {
          onlineCalls++;
          response = onlineCalls === 1
            ? {
              content: "先留一份升级可继承的脚本原回执。",
              toolCalls: [{
                id: "script-initial",
                name: "execute_script",
                args: {
                  mode: "sync",
                  timeoutMs: 5000,
                  code:
                    'const note = await tools.notes.create({title:"升级保留便签",content:"original",group:"integration",sources:[{kind:"experienced",at:"2026-11-12T00:00:00Z",description:"本轮本地集成"}]}); return {handover:"original",noteId:note.id};',
                },
              }],
            }
            : { content: "这是在线实例本轮的真实模型协议回执，现在先休息。", toolCalls: [] };
        }
        return Promise.resolve({
          ...response,
          usage: {
            status: "reported" as const,
            inputTokens: 100,
            cachedInputTokens: 80,
            outputTokens: 10,
          },
        });
      },
      close() {},
    },
  });
  await engine.initialize();
  const fence = new OwnershipFence(
    `${directory}/fence`,
    "runtime",
    new FileOwnershipExit(guardianConnectionFile, `${directory}/guardian-exit.key`),
  );
  await fence.initialize();
  const runtime = await startRuntimeRpc(engine, token("runtime"), fence);
  const admin = startRuntimeAdmin(engine, token("runtime-admin"), () => runtime.quiesce());
  await writeDurableJson(`${directory}/runtime.json`, { url: runtime.url });
  await Deno.chmod(`${directory}/runtime.json`, 0o644);
  await writeDurableJson(`${directory}/runtime-admin.json`, { url: admin.url });
  const backend = new RuntimeAdminClient({
    connectionFile: `${directory}/runtime-admin.json`,
    keyFile: `${directory}/runtime-admin.key`,
  });
  const phone = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const body = await request.json();
    assert.equal(request.headers.get("authorization"), `Bearer ${token("phone")}`);
    if (body.method === "health") return Response.json({ ok: true, value: { available: true } });
    try {
      await authority.actorStatus(body.ownership.proof);
      return Response.json({
        ok: true,
        value: {
          items: phoneSeq
            ? [{ kind: "message", seq: phoneSeq, preview: "升级观察期间的真实测试通知" }]
            : [],
          throughSeq: phoneSeq,
          hasMore: false,
        },
      });
    } catch {
      return Response.json({ ok: false, error: { category: "unavailable" } });
    }
  });
  const instances: InstanceSpec[] = [];
  for (const [index, id] of ["old", "candidate"].entries()) {
    const identity = identities[replace ? 0 : index];
    const privateDirectory = `${directory}/private/${id}`;
    const instanceDirectory = `${directory}/instances/${replace ? "old" : id}`;
    await Deno.mkdir(privateDirectory, { mode: 0o750 });
    await Deno.chmod(privateDirectory, 0o750);
    await Deno.chown(privateDirectory, 0, identity.gid);
    if (!replace || index === 0) {
      await Deno.mkdir(instanceDirectory, { mode: 0o700 });
      await Deno.chown(instanceDirectory, identity.uid, identity.gid);
      for (const name of ["host", "eve", "scripts", "mind", "kernel", "script-cache"]) {
        const path = `${instanceDirectory}/${name}`;
        await Deno.mkdir(path, { mode: 0o700 });
        await Deno.chown(path, identity.uid, identity.gid);
      }
    }
    await Deno.writeTextFile(`${privateDirectory}/runtime.key`, token("runtime"));
    await Deno.writeTextFile(`${privateDirectory}/phone.key`, token("phone"));
    await Deno.writeTextFile(`${privateDirectory}/control.key`, token(`${id}-control`));
    await Deno.writeTextFile(`${privateDirectory}/actor.key`, token(`${id}-actor`));
    await Deno.writeTextFile(
      `${privateDirectory}/prompt.md`,
      `你是固定源码 ${id} 的 Eve；这是不调用付费模型的真实进程集成测试。`,
    );
    const spec: InstanceSpec = {
      id,
      identity,
      source: artifacts[index],
      configFile: `${privateDirectory}/config.json`,
      connectionFile: `${instanceDirectory}/host/connection-${id}.json`,
      keyFile: `${privateDirectory}/control.key`,
      credentialFile: `${privateDirectory}/actor.key`,
      proofFile: `${privateDirectory}/proof.json`,
    };
    await writeDurableJson(spec.configFile, {
      instance: {
        id,
        sourceCommit: spec.source.commit,
        directory: `${instanceDirectory}/host`,
        keyFile: spec.keyFile,
        connectionFile: spec.connectionFile,
        guardianConnectionFile,
        credentialFile: spec.credentialFile,
        leaseMs: 600,
      },
      runtime: {
        connectionFile: `${directory}/runtime.json`,
        keyFile: `${privateDirectory}/runtime.key`,
        proofFile: spec.proofFile,
      },
      phone: {
        adapterUrl: `http://127.0.0.1:${phone.addr.port}/rpc`,
        adapterKeyFile: `${privateDirectory}/phone.key`,
        ownershipProofFile: spec.proofFile,
      },
      eve: {
        directory: `${instanceDirectory}/eve`,
        promptFile: `${privateDirectory}/prompt.md`,
        sourceCommit: spec.source.commit,
        remoteRecords: true,
        modelTimeoutMs: 5000,
        maxOutputTokens: 100,
        pollMs: 50,
        defaultSleepMs: 3_600_000,
        dailyTokenBudget: 200_000_000,
      },
      scripts: {
        directory: `${instanceDirectory}/scripts`,
        cacheDirectory: `${instanceDirectory}/script-cache`,
        denoBinary: deno,
        defaultTimeoutMs: 5000,
      },
      kernelDataDirectory: `${instanceDirectory}/kernel`,
      mindDirectory: `${instanceDirectory}/mind`,
    });
    for (
      const path of [
        spec.configFile,
        spec.keyFile,
        spec.credentialFile,
        `${privateDirectory}/prompt.md`,
        `${privateDirectory}/runtime.key`,
        `${privateDirectory}/phone.key`,
      ]
    ) {
      await Deno.chmod(path, 0o440);
      await Deno.chown(path, 0, identity.gid);
    }
    instances.push(spec);
  }
  // 监督源码本身也须位于低 UID 可遍历的固定可信产物目录，供交接 helper 执行。
  const { LocalInstanceDriver: FixtureDriver }: typeof import("../supervisor/driver.ts") =
    await import(
      `${artifacts[0].directory}/supervisor/driver.ts`
    );
  const driver = new FixtureDriver(deno, `${directory}/processes`, identities);
  const runtimeClient = async (instance: InstanceSpec) => {
    const config = JSON.parse(await Deno.readTextFile(instance.configFile));
    return new RuntimeClient(config.runtime);
  };
  let supervisor: Supervisor;
  const createSupervisor = async () => {
    const controller = new Supervisor({
      directory: `${directory}/supervisor`,
      instances: [...instances],
      driver,
      guardian,
      confirmationMs: 60_000,
      backend: {
        quiesce: () => backend.quiesce(),
        watermark: () => backend.watermark(),
        rollback: (args) => backend.rollback(args),
        async complete(instance, request, actionId) {
          const client = await runtimeClient(instance);
          try {
            return await client.call(
              "model",
              "complete",
              request as unknown as Json,
              actionId,
            ) as unknown as ModelResponse;
          } finally {
            client.close();
          }
        },
        async saveNote(instance, args, actionId) {
          const client = await runtimeClient(instance);
          try {
            return await client.call("notes", "create", {
              ...args,
              group: "observation",
              sources: [],
            }, actionId);
          } finally {
            client.close();
          }
        },
      },
    });
    await controller.initialize();
    return controller;
  };
  supervisor = await createSupervisor();
  return {
    directory,
    instances,
    driver,
    engine,
    guardian,
    backend,
    modelRequests,
    runtimeClient,
    controller: () => supervisor,
    observations: () => observations,
    onlineCalls: () => onlineCalls,
    async restartSupervisor() {
      await supervisor.close();
      supervisor = await createSupervisor();
    },
    async restartGuardian() {
      await guardianRpc.close();
      await authority.close();
      authority = new OwnershipAuthority(`${directory}/guardian`, 60_000);
      await authority.initialize();
      guardianRpc = startOwnershipRpc(authority, token("guardian-admin"), token("guardian-exit"));
      await writeDurableJson(guardianConnectionFile, { url: guardianRpc.url });
      await Deno.chmod(guardianConnectionFile, 0o644);
    },
    async close() {
      await supervisor.close();
      for (const instance of instances) await driver.terminate(instance);
      await admin.close();
      await runtime.close();
      await phone.shutdown();
      await guardianRpc.close();
      await authority.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}
async function waitFor(check: () => boolean | Promise<boolean>) {
  for (let i = 0; i < 500; i++) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error("真实实例10秒内未达到预期状态");
}

async function verifyIsolatedProcess(
  directory: string,
  instance: InstanceSpec,
  peer: InstanceSpec,
) {
  const process = JSON.parse(
    await Deno.readTextFile(`${directory}/processes/${instance.id}.process.json`),
  );
  assert.equal(
    Number(await command("/bin/ps", ["-o", "uid=", "-p", String(process.pid)], root)),
    instance.identity!.uid,
  );
  for (
    const file of [
      `${directory}/guardian-admin.key`,
      `${directory}/guardian-exit.key`,
      peer.credentialFile,
    ]
  ) {
    const check = await new Deno.Command("/usr/bin/setpriv", {
      args: [
        "--reuid",
        String(instance.identity!.uid),
        "--regid",
        String(instance.identity!.gid),
        "--clear-groups",
        "--no-new-privs",
        "--bounding-set=-all",
        "--inh-caps=-all",
        "--ambient-caps=-all",
        "/usr/bin/test",
        "-r",
        file,
      ],
      stdout: "null",
      stderr: "null",
      clearEnv: true,
    }).output();
    assert.equal(check.code, 1, `隔离实例不能读到管理密钥或另一实例凭据：${file}`);
  }
}

async function verifyInheritedScript(instance: InstanceSpec) {
  const config = JSON.parse(await Deno.readTextFile(instance.configFile));
  const ledger = `${config.scripts.directory}/ledger`;
  const manifest = JSON.parse(await Deno.readTextFile(`${ledger}/index.json`));
  assert.equal(manifest.schema, 2, "真实升级必须交接新的 v2 目录账本");
  assert.ok((await Deno.stat(`${ledger}/commits`)).isDirectory);
  assert.ok((await Deno.stat(`${ledger}/objects`)).isDirectory);
  const verifyOwners = async (path: string) => {
    const stat = await Deno.lstat(path);
    assert.equal(stat.isSymlink, false);
    assert.equal(stat.uid, instance.identity!.uid, `交接文件应由候选身份自然创建：${path}`);
    if (stat.isDirectory) {
      for await (const entry of Deno.readDir(path)) await verifyOwners(`${path}/${entry.name}`);
    }
  };
  await verifyOwners(ledger);
  let unexpectedCalls = 0;
  const scripts = new ScriptRuntime({
    call() {
      unexpectedCalls++;
      throw new Error("已完成旧脚本不能重放工具");
    },
  } as unknown as PluginContext, config.scripts);
  await scripts.initialize();
  try {
    const original = scripts.store.state.jobs[0];
    assert.ok(original, "候选应继承旧版脚本原件");
    assert.equal(original.status, "completed");
    assert.equal(original.actions.length, 1);
    assert.equal(original.actions[0].status, "completed");
    assert.equal((original.result as { handover: string }).handover, "original");
    const receipt = await scripts.start(original.request);
    assert.equal(receipt.jobId, original.jobId);
    assert.equal(receipt.status, "completed");
    assert.equal(unexpectedCalls, 0);
  } finally {
    await scripts.close();
  }
}

Deno.test({
  name: "真实低UID监督贯通：固定Git源码双进程，旧Eve先普通观察再确认，监督/守护重启不复用旧令牌",
  ignore: !isRoot,
  async fn() {
    const f = await fixture(true);
    let detached: Deno.ChildProcess | undefined;
    let detachedExited = false;
    try {
      await f.controller().bootstrap("old");
      await verifyIsolatedProcess(f.directory, f.instances[0], f.instances[1]);
      await waitFor(() => f.engine.records.events.some((event) => event.kind === "rest"));
      const originalProof = JSON.parse(await Deno.readTextFile(f.instances[0].proofFile));
      const oldClient = await f.runtimeClient(f.instances[0]);
      await f.controller().upgrade("candidate");
      await verifyIsolatedProcess(f.directory, f.instances[1], f.instances[0]);
      assert.equal(
        f.controller().state.upgrade?.phase,
        "observing",
        JSON.stringify(f.controller().state),
      );
      assert.equal(f.observations(), 1);
      await assert.rejects(
        () => oldClient.call("records", "page"),
        (error) => error instanceof KernelError && error.kind === "unavailable",
      );
      oldClient.close();
      await waitFor(() =>
        f.engine.records.events.some((event) =>
          event.kind === "model_start" &&
          (event.data as { sourceCommit: string }).sourceCommit === f.instances[1].source.commit
        )
      );
      await f.restartSupervisor();
      await f.controller().tick();
      assert.equal(f.controller().state.lastOutcome?.outcome, "confirmed");
      assert.equal(f.controller().state.current, "candidate");
      assert.equal(f.observations(), 2);
      assert.ok(f.onlineCalls() >= 3);
      await f.driver.call(f.instances[1], "deactivate");
      await verifyInheritedScript(f.instances[1]);
      assert.equal(f.engine.notes.journal.events.length, 1);
      // 新版已推进共享游标、旧版目录尚未推进：复现生产的692/701及33/40错位。
      const latestConfig = JSON.parse(await Deno.readTextFile(f.instances[1].configFile));
      const latestScripts = new ScriptStore(`${latestConfig.scripts.directory}/ledger`);
      let scriptThrough = 0;
      const unknownJobId = await fingerprint(["script-job-v1", "latest-unknown"]);
      const unknownFingerprint = await fingerprint({
        code: "return 1",
        timeoutMs: latestConfig.scripts.defaultTimeoutMs,
      });
      const unknownAt = new Date().toISOString();
      await latestScripts.initialize();
      try {
        await latestScripts.change((state) => {
          state.jobs.push({
            jobId: unknownJobId,
            fingerprint: unknownFingerprint,
            request: { requestId: "latest-unknown", code: "return 1" },
            status: "unknown",
            scriptStarted: true,
            actions: [],
            startedAt: unknownAt,
            finishedAt: unknownAt,
          });
          scriptThrough = state.completions.length + 1;
          state.completions.push({
            seq: scriptThrough,
            job: {
              jobId: unknownJobId,
              status: "unknown",
              scriptStarted: true,
              actions: 0,
              startedAt: unknownAt,
              finishedAt: unknownAt,
            },
          });
        });
      } finally {
        await latestScripts.close();
      }
      const latestMind = JSON.parse(
        await Deno.readTextFile(`${latestConfig.mindDirectory}/index.json`),
      );
      const mindThrough = latestMind.events.length + 1;
      latestMind.events.push({
        seq: mindThrough,
        requestId: "latest-control",
        kind: "rest",
        args: { text: "fixture" },
        at: new Date().toISOString(),
      });
      await writeDurableJson(`${latestConfig.mindDirectory}/index.json`, latestMind);
      // 测试由root追加状态；生产写入者是实例UID，恢复它实际拥有的文件权限。
      const ownState = async (path: string) => {
        await Deno.chown(path, f.instances[1].identity!.uid, f.instances[1].identity!.gid);
        const stat = await Deno.lstat(path);
        if (stat.isDirectory) {
          for await (const entry of Deno.readDir(path)) await ownState(`${path}/${entry.name}`);
        }
      };
      await ownState(latestConfig.mindDirectory);
      await ownState(`${latestConfig.scripts.directory}/ledger`);

      await f.engine.records.append("notice", {
        source: "scripts",
        throughSeq: scriptThrough,
        items: [],
      });
      await f.engine.records.append("mind_applied", {
        throughSeq: mindThrough,
        requestId: "latest-control",
        outcome: { status: "applied" },
      });
      // 模拟实例留下的 setsid 后代；只终止已跟踪的宿主 PID 不足以回收专属 UID。
      const identity = f.instances[1].identity!;
      detached = new Deno.Command("/usr/bin/setsid", {
        args: [
          "/usr/bin/setpriv",
          "--reuid",
          String(identity.uid),
          "--regid",
          String(identity.gid),
          "--clear-groups",
          "--no-new-privs",
          "--bounding-set=-all",
          "--inh-caps=-all",
          "--ambient-caps=-all",
          "/bin/sleep",
          "600",
        ],
        clearEnv: true,
        stdout: "null",
        stderr: "null",
        stdin: "null",
      }).spawn();
      void detached.status.then(() => {
        detachedExited = true;
      });
      await waitFor(async () => {
        const result = await new Deno.Command("/bin/ps", {
          args: ["-o", "uid=", "-p", String(detached!.pid)],
          stdout: "piped",
          stderr: "null",
        }).output();
        return Number(new TextDecoder().decode(result.stdout).trim()) === identity.uid;
      });
      await f.restartGuardian();
      const status = await f.guardian.status();
      assert.ok(status.epoch > originalProof.epoch);
      await f.controller().tick();
      assert.equal(f.controller().state.current, "old");
      assert.equal(
        (await f.driver.call<{ state: string }>(f.instances[0], "status")).state,
        "active",
      );
      assert.ok((await f.guardian.status()).owner!.epoch > status.epoch);
      await waitFor(() => detachedExited);
      await waitFor(async () => {
        const status = await f.driver.call<
          { eve?: { scriptThroughSeq: number; mindThroughSeq: number; lastError: string | null } }
        >(f.instances[0], "status");
        return status.eve?.scriptThroughSeq === scriptThrough &&
          status.eve?.mindThroughSeq === mindThrough && status.eve?.lastError === null;
      });
      await f.driver.call(f.instances[0], "deactivate");
      await verifyInheritedScript(f.instances[0]);
    } finally {
      await f.close();
      if (detached) {
        try {
          detached.kill("SIGKILL");
        } catch { /* Already reaped by the exclusive UID cleanup. */ }
        await detached.status;
      }
    }
  },
});

Deno.test({
  name: "真实低UID监督贯通：旧Eve拒绝则回切，候选原件隔离且旧历史保留",
  ignore: !isRoot,
  async fn() {
    const f = await fixture(false);
    try {
      await f.controller().bootstrap("old");
      await verifyIsolatedProcess(f.directory, f.instances[0], f.instances[1]);
      await waitFor(() => f.engine.records.events.some((event) => event.kind === "rest"));
      const oldThrough = f.engine.records.events.length;
      await f.controller().upgrade("candidate");
      await verifyIsolatedProcess(f.directory, f.instances[1], f.instances[0]);
      await waitFor(() =>
        f.engine.records.events.some((event) =>
          event.kind === "model_start" &&
          (event.data as { sourceCommit: string }).sourceCommit === f.instances[1].source.commit
        )
      );
      await f.controller().tick();
      assert.equal(f.controller().state.lastOutcome?.outcome, "rolled_back");
      assert.equal(f.controller().state.current, "old");
      const marker = f.engine.records.events.find((event) => event.kind === "upgrade_rollback")!;
      assert.ok(marker);
      const range = marker.data as { afterSeq: number; throughSeq: number };
      assert.ok(range.afterSeq >= oldThrough);
      assert.ok(range.throughSeq > range.afterSeq);
      const visible = activeEvents(f.engine.records.events);
      assert.ok(visible.some((event) => event.seq === 1));
      assert.ok(
        !visible.some((event) => event.seq > range.afterSeq && event.seq <= range.throughSeq),
      );
      assert.ok(f.engine.records.events.length > visible.length);
      assert.equal((await f.guardian.status()).owner?.instanceId, "old");
      await verifyInheritedScript(f.instances[1]);
      assert.equal(f.engine.notes.journal.events.length, 1);
    } finally {
      await f.close();
    }
  },
});

for (const fail of [false, true]) {
  Deno.test({
    name: `真实低UID直接替换：共用原业务目录、脚本不重放、${
      fail ? "候选校验失败恢复旧程序" : "跳过交接与观察并核实实际新版"
    }`,
    ignore: !isRoot,
    async fn() {
      const f = await fixture(true, true);
      try {
        await f.controller().bootstrap("old");
        await waitFor(() => f.engine.records.events.some((e) => e.kind === "rest"));
        const oldConfig = JSON.parse(await Deno.readTextFile(f.instances[0].configFile));
        const newConfig = JSON.parse(await Deno.readTextFile(f.instances[1].configFile));
        for (const field of ["kernelDataDirectory", "mindDirectory"]) {
          assert.equal(oldConfig[field], newConfig[field]);
        }
        assert.equal(oldConfig.scripts.directory, newConfig.scripts.directory);
        if (fail) {
          await Deno.writeTextFile(
            `${f.instances[1].source.directory}/runtime/instance.ts`,
            "tampered",
          );
        }
        await f.controller().stageReplacement(
          () => Promise.resolve(f.instances[1]),
          "upgrade_real_direct",
        );
        const current = f.instances[fail ? 0 : 1];
        assert.equal(f.controller().state.current, current.id);
        assert.equal(f.controller().state.lastOutcome?.outcome, fail ? "rolled_back" : "confirmed");
        assert.equal(f.controller().state.standby, null);
        assert.equal(f.observations(), 0);
        const status = await f.driver.call<{ sourceCommit: string; state: string }>(
          current,
          "status",
        );
        assert.equal(status.sourceCommit, current.source.commit);
        assert.equal(status.state, "active");
        await f.driver.call(current, "deactivate");
        await verifyInheritedScript(current);
        assert.equal(f.engine.notes.journal.events.length, 1);
        await f.restartSupervisor();
        await f.controller().tick();
        assert.equal(f.controller().state.current, current.id);
      } finally {
        await f.close();
      }
    },
  });
}
