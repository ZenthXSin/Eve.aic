import type { OwnershipOwner, OwnershipPermit, PermitOutcome } from "../contracts/ownership.ts";
import { fingerprint } from "../kernel/calls.ts";
import type { Json } from "../kernel/protocol.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import { KernelError } from "../kernel/protocol.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

export interface OwnershipState {
  schema: 1;
  epoch: number;
  mode: "idle" | "active" | "draining" | "paused";
  owner: OwnershipOwner | null;
  instances: { id: string; sourceCommit: string; credentialHash: string }[];
  requests: { id: string; method: "grant" | "pause"; hash: string; result?: unknown }[];
  pending: { requestId: string; instanceId: string } | null;
  cancellations: (OwnershipPermit & { requestHash: string })[];
  permits: (OwnershipPermit & {
    requestHash: string;
    startedAt: number;
    outcome?: PermitOutcome;
    finishedAt?: number;
    resultHash?: string;
  })[];
  events: { seq: number; at: number; kind: string; epoch: number; requestId?: string }[];
}
const HASH = /^[a-f0-9]{64}$/;
export function validEpoch(value: unknown): value is number {
  return Number.isSafeInteger(value) && Number(value) >= 0;
}
export function validId(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= 512;
}
function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function hash(value: unknown): value is string {
  return typeof value === "string" && HASH.test(value);
}
function owner(value: unknown, instances: Set<string>, maximum: number): value is OwnershipOwner {
  return record(value) && typeof value.instanceId === "string" && instances.has(value.instanceId) &&
    validEpoch(value.epoch) && value.epoch <= maximum && validEpoch(value.expiresAt);
}
export function validateOwnershipState(value: unknown): value is OwnershipState {
  if (!record(value)) return false;
  const s = value as unknown as OwnershipState;
  if (
    s.schema !== 1 || !validEpoch(s.epoch) ||
    !["idle", "active", "draining", "paused"].includes(s.mode) ||
    !Array.isArray(s.instances) || !Array.isArray(s.requests) || !Array.isArray(s.permits) ||
    !Array.isArray(s.events) || !Array.isArray(s.cancellations)
  ) return false;
  const instances = new Set<string>(), requests = new Set<string>(), permits = new Set<string>();
  for (const i of s.instances) {
    if (
      !record(i) || !validId(i.id) || instances.has(i.id) || typeof i.sourceCommit !== "string" ||
      !/^[a-f0-9]{40}$/.test(i.sourceCommit) || !hash(i.credentialHash)
    ) return false;
    instances.add(i.id);
  }
  if (s.owner !== null && (!owner(s.owner, instances, s.epoch) || s.owner.epoch !== s.epoch)) {
    return false;
  }
  if (
    (s.mode === "active" && s.owner === null) ||
    ((s.mode === "idle" || s.mode === "paused") && s.owner !== null)
  ) return false;
  for (const r of s.requests) {
    if (
      !record(r) || !validId(r.id) || requests.has(r.id) ||
      !["grant", "pause"].includes(String(r.method)) || !hash(r.hash)
    ) return false;
    requests.add(r.id);
    if (r.method === "pause") {
      if (
        !record(r.result) || r.result.paused !== true || !validEpoch(r.result.epoch) ||
        r.result.epoch > s.epoch || !validEpoch(r.result.activePermits)
      ) return false;
    } else if (r.result !== undefined) {
      if (
        !record(r.result) ||
        (r.result.status !== "interrupted" &&
          (r.result.status !== "granted" || !owner(r.result.owner, instances, s.epoch)))
      ) return false;
    }
  }
  const unresolved = s.requests.filter((r) => r.result === undefined);
  if (s.pending !== null) {
    if (
      !record(s.pending) || !validId(s.pending.requestId) || !requests.has(s.pending.requestId) ||
      !validId(s.pending.instanceId) || !instances.has(s.pending.instanceId) ||
      s.mode !== "draining"
    ) return false;
    if (
      unresolved.length !== 1 || unresolved[0].id !== s.pending.requestId ||
      unresolved[0].method !== "grant"
    ) return false;
  } else if (s.mode === "draining" || unresolved.length) return false;
  const activeActions = new Set<string>();
  for (const p of s.permits) {
    if (
      !record(p) || !hash(p.id) || permits.has(p.id) || !validId(p.instanceId) ||
      !instances.has(p.instanceId) || !validEpoch(p.epoch) || p.epoch > s.epoch ||
      !validId(p.resource) || !validId(p.actionId) || !hash(p.requestHash) ||
      !validEpoch(p.startedAt) ||
      (p.outcome !== undefined &&
        !["completed", "failed", "unknown", "not_executed"].includes(String(p.outcome))) ||
      (p.outcome === undefined && (p.finishedAt !== undefined || p.resultHash !== undefined)) ||
      (p.outcome !== undefined && (!validEpoch(p.finishedAt) || p.finishedAt < p.startedAt)) ||
      (p.resultHash !== undefined && !hash(p.resultHash))
    ) return false;
    permits.add(p.id);
    if (p.outcome === undefined) {
      const key = JSON.stringify([p.resource, p.actionId]);
      if (activeActions.has(key)) return false;
      activeActions.add(key);
    }
  }
  const cancelled = new Set<string>();
  for (const p of s.cancellations) {
    if (
      !record(p) || !hash(p.id) || permits.has(p.id) || cancelled.has(p.id) ||
      !validId(p.instanceId) || !validEpoch(p.epoch) || !validId(p.resource) ||
      !validId(p.actionId) || !hash(p.requestHash)
    ) return false;
    cancelled.add(p.id);
  }
  let eventEpoch = 0;
  for (const [index, e] of s.events.entries()) {
    if (
      !record(e) || e.seq !== index + 1 || !validEpoch(e.at) || !validEpoch(e.epoch) ||
      e.epoch > s.epoch || e.epoch < eventEpoch || !validId(e.kind) ||
      (e.requestId !== undefined && (!validId(e.requestId) || !requests.has(e.requestId)))
    ) return false;
    eventEpoch = e.epoch;
  }
  return true;
}

export class OwnershipStore {
  state: OwnershipState = {
    schema: 1,
    epoch: 0,
    mode: "idle",
    owner: null,
    instances: [],
    requests: [],
    pending: null,
    permits: [],
    cancellations: [],
    events: [],
  };
  #lock?: Deno.FsFile;
  #failed = false;
  #initialized = false;
  #closing = false;
  #tail: Promise<unknown> = Promise.resolve();
  #changes: FactJournal;
  #permitPositions = new Map<string, number>();
  #cancelPositions = new Map<string, number>();
  constructor(readonly directory: string) {
    this.#changes = new FactJournal(`${directory}/changes`);
  }
  #control(state: OwnershipState) {
    const { permits: _permits, cancellations: _cancellations, ...control } = state;
    return control;
  }
  #apply(data: Record<string, Json>) {
    if (data.control) {
      const control = data.control as unknown as Omit<OwnershipState, "permits" | "cancellations">;
      this.state = {
        ...control,
        permits: this.state.permits,
        cancellations: this.state.cancellations,
      };
    } else {
      if (typeof data.id !== "string" || (!data.permit && !data.cancellation)) {
        throw new KernelError("corrupt", "守护增量记录无效");
      }
      for (
        const [key, positions] of [
          ["permits", this.#permitPositions],
          ["cancellations", this.#cancelPositions],
        ] as const
      ) {
        const value = data[key === "permits" ? "permit" : "cancellation"];
        if (!value) continue;
        if (typeof value !== "object" || Array.isArray(value) || value.id !== data.id) {
          throw new KernelError("corrupt", "守护增量许可编号不符");
        }
        const index = positions.get(data.id) ?? this.state[key].length;
        // Both arrays hold permits; their full invariants are checked before admission/recovery.
        (this.state[key] as unknown[])[index] = value;
        positions.set(data.id, index);
      }
    }
  }
  get available() {
    return this.#lock !== undefined && !this.#failed;
  }
  async initialize() {
    if (this.#initialized) throw new KernelError("rejected", "守护存储已经启动或关闭");
    this.#initialized = true;
    await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
    if ((await Deno.lstat(this.directory)).isSymlink) {
      throw new KernelError("corrupt", "守护目录不能是符号链接");
    }
    try {
      const stat = await Deno.lstat(`${this.directory}/writer.lock`);
      if (!stat.isFile || stat.isSymlink) throw new KernelError("corrupt", "守护锁不是独立文件");
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    const lock = await Deno.open(`${this.directory}/writer.lock`, {
      create: true,
      read: true,
      write: true,
      mode: 0o600,
    });
    if (!(await lock.tryLock(true))) {
      lock.close();
      throw new KernelError("unavailable", "已有守护仲裁实例");
    }
    this.#lock = lock;
    try {
      for await (const file of Deno.readDir(this.directory)) {
        if (file.name === "changes" && file.isDirectory) continue;
        if (!file.isFile || !["writer.lock", "index.json", "connection.json"].includes(file.name)) {
          throw new KernelError("corrupt", "守护有未完成文件，原件保留");
        }
      }
      try {
        const stat = await Deno.lstat(`${this.directory}/index.json`);
        if (!stat.isFile || stat.isSymlink || stat.size > 33_554_432) {
          throw new KernelError("corrupt", "守护原件损坏或过大");
        }
        const value = JSON.parse(await Deno.readTextFile(`${this.directory}/index.json`));
        if (!validateOwnershipState(value)) {
          throw new KernelError("corrupt", "所有权状态无效，原件保留");
        }
        this.state = value;
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
        // A ledger can never be attached to a fabricated or missing checkpoint.
        try {
          await Deno.lstat(`${this.directory}/changes`);
          throw new KernelError("corrupt", "守护检查点缺失，增量原件保留");
        } catch (missing) {
          if (!(missing instanceof Deno.errors.NotFound)) throw missing;
        }
        await writeDurableJson(`${this.directory}/index.json`, this.state);
      }
      this.state.permits.forEach((p, i) => this.#permitPositions.set(p.id, i));
      this.state.cancellations.forEach((p, i) => this.#cancelPositions.set(p.id, i));
      await this.#changes.initialize();
      const digest = await fingerprint(this.state as unknown as Json);
      if (!this.#changes.count) await this.#changes.append("checkpoint", { digest });
      else {
        const first = this.#changes.events[0];
        if (first.kind !== "checkpoint" || (first.data as { digest?: string }).digest !== digest) {
          throw new KernelError("corrupt", "守护检查点与增量链不符，原件保留");
        }
        let after = 1;
        while (after < this.#changes.count) {
          const page = this.#changes.readPage(after, 128);
          for (const event of page.events) {
            if (event.kind !== "change" || !record(event.data)) {
              throw new KernelError("corrupt", "未知守护增量记录");
            }
            this.#apply(event.data as Record<string, Json>);
          }
          after = page.nextSeq;
        }
        if (!validateOwnershipState(this.state)) {
          throw new KernelError("corrupt", "守护增量恢复状态无效，原件保留");
        }
      }
    } catch (error) {
      await this.#changes.close().catch(() => {});
      lock.close();
      this.#lock = undefined;
      throw error;
    }
  }
  /** Control changes clone only metadata; an exit changes exactly one indexed permit.
   * The legacy checkpoint remains intact. Every accepted delta is fsynced in a chained
   * append-only ledger before becoming visible, including lease renewal and terminal receipts.
   */
  change<T>(edit: (state: OwnershipState) => T, permitId?: string): Promise<T> {
    if (this.#closing) return Promise.reject(new KernelError("unavailable", "守护正在关闭"));
    const operation = this.#tail.then(async () => {
      if (!this.available) throw new KernelError("storage", "守护不可提交");
      let next: OwnershipState;
      if (permitId === undefined) {
        next = {
          ...structuredClone(this.#control(this.state)),
          permits: this.state.permits,
          cancellations: this.state.cancellations,
        };
      } else {
        next = {
          ...this.state,
          permits: this.state.permits.slice(),
          cancellations: this.state.cancellations.slice(),
        };
        const pi = this.#permitPositions.get(permitId), ci = this.#cancelPositions.get(permitId);
        if (pi !== undefined) next.permits[pi] = structuredClone(next.permits[pi]);
        if (ci !== undefined) next.cancellations[ci] = structuredClone(next.cancellations[ci]);
      }
      const result = edit(next);
      const permit = permitId === undefined
        ? undefined
        : next.permits[this.#permitPositions.get(permitId) ?? this.state.permits.length];
      const cancellation = permitId === undefined ? undefined : next
        .cancellations[this.#cancelPositions.get(permitId) ?? this.state.cancellations.length];
      const checked = {
        ...next,
        permits: permit ? [permit] : [],
        cancellations: cancellation ? [cancellation] : [],
      };
      if (!validateOwnershipState(checked)) {
        throw new KernelError("corrupt", "拒绝无效的所有权状态");
      }
      const data = permitId === undefined
        ? { control: this.#control(next) }
        : { id: permitId, permit: permit ?? null, cancellation: cancellation ?? null };
      try {
        await this.#changes.append("change", data as unknown as Json);
      } catch {
        this.#failed = true;
        throw new KernelError("storage", "守护提交失败，原件保留");
      }
      this.state = next;
      if (permit) this.#permitPositions.set(permit.id, next.permits.indexOf(permit));
      if (cancellation) {
        this.#cancelPositions.set(cancellation.id, next.cancellations.indexOf(cancellation));
      }
      return result;
    });
    this.#tail = operation.catch(() => {});
    return operation;
  }
  async close() {
    this.#closing = true;
    await this.#tail;
    await this.#changes.close();
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failed) throw new KernelError("storage", "守护关闭时有未完成写入");
  }
}
