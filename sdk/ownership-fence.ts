import type {
  OwnershipExit,
  OwnershipPermit,
  OwnershipProof,
  PermitOutcome,
} from "../contracts/ownership.ts";
import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError, toWireError } from "../kernel/protocol.ts";
import { writeDurableJson } from "./durable-json.ts";

interface Pending {
  permit: OwnershipPermit;
  requestHash: string;
  phase: "requested" | "admitted" | "settled";
  outcome?: PermitOutcome;
  resultHash?: string;
}
interface FenceState {
  schema: 1;
  maxEpoch: number;
  pending: Pending[];
}
const HASH = /^[a-f0-9]{64}$/;
function id(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= 512;
}
function hash(value: unknown): value is string {
  return typeof value === "string" && HASH.test(value);
}
function epoch(value: unknown): value is number {
  return Number.isSafeInteger(value) && Number(value) >= 0;
}
function validPermit(p: OwnershipPermit, resource: string): boolean {
  return !!p && hash(p.id) && p.resource === resource && epoch(p.epoch) && id(p.actionId) &&
    id(p.instanceId);
}
function validState(s: FenceState, resource: string): boolean {
  if (!s || s.schema !== 1 || !epoch(s.maxEpoch) || !Array.isArray(s.pending)) return false;
  const ids = new Set<string>();
  for (const p of s.pending) {
    if (
      !p || !validPermit(p.permit, resource) || ids.has(p.permit.id) || !hash(p.requestHash) ||
      !["requested", "admitted", "settled"].includes(p.phase) ||
      (p.phase !== "requested" && p.permit.epoch > s.maxEpoch) ||
      (p.phase === "settled" &&
        (!hash(p.resultHash) ||
          !["completed", "failed", "unknown", "not_executed"].includes(p.outcome!))) ||
      (p.phase !== "settled" && (p.outcome !== undefined || p.resultHash !== undefined))
    ) return false;
    ids.add(p.permit.id);
  }
  return true;
}
/** 持真实出口许可至执行终结；调用方超时不释放。申请、号码与待结算状态均存盘。 */
export class OwnershipFence {
  #state: FenceState = { schema: 1, maxEpoch: 0, pending: [] };
  #lock?: Deno.FsFile;
  #initialized = false;
  #closing = false;
  #storageFailed = false;
  #tail: Promise<unknown> = Promise.resolve();
  #live = new Set<string>();
  #jobs = new Map<string, { hash: string; promise: Promise<Json> }>();
  #reconciling?: Promise<{ remaining: number }>;
  constructor(readonly directory: string, readonly resource: string, readonly exit: OwnershipExit) {
    if (!directory.startsWith("/") || !id(resource)) {
      throw new KernelError("rejected", "出口使用绝对路径与资源编号");
    }
  }
  get available() {
    return this.#lock !== undefined && !this.#storageFailed && !this.#closing &&
      !this.#state.pending.some((p) => !this.#live.has(p.permit.id));
  }
  async initialize() {
    if (this.#initialized) throw new KernelError("rejected", "出口已经启动或关闭");
    this.#initialized = true;
    await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
    if ((await Deno.lstat(this.directory)).isSymlink) {
      throw new KernelError("corrupt", "出口目录不能是符号链接");
    }
    for await (const file of Deno.readDir(this.directory)) {
      if (!file.isFile || !["writer.lock", "index.json"].includes(file.name)) {
        throw new KernelError("corrupt", "出口有未完成记录，原件保留");
      }
    }
    const lock = await Deno.open(`${this.directory}/writer.lock`, {
      create: true,
      read: true,
      write: true,
      mode: 0o600,
    });
    if (!(await lock.tryLock(true))) {
      lock.close();
      throw new KernelError("unavailable", "已有同一出口实例");
    }
    this.#lock = lock;
    try {
      const stat = await Deno.lstat(`${this.directory}/index.json`);
      if (!stat.isFile || stat.isSymlink || stat.size > 4_194_304) {
        throw new KernelError("corrupt", "出口原件异常");
      }
      const state = JSON.parse(await Deno.readTextFile(`${this.directory}/index.json`));
      if (!validState(state, this.resource)) {
        throw new KernelError("corrupt", "出口状态无效，原件保留");
      }
      this.#state = state;
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) {
        lock.close();
        this.#lock = undefined;
        throw error;
      }
    }
  }
  #change(edit: (state: FenceState) => void) {
    const operation = this.#tail.then(async () => {
      if (!this.#lock || this.#storageFailed) throw new KernelError("storage", "出口无法提交");
      const next = structuredClone(this.#state);
      edit(next);
      if (!validState(next, this.resource)) throw new KernelError("corrupt", "拒绝无效出口状态");
      if (new TextEncoder().encode(JSON.stringify(next)).length > 4_194_304) {
        throw new KernelError("storage", "出口记录超过当前格式，原件保留");
      }
      try {
        await writeDurableJson(`${this.directory}/index.json`, next);
      } catch {
        this.#storageFailed = true;
        throw new KernelError("storage", "出口提交失败，原件保留");
      }
      this.#state = next;
    });
    this.#tail = operation.catch(() => {});
    return operation;
  }
  async run<T extends Json>(
    proof: OwnershipProof,
    actionId: string,
    args: Json,
    operation: (checkpoint: () => Promise<void>) => Promise<T>,
  ): Promise<T> {
    if (
      !proof || !id(proof.instanceId) || !epoch(proof.epoch) ||
      typeof proof.credential !== "string" || !/^[a-zA-Z0-9_-]{32,256}$/.test(proof.credential) ||
      !id(actionId)
    ) throw new KernelError("rejected", "出口动作与证明无效");
    const captured = structuredClone(proof);
    const requestHash = await fingerprint(args),
      key = await fingerprint([captured.instanceId, captured.epoch, actionId]);
    if (!this.available) throw new KernelError("unavailable", "出口需先核对未结算记录");
    if (captured.epoch < this.#state.maxEpoch) {
      throw new KernelError("unavailable", "出口拒绝旧号码");
    }
    const live = this.#jobs.get(key);
    if (live) {
      if (live.hash !== requestHash) throw new KernelError("rejected", "同一动作不能改变参数");
      return await live.promise as T;
    }
    const work = this.#run(captured, actionId, requestHash, operation);
    this.#jobs.set(key, { hash: requestHash, promise: work });
    try {
      return await work;
    } finally {
      this.#jobs.delete(key);
    }
  }
  async #run<T extends Json>(
    proof: OwnershipProof,
    actionId: string,
    requestHash: string,
    operation: (checkpoint: () => Promise<void>) => Promise<T>,
  ): Promise<T> {
    const permit: OwnershipPermit = {
      id: await fingerprint(["exit-permit-v1", crypto.randomUUID()]),
      epoch: proof.epoch,
      instanceId: proof.instanceId,
      resource: this.resource,
      actionId,
    };
    this.#live.add(permit.id);
    try {
      await this.#change((state) =>
        state.pending.push({ permit, requestHash, phase: "requested" })
      );
      const admitted = await this.exit.begin({
        permitId: permit.id,
        proof,
        resource: this.resource,
        actionId,
        requestHash,
      });
      if (
        !validPermit(admitted, this.resource) ||
        Object.keys(permit).some((key) =>
          admitted[key as keyof OwnershipPermit] !== permit[key as keyof OwnershipPermit]
        )
      ) throw new KernelError("unknown", "守护许可与原申请不匹配");
      await this.#change((state) => {
        if (permit.epoch < state.maxEpoch) {
          throw new KernelError("unavailable", "出口已见过更新号码");
        }
        state.maxEpoch = permit.epoch;
        state.pending.find((p) => p.permit.id === permit.id)!.phase = "admitted";
      });
    } catch (error) {
      // 此处尚未调用真实动作。持久 requested 可在重启后取消，也封住迟到的 begin。
      try {
        if (this.#state.pending.some((p) => p.permit.id === permit.id)) {
          await this.exit.cancel(permit, requestHash);
          await this.#change((state) => {
            state.pending = state.pending.filter((p) => p.permit.id !== permit.id);
          });
        }
      } finally {
        this.#live.delete(permit.id);
      }
      throw error;
    }
    let value: T | undefined,
      failure: unknown,
      failed = false,
      outcome: PermitOutcome,
      resultHash: string;
    try {
      // 长重试链每次真正发网前复核同一许可；暂停、交接或失联不能继续下一次尝试。
      const checkpoint = async () => {
        if (this.#closing || this.#storageFailed) {
          throw new KernelError("unavailable", "出口已停止接续操作");
        }
        const admitted = await this.exit.begin({
          permitId: permit.id,
          proof,
          resource: this.resource,
          actionId,
          requestHash,
        });
        if (
          !validPermit(admitted, this.resource) ||
          Object.keys(permit).some((key) =>
            admitted[key as keyof OwnershipPermit] !== permit[key as keyof OwnershipPermit]
          )
        ) throw new KernelError("unknown", "续接许可与原申请不匹配");
      };
      value = await operation(checkpoint);
      outcome = "completed";
      resultHash = await fingerprint(value);
    } catch (error) {
      failure = error;
      failed = true;
      const wire = toWireError(error);
      outcome = ["unknown", "timeout", "storage"].includes(wire.kind) ? "unknown" : "failed";
      resultHash = await fingerprint(wire as unknown as Json);
    }
    try {
      await this.#change((state) => {
        const p = state.pending.find((p) => p.permit.id === permit.id)!;
        p.phase = "settled";
        p.outcome = outcome;
        p.resultHash = resultHash;
      });
      await this.exit.finish(permit.id, outcome, resultHash);
      await this.#change((state) => {
        state.pending = state.pending.filter((p) => p.permit.id !== permit.id);
      });
    } catch (error) {
      throw error instanceof KernelError && error.kind === "storage"
        ? error
        : new KernelError("unknown", "真实动作已终结，守护结算未确认；先查原记录");
    } finally {
      this.#live.delete(permit.id);
    }
    if (failed) throw failure;
    return value!;
  }
  reconcile(
    resolve?: (
      permit: OwnershipPermit,
    ) => Promise<{ outcome: PermitOutcome; resultHash: string } | undefined>,
  ) {
    if (this.#closing || !this.#lock || this.#storageFailed) {
      return Promise.reject(new KernelError("unavailable", "出口无法核对"));
    }
    if (this.#reconciling) return this.#reconciling;
    const work = this.#reconcile(resolve);
    this.#reconciling = work;
    void work.finally(() => {
      this.#reconciling = undefined;
    }).catch(() => {});
    return work;
  }
  async #reconcile(
    resolve?: (
      permit: OwnershipPermit,
    ) => Promise<{ outcome: PermitOutcome; resultHash: string } | undefined>,
  ) {
    for (const pending of [...this.#state.pending]) {
      if (this.#live.has(pending.permit.id)) continue;
      if (pending.phase === "requested") {
        await this.exit.cancel(pending.permit, pending.requestHash);
      } else {
        const known = pending.phase === "settled"
          ? { outcome: pending.outcome!, resultHash: pending.resultHash! }
          : await resolve?.(pending.permit);
        if (!known) continue;
        if (pending.phase === "admitted") {
          await this.#change((state) => {
            const p = state.pending.find((p) => p.permit.id === pending.permit.id)!;
            p.phase = "settled";
            p.outcome = known.outcome;
            p.resultHash = known.resultHash;
          });
        }
        await this.exit.finish(pending.permit.id, known.outcome, known.resultHash);
      }
      await this.#change((state) => {
        state.pending = state.pending.filter((p) => p.permit.id !== pending.permit.id);
      });
    }
    return { remaining: this.#state.pending.filter((p) => !this.#live.has(p.permit.id)).length };
  }
  pending() {
    return structuredClone(this.#state.pending);
  }
  async close() {
    this.#closing = true;
    await Promise.allSettled([...this.#jobs.values()].map((p) => p.promise));
    await this.#reconciling?.catch(() => {});
    await this.#tail;
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#storageFailed) throw new KernelError("storage", "出口保存失败");
  }
}
