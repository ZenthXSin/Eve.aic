import { KernelError } from "../kernel/protocol.ts";
import type { OwnershipProof } from "../contracts/ownership.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import type { InstanceDriver, InstanceSpec, InstanceStatus } from "./controller.ts";
import { InstanceClient } from "./client.ts";
import { verifySource } from "./source.ts";
import { instanceStopControlDeadlineMs } from "../sdk/lifecycle-timeouts.ts";

export async function readTrustedInstanceConfig(instance: InstanceSpec) {
  if (instance.identity) {
    let path = instance.configFile;
    while (path && path !== "/") {
      const stat = await Deno.lstat(path);
      if (stat.isSymlink || stat.uid !== 0 || ((stat.mode ?? 0) & 0o022) !== 0) {
        throw new KernelError("rejected", "实例私有配置必须由root只读持有，候选不可改写其路径");
      }
      path = path.slice(0, path.lastIndexOf("/")) || "/";
    }
  }
  return JSON.parse(await Deno.readTextFile(instance.configFile));
}

async function ticks(pid: number): Promise<string | null> {
  if (!Number.isSafeInteger(pid) || pid < 2) return null;
  // /proc text may report zero size, read through cat rather than stat-sized Deno read.
  const result = await new Deno.Command("/bin/cat", {
    args: [`/proc/${pid}/stat`],
    stdout: "piped",
    stderr: "null",
  }).output();
  if (!result.success) return null;
  const fields = new TextDecoder().decode(result.stdout).split(") ").at(-1)!.split(" ");
  return fields[19] ?? null;
}
function killHelper(process: Deno.ChildProcess) {
  try {
    process.kill("SIGKILL");
  } catch (error) {
    if (
      !(error instanceof Deno.errors.NotFound) &&
      !(error instanceof TypeError && error.message === "Child process has already terminated")
    ) throw error;
  }
}
/** 监督拥有进程控制与独立实例私有目录，候选从来不收到管理/出口密钥。 */
export class LocalInstanceDriver implements InstanceDriver {
  #children = new Map<string, Deno.ChildProcess>();
  constructor(
    readonly deno: string,
    readonly directory: string,
    readonly managedIdentities: { uid: number; gid: number }[] = [],
  ) {}
  #isolation(identity?: { uid: number; gid: number }): string[] {
    if (!identity) return [];
    if (![identity.uid, identity.gid].every((n) => Number.isSafeInteger(n) && n > 0)) {
      throw new KernelError("rejected", "实例不得使用root身份");
    }
    return [
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
    ];
  }
  async #uidProcesses(uid: number) {
    const result = await new Deno.Command("/bin/ps", {
      args: ["-eo", "uid=,pid=,stat="],
      stdout: "piped",
      stderr: "null",
      clearEnv: true,
    }).output();
    if (!result.success) throw new KernelError("unavailable", "无法核对专属实例UID进程");
    const processes: number[] = [];
    for (const line of new TextDecoder().decode(result.stdout).trim().split("\n")) {
      const fields = line.trim().split(/\s+/);
      if (fields.length !== 3 || !fields.slice(0, 2).every((n) => /^\d+$/.test(n))) {
        throw new KernelError("unavailable", "专属实例进程清单格式异常");
      }
      if (Number(fields[0]) === uid && !fields[2].startsWith("Z")) {
        processes.push(Number(fields[1]));
      }
    }
    return processes;
  }
  async prepareIdentity(identity: { uid: number; gid: number }) {
    this.#isolation(identity);
    if (!this.managedIdentities.some((i) => i.uid === identity.uid && i.gid === identity.gid)) {
      throw new KernelError("rejected", "仅可回收监督显式配置的专属实例UID池");
    }
    const self = await new Deno.Command("/usr/bin/id", {
      args: ["-u"],
      stdout: "piped",
      stderr: "null",
      clearEnv: true,
    }).output();
    if (!self.success || Number(new TextDecoder().decode(self.stdout).trim()) === identity.uid) {
      throw new KernelError("rejected", "不能回收监督自身运行身份");
    }
    for (const signal of ["TERM", "KILL"]) {
      const result = await new Deno.Command("/usr/bin/pkill", {
        args: ["--signal", signal, "--uid", String(identity.uid)],
        stdout: "null",
        stderr: "null",
        clearEnv: true,
      }).output();
      if (result.code !== 0 && result.code !== 1) {
        throw new KernelError("unavailable", "专属实例UID清理失败");
      }
      const deadline = Date.now() + (signal === "TERM" ? 2000 : 5000);
      while (Date.now() < deadline) {
        if (!(await this.#uidProcesses(identity.uid)).length) return;
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
    }
    throw new KernelError("unavailable", "专属实例UID仍有存活后代，拒绝复用凭据");
  }
  async #ownedCommand(instance: InstanceSpec, command: string, args: string[]) {
    const isolation = this.#isolation(instance.identity);
    const result = await new Deno.Command(isolation[0] ?? command, {
      args: isolation.length ? [...isolation.slice(1), command, ...args] : args,
      clearEnv: true,
      stdout: "null",
      stderr: "null",
      stdin: "null",
    }).output();
    if (!result.success) throw new KernelError("unavailable", "实例身份目录操作失败");
  }
  async call<T>(
    instance: InstanceSpec,
    method: string,
    args: import("../kernel/protocol.ts").Json = {},
  ) {
    const timeoutMs = method === "deactivate"
      ? instanceStopControlDeadlineMs(
        (await readTrustedInstanceConfig(instance)).eve.modelTimeoutMs,
      )
      : 180_000;
    const client = new InstanceClient(instance.connectionFile, instance.keyFile, timeoutMs);
    try {
      return await client.call<T>(method, args);
    } finally {
      client.close();
    }
  }
  async proof(instance: InstanceSpec, proof: OwnershipProof) {
    await writeDurableJson(instance.proofFile, proof);
    if (instance.identity) {
      await Deno.chmod(instance.proofFile, 0o440);
      await Deno.chown(instance.proofFile, 0, instance.identity.gid);
    }
  }
  async handover(
    old: InstanceSpec,
    candidate: InstanceSpec,
    actionId: string,
    mode: "forward" | "rollback" = "forward",
  ) {
    const configs = await Promise.all([old, candidate].map(readTrustedInstanceConfig));
    const helper = new URL("./transfer.ts", import.meta.url).pathname;
    const restoration = mode === "rollback";
    const spawn = (instance: InstanceSpec, index: number, mode: "export" | "import") => {
      const isolation = this.#isolation(instance.identity);
      const args = [
        this.deno,
        "run",
        "--no-config",
        "--no-check",
        "--allow-read",
        "--allow-write",
        helper,
        instance.configFile,
        mode === "import" && restoration ? "restore" : mode,
        actionId,
      ];
      return new Deno.Command(isolation[0] ?? args[0], {
        args: isolation.length ? [...isolation.slice(1), ...args] : args.slice(1),
        clearEnv: true,
        env: {
          DENO_DIR: `${configs[index].instance.directory}/cache`,
          HOME: configs[index].instance.directory,
        },
        stdin: mode === "import" ? "piped" : "null",
        stdout: "piped",
        stderr: "piped",
      }).spawn();
    };
    const exporter = spawn(old, 0, "export"), importer = spawn(candidate, 1, "import");
    const readers: ReadableStreamDefaultReader<Uint8Array>[] = [];
    const limited = async (stream: ReadableStream<Uint8Array>, limit: number) => {
      const reader = stream.getReader();
      readers.push(reader);
      const chunks: Uint8Array[] = [];
      let size = 0;
      for (;;) {
        const { done, value: bytes } = await reader.read();
        if (done) break;
        size += bytes.length;
        if (size > limit) throw new KernelError("rejected", "交接诊断或回执超过限额");
        chunks.push(bytes);
      }
      const joined = new Uint8Array(size);
      let offset = 0;
      for (const bytes of chunks) {
        joined.set(bytes, offset);
        offset += bytes.length;
      }
      return new TextDecoder().decode(joined);
    };
    let transmitted = 0;
    const abort = new AbortController();
    const pipe = exporter.stdout.pipeThrough(
      new TransformStream<Uint8Array, Uint8Array>({
        transform(bytes, controller) {
          transmitted += bytes.length;
          if (transmitted > 370 * 1024 * 1024) {
            throw new KernelError("rejected", "交接总量超过限额");
          }
          controller.enqueue(bytes);
        },
      }),
    ).pipeTo(importer.stdin, { signal: abort.signal });
    const tasks = [
      exporter.status,
      importer.status,
      pipe,
      limited(exporter.stderr, 65536),
      limited(importer.stderr, 65536),
      limited(importer.stdout, 65536),
    ] as const;
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      const result = await Promise.race([
        Promise.all(tasks),
        new Promise<never>((_, reject) => {
          timer = setTimeout(() => reject(new KernelError("timeout", "隔离交接超时")), 120_000);
        }),
      ]);
      if (!result[0].success || !result[1].success) {
        await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
        await writeDurableJson(`${this.directory}/handover-failure.json`, {
          at: new Date().toISOString(),
          actionId,
          mode,
          from: old.id,
          to: candidate.id,
          exporter: { code: result[0].code, stderr: result[3] },
          importer: { code: result[1].code, stderr: result[4] },
        });
        throw new KernelError("rejected", "隔离交接校验失败，原件保留");
      }
      const receipt = JSON.parse(result[5]);
      if (receipt.actionId !== actionId) throw new KernelError("corrupt", "隔离交接回执不一致");
    } finally {
      clearTimeout(timer);
      abort.abort();
      // 同UID恶意同伴可能复制管道FD；取消读取，不以EOF作为退出条件。
      for (const reader of readers) void reader.cancel().catch(() => {});
      for (const process of [exporter, importer]) killHelper(process);
      await Promise.allSettled([exporter.status, importer.status]);
    }
  }

  async ensure(instance: InstanceSpec) {
    await verifySource(instance.source);
    await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
    try {
      const status = await this.call<InstanceStatus>(instance, "status");
      if (status.instanceId !== instance.id || status.sourceCommit !== instance.source.commit) {
        throw new KernelError("corrupt", "实例控制端对应了不同源码");
      }
      return;
    } catch (error) {
      if (error instanceof KernelError && error.kind === "corrupt") throw error;
    }
    const path = this.#processFile(instance);
    try {
      const previous = JSON.parse(await Deno.readTextFile(path));
      if (previous.instanceId !== instance.id || previous.sourceCommit !== instance.source.commit) {
        throw new KernelError("corrupt", "实例进程事实不一致");
      }
      if (await ticks(previous.pid) === previous.startTicks) {
        throw new KernelError(
          "unavailable",
          "原实例进程仍存活但控制不可用，保留现场并拒绝重复拉起",
        );
      }
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    const config = await readTrustedInstanceConfig(instance);
    if (
      config.instance?.id !== instance.id ||
      config.instance.sourceCommit !== instance.source.commit ||
      config.instance.connectionFile !== instance.connectionFile ||
      config.instance.keyFile !== instance.keyFile ||
      config.instance.credentialFile !== instance.credentialFile ||
      config.runtime?.proofFile !== instance.proofFile
    ) throw new KernelError("rejected", "实例配置与固定身份不一致");
    // 手机连接文件由独立适配器刷新；每次重拉起使用当前地址。
    if (config.phone?.connectionFile) {
      const connection = JSON.parse(await Deno.readTextFile(config.phone.connectionFile));
      config.phone = {
        ...config.phone,
        adapterUrl: connection.phone?.adapterUrl ?? connection.adapterUrl,
      };
      await writeDurableJson(instance.configFile, config);
      if (instance.identity) {
        await Deno.chmod(instance.configFile, 0o440);
        await Deno.chown(instance.configFile, 0, instance.identity.gid);
      }
    }
    if (instance.identity && this.managedIdentities.some((i) => i.uid === instance.identity!.uid)) {
      await this.prepareIdentity(instance.identity);
    }
    const cache = `${config.instance.directory}/cache`;
    await this.#ownedCommand(instance, "/bin/mkdir", ["-p", "--", cache]);
    const isolation = this.#isolation(instance.identity);
    const child = new Deno.Command("/bin/bash", {
      args: [
        "-c",
        'exec "$@" >> "$EVE_INSTANCE_LOG" 2>&1',
        "eve-instance",
        ...isolation,
        this.deno,
        "run",
        "--allow-read",
        "--allow-write",
        "--allow-net=127.0.0.1",
        `--allow-run=${this.deno}`,
        "--allow-env",
        `${instance.source.directory}/runtime/instance.ts`,
        instance.configFile,
      ],
      cwd: instance.source.directory,
      env: {
        DENO_DIR: cache,
        HOME: config.instance.directory,
        PATH: `${this.deno.slice(0, this.deno.lastIndexOf("/"))}:/usr/bin:/bin`,
        EVE_INSTANCE_LOG: `${this.directory}/${instance.id}.log`,
      },
      clearEnv: true,
      stdin: "null",
      stdout: "null",
      stderr: "null",
    }).spawn();
    this.#children.set(instance.id, child);
    const startTicks = await ticks(child.pid);
    if (!startTicks) throw new KernelError("unavailable", "实例启动前已退出，查看私有运行日志");
    await writeDurableJson(path, {
      instanceId: instance.id,
      sourceCommit: instance.source.commit,
      pid: child.pid,
      startTicks,
    });
    const end = Date.now() + 30_000;
    while (Date.now() < end) {
      try {
        const status = await this.call<InstanceStatus>(instance, "status");
        if (status.instanceId === instance.id && status.sourceCommit === instance.source.commit) {
          return;
        }
        throw new KernelError("corrupt", "拉起后实例身份不一致");
      } catch (error) {
        if (error instanceof KernelError && error.kind === "corrupt") throw error;
      }
      if (await ticks(child.pid) !== startTicks) {
        throw new KernelError("unavailable", "实例启动失败，查看私有运行日志");
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new KernelError("timeout", "实例启动未就绪，进程及日志保留");
  }
  #processFile(instance: InstanceSpec) {
    if (!/^[a-zA-Z0-9_-]{1,128}$/.test(instance.id)) {
      throw new KernelError("rejected", "实例编号不适合持久进程路径");
    }
    return `${this.directory}/${instance.id}.process.json`;
  }
  async terminate(instance: InstanceSpec) {
    if (instance.identity && this.managedIdentities.some((i) => i.uid === instance.identity!.uid)) {
      await this.prepareIdentity(instance.identity);
      const child = this.#children.get(instance.id);
      if (child) {
        await child.status;
        this.#children.delete(instance.id);
      }
      const config = await readTrustedInstanceConfig(instance);
      await this.#ownedCommand(instance, "/bin/rm", [
        "-rf",
        "--",
        `${config.instance.directory}/cache`,
      ]);
      return;
    }
    let process;
    try {
      process = JSON.parse(await Deno.readTextFile(this.#processFile(instance)));
    } catch (error) {
      if (error instanceof Deno.errors.NotFound) return;
      throw error;
    }
    if (process.instanceId !== instance.id || process.sourceCommit !== instance.source.commit) {
      throw new KernelError("corrupt", "不能终止身份不同的进程");
    }
    if (await ticks(process.pid) !== process.startTicks) return;
    try {
      Deno.kill(process.pid, "SIGTERM");
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    const end = Date.now() + 10_000;
    while (Date.now() < end && await ticks(process.pid) === process.startTicks) {
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    if (await ticks(process.pid) === process.startTicks) Deno.kill(process.pid, "SIGKILL");
    const child = this.#children.get(instance.id);
    if (child) {
      await child.status;
      this.#children.delete(instance.id);
    }
    const deathDeadline = Date.now() + 5000;
    while (await ticks(process.pid) === process.startTicks && Date.now() < deathDeadline) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    if (await ticks(process.pid) === process.startTicks) {
      throw new KernelError("unavailable", "实例仍存在，不能清理其缓存");
    }
    // 仅已终结实例的可再生缓存可清理。源码、日志和回退产物保留。
    const config = await readTrustedInstanceConfig(instance);
    const cache = `${config.instance.directory}/cache`;
    await this.#ownedCommand(instance, "/bin/rm", ["-rf", "--", cache]);
  }
}
