import type {
  ExitPermitRequest,
  OwnershipGrant,
  OwnershipOwner,
  OwnershipPermit,
  OwnershipProof,
  OwnershipSnapshot,
  PermitOutcome,
} from "../contracts/ownership.ts";
import { fingerprint } from "../kernel/calls.ts";
import { KernelError } from "../kernel/protocol.ts";
import { type OwnershipState, OwnershipStore, validEpoch, validId } from "./ownership-store.ts";

const hashPattern = /^[a-f0-9]{64}$/;
const credentialPattern = /^[a-zA-Z0-9_-]{32,256}$/;
export function validProof(value: unknown): value is OwnershipProof {
  if (value === null || typeof value !== "object") return false;
  const proof = value as OwnershipProof;
  return validId(proof.instanceId) && validEpoch(proof.epoch) &&
    typeof proof.credential === "string" && credentialPattern.test(proof.credential);
}
function nextEpoch(state: OwnershipState) {
  if (state.epoch >= Number.MAX_SAFE_INTEGER) {
    throw new KernelError("storage", "所有权号码已到格式上限，不能回绕");
  }
  return ++state.epoch;
}
function event(state: OwnershipState, now: number, kind: string, requestId?: string) {
  state.events.push({
    seq: state.events.length + 1,
    at: now,
    kind,
    epoch: state.epoch,
    ...(requestId ? { requestId } : {}),
  });
}
export class OwnershipAuthority {
  readonly store: OwnershipStore;
  constructor(directory: string, readonly leaseMs: number, readonly now = Date.now) {
    if (!Number.isSafeInteger(leaseMs) || leaseMs < 1) {
      throw new KernelError("rejected", "租约长度无效");
    }
    this.store = new OwnershipStore(directory);
  }
  async initialize() {
    await this.store.initialize();
    try {
      await this.store.change((state) => {
        nextEpoch(state);
        // 守护中断后旧租约失效；已开始动作仍在，不能用重启抹掉排空义务。
        state.owner = null;
        if (state.pending) {
          state.requests.find((r) => r.id === state.pending!.requestId)!.result = {
            status: "interrupted",
          };
          state.pending = null;
        }
        if (state.mode !== "paused") state.mode = "idle";
        event(state, this.now(), "guardian_started");
      });
    } catch (error) {
      await this.store.close().catch(() => {});
      throw error;
    }
  }
  status(): OwnershipSnapshot {
    const s = this.store.state;
    return {
      epoch: s.epoch,
      mode: s.mode,
      owner: structuredClone(s.owner),
      activePermits: s.permits.filter((p) => p.outcome === undefined).length,
      pendingRequest: s.pending?.requestId ?? null,
    };
  }
  #authenticated(state: OwnershipState, proof: OwnershipProof, credentialHash: string) {
    const instance = state.instances.find((i) => i.id === proof.instanceId);
    if (!instance || instance.credentialHash !== credentialHash) {
      throw new KernelError("rejected", "实例凭据无效");
    }
  }
  #online(state: OwnershipState, proof: OwnershipProof, credentialHash: string) {
    this.#authenticated(state, proof, credentialHash);
    if (
      state.mode !== "active" || state.owner?.instanceId !== proof.instanceId ||
      proof.epoch !== state.epoch || state.owner.expiresAt <= this.now()
    ) {
      throw new KernelError("unavailable", "实例不持有当前有效在线租约");
    }
  }
  async register(args: { instanceId: string; sourceCommit: string; credential: string }) {
    if (
      !args || !validId(args.instanceId) || typeof args.sourceCommit !== "string" ||
      !/^[a-f0-9]{40}$/.test(args.sourceCommit) || typeof args.credential !== "string" ||
      !credentialPattern.test(args.credential)
    ) throw new KernelError("rejected", "需要真实实例、源码提交和独立凭据");
    const credentialHash = await fingerprint(["instance-credential-v1", args.credential]);
    await this.store.change((state) => {
      const old = state.instances.find((i) => i.id === args.instanceId);
      if (old) {
        if (old.sourceCommit !== args.sourceCommit || old.credentialHash !== credentialHash) {
          throw new KernelError("rejected", "实例编号不能换源码或凭据");
        }
        return;
      }
      state.instances.push({
        id: args.instanceId,
        sourceCommit: args.sourceCommit,
        credentialHash,
      });
      event(state, this.now(), "instance_registered");
    });
    return { registered: true };
  }
  async renew(proof: OwnershipProof): Promise<OwnershipOwner> {
    if (!validProof(proof)) throw new KernelError("rejected", "租约证明格式无效");
    const credentialHash = await fingerprint(["instance-credential-v1", proof.credential]);
    return await this.store.change((state) => {
      this.#online(state, proof, credentialHash);
      state.owner!.expiresAt = this.now() + this.leaseMs;
      return structuredClone(state.owner!);
    });
  }
  async grant(
    args: OwnershipGrant,
  ): Promise<
    { status: "granted"; owner: OwnershipOwner } | { status: "draining"; activePermits: number } | {
      status: "interrupted";
    }
  > {
    if (
      !args || !validId(args.instanceId) || !validId(args.requestId) ||
      !validEpoch(args.expectedEpoch) ||
      (args.resumePaused !== undefined && typeof args.resumePaused !== "boolean")
    ) throw new KernelError("rejected", "交接参数无效");
    const hash = await fingerprint({ ...args, resumePaused: args.resumePaused ?? false });
    return await this.store.change((state) => {
      if (!state.instances.some((i) => i.id === args.instanceId)) {
        throw new KernelError("not_found", "候选实例未登记");
      }
      let request = state.requests.find((r) => r.id === args.requestId);
      if (request) {
        if (request.method !== "grant" || request.hash !== hash) {
          throw new KernelError("rejected", "同一交接编号不能改变目标或条件");
        }
        if (request.result !== undefined) {
          return structuredClone(request.result) as { status: "granted"; owner: OwnershipOwner } | {
            status: "interrupted";
          };
        }
      } else {
        if (state.pending) throw new KernelError("unavailable", "另一次交接尚在排空");
        if (state.epoch !== args.expectedEpoch) {
          throw new KernelError("rejected", "所有权已变化，请核对当前状态");
        }
        if (state.mode === "paused" && args.resumePaused !== true) {
          throw new KernelError("unavailable", "主人已暂停，不能自动接管恢复");
        }
        request = { id: args.requestId, method: "grant", hash };
        state.requests.push(request);
        state.pending = { requestId: args.requestId, instanceId: args.instanceId };
        state.mode = "draining";
        event(state, this.now(), "handover_draining", args.requestId);
      }
      const activePermits = state.permits.filter((p) => p.outcome === undefined).length;
      if (activePermits) return { status: "draining" as const, activePermits };
      const owner = {
        instanceId: args.instanceId,
        epoch: nextEpoch(state),
        expiresAt: this.now() + this.leaseMs,
      };
      state.owner = owner;
      state.mode = "active";
      state.pending = null;
      const result = { status: "granted" as const, owner: structuredClone(owner) };
      request.result = result;
      event(state, this.now(), "owner_granted", args.requestId);
      return result;
    });
  }
  async pause(
    args: { requestId: string; expectedEpoch: number },
  ): Promise<{ paused: true; epoch: number; activePermits: number }> {
    if (!args || !validId(args.requestId) || !validEpoch(args.expectedEpoch)) {
      throw new KernelError("rejected", "暂停参数无效");
    }
    const hash = await fingerprint(args);
    return await this.store.change((state) => {
      const old = state.requests.find((r) => r.id === args.requestId);
      if (old) {
        if (old.method !== "pause" || old.hash !== hash) {
          throw new KernelError("rejected", "同一暂停编号不能改变条件");
        }
        return structuredClone(old.result) as {
          paused: true;
          epoch: number;
          activePermits: number;
        };
      }
      if (state.epoch !== args.expectedEpoch) throw new KernelError("rejected", "所有权已变化");
      nextEpoch(state);
      if (state.pending) {
        state.requests.find((r) => r.id === state.pending!.requestId)!.result = {
          status: "interrupted",
        };
      }
      state.pending = null;
      state.owner = null;
      state.mode = "paused";
      const result = {
        paused: true as const,
        epoch: state.epoch,
        activePermits: state.permits.filter((p) => !p.outcome).length,
      };
      state.requests.push({ id: args.requestId, method: "pause", hash, result });
      event(state, this.now(), "owner_paused", args.requestId);
      return result;
    });
  }
  async begin(request: ExitPermitRequest) {
    if (
      !request || !validProof(request.proof) || !validId(request.resource) ||
      !validId(request.actionId) || typeof request.permitId !== "string" ||
      !hashPattern.test(request.permitId) || typeof request.requestHash !== "string" ||
      !hashPattern.test(request.requestHash)
    ) throw new KernelError("rejected", "出口许可请求无效");
    const credentialHash = await fingerprint(["instance-credential-v1", request.proof.credential]);
    const id = request.permitId;
    return await this.store.change((state) => {
      if (state.cancellations.some((p) => p.id === id)) {
        throw new KernelError("rejected", "出口已取消此申请，不能接受迟到请求");
      }
      const old = state.permits.find((p) => p.id === id);
      if (
        old &&
        (old.instanceId !== request.proof.instanceId || old.epoch !== request.proof.epoch ||
          old.resource !== request.resource || old.actionId !== request.actionId ||
          old.requestHash !== request.requestHash)
      ) throw new KernelError("rejected", "同一许可编号不能改变申请");
      if (old?.outcome !== undefined) {
        throw new KernelError("rejected", "此许可已终结，不能重新执行");
      }
      this.#online(state, request.proof, credentialHash);
      if (old) {
        return {
          id: old.id,
          epoch: old.epoch,
          instanceId: old.instanceId,
          resource: old.resource,
          actionId: old.actionId,
        };
      }
      if (
        state.permits.some((p) =>
          p.resource === request.resource && p.actionId === request.actionId &&
          p.requestHash !== request.requestHash
        )
      ) throw new KernelError("rejected", "原动作编号已有不同参数，不能换版重写");
      if (
        state.permits.some((p) =>
          p.resource === request.resource && p.actionId === request.actionId &&
          p.outcome === undefined
        )
      ) throw new KernelError("unavailable", "原动作尚未终结，请查询已有结果");
      const permit = {
        id,
        epoch: request.proof.epoch,
        instanceId: request.proof.instanceId,
        resource: request.resource,
        actionId: request.actionId,
      };
      state.permits.push({ ...permit, requestHash: request.requestHash, startedAt: this.now() });
      return permit;
    });
  }
  async cancel(permit: OwnershipPermit, requestHash: string) {
    if (
      !permit || typeof permit.id !== "string" || !hashPattern.test(permit.id) ||
      !validEpoch(permit.epoch) || !validId(permit.instanceId) || !validId(permit.resource) ||
      !validId(permit.actionId) || typeof requestHash !== "string" || !hashPattern.test(requestHash)
    ) throw new KernelError("rejected", "取消申请记录无效");
    await this.store.change((state) => {
      const cancelled = state.cancellations.find((p) => p.id === permit.id);
      if (cancelled) {
        if (
          cancelled.instanceId !== permit.instanceId || cancelled.epoch !== permit.epoch ||
          cancelled.resource !== permit.resource || cancelled.actionId !== permit.actionId ||
          cancelled.requestHash !== requestHash
        ) throw new KernelError("rejected", "取消编号不能改写");
        return;
      }
      const old = state.permits.find((p) => p.id === permit.id);
      if (
        old &&
        (old.instanceId !== permit.instanceId || old.epoch !== permit.epoch ||
          old.resource !== permit.resource || old.actionId !== permit.actionId ||
          old.requestHash !== requestHash)
      ) throw new KernelError("rejected", "取消不能改变原申请");
      if (old?.outcome !== undefined) {
        if (old.outcome !== "not_executed") {
          throw new KernelError("rejected", "已执行的许可不能声称未执行");
        }
        return;
      }
      const now = this.now();
      if (old) {
        old.outcome = "not_executed";
        old.finishedAt = Math.max(old.startedAt, now);
      } else state.cancellations.push({ ...permit, requestHash });
    });
  }
  async finish(id: string, outcome: PermitOutcome, resultHash?: string) {
    if (
      !hashPattern.test(id) ||
      !["completed", "failed", "unknown", "not_executed"].includes(outcome) ||
      (resultHash !== undefined && !hashPattern.test(resultHash))
    ) throw new KernelError("rejected", "出口终态无效");
    await this.store.change((state) => {
      const permit = state.permits.find((p) => p.id === id);
      if (!permit) throw new KernelError("not_found", "没有该出口许可");
      if (permit.outcome !== undefined) {
        if (permit.outcome !== outcome || permit.resultHash !== resultHash) {
          throw new KernelError("rejected", "真实终态不能被另一结果覆盖");
        }
        return;
      }
      permit.outcome = outcome;
      permit.finishedAt = Math.max(permit.startedAt, this.now());
      if (resultHash !== undefined) permit.resultHash = resultHash;
    });
  }
  outstanding() {
    return structuredClone(this.store.state.permits.filter((p) => p.outcome === undefined));
  }
  async actorStatus(proof: OwnershipProof) {
    if (!validProof(proof)) throw new KernelError("rejected", "实例证明格式无效");
    this.#authenticated(
      this.store.state,
      proof,
      await fingerprint(["instance-credential-v1", proof.credential]),
    );
    return this.status();
  }
  close() {
    return this.store.close();
  }
}
