import type {
  ExitPermitRequest,
  OwnershipExit,
  OwnershipPermit,
  PermitOutcome,
} from "../contracts/ownership.ts";
import { OwnershipExitClient, OwnershipRpcClient } from "../guardian/rpc.ts";
import { KernelError } from "../kernel/protocol.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";

/** 每次连接读取守护当前地址。守护重启换端口后，适配器不需要断开 QQ 网关。 */
export class FileOwnershipExit implements OwnershipExit {
  constructor(readonly connectionFile: string, readonly exitKeyFile: string) {
    if (![connectionFile, exitKeyFile].every((path) => path.startsWith("/"))) {
      throw new KernelError("rejected", "守护连接使用绝对路径");
    }
  }
  async #with<T>(call: (exit: OwnershipExitClient) => Promise<T>): Promise<T> {
    const [connection, secret] = await Promise.all([
      this.#read(this.connectionFile, 4096),
      this.#read(this.exitKeyFile, 512),
    ]);
    const { url } = JSON.parse(connection);
    const client = new OwnershipRpcClient(url, secret.trim());
    try {
      return await call(new OwnershipExitClient(client));
    } finally {
      client.close();
    }
  }
  async #read(path: string, limit: number) {
    const stat = await Deno.lstat(path);
    if (!stat.isFile || stat.isSymlink || stat.size > limit) {
      throw new KernelError("corrupt", "守护连接文件无效，原件保留");
    }
    return await Deno.readTextFile(path);
  }
  begin(request: ExitPermitRequest) {
    return this.#with((exit) => exit.begin(request));
  }
  cancel(permit: OwnershipPermit, requestHash: string) {
    return this.#with((exit) => exit.cancel(permit, requestHash));
  }
  finish(id: string, outcome: PermitOutcome, resultHash?: string) {
    return this.#with((exit) => exit.finish(id, outcome, resultHash));
  }
}
export async function readQqOwnershipFence(
  configPath: string,
  directory: string,
): Promise<OwnershipFence | undefined> {
  const config = JSON.parse(await Deno.readTextFile(configPath)).qqOwnership;
  // 老的独立接收探针与离线契约测试保持兼容；正式主程序组装必须配置此项。
  if (config === undefined) return undefined;
  if (
    !config || typeof config.guardianConnectionFile !== "string" ||
    typeof config.exitKeyFile !== "string"
  ) throw new KernelError("rejected", "QQ 所有权组合配置无效");
  const fence = new OwnershipFence(
    `${directory}/ownership`,
    "phone",
    new FileOwnershipExit(config.guardianConnectionFile, config.exitKeyFile),
  );
  await fence.initialize();
  return fence;
}
