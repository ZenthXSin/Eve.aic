import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

export interface SourceArtifact {
  commit: string;
  directory: string;
  digest: string;
  checkedAt: string;
  files: { path: string; sha256: string; executable: boolean }[];
}
async function command(
  cmd: string,
  args: string[],
  cwd?: string,
  env?: Record<string, string>,
  identity?: { uid: number; gid: number },
  failureLog?: string,
) {
  const isolation = identity
    ? [
      "--reuid",
      String(identity.uid),
      "--regid",
      String(identity.gid),
      "--clear-groups",
      "--no-new-privs",
      "--bounding-set=-all",
      "--inh-caps=-all",
      "--ambient-caps=-all",
      cmd,
    ]
    : [];
  const output = await new Deno.Command(identity ? "/usr/bin/setpriv" : cmd, {
    args: identity ? [...isolation, ...args] : args,
    cwd,
    env: { PATH: "/usr/bin:/bin", ...env },
    clearEnv: true,
    stdout: "piped",
    stderr: "piped",
  })
    .output();
  if (!output.success) {
    if (failureLog) {
      const log = await Deno.open(failureLog, { create: true, append: true, mode: 0o600 });
      try {
        for (
          const bytes of [
            new TextEncoder().encode(
              JSON.stringify({ cmd, args, code: output.code }) + "\nstdout:\n",
            ),
            output.stdout,
            new TextEncoder().encode("\nstderr:\n"),
            output.stderr,
          ]
        ) {
          let offset = 0;
          while (offset < bytes.length) offset += await log.write(bytes.subarray(offset));
        }
      } finally {
        log.close();
      }
    }
    throw new KernelError(
      "rejected",
      `${cmd} 校验失败，原始输出仅保留于私有诊断目录`,
    );
  }
  return output.stdout;
}
async function hash(bytes: Uint8Array) {
  return [...new Uint8Array(await crypto.subtle.digest("SHA-256", new Uint8Array(bytes)))].map((
    b,
  ) => b.toString(16).padStart(2, "0")).join("");
}

/** Production validation owns a cgroup: setsid cannot escape it or retain a pipe. */
async function validateTask(options: {
  task: string;
  source: string;
  directory: string;
  deno: string;
  env: Record<string, string>;
  identity?: { uid: number; gid: number };
  timeoutMs: number;
}) {
  const unit = options.identity
    ? `eve-source-validation-${crypto.randomUUID()}.service`
    : undefined;
  const args = unit
    ? [
      "--quiet",
      "--pipe",
      "--wait",
      "--collect",
      "--service-type=exec",
      `--unit=${unit}`,
      `--working-directory=${options.source}`,
      `--property=User=${options.identity!.uid}`,
      `--property=Group=${options.identity!.gid}`,
      "--property=KillMode=control-group",
      "--property=NoNewPrivileges=yes",
      "--property=CapabilityBoundingSet=",
      "--property=AmbientCapabilities=",
      "--property=TimeoutStopSec=1s",
      "--property=SendSIGKILL=yes",
      `--property=RuntimeMaxSec=${options.timeoutMs}ms`,
      "/usr/bin/env",
      "-i",
      ...Object.entries(options.env).map(([k, v]) => `${k}=${v}`),
      options.deno,
      "task",
      options.task,
    ]
    : [options.deno, "task", options.task];
  const log = await Deno.open(`${options.directory}/validation-${options.task}.log`, {
    createNew: true,
    write: true,
    mode: 0o600,
  });
  let child: Deno.ChildProcess | undefined;
  const readers: ReadableStreamDefaultReader<Uint8Array>[] = [];
  const pump = async (stream: ReadableStream<Uint8Array>) => {
    const reader = stream.getReader();
    readers.push(reader);
    try {
      while (true) {
        const { value: chunk, done } = await reader.read();
        if (done) break;
        let offset = 0;
        while (offset < chunk.length) offset += await log.write(chunk.subarray(offset));
      }
    } finally {
      reader.releaseLock();
    }
  };
  const control = async (args: string[]) =>
    await new Deno.Command("/usr/bin/systemctl", {
      args,
      stdout: "piped",
      stderr: "null",
      clearEnv: true,
      env: { PATH: "/usr/bin:/bin" },
    }).output();
  const terminate = async () => {
    if (!child) return;
    if (unit) {
      // Unit-specific operations never signal another process sharing the execution UID.
      await control(["kill", "--kill-who=all", "--signal=KILL", unit]);
      await control(["stop", "--no-block", unit]);
      for (let attempt = 0; attempt < 100; attempt++) {
        const result = await control([
          "show",
          unit,
          "--property=LoadState",
          "--property=ActiveState",
        ]);
        const status = new TextDecoder().decode(result.stdout);
        if (status.includes("LoadState=not-found")) return;
        if (!result.success) throw new KernelError("unavailable", "源码校验 cgroup 状态无法确认");
        if (/^ActiveState=(inactive|failed)$/m.test(status)) return;
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
      throw new KernelError("unknown", "源码校验 cgroup 未确认清空，保留临时诊断目录");
    }
    // Non-production fixtures without an execution identity need no system manager.
    await new Deno.Command("/bin/kill", {
      args: ["-KILL", "--", `-${child.pid}`],
      stdout: "null",
      stderr: "null",
    }).output();
  };
  const finishLauncher = async () => {
    if (child) {
      try {
        child.kill("SIGKILL");
      } catch (error) {
        if (
          !(error instanceof Deno.errors.NotFound) &&
          !(error instanceof TypeError &&
            error.message === "Child process has already terminated")
        ) throw error;
      }
      await child.status;
    }
  };
  let deadline: ReturnType<typeof setTimeout> | undefined;
  let pumps: Promise<void>[] = [];
  try {
    child = new Deno.Command(unit ? "/usr/bin/systemd-run" : "/usr/bin/setsid", {
      args,
      cwd: options.source,
      env: options.env,
      clearEnv: true,
      stdout: "piped",
      stderr: "piped",
    }).spawn();
    pumps = [pump(child.stdout), pump(child.stderr)];
    const outputComplete = Promise.all(pumps);
    // Attach immediately so a log write failure cannot become an unhandled rejection.
    void outputComplete.catch(() => {});
    await Promise.race([
      (async () => {
        const status = await child!.status;
        // Even a successful main process may have left daemonized descendants.
        await terminate();
        await outputComplete;
        if (!status.success) {
          throw new KernelError("rejected", `候选 ${options.task} 校验失败，查看监督私有诊断日志`);
        }
      })(),
      new Promise<never>((_, reject) => {
        deadline = setTimeout(
          () => reject(new KernelError("rejected", `候选 ${options.task} 校验超时`)),
          options.timeoutMs,
        );
      }),
    ]);
  } finally {
    clearTimeout(deadline);
    try {
      await terminate();
    } finally {
      await finishLauncher();
      await Promise.allSettled(readers.map((reader) => reader.cancel().catch(() => {})));
      await Promise.allSettled(pumps);
      log.close();
    }
  }
}

/** 只从完整固定提交构建；拒绝链接/子模块，避免归档跳出来源目录。 */
export async function pinSource(options: {
  repository: string;
  commit: string;
  directory: string;
  deno: string;
  validate?: boolean;
  /** Production uses a systemd service; provision the non-root user/group before validation. */
  validationIdentity?: { uid: number; gid: number };
  validationTimeoutMs?: number;
}): Promise<SourceArtifact> {
  if (
    options.validationIdentity &&
    ![options.validationIdentity.uid, options.validationIdentity.gid].every((n) =>
      Number.isSafeInteger(n) && n > 0
    )
  ) {
    throw new KernelError("rejected", "源码验证不能使用root身份");
  }
  if (!/^[a-f0-9]{40}$/.test(options.commit) || !options.directory.startsWith("/")) {
    throw new KernelError("rejected", "源码必须固定完整提交与绝对产物目录");
  }
  await Deno.mkdir(options.directory, { mode: 0o755 });
  await Deno.chmod(options.directory, 0o755);
  const failureLog = `${options.directory}/source-command.log`;
  const actual = new TextDecoder().decode(
    await command(
      "git",
      [
        "-c",
        `safe.directory=${options.repository}`,
        "rev-parse",
        "--verify",
        `${options.commit}^{commit}`,
      ],
      options.repository,
      undefined,
      options.validationIdentity,
      failureLog,
    ),
  ).trim();
  if (actual !== options.commit) throw new KernelError("rejected", "源码提交不匹配");
  const tree = new TextDecoder().decode(
    await command(
      "git",
      [
        "-c",
        `safe.directory=${options.repository}`,
        "ls-tree",
        "-rz",
        options.commit,
      ],
      options.repository,
      undefined,
      options.validationIdentity,
      failureLog,
    ),
  );
  const entries = tree.split("\0").filter(Boolean).map((entry) => {
    const match = /^(100644|100755) blob ([a-f0-9]{40})\t(.+)$/.exec(entry);
    if (
      !match || match[3].split("/").some((p) => !p || p === "." || p === "..") ||
      /[\r\n\\]/.test(match[3])
    ) {
      throw new KernelError("rejected", "源码包含不支持的链接、子模块或路径");
    }
    return { mode: match[1], object: match[2], path: match[3] };
  });
  if (entries.length > 10_000) throw new KernelError("rejected", "候选源码文件数超限");
  let totalBytes = 0;
  const source = `${options.directory}/source`;
  await Deno.mkdir(source, { mode: 0o755 });
  await Deno.chmod(source, 0o755);
  const files: SourceArtifact["files"] = [];
  for (const entry of entries) {
    const byteCount = Number(
      new TextDecoder().decode(
        await command(
          "git",
          ["-c", `safe.directory=${options.repository}`, "cat-file", "-s", entry.object],
          options.repository,
          undefined,
          options.validationIdentity,
          failureLog,
        ),
      ).trim(),
    );
    totalBytes += byteCount;
    if (
      !Number.isSafeInteger(byteCount) || byteCount < 0 || byteCount > 8_388_608 ||
      totalBytes > 268_435_456
    ) throw new KernelError("rejected", "候选源码大小超限");
    const path = `${source}/${entry.path}`;
    await Deno.mkdir(path.slice(0, path.lastIndexOf("/")), { recursive: true, mode: 0o755 });
    const bytes = await command(
      "git",
      [
        "-c",
        `safe.directory=${options.repository}`,
        "cat-file",
        "blob",
        entry.object,
      ],
      options.repository,
      undefined,
      options.validationIdentity,
      failureLog,
    );
    await Deno.writeFile(path, bytes, {
      createNew: true,
      mode: entry.mode === "100755" ? 0o755 : 0o644,
    });
    await Deno.chmod(path, entry.mode === "100755" ? 0o755 : 0o644);
    files.push({
      path: entry.path,
      sha256: await hash(bytes),
      executable: entry.mode === "100755",
    });
  }
  const readableDirectories = async (path: string) => {
    await Deno.chmod(path, 0o755);
    for await (const entry of Deno.readDir(path)) {
      if (entry.isDirectory) await readableDirectories(`${path}/${entry.name}`);
    }
  };
  await readableDirectories(source);
  const temporary = await Deno.makeTempDir({ dir: options.directory, prefix: "validation-work-" });
  let cleanupSafe = true;
  try {
    if (options.validate !== false) {
      const env = {
        DENO_DIR: `${temporary}/cache`,
        TMPDIR: `${temporary}/tmp`,
        HOME: temporary,
        PATH: `${options.deno.slice(0, options.deno.lastIndexOf("/"))}:/usr/bin:/bin`,
      };
      await Deno.mkdir(env.TMPDIR);
      await Deno.mkdir(env.DENO_DIR);
      if (options.validationIdentity) {
        const { uid, gid } = options.validationIdentity;
        for (const path of [temporary, env.DENO_DIR, env.TMPDIR]) await Deno.chown(path, uid, gid);
      }
      const timeoutMs = options.validationTimeoutMs ?? 600_000;
      if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 100 || timeoutMs > 600_000) {
        throw new KernelError("rejected", "源码校验超时范围无效");
      }
      for (const task of ["check", "test"]) {
        await validateTask({
          task,
          source,
          directory: options.directory,
          deno: options.deno,
          env,
          identity: options.validationIdentity,
          timeoutMs,
        });
      }
    }
  } catch (error) {
    if (error instanceof KernelError && ["unknown", "unavailable"].includes(error.kind)) {
      cleanupSafe = false;
    }
    throw error;
  } finally {
    // Preserve diagnostics if the system manager could not prove cleanup completed.
    if (cleanupSafe) await Deno.remove(temporary, { recursive: true });
  }
  const artifact: SourceArtifact = {
    commit: options.commit,
    directory: source,
    digest: await fingerprint(files as unknown as Json),
    checkedAt: new Date().toISOString(),
    files,
  };
  await writeDurableJson(`${options.directory}/artifact.json`, artifact);
  return artifact;
}

/** 每次拉起前重新核实产物；运行期缓存或额外文件也不能混进固定源码。 */
export async function verifySource(artifact: SourceArtifact): Promise<void> {
  if (
    !/^[a-f0-9]{40}$/.test(artifact.commit) ||
    artifact.digest !== await fingerprint(artifact.files as unknown as Json)
  ) {
    throw new KernelError("corrupt", "源码产物清单损坏");
  }
  const found = new Set<string>();
  async function walk(directory: string, prefix = "") {
    for await (const entry of Deno.readDir(directory)) {
      const relative = `${prefix}${entry.name}`;
      if (entry.isSymlink || (!entry.isDirectory && !entry.isFile)) {
        throw new KernelError("corrupt", "源码产物有链接或未知文件");
      }
      if (entry.isDirectory) await walk(`${directory}/${entry.name}`, `${relative}/`);
      else found.add(relative);
    }
  }
  await walk(artifact.directory);
  if (found.size !== artifact.files.length) {
    throw new KernelError("corrupt", "源码产物文件集合变化");
  }
  for (const item of artifact.files) {
    if (!found.has(item.path) || /(^|\/)\.\.?($|\/)/.test(item.path) || item.path.startsWith("/")) {
      throw new KernelError("corrupt", "源码清单路径无效");
    }
    if (await hash(await Deno.readFile(`${artifact.directory}/${item.path}`)) !== item.sha256) {
      throw new KernelError("corrupt", "源码产物摘要变化");
    }
  }
}
