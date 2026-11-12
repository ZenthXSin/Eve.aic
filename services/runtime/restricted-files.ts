import { KernelError, type WireError } from "../../kernel/protocol.ts";
import type { CommandIdentity } from "./commands.ts";
import { FileTools } from "./files.ts";
import { Buffer } from "node:buffer";

export interface FileRuntime {
  denoBinary?: string;
  workerPath?: string;
  cacheDirectory?: string;
}
/** 路径校验与真正打开之间即使发生符号链接竞态，也只有非 root 身份的权限。 */
export class RestrictedFileTools extends FileTools {
  #writes: Promise<unknown> = Promise.resolve();
  #operations: Promise<unknown> = Promise.resolve();
  constructor(
    root: string,
    allowedRoots: string[] | undefined,
    readonly identity: CommandIdentity,
    readonly runtime: FileRuntime = {},
  ) {
    super(root, allowedRoots);
    if (
      !Number.isSafeInteger(identity.uid) || identity.uid <= 0 ||
      !Number.isSafeInteger(identity.gid) || identity.gid <= 0
    ) {
      throw new KernelError("rejected", "文件工具必须使用明确的非 root 身份");
    }
  }
  #call<T>(method: string, args: unknown): Promise<T> {
    const operation = this.#operations.then(() => this.#execute<T>(method, args));
    this.#operations = operation.catch(() => {});
    return operation;
  }
  async #execute<T>(method: string, args: unknown): Promise<T> {
    const roots = await Promise.all(this.allowedRoots.map((root) => Deno.realPath(root)));
    if (roots.some((root) => root.includes(","))) {
      throw new KernelError("rejected", "文件根目录不能包含权限列表分隔符");
    }
    const cacheDirectory = this.runtime.cacheDirectory ?? `${this.root}/.eve-tool-cache`;
    const child = new Deno.Command("/usr/bin/setpriv", {
      args: [
        "--reuid",
        String(this.identity.uid),
        "--regid",
        String(this.identity.gid),
        "--clear-groups",
        "--no-new-privs",
        "--bounding-set=-all",
        "--inh-caps=-all",
        "--ambient-caps=-all",
        this.runtime.denoBinary ?? Deno.execPath(),
        "run",
        "--no-config",
        "--no-prompt",
        `--allow-read=${roots.join(",")}`,
        `--allow-write=${roots.join(",")}`,
        this.runtime.workerPath ?? new URL("./files-worker.ts", import.meta.url).pathname,
      ],
      cwd: this.root,
      clearEnv: true,
      env: { PATH: "/usr/bin:/bin", HOME: this.root, LANG: "C.UTF-8", DENO_DIR: cacheDirectory },
      stdin: "piped",
      stdout: "piped",
      stderr: "piped",
    }).spawn();
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      try {
        child.kill("SIGKILL");
      } catch { /* 已退出的进程仍由 output 收齐。 */ }
    }, 15000);
    const output = child.output().finally(() => clearTimeout(timer));
    const writer = child.stdin.getWriter();
    try {
      await writer.write(new TextEncoder().encode(JSON.stringify({
        root: this.root,
        allowedRoots: roots,
        method,
        args,
      })));
      await writer.close();
    } catch (error) {
      try {
        child.kill("SIGKILL");
      } catch { /* 已结束则收齐原输出。 */ }
      await output;
      clearTimeout(timer);
      throw error;
    } finally {
      writer.releaseLock();
    }
    const result = await output.finally(() => clearTimeout(timer));
    if (timedOut) throw new KernelError("timeout", "低权限文件工具超时，结果未确认且不自动重放");
    if (!result.success) {
      throw new KernelError(
        "unavailable",
        `低权限文件工具启动或执行失败：${new TextDecoder().decode(result.stderr).slice(0, 2048)}`,
      );
    }
    const response = JSON.parse(new TextDecoder().decode(result.stdout)) as {
      value: T;
      error?: WireError;
    };
    if (response.error) throw new KernelError(response.error.kind, response.error.message);
    return response.value;
  }
  override read(args: Parameters<FileTools["read"]>[0]) {
    return this.#call<Awaited<ReturnType<FileTools["read"]>>>("read", args);
  }
  async readBytes(args: { path: string; maxBytes?: number }): Promise<Uint8Array> {
    return Buffer.from(await this.#call<string>("readBytes", args), "base64");
  }
  #change<T>(method: string, args: unknown): Promise<T> {
    const operation = this.#writes.then(() => this.#call<T>(method, args));
    this.#writes = operation.catch(() => {});
    return operation;
  }
  override write(args: Parameters<FileTools["write"]>[0]) {
    return this.#change<Awaited<ReturnType<FileTools["write"]>>>("write", args);
  }
  override replace(args: Parameters<FileTools["replace"]>[0]) {
    return this.#change<Awaited<ReturnType<FileTools["replace"]>>>("replace", args);
  }
  override list(args: Parameters<FileTools["list"]>[0] = {}) {
    return this.#call<Awaited<ReturnType<FileTools["list"]>>>("list", args);
  }
  override search(args: Parameters<FileTools["search"]>[0]) {
    return this.#call<Awaited<ReturnType<FileTools["search"]>>>("search", args);
  }
  override async close() {
    await this.#writes;
    await this.#operations;
    await super.close();
  }
}
