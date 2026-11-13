import { fingerprint } from "../../kernel/calls.ts";
import { KernelError } from "../../kernel/protocol.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";
import { FileTools } from "./files.ts";
import type { CommandCompletion, CommandView } from "../../contracts/workspace.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";

interface Saved {
  sessionId: string;
  requestId: string;
  cmd: string;
  cwd: string;
  pid?: number;
  startTicks?: string;
  unitName?: string;
  state: "starting" | "running" | "completed" | "unknown";
  exitCode?: number;
}
interface Live {
  child: Deno.ChildProcess;
  done: Promise<void>;
  stdin: WritableStreamDefaultWriter<Uint8Array>;
}
export interface CommandIdentity {
  uid: number;
  gid: number;
}
export class CommandSessions {
  #jobs = new Map<string, Saved>();
  #live = new Map<string, Live>();
  #closing = false;
  #waiters = new Map<string, Set<() => void>>();
  #lock?: Deno.FsFile;
  #completionIds = new Set<string>();
  readonly completionJournal: FactJournal;
  constructor(
    readonly directory: string,
    readonly files: FileTools,
    readonly identity?: CommandIdentity,
    readonly useSystemd = false,
    readonly unrestricted = false,
  ) {
    this.completionJournal = new FactJournal(`${directory}/completions`);
    if (useSystemd && !identity) {
      throw new KernelError("rejected", "systemd 命令必须明确非 root 身份");
    }
    if (
      identity && (!Number.isSafeInteger(identity.uid) || identity.uid < (unrestricted ? 0 : 1) ||
        !Number.isSafeInteger(identity.gid) || identity.gid < (unrestricted ? 0 : 1))
    ) {
      throw new KernelError("rejected", "命令身份必须是明确的非 root 用户与组");
    }
    if (unrestricted && (identity?.uid !== 0 || identity?.gid !== 0)) {
      throw new KernelError("rejected", "整机管理员模式必须明确配置 root 命令身份");
    }
  }
  async initialize() {
    if (this.#lock || this.#closing) throw new KernelError("unavailable", "命令会话已被持有或停止");
    await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
    const lock = await Deno.open(`${this.directory}/writer.lock`, {
      create: true,
      read: true,
      write: true,
      mode: 0o600,
    });
    if (!(await lock.tryLock(true))) {
      lock.close();
      throw new KernelError("unavailable", "已有进程持有命令会话");
    }
    this.#lock = lock;
    try {
      await this.completionJournal.initialize();
      for await (const entry of Deno.readDir(this.directory)) {
        if (
          entry.name === "writer.lock" && entry.isFile ||
          entry.name === "completions" && entry.isDirectory
        ) continue;
        if (!entry.isDirectory || !/^[a-f0-9]{64}$/.test(entry.name)) {
          throw new KernelError("corrupt", "命令会话目录异常，原件保留");
        }
        const path = `${this.directory}/${entry.name}/state.json`;
        const stat = await Deno.lstat(path);
        if (!stat.isFile || stat.isSymlink || stat.size > 1_048_576) {
          throw new KernelError("corrupt", "命令会话原件无效");
        }
        const saved: Saved = JSON.parse(await Deno.readTextFile(path));
        if (
          saved.sessionId !== entry.name || typeof saved.requestId !== "string" ||
          !saved.requestId ||
          typeof saved.cmd !== "string" || typeof saved.cwd !== "string" ||
          !["starting", "running", "completed", "unknown"].includes(saved.state) ||
          (saved.pid !== undefined && (!Number.isSafeInteger(saved.pid) || saved.pid <= 0)) ||
          (saved.startTicks !== undefined && !/^\d+$/.test(saved.startTicks)) ||
          (saved.exitCode !== undefined &&
            (!Number.isSafeInteger(saved.exitCode) || saved.exitCode < 0 ||
              saved.exitCode > 255)) ||
          (saved.state === "completed" && saved.exitCode === undefined) ||
          (saved.state === "running" && saved.pid === undefined)
        ) throw new KernelError("corrupt", "命令会话无法关联原请求");
        if (
          saved.unitName !== undefined &&
          saved.unitName !== `eve-command-${saved.sessionId}.service`
        ) {
          throw new KernelError("corrupt", "命令会话 systemd unit 无法关联原请求");
        }
        if (saved.state === "running" || saved.state === "starting") {
          const ticks = saved.pid ? await this.#ticks(saved.pid) : undefined;
          if (saved.unitName && await this.#unitAlive(saved.unitName)) {
            // 已持有会话独占锁，原宿主已退出；先终止原 cgroup 再把结果标为 unknown。
            await new Deno.Command("/usr/bin/systemctl", {
              args: ["stop", saved.unitName],
              stdout: "null",
              stderr: "null",
            }).output();
            if (await this.#unitAlive(saved.unitName)) {
              throw new KernelError("unavailable", "原命令 cgroup 未确认终止，保留现场");
            }
          } else if (
            !saved.unitName && saved.pid &&
            (ticks !== undefined && ticks === saved.startTicks || await this.#groupAlive(saved.pid))
          ) {
            // 原进程仍活着时不能把它结算成结束，更不能重发命令。
            throw new KernelError("unavailable", "原命令进程仍在运行，须先由服务监督终结原进程组");
          }
          saved.state = "unknown";
          await writeDurableJson(path, saved);
        }
        this.#jobs.set(saved.sessionId, saved);
      }
      for (const event of this.completionJournal.events) {
        const data = event.data as unknown as {
          sessionId: string;
          requestId: string;
          state: Saved["state"];
          exitCode?: number;
        };
        const saved = data && this.#jobs.get(data.sessionId);
        if (
          event.kind !== "completion" || !saved || saved.requestId !== data.requestId ||
          !["completed", "unknown"].includes(data.state) ||
          this.#completionIds.has(data.sessionId) ||
          (data.exitCode !== undefined &&
            (!Number.isSafeInteger(data.exitCode) || data.exitCode < 0))
        ) {
          throw new KernelError("corrupt", "命令完成通知原件无法关联唯一会话");
        }
        this.#completionIds.add(data.sessionId);
      }
      for (const saved of this.#jobs.values()) {
        if (saved.state === "completed" || saved.state === "unknown") {
          await this.#recordCompletion(saved);
        }
      }
    } catch (error) {
      await this.completionJournal.close().catch(() => {});
      lock.close();
      this.#lock = undefined;
      throw error;
    }
  }
  async #recordCompletion(saved: Saved) {
    if (saved.state !== "completed" && saved.state !== "unknown") {
      throw new KernelError("rejected", "运行中的命令不能登记完成事实");
    }
    if (this.#completionIds.has(saved.sessionId)) return;
    await this.completionJournal.append("completion", {
      sessionId: saved.sessionId,
      requestId: saved.requestId,
      state: saved.state,
      ...(saved.exitCode !== undefined ? { exitCode: saved.exitCode } : {}),
    });
    this.#completionIds.add(saved.sessionId);
  }
  completions(
    args: { afterSeq?: number } = {},
  ): { items: CommandCompletion[]; throughSeq: number; hasMore: boolean } {
    const after = args.afterSeq ?? 0;
    if (!Number.isSafeInteger(after) || after < 0 || after > this.completionJournal.events.length) {
      throw new KernelError("rejected", "命令完成通知游标无效");
    }
    const events = this.completionJournal.events.slice(after, after + 8);
    return {
      items: events.map((event) => ({
        seq: event.seq,
        at: event.at,
        ...(event.data as unknown as Omit<CommandCompletion, "seq" | "at">),
      })),
      throughSeq: events.at(-1)?.seq ?? after,
      hasMore: after + events.length < this.completionJournal.events.length,
    };
  }
  async #ticks(pid: number) {
    const result = await new Deno.Command("/bin/cat", {
      args: [`/proc/${pid}/stat`],
      stdout: "piped",
      stderr: "null",
    }).output();
    if (!result.success) return undefined;
    const stat = new TextDecoder().decode(result.stdout);
    return stat.slice(stat.lastIndexOf(")") + 2).split(" ")[19];
  }
  async start(
    args: { cmd: string; cwd?: string },
    requestId: string,
  ): Promise<{ sessionId: string; done: Promise<void> }> {
    if (this.#closing || !this.#lock) {
      throw new KernelError("unavailable", "命令出口未运行或正在停止");
    }
    if (typeof args.cmd !== "string" || !args.cmd || args.cmd.length > 65536) {
      throw new KernelError("rejected", "命令正文无效");
    }
    const sessionId = await fingerprint(["command-v1", requestId]);
    const cwd = await this.files.path(args.cwd ?? ".");
    const old = this.#jobs.get(sessionId);
    if (old) {
      if (old.cmd !== args.cmd || old.cwd !== cwd) {
        throw new KernelError("rejected", "同一命令编号不能改变正文");
      }
      return { sessionId, done: this.#live.get(sessionId)?.done ?? Promise.resolve() };
    }
    const directory = `${this.directory}/${sessionId}`;
    await Deno.mkdir(directory, { mode: 0o700 });
    const saved: Saved = {
      sessionId,
      requestId,
      cmd: args.cmd,
      cwd,
      state: "starting",
      ...(this.useSystemd ? { unitName: `eve-command-${sessionId}.service` } : {}),
    };
    await writeDurableJson(`${directory}/state.json`, saved);
    this.#jobs.set(sessionId, saved);
    // 全部输出保留原件，模型每次只读一页。监督 unit 的 KillMode=control-group 兜住崩溃子树。
    const out = await Deno.open(`${directory}/stdout`, {
      write: true,
      createNew: true,
      mode: 0o600,
    });
    let err: Deno.FsFile;
    try {
      err = await Deno.open(`${directory}/stderr`, {
        write: true,
        createNew: true,
        mode: 0o600,
      });
    } catch (error) {
      out.close();
      saved.state = "unknown";
      await writeDurableJson(`${directory}/state.json`, saved);
      await this.#recordCompletion(saved);
      throw error;
    }
    let child: Deno.ChildProcess;
    try {
      const identityArgs = this.identity
        ? [
          "--reuid",
          String(this.identity.uid),
          "--regid",
          String(this.identity.gid),
          "--clear-groups",
          ...(this.unrestricted ? [] : [
            "--no-new-privs",
            "--bounding-set=-all",
            "--inh-caps=-all",
            "--ambient-caps=-all",
          ]),
          "/usr/bin/setsid",
        ]
        : [];
      const systemdArgs = saved.unitName
        ? [
          "--quiet",
          "--pipe",
          "--wait",
          "--collect",
          "--service-type=exec",
          `--unit=${saved.unitName}`,
          `--working-directory=${cwd}`,
          `--property=User=${this.identity!.uid}`,
          `--property=Group=${this.identity!.gid}`,
          "--property=KillMode=control-group",
          ...(this.unrestricted ? ["--property=NoNewPrivileges=no"] : [
            "--property=NoNewPrivileges=yes",
            "--property=CapabilityBoundingSet=",
            "--property=AmbientCapabilities=",
          ]),
          "--property=TimeoutStopSec=1s",
          "--property=SendSIGKILL=yes",
          "/usr/bin/env",
          "-i",
          `PATH=${
            this.unrestricted ? "/usr/local/sbin:/usr/sbin:/sbin:" : ""
          }/usr/local/bin:/usr/bin:/bin`,
          `HOME=${this.unrestricted ? "/root" : cwd}`,
          "LANG=C.UTF-8",
          "/bin/bash",
          "-lc",
          args.cmd,
        ]
        : undefined;
      child = new Deno.Command(
        saved.unitName
          ? "/usr/bin/systemd-run"
          : this.identity
          ? "/usr/bin/setpriv"
          : "/usr/bin/setsid",
        {
          args: systemdArgs ?? [...identityArgs, "/bin/bash", "-lc", args.cmd],
          cwd,
          clearEnv: true,
          env: {
            PATH: `${
              this.unrestricted ? "/usr/local/sbin:/usr/sbin:/sbin:" : ""
            }/usr/local/bin:/usr/bin:/bin`,
            HOME: this.unrestricted ? "/root" : cwd,
            LANG: "C.UTF-8",
          },
          stdin: "piped",
          stdout: "piped",
          stderr: "piped",
        },
      ).spawn();
    } catch (error) {
      out.close();
      err.close();
      saved.state = "unknown";
      await writeDurableJson(`${directory}/state.json`, saved);
      await this.#recordCompletion(saved);
      throw error;
    }
    saved.pid = child.pid;
    saved.state = "running";
    // 立即接管管道，即使登记 pid 写盘失败也要终止并收齐原进程。
    const copy = async (stream: ReadableStream<Uint8Array>, file: Deno.FsFile) => {
      try {
        for await (const bytes of stream) {
          let at = 0;
          while (at < bytes.length) {
            const n = await file.write(bytes.subarray(at));
            if (!n) throw new Error("命令输出保存未推进");
            at += n;
          }
        }
        await file.sync();
      } finally {
        file.close();
      }
    };
    const pumps = [copy(child.stdout, out), copy(child.stderr, err)];
    // 一个输出流失败仍须等另一个流落盘和真实进程退出，不能提前释放所有权许可。
    for (const pump of pumps) {
      pump.catch(() => this.#kill(child.pid, "KILL", saved.unitName).catch(() => {}));
    }
    const streams = Promise.allSettled(pumps);
    const stdin = child.stdin.getWriter();
    const done = (async () => {
      try {
        saved.startTicks = await this.#ticks(child.pid);
        await writeDurableJson(`${directory}/state.json`, saved);
        const status = await child.status;
        const outputs = await streams;
        if (outputs.some((result) => result.status === "rejected")) {
          throw new KernelError("storage", "命令输出保存失败，原件保留且不重跑");
        }
        saved.state = "completed";
        saved.exitCode = status.code;
        await writeDurableJson(`${directory}/state.json`, saved);
        await this.#recordCompletion(saved);
      } catch (error) {
        // setsid 登记或输出写盘失败也必须先终结整组并收齐原管道。
        await this.#kill(child.pid, "KILL", saved.unitName);
        await child.status;
        await streams;
        saved.state = "unknown";
        await writeDurableJson(`${directory}/state.json`, saved);
        await this.#recordCompletion(saved);
        throw error;
      }
    })().finally(() => {
      stdin.releaseLock();
      this.#live.delete(sessionId);
    });
    done.catch(() => {});
    this.#live.set(sessionId, { child, stdin, done });
    for (const resolve of this.#waiters.get(sessionId) ?? []) resolve();
    return { sessionId, done };
  }
  started(sessionId: string, signal: AbortSignal): Promise<void> {
    if (this.#live.has(sessionId) || this.#jobs.get(sessionId)?.state === "completed") {
      return Promise.resolve();
    }
    return new Promise((resolve, reject) => {
      const waiters = this.#waiters.get(sessionId) ?? new Set();
      const cleanup = () => {
        waiters.delete(ready);
        if (!waiters.size) this.#waiters.delete(sessionId);
        signal.removeEventListener("abort", abort);
      };
      const ready = () => {
        cleanup();
        resolve();
      };
      const abort = () => {
        cleanup();
        reject(new KernelError("unavailable", "原命令等待已结束"));
      };
      waiters.add(ready);
      this.#waiters.set(sessionId, waiters);
      signal.addEventListener("abort", abort, { once: true });
      if (signal.aborted) abort();
    });
  }
  async #kill(pid: number, signal: "TERM" | "KILL", unitName?: string) {
    if (unitName) {
      const result = await new Deno.Command("/usr/bin/systemctl", {
        args: ["kill", "--kill-whom=all", `--signal=SIG${signal}`, unitName],
        stdout: "null",
        stderr: "null",
      }).output();
      if (!result.success && await this.#unitAlive(unitName)) {
        throw new KernelError("unknown", "原命令 cgroup 未确认结束");
      }
      return;
    }
    const result = await new Deno.Command("/bin/kill", {
      args: [`-${signal}`, "--", `-${pid}`],
      stdout: "null",
      stderr: "null",
    }).output();
    if (!result.success && await this.#ticks(pid)) {
      // spawn 后 setsid 尚未来得及建立进程组的窄窗口，终止原子进程；不查找或猜测其他 pid。
      try {
        Deno.kill(pid, signal === "TERM" ? "SIGTERM" : "SIGKILL");
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
    }
    if (!result.success && await this.#groupAlive(pid)) {
      throw new KernelError("unknown", "原命令进程组未确认结束");
    }
  }
  async #groupAlive(pid: number) {
    const result = await new Deno.Command("/bin/kill", {
      args: ["-0", "--", `-${pid}`],
      stdout: "null",
      stderr: "null",
    }).output();
    return result.success;
  }
  async #unitAlive(unitName: string) {
    const result = await new Deno.Command("/usr/bin/systemctl", {
      args: ["show", unitName, "--property=LoadState", "--property=ActiveState"],
      stdout: "piped",
      stderr: "null",
    }).output();
    const status = new TextDecoder().decode(result.stdout);
    if (status.includes("LoadState=not-found")) return false;
    if (!result.success) throw new KernelError("unavailable", "无法核查原命令 systemd unit");
    const state = /^ActiveState=(.*)$/m.exec(status)?.[1];
    if (state === "inactive" || state === "failed") return false;
    if (["active", "activating", "deactivating", "reloading", "refreshing"].includes(state ?? "")) {
      return true;
    }
    throw new KernelError("unavailable", "原命令 unit 状态无法确认");
  }
  async wait(sessionId: string, ms = 0) {
    if (!Number.isSafeInteger(ms) || ms < 0 || ms > 10000) {
      throw new KernelError("rejected", "命令等待须在0至10000毫秒");
    }
    const done = this.#live.get(sessionId)?.done;
    if (!done || !ms) return;
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([done, new Promise<void>((r) => timer = setTimeout(r, ms))]);
    } finally {
      clearTimeout(timer);
    }
  }
  async poll(
    args: {
      sessionId: string;
      stdoutOffsetBytes?: number;
      stderrOffsetBytes?: number;
      limitBytes?: number;
      waitMs?: number;
    },
  ): Promise<CommandView> {
    const saved = this.#jobs.get(args.sessionId);
    if (!saved) throw new KernelError("not_found", "没有该命令会话");
    await this.wait(args.sessionId, args.waitMs);
    const page = async (name: string, offset = 0) => {
      const limit = args.limitBytes ?? 8192;
      if (
        !Number.isSafeInteger(offset) || offset < 0 || !Number.isSafeInteger(limit) || limit < 1 ||
        limit > 32768
      ) throw new KernelError("rejected", "命令输出页参数无效");
      let file: Deno.FsFile;
      try {
        file = await Deno.open(`${this.directory}/${saved.sessionId}/${name}`);
      } catch (error) {
        if (error instanceof Deno.errors.NotFound && saved.state === "unknown") {
          return { text: "", nextOffsetBytes: offset, sizeBytes: 0, missingOriginal: true };
        }
        throw error;
      }
      try {
        const stat = await file.stat(), bytes = new Uint8Array(limit);
        await file.seek(offset, Deno.SeekMode.Start);
        const n = await file.read(bytes) ?? 0;
        return {
          text: new TextDecoder().decode(bytes.subarray(0, n)),
          nextOffsetBytes: offset + n,
          sizeBytes: stat.size,
        };
      } finally {
        file.close();
      }
    };
    return {
      sessionId: saved.sessionId,
      state: saved.state === "completed"
        ? "completed"
        : saved.state === "unknown"
        ? "unknown"
        : "running",
      ...(saved.exitCode !== undefined ? { exitCode: saved.exitCode } : {}),
      stdout: await page("stdout", args.stdoutOffsetBytes),
      stderr: await page("stderr", args.stderrOffsetBytes),
    };
  }
  async write(args: { sessionId: string; text?: string; closeStdin?: boolean }) {
    const live = this.#live.get(args.sessionId);
    if (!live) throw new KernelError("unavailable", "原命令不再接受输入，不重建或重跑");
    if (args.text !== undefined) {
      if (typeof args.text !== "string" || args.text.length > 65536) {
        throw new KernelError("rejected", "命令输入过大");
      }
      await live.stdin.write(new TextEncoder().encode(args.text));
    }
    if (args.closeStdin) await live.stdin.close();
    return await this.poll({ sessionId: args.sessionId });
  }
  async cancel(args: { sessionId: string }) {
    const live = this.#live.get(args.sessionId);
    if (live) {
      const unitName = this.#jobs.get(args.sessionId)?.unitName;
      await this.#kill(live.child.pid, "TERM", unitName);
      await this.wait(args.sessionId, 1000);
      if (this.#live.has(args.sessionId)) await this.#kill(live.child.pid, "KILL", unitName);
      await live.done;
    }
    return await this.poll({ sessionId: args.sessionId });
  }
  async close() {
    this.#closing = true;
    try {
      await this.cancelAll();
    } finally {
      try {
        await this.completionJournal.close();
      } finally {
        this.#lock?.close();
        this.#lock = undefined;
      }
    }
  }
  async cancelAll() {
    const results = await Promise.allSettled(
      [...this.#live.keys()].map((sessionId) => this.cancel({ sessionId })),
    );
    if (results.some((result) => result.status === "rejected")) {
      throw new KernelError("unknown", "部分命令停止未确认，原件保留");
    }
  }
}
