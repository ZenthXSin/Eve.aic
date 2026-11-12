import type { ModelResponse } from "../contracts/model.ts";
import { ScriptObserver } from "../sdk/script-observer.ts";
import { QqAdminClient } from "../adapters/qq/admin.ts";
import { OwnershipRpcClient } from "../guardian/rpc.ts";
import { type Json, KernelError, toWireError } from "../kernel/protocol.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { RuntimeClient } from "../services/runtime/client.ts";
import { RuntimeAdminClient } from "../services/runtime/admin.ts";
import { readAdminBody, startAdminControlServer } from "../services/admin/server.ts";
import { LocalInstanceDriver, readTrustedInstanceConfig } from "./driver.ts";
import { type GuardianControl, type InstanceSpec, Supervisor } from "./controller.ts";
import { UpgradeRequests } from "./upgrades.ts";

export interface SupervisorConfig {
  supervisor: {
    directory: string;
    connectionFile: string;
    keyFile: string;
    guardianConnectionFile: string;
    guardianAdminKeyFile: string;
    adminConnectionFile: string;
    adminKeyFile: string;
    deno: string;
    initialInstance: string;
    tickMs?: number;
    confirmationMs?: number;
    repository?: string;
    upgradeConnectionFile?: string;
    artifactDirectory?: string;
    instancePrivateDirectory?: string;
    instanceRuntimeDirectory?: string;
    commandIdentity?: { uid: number; gid: number };
    instanceIdentities?: { uid: number; gid: number }[];
    qqAdmin?: { connectionFile: string; keyFile: string };
  };
  instances: InstanceSpec[];
  runtimeAdmin: { connectionFile: string; keyFile: string };
}

export async function startSupervisor(configPath: string) {
  const config: SupervisorConfig = JSON.parse(await Deno.readTextFile(configPath));
  const c = config.supervisor;
  if (
    !c || !config.runtimeAdmin || !Array.isArray(config.instances) ||
    [
      c.directory,
      c.connectionFile,
      c.keyFile,
      c.guardianConnectionFile,
      c.guardianAdminKeyFile,
      c.adminConnectionFile,
      c.adminKeyFile,
      c.deno,
      config.runtimeAdmin.connectionFile,
      config.runtimeAdmin.keyFile,
    ].some((p) => typeof p !== "string" || !p.startsWith("/")) ||
    !Number.isSafeInteger(c.tickMs ?? 1000) || (c.tickMs ?? 1000) < 50
  ) throw new KernelError("rejected", "独立监督私有配置不完整");
  if (
    c.qqAdmin &&
    ![c.qqAdmin.connectionFile, c.qqAdmin.keyFile].every((path) =>
      typeof path === "string" && path.startsWith("/")
    )
  ) {
    throw new KernelError("rejected", "QQ 管理连接与密钥必须为绝对路径");
  }
  if (
    !config.instances.every((i) =>
      i.identity && [i.identity.uid, i.identity.gid].every((n) => Number.isSafeInteger(n) && n > 0)
    )
  ) throw new KernelError("rejected", "生产监督必须为每个实例配置独立非root身份");
  if (
    c.instanceIdentities &&
    (new Set(c.instanceIdentities.map((i) => i.uid)).size !== c.instanceIdentities.length ||
      new Set(c.instanceIdentities.map((i) => i.gid)).size !== c.instanceIdentities.length ||
      c.instanceIdentities.some((i) =>
        i.uid === c.commandIdentity?.uid || i.gid === c.commandIdentity?.gid
      ))
  ) throw new KernelError("rejected", "命令验证与两代实例必须使用互异UID和GID");
  if (c.repository) {
    if (
      ![
        c.artifactDirectory,
        c.instancePrivateDirectory,
        c.instanceRuntimeDirectory,
        c.upgradeConnectionFile,
      ].every((p) => typeof p === "string" && p.startsWith("/")) || !c.commandIdentity ||
      !c.instanceIdentities || c.instanceIdentities.length < 2 ||
      ![c.commandIdentity, ...c.instanceIdentities].every((i) =>
        [i.uid, i.gid].every((n) => Number.isSafeInteger(n) && n > 0)
      )
    ) throw new KernelError("rejected", "自升级需要独立验证身份与两代实例身份/目录");
  }
  const [key, adminKey, guardianKey, runtimeAdminKey] = await Promise.all(
    [c.keyFile, c.adminKeyFile, c.guardianAdminKeyFile, config.runtimeAdmin.keyFile].map(
      async (path) => (await Deno.readTextFile(path)).trim(),
    ),
  );
  const qqAdminKey = c.qqAdmin ? (await Deno.readTextFile(c.qqAdmin.keyFile)).trim() : undefined;
  const controlKeys = [
    key,
    adminKey,
    guardianKey,
    runtimeAdminKey,
    ...(qqAdminKey ? [qqAdminKey] : []),
  ];
  if (
    new Set(controlKeys).size !== controlKeys.length ||
    !controlKeys.every((k) => /^[a-zA-Z0-9_-]{32,256}$/.test(k)) || (c.qqAdmin && !qqAdminKey)
  ) throw new KernelError("rejected", "监督与各管理接口凭据必须彼此独立");
  const guardianCall = async <T>(method: string, args: unknown = {}) => {
    const connection = JSON.parse(await Deno.readTextFile(c.guardianConnectionFile));
    const rpc = new OwnershipRpcClient(connection.url, guardianKey);
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
  const runtime = new RuntimeAdminClient(config.runtimeAdmin);
  const qqAdmin = c.qqAdmin ? new QqAdminClient(c.qqAdmin) : undefined;
  const driver = new LocalInstanceDriver(
    c.deno,
    `${c.directory}/processes`,
    c.instanceIdentities ?? config.instances.map((i) => i.identity!),
  );
  const runtimeClient = async (instance: InstanceSpec) => {
    const privateConfig = await readTrustedInstanceConfig(instance);
    return new RuntimeClient(privateConfig.runtime);
  };
  const controller = new Supervisor({
    directory: `${c.directory}/state`,
    instances: config.instances,
    driver,
    guardian,
    confirmationMs: c.confirmationMs,
    backend: {
      quiesce: () => runtime.quiesce(),
      watermark: () => runtime.watermark(),
      rollback: (args) => runtime.rollback(args),
      async complete(instance, request, actionId) {
        const client = await runtimeClient(instance);
        try {
          return await client.call(
            "model",
            "complete",
            { ...request, _purpose: "isolated" } as unknown as Json,
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
            group: "upgrade-observation",
            sources: [{
              kind: "experienced",
              at: new Date().toISOString(),
              description: "旧 Eve 升级观察，隔离于在线意识流",
            }],
          }, actionId);
        } finally {
          client.close();
        }
      },
    },
  });
  await controller.initialize();
  // 持久睡眠设置优先于初次启动；不同进程重启后仍先收敛暂停。
  const initialRuntime = await runtime.status() as { budget?: { paused?: boolean } };
  if (controller.state.paused || initialRuntime.budget?.paused) {
    await controller.pause();
    await runtime.settings({ paused: true });
  }
  // 守护先暂停时 bootstrap 只保留睡眠，永不自动越过主人开关。
  await controller.bootstrap(c.initialInstance);
  const upgrades = c.repository
    ? new UpgradeRequests(
      {
        directory: c.directory,
        repository: c.repository,
        artifactDirectory: c.artifactDirectory!,
        instancePrivateDirectory: c.instancePrivateDirectory!,
        instanceRuntimeDirectory: c.instanceRuntimeDirectory!,
        guardianConnectionFile: c.guardianConnectionFile,
        deno: c.deno,
        commandIdentity: c.commandIdentity!,
        instanceIdentities: c.instanceIdentities!,
      },
      controller,
      key,
    )
    : undefined;
  if (upgrades) await upgrades.initialize();
  let controlTail: Promise<unknown> = Promise.resolve();
  const serialControl = <T>(run: () => Promise<T>) => {
    const work = controlTail.then(run);
    controlTail = work.catch(() => {});
    return work;
  };
  const changePause = async (paused: boolean) => {
    if (paused) {
      await controller.pause();
      await runtime.settings({ paused: true });
    } else {
      await runtime.settings({ paused: false });
      await controller.resume();
    }
  };
  let workspaceObserver: { instanceId: string; observer: ScriptObserver } | undefined;
  const admin = await startAdminControlServer({
    async status() {
      return {
        supervisor: controller.state as unknown as Json,
        runtime: await runtime.status(),
        ownership: await guardian.status() as unknown as Json,
      };
    },
    query: async (request) => {
      if (request.resource === "workspace") {
        if (Object.keys(request.args).length) {
          throw new KernelError("rejected", "工作台只读查询不接受路径");
        }
        const current = controller.state.current;
        if (!current) throw new KernelError("unavailable", "当前实例尚未确定");
        if (workspaceObserver?.instanceId !== current) {
          const spec = config.instances.find((item) => item.id === current) ??
            controller.instance(current);
          const trusted = await readTrustedInstanceConfig(spec);
          const directory = (trusted as unknown as { scripts?: { directory?: string } }).scripts
            ?.directory;
          if (typeof directory !== "string" || !directory.startsWith("/")) {
            throw new KernelError("unavailable", "当前实例未配置脚本账本");
          }
          workspaceObserver = {
            instanceId: current,
            observer: new ScriptObserver(`${directory}/ledger`),
          };
        }
        return await workspaceObserver.observer.read();
      }
      if (request.resource === "qqConversations" || request.resource === "qqHistory") {
        if (!qqAdmin) throw new KernelError("unavailable", "QQ 只读视图尚未配置");
        return request.resource === "qqConversations"
          ? qqAdmin.conversations(request.args as never)
          : qqAdmin.history(request.args as never);
      }
      if (request.resource === "qqFailures") {
        if (!qqAdmin) throw new KernelError("unavailable", "QQ 失败队列管理尚未配置");
        return qqAdmin.failedHeads(request.args);
      }
      return runtime.query(request);
    },
    control(request) {
      return serialControl(async () => {
        if (request.action === "resolveQqFailure") {
          if (!qqAdmin) throw new KernelError("unavailable", "QQ 失败队列管理尚未配置");
          return await qqAdmin.resolveFailed(request.args);
        }
        if (request.action === "pause") {
          await changePause(true);
        } else if (request.action === "resume") {
          await changePause(false);
        } else if (request.action === "resumeModel") {
          if (!controller.state.paused) {
            throw new KernelError("rejected", "请先暂停运行，再明确恢复被拒绝的模型请求");
          }
          await runtime.resumeModel(request.args.expectedRequestId);
        } else await runtime.settings({ dailyTokenBudget: request.args.dailyTokenBudget });
        return { supervisor: controller.state as unknown as Json, runtime: await runtime.status() };
      });
    },
  }, adminKey);
  await writeDurableJson(c.adminConnectionFile, { url: admin.url });
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    if (
      request.method !== "POST" ||
      !["/supervisor", "/upgrade"].includes(new URL(request.url).pathname) ||
      request.headers.has("origin")
    ) return new Response(null, { status: 403 });
    try {
      const data = await readAdminBody(request);
      if (new URL(request.url).pathname === "/upgrade") {
        if (!upgrades) throw new KernelError("unavailable", "自升级尚未配置固定源码来源");
        return Response.json({
          ok: true,
          value: await upgrades.invoke(data, request.headers.get("authorization")),
        });
      }
      if (request.headers.get("authorization") !== `Bearer ${key}`) {
        return new Response(null, { status: 403 });
      }
      if (data.method === "status") return Response.json({ ok: true, value: controller.state });
      if (data.method === "requestUpgrade") {
        if (!upgrades) throw new KernelError("unavailable", "自升级尚未配置固定源码来源");
        const value = await serialControl(() =>
          upgrades.requestAdministrator(
            data.args,
            request.headers.get("authorization"),
          )
        );
        return Response.json({ ok: true, value });
      }
      if (data.method === "pause") await serialControl(() => changePause(true));
      else if (data.method === "resume") await serialControl(() => changePause(false));
      else if (
        data.method === "upgrade" &&
        typeof (data.args as Record<string, Json>)?.instanceId === "string"
      ) await controller.upgrade(String((data.args as Record<string, Json>).instanceId));
      else throw new KernelError("rejected", "未知监督操作");
      return Response.json({ ok: true, value: controller.state });
    } catch (error) {
      return Response.json({ ok: false, error: toWireError(error) });
    }
  });
  await writeDurableJson(c.connectionFile, {
    url: `http://127.0.0.1:${server.addr.port}/supervisor`,
  });
  if (upgrades) {
    await writeDurableJson(c.upgradeConnectionFile!, {
      url: `http://127.0.0.1:${server.addr.port}/upgrade`,
    });
    await Deno.chmod(c.upgradeConnectionFile!, 0o644);
  }
  let closed = false, timer: ReturnType<typeof setTimeout> | undefined;
  const tick = async () => {
    try {
      await controller.tick();
    } catch (error) {
      // 错误不含私密正文/凭据；原 action/permit 事实留在独立出口，下一轮核对。
      console.error(`supervisor tick: ${error instanceof KernelError ? error.kind : "unknown"}`);
    } finally {
      if (!closed) timer = setTimeout(() => void tick(), c.tickMs ?? 1000);
    }
  };
  timer = setTimeout(() => void tick(), c.tickMs ?? 1000);
  return {
    controller,
    url: `http://127.0.0.1:${server.addr.port}/supervisor`,
    async close() {
      closed = true;
      clearTimeout(timer);
      await server.shutdown();
      await admin.close();
      await controlTail;
      await upgrades?.close();
      await controller.close();
    },
  };
}
export async function runSupervisor(configPath: string) {
  const host = await startSupervisor(configPath);
  let resolve!: () => void;
  const done = new Promise<void>((r) => resolve = r), stop = () => resolve();
  Deno.addSignalListener("SIGTERM", stop);
  Deno.addSignalListener("SIGINT", stop);
  try {
    await done;
  } finally {
    Deno.removeSignalListener("SIGTERM", stop);
    Deno.removeSignalListener("SIGINT", stop);
    await host.close();
  }
}
if (import.meta.main) {
  if (Deno.args.length !== 1) throw new Error("参数：独立监督私有配置路径");
  await runSupervisor(Deno.args[0]);
}
